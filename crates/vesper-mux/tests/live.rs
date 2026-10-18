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
    let Some((mut server, url)) = serve(&path, false) else {
        return;
    };
    let video = VideoTrack {
        codec: Codec::Av1,
        width: w,
        height: h,
        fps: 30,
        extradata: enc.extradata().to_vec(),
    };
    let mut live = connect(&url, &video);
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

/// A local ffmpeg server copying what it gets into `path` (its format from
/// the extension), and the URL to send to: RTMP (`-listen`), or SRT in
/// listener mode when `srt`. `None` without ffmpeg.
fn serve(path: &std::path::Path, srt: bool) -> Option<(std::process::Child, String)> {
    let (input, url) = if srt {
        let port = std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("srt://127.0.0.1:{port}");
        (format!("{url}?mode=listener"), url)
    } else {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("rtmp://127.0.0.1:{port}/app/key");
        (url.clone(), url)
    };
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args(["-v", "error", "-y"]);
    if !srt {
        cmd.args(["-listen", "1"]);
    }
    let Ok(server) = cmd
        .args(["-i", &input, "-c", "copy"])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .spawn()
    else {
        eprintln!("no ffmpeg, skipping");
        return None;
    };
    Some((server, url))
}

/// Connects to `url`, retrying until the server listens.
fn connect(url: &str, video: &VideoTrack) -> Live {
    let mut tries = 0;
    loop {
        let abort = Arc::new(AtomicBool::new(false));
        match Live::connect(url, video, None, abort, Duration::from_secs(5)) {
            Ok(live) => return live,
            Err(e) if tries < 50 => {
                tries += 1;
                eprintln!("{e}");
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("{e}"),
        }
    }
}

/// H.264 with B-frames over RTMP (FLV): the server gets every packet with
/// a rising dts from 0 and pts never before dts, starting on a keyframe.
#[test]
fn h264_b_frames_over_rtmp_keep_dts_and_pts() {
    b_frames_reach_the_server("flv", false, 1.0);
}

/// The same over SRT (MPEG-TS, 90 kHz timestamps).
#[test]
fn h264_b_frames_over_srt_keep_dts_and_pts() {
    b_frames_reach_the_server("ts", true, 1.0 / 90.0);
}

/// Sends 3 s of H.264 with B-frames to a local server writing `ext`, and
/// checks what it got: every packet, starting on a keyframe at dts 0
/// (relative to the first dts for MPEG-TS, which adds its own offset), dts
/// rising, pts never before dts, the times as sent within a `ms` tick.
fn b_frames_reach_the_server(ext: &str, srt: bool, ms_per_tick: f64) {
    if !vesper_core::test_gpu() {
        return;
    }
    let path = std::env::temp_dir().join(format!("vesper-live-bf-{}.{ext}", std::process::id()));
    let Some((mut server, url)) = serve(&path, srt) else {
        return;
    };
    let (extradata, packets) = common::h264_b_frames(Duration::from_secs(1_000), 90);
    let video = VideoTrack {
        codec: Codec::H264,
        width: 640,
        height: 360,
        fps: 30,
        extradata,
    };
    let mut live = connect(&url, &video);
    for p in &packets {
        live.write_video(p).unwrap();
        if srt {
            // Paced like a stream: a burst then a close can beat the
            // listener to the data.
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if srt {
        // SRT delivers within its latency (120 ms): closing at once drops the tail.
        std::thread::sleep(Duration::from_millis(500));
    }
    live.finish();
    assert!(server.wait().unwrap().success(), "the server failed");

    let mut input = ffmpeg_next::format::input(&path).unwrap();
    let got: Vec<_> = input
        .packets()
        .map(|(_, p)| (p.dts().unwrap(), p.pts().unwrap(), p.is_key()))
        .collect();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(got.len(), packets.len());
    let first = got[0].0;
    assert!(got[0].2, "starts on a keyframe");
    if !srt {
        assert_eq!(first, 0, "dts from 0");
    }
    assert!(got.windows(2).all(|w| w[0].0 < w[1].0), "dts rises");
    assert!(got.iter().all(|&(dts, pts, _)| pts >= dts));
    let ms = |t: Duration| (t - packets[0].dts).as_secs_f64() * 1000.0;
    for ((dts, pts, _), p) in got.iter().zip(&packets) {
        assert!(((dts - first) as f64 * ms_per_tick - ms(p.dts)).abs() <= 1.0);
        assert!(((pts - first) as f64 * ms_per_tick - ms(p.pts)).abs() <= 1.0);
    }
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
