mod common;

use std::path::Path;
use std::time::Duration;

use common::producer::{POOL, Producer};
use common::{OPUS_FRAME, opus_packet, opus_track};
use ffmpeg_next::codec::Id;
use ffmpeg_next::media::Type;
use vesper_core::Codec;
use vesper_encode::{Encoder, RateControl};
use vesper_mux::{Muxer, VideoTrack};

#[test]
fn muxes_hevc_and_opus_readable_by_libavformat() {
    if !Path::new("/dev/dri/renderD128").exists() {
        eprintln!("no GPU, skipping");
        return;
    }
    const FRAMES: u32 = 30;
    let (w, h) = (640, 360);
    let producer = unsafe { Producer::new(w, h) };
    let mut enc = Encoder::new(Codec::Hevc, w, h, 30, RateControl::ConstantQp(30)).unwrap();
    assert!(
        !enc.extradata().is_empty(),
        "HEVC encoder exposes parameter sets"
    );
    let start = Duration::from_secs(1_000);
    let path = std::env::temp_dir().join(format!("vesper-mux-test-{}.mkv", std::process::id()));
    let video = VideoTrack {
        codec: Codec::Hevc,
        width: w,
        height: h,
        fps: 30,
        extradata: enc.extradata().to_vec(),
    };
    let mut mux = Muxer::create(&path, start, video, &[opus_track()]).unwrap();
    assert!(
        mux.write_audio(1, &opus_packet(start)).is_err(),
        "unknown track"
    );
    // Before the recording start: dropped.
    mux.write_audio(0, &opus_packet(start - OPUS_FRAME))
        .unwrap();

    let mut audio_pts = start;
    let mut audio_sent = 0;
    for t in 0..FRAMES {
        let pts = start + Duration::from_millis(t as u64 * 33);
        let frame = unsafe { producer.frame(t as usize % POOL, t, 0, pts) };
        for p in enc.encode(frame).unwrap() {
            while audio_pts <= p.pts {
                mux.write_audio(0, &opus_packet(audio_pts)).unwrap();
                audio_pts += OPUS_FRAME;
                audio_sent += 1;
            }
            mux.write_video(&p).unwrap();
        }
    }
    for p in enc.flush().unwrap() {
        mux.write_video(&p).unwrap();
    }
    mux.finish().unwrap();

    let mut input = ffmpeg_next::format::input(&path).unwrap();
    let streams: Vec<_> = input
        .streams()
        .map(|s| (s.parameters().medium(), s.parameters().id()))
        .collect();
    assert_eq!(streams, [(Type::Video, Id::HEVC), (Type::Audio, Id::OPUS)]);
    let mut video_pts = Vec::new();
    let mut audio = 0;
    for (stream, packet) in input.packets() {
        if stream.index() == 0 {
            if video_pts.is_empty() {
                assert!(packet.is_key(), "file starts with a keyframe");
            }
            video_pts.push(packet.pts().unwrap());
        } else {
            audio += 1;
        }
    }
    std::fs::remove_file(&path).unwrap();
    assert_eq!(video_pts.len(), FRAMES as usize);
    // Rebased to `start`, in milliseconds.
    assert_eq!(video_pts[0], 0);
    assert_eq!(video_pts[1], 33);
    assert_eq!(audio, audio_sent, "audio before start dropped, rest kept");
}
