mod common;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use common::producer::{POOL, Producer};
use ffmpeg_next::codec::Id;
use vesper_core::Codec;
use vesper_encode::{Encoder, RateControl};
use vesper_mux::{Live, LiveErrorKind, VideoTrack};

/// AV1 goes out over RTMP in Enhanced RTMP's FLV and a local `ffmpeg
/// -listen` server reads it as AV1, decodable. Skipped without ffmpeg.
#[test]
fn av1_over_rtmp_reads_back_as_av1() {
    if !vesper_core::test_gpu() {
        return;
    }
    let (w, h) = (640, 360);
    let producer = unsafe { Producer::new(w, h) };
    let mut enc = Encoder::new(
        Codec::Av1,
        w,
        h,
        30,
        RateControl::ConstantBitrate(1_000_000),
    )
    .unwrap();
    let path = std::env::temp_dir().join(format!("vesper-live-test-{}.mkv", std::process::id()));
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("rtmp://127.0.0.1:{port}/app/key");
    let Ok(mut server) = std::process::Command::new("ffmpeg")
        .args([
            "-v", "error", "-y", "-listen", "1", "-i", &url, "-c", "copy",
        ])
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .spawn()
    else {
        eprintln!("no ffmpeg, skipping");
        return;
    };
    let video = VideoTrack {
        codec: Codec::Av1,
        width: w,
        height: h,
        fps: 30,
        extradata: enc.extradata().to_vec(),
    };
    // Until the server listens.
    let mut tries = 0;
    let mut live = loop {
        let abort = Arc::new(AtomicBool::new(false));
        match Live::connect(&url, &video, None, abort, Duration::from_secs(5)) {
            Ok(live) => break live,
            Err(e) if tries < 50 => {
                tries += 1;
                eprintln!("{e}");
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("{e}"),
        }
    };
    let mut packets = Vec::new();
    for t in 0..10u32 {
        let pts = Duration::from_millis(1_000 + u64::from(t) * 33);
        let frame = unsafe { producer.frame(t as usize % POOL, t, 0, pts) };
        packets.extend(enc.encode(frame).unwrap());
    }
    packets.extend(enc.flush().unwrap());
    for p in &packets {
        live.write_video(p).unwrap();
    }
    live.finish();
    assert!(server.wait().unwrap().success(), "the server failed");

    let mut input = ffmpeg_next::format::input(&path).unwrap();
    let stream = input
        .streams()
        .best(ffmpeg_next::media::Type::Video)
        .unwrap();
    assert_eq!(stream.parameters().id(), Id::AV1);
    let index = stream.index();
    let mut decoder = ffmpeg_next::codec::Context::from_parameters(stream.parameters())
        .unwrap()
        .decoder()
        .video()
        .unwrap();
    let mut frame = ffmpeg_next::frame::Video::empty();
    let mut decoded = 0;
    for (s, packet) in input.packets() {
        if s.index() == index {
            decoder.send_packet(&packet).unwrap();
            while decoder.receive_frame(&mut frame).is_ok() {
                decoded += 1;
            }
        }
    }
    decoder.send_eof().unwrap();
    while decoder.receive_frame(&mut frame).is_ok() {
        decoded += 1;
    }
    std::fs::remove_file(&path).unwrap();
    assert_eq!(decoded, packets.len());
}

/// FFmpeg's MPEG-TS (SRT) writes AV1 as private data no player reads:
/// refused before any connection.
#[test]
fn av1_over_srt_is_refused() {
    let video = VideoTrack {
        codec: Codec::Av1,
        width: 64,
        height: 64,
        fps: 30,
        extradata: Vec::new(),
    };
    let abort = Arc::new(AtomicBool::new(false));
    let e = Live::connect(
        "srt://127.0.0.1:9",
        &video,
        None,
        abort,
        Duration::from_secs(1),
    )
    .err()
    .expect("refused");
    assert_eq!(e.kind(), LiveErrorKind::Other, "{e}");
    assert!(e.to_string().contains("AV1"), "{e}");
}
