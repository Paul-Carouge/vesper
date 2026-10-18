//! Failure injection for the muxer (no GPU needed): a full disk, a process
//! killed mid-recording, an output file deleted under it, and odd packets.
//!
//! Packets are made up: an H.264 stream with a made-up `avcC` (the muxer only
//! copies it) and silent Opus packets.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{OPUS_FRAME, opus_packet, opus_track};
use vesper_core::{Codec, EncodedPacket};
use vesper_mux::{Muxer, VideoTrack};

unsafe extern "C" {
    fn setrlimit(resource: i32, rlim: *const [u64; 2]) -> i32;
    fn signal(sig: i32, handler: usize) -> usize;
}
const RLIMIT_FSIZE: i32 = 1;
const SIGXFSZ: i32 = 25;

/// One SPS and one PPS, made up.
const AVCC: [u8; 17] = [
    1, 0x42, 0xC0, 0x1E, 0xFF, 0xE1, 0, 4, 0x67, 1, 2, 3, 1, 0, 2, 0x68, 4,
];

fn video() -> VideoTrack {
    VideoTrack {
        codec: Codec::H264,
        width: 640,
        height: 360,
        fps: 30,
        extradata: AVCC.to_vec(),
    }
}

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("vesper-mux-{name}-{}.mkv", std::process::id()))
}

fn video_packet(start: Duration, i: u64, size: usize) -> EncodedPacket {
    EncodedPacket {
        data: Arc::from(vec![(i % 251) as u8; size]),
        pts: start + Duration::from_millis(i * 33),
        dts: start + Duration::from_millis(i * 33),
        keyframe: i.is_multiple_of(30),
    }
}

/// Writes `frames` video packets of `size` bytes with their audio. Returns
/// how many writes failed and the first error.
fn record(mux: &mut Muxer, start: Duration, frames: u64, size: usize) -> (u32, Option<String>) {
    let (mut errors, mut first) = (0, None);
    let mut audio = start;
    for i in 0..frames {
        let p = video_packet(start, i, size);
        while audio <= p.pts {
            if let Err(e) = mux.write_audio(0, &opus_packet(audio)) {
                errors += 1;
                first.get_or_insert(format!("audio: {e}"));
            }
            audio += OPUS_FRAME;
        }
        if let Err(e) = mux.write_video(&p) {
            errors += 1;
            first.get_or_insert(format!("video: {e}"));
        }
    }
    (errors, first)
}

/// (video packets, audio packets) a player can read from `path`.
fn readable(path: &Path) -> (usize, usize) {
    let mut input = ffmpeg_next::format::input(path).unwrap();
    let (mut v, mut a) = (0, 0);
    for (stream, _) in input.packets() {
        match stream.index() {
            0 => v += 1,
            _ => a += 1,
        }
    }
    (v, a)
}

/// Runs in a child process (see `disk_full_*`): `RLIMIT_FSIZE` is per process.
#[test]
#[ignore = "run by disk_full_ends_the_recording_and_keeps_the_file in its own process"]
fn disk_full_child() {
    let path = temp("full");
    let start = Duration::from_secs(1_000);
    let mut mux = Muxer::create(&path, start, video(), &[opus_track()]).unwrap();
    // The 200 kB limit makes write(2) fail like ENOSPC would.
    unsafe {
        signal(SIGXFSZ, 1);
        setrlimit(RLIMIT_FSIZE, &[200_000, 200_000]);
    }
    let (errors, first) = record(&mut mux, start, 300, 4000);
    let finished = mux.finish();
    // Lift the soft limit again to inspect the file.
    unsafe { setrlimit(RLIMIT_FSIZE, &[u64::MAX, u64::MAX]) };
    assert!(errors > 0, "no write failed");
    let first = first.unwrap();
    assert!(first.contains("File too large"), "{first}");
    assert!(finished.is_err(), "finish() hid the failure");
    let (v, a) = readable(&path);
    std::fs::remove_file(&path).unwrap();
    // 200 kB of 4 kB packets: ~45 frames survive, and the file still parses.
    assert!((30..=60).contains(&v), "{v} video packets survived");
    assert!(a > 50, "{a} audio packets survived");
}

#[test]
fn disk_full_ends_the_recording_and_keeps_the_file() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "disk_full_child",
            "--ignored",
            "--test-threads=1",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The README promises a killed process loses about one second. `forget`
/// is a kill: no trailer, no flush on drop.
#[test]
fn a_killed_recording_is_playable_to_the_last_second() {
    let path = temp("killed");
    let start = Duration::from_secs(1_000);
    let mut mux = Muxer::create(&path, start, video(), &[opus_track()]).unwrap();
    let (errors, _) = record(&mut mux, start, 300, 3000); // 10 s
    assert_eq!(errors, 0);
    std::mem::forget(mux);
    let (v, a) = readable(&path);
    std::fs::remove_file(&path).unwrap();
    // Clusters close every second: at most ~2 s (video and audio) are lost.
    assert!(v >= 300 - 2 * 30 - 1, "{v} of 300 video packets readable");
    assert!(a >= 500 - 2 * 50 - 2, "{a} of ~500 audio packets readable");
}

/// A recording whose folder is deleted while it runs keeps writing to an
/// unlinked file: every write succeeds, so `finish()` must say the file is
/// gone instead of letting the app show "Recording saved".
#[test]
fn finish_reports_a_deleted_output() {
    let path = temp("unlinked");
    let start = Duration::from_secs(1_000);
    let mut mux = Muxer::create(&path, start, video(), &[opus_track()]).unwrap();
    record(&mut mux, start, 60, 3000);
    std::fs::remove_file(&path).unwrap();
    let (errors, _) = record(&mut mux, start + Duration::from_secs(3), 60, 3000);
    assert_eq!(errors, 0, "writes to an unlinked file succeed");
    let finished = mux.finish();
    assert!(
        finished.as_ref().is_err_and(vesper_mux::Error::is_deleted),
        "finish() returned {finished:?} but {} is gone",
        path.display()
    );
}

#[test]
fn odd_packets_never_panic() {
    let path = temp("odd");
    let start = Duration::from_secs(1_000);
    let mut mux = Muxer::create(&path, start, video(), &[opus_track()]).unwrap();
    let mk = |pts: Duration, keyframe, len| EncodedPacket {
        data: Arc::from(vec![7u8; len]),
        pts,
        dts: pts,
        keyframe,
    };
    // Empty payload, absurd timestamps, going backwards, no keyframe first.
    let _ = mux.write_video(&mk(start, false, 100));
    let _ = mux.write_video(&mk(start, true, 0));
    let _ = mux.write_video(&mk(start, true, 100));
    let _ = mux.write_video(&mk(Duration::MAX, true, 100));
    let _ = mux.write_video(&mk(Duration::from_secs(u64::MAX / 4), true, 100));
    let _ = mux.write_video(&mk(start + Duration::from_secs(1), true, 100));
    let _ = mux.write_video(&mk(start + Duration::from_millis(10), false, 100));
    let _ = mux.write_video(&mk(Duration::ZERO, true, 100));
    let _ = mux.write_audio(0, &mk(Duration::from_secs(u64::MAX / 4), true, 1));
    let _ = mux.write_audio(0, &mk(Duration::ZERO, true, 0));
    let _ = mux.write_audio(usize::MAX, &mk(start, true, 1));
    let _ = mux.finish();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn bad_destinations_fail_cleanly() {
    let start = Duration::ZERO;
    for path in [
        "/nonexistent-dir/out.mkv",
        "/proc/out.mkv",
        "/dev/full",
        "",
        "/",
    ] {
        // Err or Ok are both fine here (/dev/full opens): no panic, no hang.
        if let Ok(mux) = Muxer::create(Path::new(path), start, video(), &[opus_track()]) {
            let _ = mux.finish();
        }
    }
}
