use super::*;
use crate::{AudioTrack, Muxer, VideoTrack};
use std::sync::Arc;
use vesper_core::{Codec, EncodedPacket};

/// A pid above Linux's `pid_max` limit: never running.
const DEAD: u32 = 4_194_305;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vesper-recovery-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("markers")).unwrap();
    dir
}

fn opus(name: &str) -> AudioTrack {
    let mut head = b"OpusHead".to_vec();
    head.extend_from_slice(&[1, 2]);
    head.extend_from_slice(&312u16.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&[0, 0, 0]);
    AudioTrack {
        name: name.into(),
        sample_rate: 48_000,
        channels: 2,
        extradata: head,
    }
}

/// Writes `secs` of 30 fps video (a keyframe a second) and two audio
/// tracks; `finish`: cleanly, else left as a SIGKILL leaves it.
fn record(path: &Path, secs: u64, finish: bool) {
    let start = Duration::from_secs(10);
    let video = VideoTrack {
        codec: Codec::H264,
        width: 64,
        height: 64,
        fps: 30,
        // avcC with one SPS and one PPS (contents not decoded here).
        extradata: vec![
            1, 0x64, 0, 0x1f, 0xff, 0xe1, 0, 4, 0x67, 0x64, 0, 0x1f, 1, 0, 2, 0x68, 0xee,
        ],
    };
    let mut mux = Muxer::create(path, start, video, &[opus("Mix"), opus("Mic")]).unwrap();
    // One length-prefixed slice NAL: written as is.
    let frame: Arc<[u8]> = Arc::from(&[0, 0, 0, 2, 0x65, 0x88][..]);
    let silence: Arc<[u8]> = Arc::from(&[(31 << 3) | 0b100][..]);
    for ms in (0..secs * 1000).step_by(20) {
        let pts = start + Duration::from_millis(ms);
        if ms % 100 < 20 {
            for f in 0..3 {
                let k = (ms / 100) * 3 + f;
                let p = EncodedPacket {
                    data: frame.clone(),
                    pts: start + Duration::from_nanos(k * 1_000_000_000 / 30),
                    keyframe: k % 30 == 0,
                };
                mux.write_video(&p).unwrap();
            }
        }
        for t in 0..2 {
            let p = EncodedPacket {
                data: silence.clone(),
                pts,
                keyframe: true,
            };
            mux.write_audio(t, &p).unwrap();
        }
    }
    if finish {
        mux.finish().unwrap();
    } else {
        // No trailer, no cues, no duration: what a kill leaves.
        std::mem::forget(mux);
    }
}

fn marker(dir: &Path, name: &str, pid: u32, mkv: &Path) -> PathBuf {
    let path = dir.join("markers").join(name);
    let mut bytes = format!("{pid}\n").into_bytes();
    bytes.extend_from_slice(mkv.as_os_str().as_bytes());
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Packets per stream (pts in ms) and the video stream's Colour fields.
fn packets(path: &Path) -> (Vec<Vec<i64>>, String, Vec<Option<String>>) {
    let mut input = ffmpeg_next::format::input(path).unwrap();
    let n = input.streams().count();
    let titles = input
        .streams()
        .map(|s| s.metadata().get("title").map(str::to_owned))
        .collect();
    let colour = unsafe {
        let par = &*input.stream(0).unwrap().parameters().as_ptr();
        format!(
            "{:?} {:?} {:?} {:?} {:?} {}",
            par.color_range,
            par.color_space,
            par.color_primaries,
            par.color_trc,
            par.chroma_location,
            par.initial_padding
        )
    };
    let mut out = vec![Vec::new(); n];
    for (s, p) in input.packets() {
        out[s.index()].push(p.pts().unwrap());
    }
    (out, colour, titles)
}

#[test]
fn a_killed_recording_is_remuxed_in_place_with_cues_and_duration() {
    let dir = scratch("killed");
    let mkv = dir.join("Vesper 1.mkv");
    record(&mkv, 5, false);
    assert!(!finalized(&mkv).unwrap(), "a killed file has no duration");
    assert!(!has_cues(&mkv).unwrap(), "nor cues");
    let before = packets(&mkv);
    let m = marker(&dir, "1-0", DEAD, &mkv);

    let out = recover_stale(&dir.join("markers"), "recovered");

    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].1, Outcome::Recovered), "{:?}", out[0].1);
    assert_eq!(out[0].0, mkv);
    assert!(finalized(&mkv).unwrap());
    assert!(has_cues(&mkv).unwrap());
    assert!(!m.exists());
    let after = packets(&mkv);
    assert_eq!(after, before, "every packet, Colour and track names kept");
    // About 5 s of video made it into the killed file (a cluster may be lost).
    assert!(before.0[0].len() > 100, "{}", before.0[0].len());
    assert!(before.1.contains("AVCOL_RANGE_MPEG"), "{}", before.1);
    let names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(names.len(), 2, "no copy left beside it: {names:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_complete_file_is_left_alone() {
    let dir = scratch("complete");
    let mkv = dir.join("done.mkv");
    record(&mkv, 2, true);
    let bytes = std::fs::read(&mkv).unwrap();
    let m = marker(&dir, "1-0", DEAD, &mkv);
    let out = recover_stale(&dir.join("markers"), "recovered");
    assert!(matches!(out[0].1, Outcome::Complete), "{:?}", out[0].1);
    assert_eq!(std::fs::read(&mkv).unwrap(), bytes);
    assert!(!m.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_missing_file_only_removes_its_marker() {
    let dir = scratch("missing");
    let m = marker(&dir, "1-0", DEAD, &dir.join("gone.mkv"));
    let out = recover_stale(&dir.join("markers"), "recovered");
    assert!(matches!(out[0].1, Outcome::Missing));
    assert!(!m.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn corrupt_markers_are_removed() {
    let dir = scratch("corrupt");
    let markers = dir.join("markers");
    for (name, bytes) in [
        ("a", &b"\xff\xfe garbage"[..]),
        ("b", b""),
        ("c", b"12\nrelative.mkv"),
    ] {
        std::fs::write(markers.join(name), bytes).unwrap();
    }
    // A temporary marker of a dead writer.
    std::fs::write(markers.join(format!(".{DEAD}-0.tmp")), b"x").unwrap();
    assert!(recover_stale(&markers, "recovered").is_empty());
    assert_eq!(std::fs::read_dir(&markers).unwrap().count(), 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_failed_recovery_keeps_the_original() {
    let dir = scratch("failed");
    let mkv = dir.join("broken.mkv");
    let garbage = vec![0x5a; 10_000];
    std::fs::write(&mkv, &garbage).unwrap();
    let m = marker(&dir, "1-0", DEAD, &mkv);
    let out = recover_stale(&dir.join("markers"), "recovered");
    assert!(matches!(out[0].1, Outcome::Failed(_)), "{:?}", out[0].1);
    assert_eq!(std::fs::read(&mkv).unwrap(), garbage);
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        2,
        "only the original and markers/"
    );
    assert!(!m.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_running_writer_keeps_its_marker() {
    let dir = scratch("running");
    let mkv = dir.join("live.mkv");
    record(&mkv, 3, false);
    let m = marker(&dir, "1-0", std::process::id(), &mkv);
    assert!(recover_stale(&dir.join("markers"), "recovered").is_empty());
    assert!(m.exists());
    assert!(!finalized(&mkv).unwrap());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_recovered_name_does_not_overwrite_a_file() {
    let dir = scratch("names");
    let mkv = dir.join("a.mkv");
    std::fs::write(dir.join("a (recovered).mkv"), b"mine").unwrap();
    assert_eq!(
        free_name(&mkv, "recovered"),
        Some(dir.join("a (recovered 2).mkv"))
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_guard_removes_its_marker_only_on_a_clean_end() {
    let dir = scratch("guard");
    let markers = dir.join("markers");
    let mkv = dir.join("x.mkv");
    let file = File::create(&mkv).unwrap();
    let count = || std::fs::read_dir(&markers).unwrap().count();
    let wait = |n: usize| {
        for _ in 0..200 {
            if count() == n {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{} markers, expected {n}", count());
    };
    let clean = Guard::start_in(Some(markers.clone()), &mkv, &file).unwrap();
    wait(1);
    let (pid, path) = read_marker(
        std::fs::read(
            std::fs::read_dir(&markers)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!((pid, path), (std::process::id(), mkv.clone()));
    clean.end(true);
    wait(0);
    Guard::start_in(Some(markers.clone()), &mkv, &file)
        .unwrap()
        .end(false);
    wait(1);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(count(), 1, "a failed finish keeps the marker");
    std::fs::remove_dir_all(&dir).unwrap();
}
