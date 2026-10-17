//! Hand-off from the PipeWire real-time thread to the encode thread: a
//! preallocated, lock-free single-producer single-consumer ring of samples,
//! with a ring of per-buffer marks (capture time, length) beside it.
//!
//! The producer side is real-time safe: no allocation, no lock, no logging.
//! Its only syscall is a futex wake of the encode thread (as PipeWire's own
//! eventfd writes), once the ring holds what the encode thread needs to
//! complete its next 20 ms block: it wakes once per block whatever the
//! quantum, as soon as it can work. A buffer that does not fit is dropped
//! whole and counted: the timeline fills the hole with silence, and the
//! encode thread logs the count.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence};
use std::sync::{Arc, OnceLock};
use std::thread::Thread;
use std::time::Duration;

use crate::{CHANNELS, FRAME, SAMPLE_RATE};

/// Room for this much audio, whatever the quantum: an encode thread held up
/// this long (a disk stall under the muxer's lock) loses nothing.
const RING_SECONDS: usize = 2;
/// Marks: one per PipeWire buffer, so more than `RING_SECONDS` of the
/// smallest quantum (32 frames: 1500 per second).
const MARKS: usize = 4_096;

/// A fixed-capacity single-producer single-consumer queue of `T`. One slot
/// stays empty, so `head == tail` is empty and full is never ambiguous.
struct Queue<T> {
    slots: Box<[UnsafeCell<T>]>,
    /// Next slot to write: stored by the producer only.
    head: AtomicUsize,
    /// Next slot to read: stored by the consumer only.
    tail: AtomicUsize,
}

// SAFETY: a slot is written by the producer only while it is free (between
// `head` and `tail`) and read by the consumer only once published by `head`
// (Release/Acquire): never both at once. `Ring` hands out one producer and
// one consumer.
unsafe impl<T: Send> Sync for Queue<T> {}

impl<T: Copy + Default> Queue<T> {
    fn new(capacity: usize) -> Self {
        Self {
            slots: (0..=capacity)
                .map(|_| UnsafeCell::new(T::default()))
                .collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    fn len_between(&self, head: usize, tail: usize) -> usize {
        (head + self.slots.len() - tail) % self.slots.len()
    }

    /// Room left, for the producer.
    fn free(&self) -> usize {
        let len = self.len_between(
            self.head.load(Ordering::Relaxed),
            self.tail.load(Ordering::Acquire),
        );
        self.slots.len() - 1 - len
    }

    /// Elements waiting, for the consumer.
    fn len(&self) -> usize {
        self.len_between(
            self.head.load(Ordering::Acquire),
            self.tail.load(Ordering::Relaxed),
        )
    }

    /// Appends `n` elements, element `i` being `f(i)`.
    ///
    /// # Safety
    /// Producer only, with `n <= self.free()`.
    unsafe fn push(&self, n: usize, mut f: impl FnMut(usize) -> T) {
        let head = self.head.load(Ordering::Relaxed);
        let cap = self.slots.len();
        // Two runs: up to the end of the slots, then from the start.
        let first = n.min(cap - head);
        for (i, slot) in self.slots[head..head + first].iter().enumerate() {
            // SAFETY: a free slot, see `Sync`.
            unsafe { *slot.get() = f(i) };
        }
        for (i, slot) in self.slots[..n - first].iter().enumerate() {
            // SAFETY: as above.
            unsafe { *slot.get() = f(first + i) };
        }
        self.head.store((head + n) % cap, Ordering::Release);
    }

    /// Moves `n` elements to the end of `out`.
    ///
    /// # Safety
    /// Consumer only, with `n <= self.len()`.
    unsafe fn pop(&self, n: usize, out: &mut Vec<T>) {
        let tail = self.tail.load(Ordering::Relaxed);
        let cap = self.slots.len();
        let first = n.min(cap - tail);
        // SAFETY: published slots, see `Sync`.
        let get = |s: &UnsafeCell<T>| unsafe { *s.get() };
        out.extend(self.slots[tail..tail + first].iter().map(get));
        out.extend(self.slots[..n - first].iter().map(get));
        self.tail.store((tail + n) % cap, Ordering::Release);
    }

    /// Removes the oldest element.
    ///
    /// # Safety
    /// Consumer only, with `self.len() > 0`.
    unsafe fn pop_one(&self) -> T {
        let tail = self.tail.load(Ordering::Relaxed);
        // SAFETY: a published slot, see `Sync`.
        let value = unsafe { *self.slots[tail].get() };
        self.tail
            .store((tail + 1) % self.slots.len(), Ordering::Release);
        value
    }
}

/// What the producer and the consumer share.
struct Ring {
    /// Interleaved stereo F32.
    samples: Queue<f32>,
    /// Per buffer: capture time of its first frame (ns on `CLOCK_MONOTONIC`)
    /// and its length in frames. Pushed after its samples.
    marks: Queue<(u64, u32)>,
    /// Frames dropped since the consumer last took the count.
    dropped: AtomicU64,
    /// A producer exists: there is only ever one at a time.
    producing: AtomicBool,
    /// The writer is gone: once drained, no more samples come.
    closed: AtomicBool,
    /// The consumer's thread, once it waited: who to wake.
    consumer: OnceLock<Thread>,
    /// Frames the consumer waits for: it is woken once the ring holds them.
    need: AtomicUsize,
}

impl Ring {
    fn wake(&self) {
        if let Some(t) = self.consumer.get() {
            t.unpark();
        }
    }

    /// Frames waiting.
    fn frames(&self) -> usize {
        self.samples.len() / usize::from(CHANNELS)
    }
}

/// A new ring holding `RING_SECONDS` of audio, and its two ends.
pub(crate) fn ring() -> (Writer, Reader) {
    let frames = RING_SECONDS * SAMPLE_RATE as usize;
    ring_of(frames, MARKS)
}

/// A ring of `frames` stereo frames and `marks` buffers.
fn ring_of(frames: usize, marks: usize) -> (Writer, Reader) {
    let ring = Arc::new(Ring {
        samples: Queue::new(frames * usize::from(CHANNELS)),
        marks: Queue::new(marks),
        dropped: AtomicU64::new(0),
        producing: AtomicBool::new(false),
        closed: AtomicBool::new(false),
        consumer: OnceLock::new(),
        need: AtomicUsize::new(FRAME),
    });
    (Writer(ring.clone()), Reader(ring))
}

/// The producer's end, kept across reconnections. Dropping it closes the
/// ring: the reader drains what is left, then ends.
pub(crate) struct Writer(Arc<Ring>);

impl Writer {
    /// The one producer, for one stream at a time: `None` while the
    /// previous one is alive.
    pub(crate) fn producer(&self) -> Option<Producer> {
        (!self.0.producing.swap(true, Ordering::Acquire)).then(|| Producer {
            ring: self.0.clone(),
        })
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.0.closed.store(true, Ordering::Release);
        // Pairs with `Reader::wait`, as in `Producer::due`.
        fence(Ordering::SeqCst);
        self.0.wake();
    }
}

/// Pushes buffers from the real-time thread. See the module docs.
pub(crate) struct Producer {
    ring: Arc<Ring>,
}

impl Producer {
    /// Pushes a buffer of `frames` stereo frames captured from `start`,
    /// sample `i` (interleaved) being `sample(i)`. Returns false, with the
    /// buffer dropped and counted, when the ring is full.
    pub(crate) fn push(
        &mut self,
        start: Duration,
        frames: usize,
        sample: impl FnMut(usize) -> f32,
    ) -> bool {
        let ring = &*self.ring;
        let n = frames * usize::from(CHANNELS);
        if ring.marks.free() == 0 || ring.samples.free() < n {
            ring.dropped.fetch_add(frames as u64, Ordering::Relaxed);
            return false;
        }
        // SAFETY: the only producer (`producing`), room checked above; free
        // room only grows meanwhile. Samples before their mark.
        unsafe {
            ring.samples.push(n, sample);
            ring.marks
                .push(1, |_| (start.as_nanos() as u64, frames as u32));
        }
        if self.due() {
            // Again on each push until the consumer runs: a no-op (no
            // syscall) while it is not parked.
            ring.wake();
        }
        true
    }

    /// Whether the ring holds what the consumer waits for.
    fn due(&self) -> bool {
        // Pairs with `Reader::wait`: either it sees our samples, or we see
        // its `need`.
        fence(Ordering::SeqCst);
        self.ring.frames() >= self.ring.need.load(Ordering::Relaxed)
    }
}

impl Drop for Producer {
    fn drop(&mut self) {
        // What it pushed last must not wait for the next stream.
        self.ring.wake();
        self.ring.producing.store(false, Ordering::Release);
    }
}

/// The encode thread's end.
pub(crate) struct Reader(Arc<Ring>);

impl Reader {
    /// The next buffer: its samples into `out` (replacing what it held), and
    /// the capture time of its first frame. `None` when none is waiting.
    pub(crate) fn next(&mut self, out: &mut Vec<f32>) -> Option<Duration> {
        let ring = &*self.0;
        if ring.marks.len() == 0 {
            return None;
        }
        // SAFETY: the only consumer (`&mut self`, one `Reader` per ring); a
        // mark is published after its samples, so they are there.
        let (start, frames) = unsafe { ring.marks.pop_one() };
        out.clear();
        unsafe {
            ring.samples
                .pop(frames as usize * usize::from(CHANNELS), out)
        };
        Some(Duration::from_nanos(start))
    }

    /// The writer is gone. Buffers may still wait: drain them first.
    pub(crate) fn closed(&self) -> bool {
        self.0.closed.load(Ordering::Acquire)
    }

    /// Frames dropped since the last call.
    pub(crate) fn take_dropped(&self) -> u64 {
        self.0.dropped.swap(0, Ordering::Relaxed)
    }

    /// Sleeps until the ring holds `need` frames (at least one), the writer
    /// closes or `timeout` passes (or spuriously).
    pub(crate) fn wait(&self, need: usize, timeout: Duration) {
        let ring = &*self.0;
        ring.consumer.get_or_init(std::thread::current);
        let need = need.max(1);
        ring.need.store(need, Ordering::Relaxed);
        // Pairs with `Producer::due`.
        fence(Ordering::SeqCst);
        if ring.frames() < need && !self.closed() {
            std::thread::park_timeout(timeout);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Buffer `k`: sample `i` is `k * 1000 + i`, exact in f32.
    fn buffer(k: usize) -> impl FnMut(usize) -> f32 {
        move |i| (k * 1_000 + i) as f32
    }

    fn expect(k: usize, frames: usize, got: &[f32]) {
        assert_eq!(got.len(), frames * 2, "buffer {k}");
        for (i, &s) in got.iter().enumerate() {
            assert_eq!(s, (k * 1_000 + i) as f32, "buffer {k} sample {i}");
        }
    }

    #[test]
    fn buffers_come_out_in_order_across_the_wrap() {
        // 100 frames of room: buffers of 7..=37 frames wrap many times.
        let (w, mut r) = ring_of(100, 8);
        let mut p = w.producer().unwrap();
        let mut out = Vec::new();
        for k in 0..500 {
            let frames = 7 + k % 31;
            assert!(p.push(Duration::from_nanos(k as u64), frames, buffer(k)));
            assert_eq!(r.next(&mut out), Some(Duration::from_nanos(k as u64)));
            expect(k, frames, &out);
        }
        assert_eq!(r.next(&mut out), None);
        assert_eq!(r.take_dropped(), 0);
    }

    #[test]
    fn a_full_ring_drops_whole_buffers_and_counts_them() {
        let (w, mut r) = ring_of(100, 4);
        let mut p = w.producer().unwrap();
        // Samples full: 3 x 30 frames fit, the 4th does not.
        for k in 0..3 {
            assert!(p.push(Duration::ZERO, 30, buffer(k)));
        }
        assert!(!p.push(Duration::ZERO, 30, buffer(3)));
        // Exactly full still fits.
        assert!(p.push(Duration::ZERO, 10, buffer(4)));
        // Marks full (4): a 1-frame buffer does not fit either.
        assert!(!p.push(Duration::ZERO, 1, buffer(5)));
        assert_eq!(r.take_dropped(), 31);
        assert_eq!(r.take_dropped(), 0, "taken");
        let mut out = Vec::new();
        for (k, frames) in [(0, 30), (1, 30), (2, 30), (4, 10)] {
            assert!(r.next(&mut out).is_some());
            expect(k, frames, &out);
        }
        // Room again once drained.
        assert!(p.push(Duration::ZERO, 100, buffer(6)));
        assert!(r.next(&mut out).is_some());
        expect(6, 100, &out);
    }

    #[test]
    fn one_producer_at_a_time_and_closing_after_the_last_buffer() {
        let (w, mut r) = ring_of(100, 4);
        let mut p = w.producer().unwrap();
        assert!(w.producer().is_none(), "a second producer");
        assert!(p.push(Duration::ZERO, 5, buffer(0)));
        drop(p);
        // The next stream's.
        let mut p = w.producer().unwrap();
        assert!(p.push(Duration::ZERO, 5, buffer(1)));
        drop(p);
        assert!(!r.closed());
        drop(w);
        // Closed, with what was pushed still there to drain.
        assert!(r.closed());
        let mut out = Vec::new();
        for k in 0..2 {
            assert!(r.next(&mut out).is_some());
            expect(k, 5, &out);
        }
        assert_eq!(r.next(&mut out), None);
    }

    #[test]
    fn the_consumer_is_woken_once_it_can_complete_a_block() {
        let (w, mut r) = ring_of(4_096, 64);
        let mut p = w.producer().unwrap();
        let mut out = Vec::new();
        let mut woken_at = |need: usize, r: &mut Reader| {
            r.wait(need, Duration::ZERO);
            let at = (0..30)
                .position(|k| p.push(Duration::ZERO, 64, buffer(k)) && p.due())
                .unwrap();
            while r.next(&mut out).is_some() {}
            at
        };
        // A whole block to go, at a 64 quantum: the 15th buffer (960 frames).
        assert_eq!(woken_at(FRAME, &mut r), 14);
        // 100 frames short: the 2nd.
        assert_eq!(woken_at(100, &mut r), 1);
        // Not waiting for anything still means one buffer.
        assert_eq!(woken_at(0, &mut r), 0);
    }

    thread_local! {
        static COUNTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// Counts the allocator calls of a thread while `COUNTING`.
    struct Counting;

    // SAFETY: forwards to the system allocator.
    unsafe impl std::alloc::GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            if COUNTING.get() {
                CALLS.set(CALLS.get() + 1);
            }
            unsafe { std::alloc::System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
            if COUNTING.get() {
                CALLS.set(CALLS.get() + 1);
            }
            unsafe { std::alloc::System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: Counting = Counting;

    #[test]
    fn pushing_does_not_allocate() {
        let (w, r) = ring();
        let consumer = std::thread::spawn(move || {
            // Registered: the producer wakes it.
            r.wait(1, Duration::ZERO);
            r
        });
        let r = consumer.join().unwrap();
        let mut p = w.producer().unwrap();
        COUNTING.set(true);
        // Wakes, then overflows.
        for k in 0..200 {
            p.push(Duration::from_millis(k), 1_024, buffer(k as usize));
        }
        COUNTING.set(false);
        assert_eq!(CALLS.get(), 0);
        assert!(r.take_dropped() > 0);
    }

    #[test]
    fn concurrent_producer_and_consumer_lose_nothing() {
        // A small ring and a fast producer: it fills up often, and every
        // buffer comes out whole and in order or is counted as dropped.
        const N: usize = 50_000;
        let frames = |k: usize| 1 + k % 97;
        let (w, mut r) = ring_of(512, 16);
        let producer = std::thread::spawn(move || {
            let mut p = w.producer().unwrap();
            let mut pushed = Vec::new();
            for k in 0..N {
                let start = Duration::from_nanos(k as u64);
                if p.push(start, frames(k), buffer(k % 10_000)) {
                    pushed.push(k);
                }
                if k % 1_000 == 0 {
                    std::thread::yield_now();
                }
            }
            // Dropping the writer closes the ring.
            pushed
        });
        let mut out = Vec::new();
        let mut got = Vec::new();
        loop {
            let closed = r.closed();
            match r.next(&mut out) {
                Some(start) => {
                    let k = start.as_nanos() as usize;
                    expect(k % 10_000, frames(k), &out);
                    got.push(k);
                }
                None if closed => break,
                None => r.wait(1, Duration::from_millis(1)),
            }
        }
        let pushed = producer.join().unwrap();
        assert_eq!(got, pushed);
        assert!(!pushed.is_empty());
        let dropped: usize = (0..N)
            .filter(|k| pushed.binary_search(k).is_err())
            .map(frames)
            .sum();
        assert_eq!(r.take_dropped(), dropped as u64);
    }
}
