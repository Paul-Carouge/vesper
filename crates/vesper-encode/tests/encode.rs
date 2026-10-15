mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{POOL, Producer};
use vesper_core::{Codec, ReleaseGuard};
use vesper_encode::{Encoder, RateControl};

#[test]
fn encodes_frames_releases_each_once_and_keeps_pts() {
    if !Path::new("/dev/dri/renderD128").exists() {
        eprintln!("no GPU, skipping");
        return;
    }
    const FRAMES: u32 = 8;
    let (w, h) = (640, 360);
    let producer = unsafe { Producer::new(w, h) };
    let mut enc = Encoder::new(Codec::Hevc, w, h, 30, RateControl::ConstantQp(30)).unwrap();
    let released = Arc::new(AtomicUsize::new(0));
    let mut sent = Vec::new();
    let mut packets = Vec::new();

    for t in 0..FRAMES {
        let pts = Duration::from_millis(1_000 + t as u64 * 33);
        let mut frame = unsafe { producer.frame(t as usize % POOL, t, 0, pts) };
        let r = released.clone();
        frame.release = ReleaseGuard::new(move || {
            r.fetch_add(1, Ordering::SeqCst);
        });
        sent.push(pts);
        packets.extend(enc.encode(frame).unwrap());
        assert_eq!(released.load(Ordering::SeqCst), t as usize + 1);
    }
    packets.extend(enc.flush().unwrap());

    assert_eq!(released.load(Ordering::SeqCst), FRAMES as usize);
    assert_eq!(packets.len(), FRAMES as usize, "one packet per frame");
    assert!(packets[0].keyframe);
    let got: Vec<Duration> = packets.iter().map(|p| p.pts).collect();
    assert_eq!(got, sent, "pts preserved, in order");
}

#[test]
fn repeat_reencodes_last_picture_with_new_pts() {
    if !Path::new("/dev/dri/renderD128").exists() {
        eprintln!("no GPU, skipping");
        return;
    }
    let (w, h) = (640, 360);
    let producer = unsafe { Producer::new(w, h) };
    let mut enc = Encoder::new(Codec::Hevc, w, h, 30, RateControl::ConstantQp(30)).unwrap();
    assert!(enc.repeat(Duration::ZERO).is_err(), "nothing to repeat yet");

    let ms = |t: u64| Duration::from_millis(1_000 + t * 33);
    let mut packets = Vec::new();
    packets.extend(
        enc.encode(unsafe { producer.frame(0, 0, 0, ms(0)) })
            .unwrap(),
    );
    // Many more repeats than any surface pool holds: the kept surface must not leak.
    for t in 1..40 {
        packets.extend(enc.repeat(ms(t)).unwrap());
    }
    packets.extend(
        enc.encode(unsafe { producer.frame(1, 40, 0, ms(40)) })
            .unwrap(),
    );
    packets.extend(enc.repeat(ms(41)).unwrap());
    packets.extend(enc.flush().unwrap());

    let got: Vec<Duration> = packets.iter().map(|p| p.pts).collect();
    let want: Vec<Duration> = (0..42).map(ms).collect();
    assert_eq!(got, want);
    assert!(packets[0].keyframe);
}

#[test]
fn force_keyframe_makes_only_the_next_picture_a_keyframe() {
    if !Path::new("/dev/dri/renderD128").exists() {
        eprintln!("no GPU, skipping");
        return;
    }
    let (w, h) = (640, 360);
    let producer = unsafe { Producer::new(w, h) };
    // GOP of 2 s: no natural keyframe in these 20 pictures but the first.
    let mut enc = Encoder::new(Codec::Hevc, w, h, 30, RateControl::ConstantQp(30)).unwrap();
    let ms = |t: u64| Duration::from_millis(1_000 + t * 33);
    let mut packets = Vec::new();
    packets.extend(
        enc.encode(unsafe { producer.frame(0, 0, 0, ms(0)) })
            .unwrap(),
    );
    for t in 1..10 {
        packets.extend(enc.repeat(ms(t)).unwrap());
    }
    enc.force_keyframe();
    for t in 10..15 {
        packets.extend(enc.repeat(ms(t)).unwrap());
    }
    enc.force_keyframe();
    packets.extend(
        enc.encode(unsafe { producer.frame(1, 15, 0, ms(15)) })
            .unwrap(),
    );
    for t in 16..20 {
        packets.extend(enc.repeat(ms(t)).unwrap());
    }
    packets.extend(enc.flush().unwrap());

    assert_eq!(packets.len(), 20);
    let keys: Vec<usize> = (0..20).filter(|&i| packets[i].keyframe).collect();
    assert_eq!(keys, [0, 10, 15]);
}
