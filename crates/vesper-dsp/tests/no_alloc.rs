//! `Chain::process` must not allocate or free: count the allocator calls made
//! by this thread while it runs.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use vesper_dsp::{Chain, DuckSettings, Ducker, FilterSpec, VoiceDetector, VoiceMeter, Voices};

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static CALLS: Cell<usize> = const { Cell::new(0) };
}

fn count() {
    if COUNTING.get() {
        CALLS.set(CALLS.get() + 1);
    }
}

struct Counting;

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

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[test]
fn process_does_not_allocate() {
    let mut specs = FilterSpec::voice_chain();
    specs.push(FilterSpec::Gain { db: 3.0 });
    let (mut chain, mut control) = Chain::new(&specs, 2);
    let mut seed = 1u32;
    let input: Vec<f32> = (0..48_000 * 2)
        .map(|i| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 8) as f32 / (1 << 24) as f32 - 0.5;
            0.5 * (i as f32 * 0.03).sin() + 0.1 * noise
        })
        .collect();
    // The audio thread is not the one that built the chain.
    let calls = std::thread::spawn(move || {
        chain.warm_up();
        let mut buf = input.clone();
        // Blocks of odd sizes, and parameter changes in flight.
        let mut done = 0;
        for (i, frames) in [960usize, 1, 479, 481, 128, 2_048]
            .iter()
            .cycle()
            .take(60)
            .enumerate()
        {
            if i % 10 == 5 {
                let spec = FilterSpec::Compressor {
                    threshold_db: -30.0 + i as f32 / 10.0,
                    ratio: 4.0,
                    attack_ms: 3.0,
                    release_ms: 50.0,
                    knee_db: 3.0,
                    makeup_db: 2.0,
                };
                assert!(control.set(3, spec));
                assert!(control.set(
                    0,
                    FilterSpec::HighPass {
                        freq_hz: 100.0 + i as f32
                    }
                ));
            }
            let len = frames * 2;
            let start = done % (input.len() - len);
            buf[..len].copy_from_slice(&input[start..start + len]);
            COUNTING.set(true);
            chain.process(&mut buf[..len]);
            COUNTING.set(false);
            done += len;
        }
        CALLS.get()
    })
    .join()
    .unwrap();
    assert_eq!(calls, 0, "allocator calls in process()");
}

#[test]
fn voice_detector_does_not_allocate() {
    let mut seed = 5u32;
    let input: Vec<f32> = (0..48_000 * 2)
        .map(|i| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 8) as f32 / (1 << 24) as f32 - 0.5;
            0.3 * (i as f32 * 0.02).sin() * (i as f32 * 1e-4).sin() + 0.01 * noise
        })
        .collect();
    let mut det = VoiceDetector::new(2);
    let calls = std::thread::spawn(move || {
        det.warm_up();
        COUNTING.set(true);
        for b in input.chunks(2 * 481) {
            std::hint::black_box(det.process(b));
        }
        det.process(&[0.0; 960 * 2]);
        det.reset();
        COUNTING.set(false);
        CALLS.get()
    })
    .join()
    .unwrap();
    assert_eq!(calls, 0, "allocator calls in VoiceDetector");
}

#[test]
fn voice_meter_and_ducker_do_not_allocate() {
    let input: Vec<f32> = (0..48_000 * 2 * 6)
        .map(|i| 0.3 * (i as f32 * 0.02).sin() * (i as f32 * 1e-4).sin())
        .collect();
    let mut meter = VoiceMeter::new(2);
    let mut duck = Ducker::new(2);
    let set = DuckSettings::at_levels(Some(1), Some(1));
    let calls = std::thread::spawn(move || {
        let mut buf = input.clone();
        COUNTING.set(true);
        let mut voice = None;
        for (i, b) in buf.chunks_mut(960 * 2).enumerate() {
            voice = meter
                .process(b, |a, _| a % 960 == 0)
                .map(Voices::one)
                .or(voice);
            let at = (i * 960) as u64;
            duck.look_ahead(b, at, set, voice);
            duck.process_at(b, at, set, i % 50 < 30, voice);
        }
        meter.reset();
        COUNTING.set(false);
        assert!(voice.is_some());
        CALLS.get()
    })
    .join()
    .unwrap();
    assert_eq!(calls, 0, "allocator calls in VoiceMeter or Ducker");
}
