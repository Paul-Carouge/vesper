//! A plugin filter never allocates or frees on the audio thread: neither
//! wet, nor dry while its host is down. Its own test binary: the counting
//! allocator is global.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::Duration;

use common::*;
use vesper_dsp::Filter;

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
fn plugin_filter_never_allocates() {
    let dir = temp_dir("no-alloc");
    let reg = common::registry(&dir, "gain", 32, 0);
    let id = reg.add(&fake_plugin()).unwrap();
    let inst = reg.instance(id).unwrap();
    wait_for("the host", || running(&inst));
    let mut f = inst.filter();
    let mut buf = ramp(0, 960);
    f.process(&mut buf);
    let blocks = |f: &mut dyn Filter, buf: &mut [f32]| {
        let before = calls();
        for _ in 0..20 {
            f.process(buf);
        }
        calls() - before
    };
    assert_eq!(blocks(&mut f, &mut buf), 0, "wet");
    assert_eq!(f.latency(), 32);
    let pid = inst.pid().unwrap();
    // SAFETY: a process this test started.
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(blocks(&mut f, &mut buf), 0, "dry, host down");
    drop((f, inst, reg));
    std::fs::remove_dir_all(&dir).unwrap();
}
