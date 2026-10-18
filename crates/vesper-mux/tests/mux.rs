mod common;

use std::time::Duration;

use common::producer::{POOL, Producer};
use common::{OPUS_FRAME, h264_b_frames, opus_packet, opus_track};
use ffmpeg_next::codec::Id;
use ffmpeg_next::media::Type;
use vesper_core::Codec;
use vesper_encode::{Encoder, RateControl};
use vesper_mux::{Muxer, VideoTrack};

#[test]
fn muxes_hevc_and_opus_readable_by_libavformat() {
    if !vesper_core::test_gpu() {
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
    // The encoder's first packet starts pre-skip (312 samples) early: kept.
    let pre_skip = Duration::from_nanos(312 * 1_000_000_000 / 48_000);
    mux.write_audio(0, &opus_packet(start - pre_skip)).unwrap();

    let mut audio_pts = start - pre_skip + OPUS_FRAME;
    let mut audio_sent = 1;
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
    // The container itself says BT.709 limited / sRGB (not only the VUI):
    // Matroska Colour (0x55B0) with MatrixCoefficients (0x55B1) = 1,
    // Range (0x55B9) = 1 (broadcast), TransferCharacteristics (0x55BA) = 13.
    let bytes = std::fs::read(&path).unwrap();
    for element in [
        [0x55, 0xB1, 0x81, 1],
        [0x55, 0xB9, 0x81, 1],
        [0x55, 0xBA, 0x81, 13],
    ] {
        assert!(
            bytes.windows(4).any(|w| w == element),
            "Colour element {element:02x?} missing"
        );
    }
    std::fs::remove_file(&path).unwrap();
    assert_eq!(video_pts.len(), FRAMES as usize);
    // Rebased to `start`, in milliseconds.
    assert_eq!(video_pts[0], 0);
    assert_eq!(video_pts[1], 33);
    assert_eq!(audio, audio_sent, "audio before start dropped, rest kept");
}

/// H.264 with B-frames: the packets go in decode order with audio
/// interleaved by dts, and every frame decodes back in presentation order.
#[test]
fn h264_b_frames_mux_and_decode_in_order() {
    if !vesper_core::test_gpu() {
        return;
    }
    const FRAMES: u32 = 90;
    let start = Duration::from_secs(1_000);
    let (extradata, packets) = h264_b_frames(start, FRAMES);
    // The first keyframe's dts is before `start`: kept, rebased below 0.
    assert!(packets[0].keyframe && packets[0].dts < start);
    let path = std::env::temp_dir().join(format!("vesper-mux-bframes-{}.mkv", std::process::id()));
    let video = VideoTrack {
        codec: Codec::H264,
        width: 640,
        height: 360,
        fps: 30,
        extradata,
    };
    let mut mux = Muxer::create(&path, start, video, &[opus_track()]).unwrap();
    let mut audio_pts = start;
    for p in &packets {
        while audio_pts <= p.dts {
            mux.write_audio(0, &opus_packet(audio_pts)).unwrap();
            audio_pts += OPUS_FRAME;
        }
        mux.write_video(p).unwrap();
    }
    mux.finish().unwrap();

    let mut input = ffmpeg_next::format::input(&path).unwrap();
    let stream = input.streams().best(Type::Video).unwrap();
    let index = stream.index();
    let mut decoder = ffmpeg_next::codec::Context::from_parameters(stream.parameters())
        .unwrap()
        .decoder()
        .video()
        .unwrap();
    let mut frame = ffmpeg_next::frame::Video::empty();
    // Matroska stores no dts: what the encoder gave is checked in vesper-encode.
    let (mut shown, mut read) = (Vec::new(), 0);
    for (s, packet) in input.packets() {
        if s.index() != index {
            continue;
        }
        if read == 0 {
            assert!(packet.is_key(), "starts on a keyframe");
        }
        read += 1;
        decoder.send_packet(&packet).unwrap();
        while decoder.receive_frame(&mut frame).is_ok() {
            shown.push(frame.pts().unwrap());
        }
    }
    decoder.send_eof().unwrap();
    while decoder.receive_frame(&mut frame).is_ok() {
        shown.push(frame.pts().unwrap());
    }
    std::fs::remove_file(&path).unwrap();
    assert_eq!(read, FRAMES);
    assert_eq!(shown.len(), FRAMES as usize, "every frame decodes");
    assert_eq!(shown[0], 0, "presentation starts at `start`");
    assert!(shown.windows(2).all(|w| w[0] < w[1]), "shown in pts order");
}
