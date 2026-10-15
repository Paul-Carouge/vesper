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
