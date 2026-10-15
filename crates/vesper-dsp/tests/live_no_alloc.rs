//! A live chain's audio side never allocates or frees once warmed up, not
//! even while it takes a new chain. Its own test binary: the counting
//! allocator is global.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use vesper_dsp::{FilterSpec, live};

struct Counting;

thread_local! {
    /// Allocations and frees made by this thread.
    static CALLS: Cell<usize> = const { Cell::new(0) };
}

fn count() {
    // `try_with`: the allocator also runs while thread locals are torn down.
    let _ = CALLS.try_with(|c| c.set(c.get() + 1));
}

// SAFETY: forwards to the system allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        count();
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count();
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn calls() -> usize {
    CALLS.with(Cell::get)
}

#[test]
fn audio_side_never_allocates_after_warm_up() {
    let (mut chain, mut control) = live(&[FilterSpec::Gain { db: 0.0 }], 2);
    let (tx, rx) = std::sync::mpsc::sync_channel::<()>(0);
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel::<usize>(0);
    // The audio thread: one block per message, reporting its own calls.
    let audio = std::thread::spawn(move || {
        let mut buf = vec![0.1f32; 960 * 2];
        let before = calls();
        chain.process(&mut buf);
        // The warm-up allocates: the counter sees this thread's calls.
        assert!(calls() > before);
        while rx.recv().is_ok() {
            let before = calls();
            for _ in 0..10 {
                chain.process(&mut buf);
            }
            done_tx.send(calls() - before).unwrap();
        }
    });
    let block = |what: &str| {
        tx.send(()).unwrap();
        assert_eq!(done_rx.recv().unwrap(), 0, "{what}");
    };
    block("steady state");
    assert!(!control.set(&[FilterSpec::Gain { db: -6.0 }]));
    block("parameter change");
    // Noise suppression arrives after the warm-up: its FFT is warm anyway.
    assert!(control.set(&FilterSpec::voice_chain()));
    block("first handover");
    assert!(control.set(&[FilterSpec::Gain { db: 3.0 }]));
    block("second handover");
    block("steady state after handovers");
    drop(tx);
    audio.join().unwrap();
}
