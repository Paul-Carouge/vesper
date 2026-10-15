//! Measurement build only (`--features perf-audit`): a counting allocator and
//! per-tick stats of the pipeline thread, printed when the pipeline ends.
//! Never in a product build.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

use vesper_core::{EncodedPacket, VideoFrame};

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static T_ALLOCS: Cell<u64> = const { Cell::new(0) };
    static T_BYTES: Cell<u64> = const { Cell::new(0) };
}

fn count(size: usize) {
    ALLOCS.fetch_add(1, Relaxed);
    BYTES.fetch_add(size as u64, Relaxed);
    let _ = T_ALLOCS.try_with(|c| c.set(c.get() + 1));
    let _ = T_BYTES.try_with(|c| c.set(c.get() + size as u64));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        count(l.size());
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        count(l.size());
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        count(new);
        unsafe { System.realloc(p, l, new) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Rust heap allocations (count, bytes) made by this thread so far.
pub fn thread_allocs() -> (u64, u64) {
    (T_ALLOCS.with(Cell::get), T_BYTES.with(Cell::get))
}

/// Frames the slot replaced before the pipeline took them.
pub static REPLACED: AtomicU64 = AtomicU64::new(0);
/// Frames the game capture server delivered, and its thread's allocations
/// at the first and last of them.
pub static SERVER_FRAMES: AtomicU64 = AtomicU64::new(0);
pub static SERVER_ALLOCS_FIRST: AtomicU64 = AtomicU64::new(0);
pub static SERVER_ALLOCS_LAST: AtomicU64 = AtomicU64::new(0);
pub static SERVER_BYTES_FIRST: AtomicU64 = AtomicU64::new(0);
pub static SERVER_BYTES_LAST: AtomicU64 = AtomicU64::new(0);

/// Called on the server thread for each delivered frame.
pub fn server_frame() {
    let (n, b) = thread_allocs();
    if SERVER_FRAMES.fetch_add(1, Relaxed) == 0 {
        SERVER_ALLOCS_FIRST.store(n, Relaxed);
        SERVER_BYTES_FIRST.store(b, Relaxed);
    }
    SERVER_ALLOCS_LAST.store(n, Relaxed);
    SERVER_BYTES_LAST.store(b, Relaxed);
}

#[derive(Clone, Copy, Default)]
struct Tick {
    late_ns: u64,
    encode_ns: u64,
    write_ns: u64,
    work_ns: u64,
    fresh: bool,
    allocs: u64,
    bytes: u64,
    packets: u32,
}

/// Per-tick stats of the pipeline thread. Preallocated: recording a tick
/// allocates nothing.
pub struct Audit {
    ticks: Vec<Tick>,
    cur: Tick,
    start_ns: u64,
    alloc0: (u64, u64),
    t_encode: Duration,
    t_write: Duration,
    /// (grid pts, capture pts) of frames sent to the encoder.
    inflight: VecDeque<(Duration, Duration)>,
    /// Capture pts of the packets of the last encode, then their latency.
    to_packet: Vec<u64>,
    to_disk: Vec<u64>,
    age: Vec<u64>,
    done: Vec<Duration>,
}

const CAP: usize = 200_000;

impl Audit {
    pub fn new() -> Self {
        Self {
            ticks: Vec::with_capacity(CAP),
            cur: Tick::default(),
            start_ns: 0,
            alloc0: (0, 0),
            t_encode: Duration::ZERO,
            t_write: Duration::ZERO,
            inflight: VecDeque::with_capacity(256),
            to_packet: Vec::with_capacity(CAP),
            to_disk: Vec::with_capacity(CAP),
            age: Vec::with_capacity(CAP),
            done: Vec::with_capacity(64),
        }
    }

    /// Right after the tick's sleep.
    pub fn woke(&mut self, deadline: Duration, now: Duration) {
        self.cur = Tick {
            late_ns: now.saturating_sub(deadline).as_nanos() as u64,
            ..Tick::default()
        };
        self.start_ns = now.as_nanos() as u64;
        self.alloc0 = thread_allocs();
    }

    pub fn before_encode(&mut self, frame: Option<&VideoFrame>, grid: Duration, now: Duration) {
        if let Some(f) = frame {
            self.cur.fresh = true;
            if self.ticks.len() < CAP && self.age.len() < CAP {
                self.age.push(now.saturating_sub(f.pts).as_nanos() as u64);
            }
            if self.inflight.len() == self.inflight.capacity() {
                self.inflight.pop_front();
            }
            self.inflight.push_back((grid, f.pts));
        }
        self.t_encode = now;
    }

    pub fn after_encode(&mut self, packets: &[EncodedPacket], now: Duration) {
        self.cur.encode_ns = (now - self.t_encode).as_nanos() as u64;
        self.cur.packets = packets.len() as u32;
        self.done.clear();
        for p in packets {
            while let Some(&(grid, cap)) = self.inflight.front() {
                if grid > p.pts {
                    break;
                }
                self.inflight.pop_front();
                if grid == p.pts {
                    if self.to_packet.len() < CAP {
                        self.to_packet
                            .push(now.saturating_sub(cap).as_nanos() as u64);
                    }
                    if self.done.len() < self.done.capacity() {
                        self.done.push(cap);
                    }
                }
            }
        }
        self.t_write = now;
    }

    pub fn after_write(&mut self, now: Duration) {
        self.cur.write_ns = (now - self.t_write).as_nanos() as u64;
        for &cap in &self.done {
            if self.to_disk.len() < CAP {
                self.to_disk.push(now.saturating_sub(cap).as_nanos() as u64);
            }
        }
    }

    pub fn end(&mut self, now: Duration) {
        let (n, b) = thread_allocs();
        self.cur.allocs = n - self.alloc0.0;
        self.cur.bytes = b - self.alloc0.1;
        self.cur.work_ns = (now.as_nanos() as u64).saturating_sub(self.start_ns);
        if self.ticks.len() < CAP {
            self.ticks.push(self.cur);
        }
    }

    pub fn report(&mut self, fps: u32, skipped: u64) {
        // Drop the first second (encoder warm-up).
        let warm = (fps as usize).min(self.ticks.len());
        let ticks = &self.ticks[warm..];
        if ticks.is_empty() {
            eprintln!("[audit] no ticks");
            return;
        }
        let period = 1_000_000_000 / u64::from(fps);
        let col = |f: fn(&Tick) -> u64| {
            let mut v: Vec<u64> = ticks.iter().map(f).collect();
            v.sort_unstable();
            v
        };
        let pct = |v: &[u64], p: f64| -> f64 {
            if v.is_empty() {
                return f64::NAN;
            }
            v[((v.len() - 1) as f64 * p) as usize] as f64 / 1e6
        };
        let line = |name: &str, v: &[u64]| {
            let mean = v.iter().sum::<u64>() as f64 / v.len().max(1) as f64 / 1e6;
            eprintln!(
                "[audit] {name:<14} ms  mean {mean:7.3}  p50 {:7.3}  p90 {:7.3}  p99 {:7.3}  max {:7.3}",
                pct(v, 0.5),
                pct(v, 0.9),
                pct(v, 0.99),
                pct(v, 1.0)
            );
        };
        let late = col(|t| t.late_ns);
        line("tick late", &late);
        line("encode call", &col(|t| t.encode_ns));
        line("sink write", &col(|t| t.write_ns));
        line("tick work", &col(|t| t.work_ns));
        let mut v = self.age.clone();
        v.sort_unstable();
        line("frame age", &v);
        let mut v = self.to_packet.clone();
        v.sort_unstable();
        line("capt->packet", &v);
        let mut v = self.to_disk.clone();
        v.sort_unstable();
        line("capt->written", &v);
        let n = ticks.len() as f64;
        let fresh = ticks.iter().filter(|t| t.fresh).count();
        eprintln!(
            "[audit] ticks {}  fresh {fresh}  repeated {}  late>1ms {}  late>period {}  work>period {}  skipped {skipped}  replaced-in-slot {}",
            ticks.len(),
            ticks.len() - fresh,
            late.iter().filter(|&&l| l > 1_000_000).count(),
            late.iter().filter(|&&l| l > period).count(),
            ticks.iter().filter(|t| t.work_ns > period).count(),
            REPLACED.load(Relaxed),
        );
        let allocs: u64 = ticks.iter().map(|t| t.allocs).sum();
        let bytes: u64 = ticks.iter().map(|t| t.bytes).sum();
        let fresh_allocs: u64 = ticks.iter().filter(|t| t.fresh).map(|t| t.allocs).sum();
        let rep_allocs: u64 = ticks.iter().filter(|t| !t.fresh).map(|t| t.allocs).sum();
        let packets: u64 = ticks.iter().map(|t| u64::from(t.packets)).sum();
        eprintln!(
            "[audit] pipeline thread allocs/tick {:.2} ({:.0} B/tick)  fresh {:.2}  repeat {:.2}  packets/tick {:.3}",
            allocs as f64 / n,
            bytes as f64 / n,
            fresh_allocs as f64 / fresh.max(1) as f64,
            rep_allocs as f64 / (ticks.len() - fresh).max(1) as f64,
            packets as f64 / n,
        );
        let frames = SERVER_FRAMES.load(Relaxed);
        if frames > 1 {
            let a = SERVER_ALLOCS_LAST.load(Relaxed) - SERVER_ALLOCS_FIRST.load(Relaxed);
            let b = SERVER_BYTES_LAST.load(Relaxed) - SERVER_BYTES_FIRST.load(Relaxed);
            eprintln!(
                "[audit] game server thread: frames {frames}  allocs/frame {:.2}  B/frame {:.0}",
                a as f64 / (frames - 1) as f64,
                b as f64 / (frames - 1) as f64
            );
        }
        eprintln!(
            "[audit] process total allocs {}  bytes {}",
            ALLOCS.load(Relaxed),
            BYTES.load(Relaxed)
        );
    }
}

/// Logs the time since `VESPER_T0` (CLOCK_REALTIME ns, set by the launcher).
pub fn startup(what: &str) {
    let Some(t0) = std::env::var("VESPER_T0")
        .ok()
        .and_then(|t| t.parse::<u128>().ok())
    else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    eprintln!(
        "[audit] startup {what}: {:.1} ms",
        (now.saturating_sub(t0)) as f64 / 1e6
    );
}

/// Prints the UI thread's allocations every 5 s. Keep the timer.
pub fn ui_thread_report() -> slint::Timer {
    let timer = slint::Timer::default();
    let mut last = (thread_allocs(), ALLOCS.load(Relaxed), BYTES.load(Relaxed));
    timer.start(slint::TimerMode::Repeated, Duration::from_secs(5), move || {
        let now = (thread_allocs(), ALLOCS.load(Relaxed), BYTES.load(Relaxed));
        eprintln!(
            "[audit] ui thread allocs/s {:.0} ({:.0} KB/s)  process allocs/s {:.0} ({:.0} KB/s)",
            (now.0.0 - last.0.0) as f64 / 5.0,
            (now.0.1 - last.0.1) as f64 / 5e3,
            (now.1 - last.1) as f64 / 5.0,
            (now.2 - last.2) as f64 / 5e3,
        );
        last = now;
    });
    timer
}
