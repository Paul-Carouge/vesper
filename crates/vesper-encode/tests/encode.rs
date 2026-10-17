mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{POOL, Producer};
use vesper_core::{Codec, ReleaseGuard, next_pool_generation};
use vesper_encode::{Encoder, RateControl};

#[test]
fn encodes_frames_releases_each_once_and_keeps_pts() {
    if !vesper_core::test_gpu() {
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
    if !vesper_core::test_gpu() {
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
    if !vesper_core::test_gpu() {
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

#[test]
fn scales_frames_of_other_sizes_to_the_encoder_size() {
    if !vesper_core::test_gpu() {
        return;
    }
    let (w, h) = (640, 360);
    let mut enc = Encoder::new(Codec::Hevc, w, h, 30, RateControl::ConstantQp(30)).unwrap();
    let ms = |t: u64| Duration::from_millis(1_000 + t * 33);
    let mut packets = Vec::new();
    let mut t = 0;
    // Native, larger, smaller, another aspect, then native again. Each size
    // is a new producer pool and generation, as after a game resize.
    let sizes = [(640, 360), (1280, 720), (400, 300), (1920, 800), (640, 360)];
    for (generation, (pw, ph)) in sizes.into_iter().enumerate() {
        let producer = unsafe { Producer::new(pw, ph) };
        for id in 0..3 {
            let frame = unsafe { producer.frame(id, t as u32, generation as u32, ms(t)) };
            packets.extend(enc.encode(frame).unwrap());
            t += 1;
        }
        packets.extend(enc.repeat(ms(t)).unwrap());
        t += 1;
    }
    packets.extend(enc.flush().unwrap());
    let got: Vec<Duration> = packets.iter().map(|p| p.pts).collect();
    assert_eq!(got, (0..t).map(ms).collect::<Vec<_>>());

    // Aspect kept with black bars (luma 16): 400x300 fits as 480x360 with
    // 80 px bars left and right, 1920x800 as 640x266 with 47 px bars.
    let frames = unsafe { decode_luma(enc.extradata(), &packets) };
    assert_eq!(frames.len(), 20);
    let col =
        |f: &Luma, x: usize| (0..f.h).map(|y| f.y[y * f.w + x] as f64).sum::<f64>() / f.h as f64;
    let row = |f: &Luma, y: usize| {
        f.y[y * f.w..(y + 1) * f.w]
            .iter()
            .map(|&v| v as f64)
            .sum::<f64>()
            / f.w as f64
    };
    for f in &frames {
        assert_eq!((f.w, f.h), (640, 360));
    }
    let small = &frames[8];
    assert!(col(small, 20) < 20.0 && col(small, 620) < 20.0, "side bars");
    assert!(col(small, 320) > 40.0, "picture");
    // Black, not green: neutral chroma in the bars.
    assert!(
        small.corner_u.abs_diff(128) < 8,
        "bar chroma {}",
        small.corner_u
    );
    let wide = &frames[12];
    assert!(
        row(wide, 15) < 20.0 && row(wide, 345) < 20.0,
        "top and bottom bars"
    );
    assert!(row(wide, 180) > 40.0, "picture");
    let native = &frames[16];
    assert!(
        row(native, 2) > 20.0 || col(native, 2) > 20.0,
        "no bars at native size"
    );
}

/// Two producers of one size (the game window, then the game's layer) both
/// hand out buffer 0: each is imported, never the other's stale picture.
#[test]
fn another_producer_with_the_same_buffer_id_is_imported_anew() {
    if !vesper_core::test_gpu() {
        return;
    }
    let (w, h) = (640, 360);
    let mut enc = Encoder::new(Codec::Hevc, w, h, 30, RateControl::ConstantQp(30)).unwrap();
    let ms = |t: u64| Duration::from_millis(1_000 + t * 33);
    let mut packets = Vec::new();
    for (t, pattern) in [(0, 0), (1, 40)] {
        let producer = unsafe { Producer::new(w, h) };
        let frame = unsafe { producer.frame(0, pattern, next_pool_generation(), ms(t)) };
        packets.extend(enc.encode(frame).unwrap());
    }
    packets.extend(enc.flush().unwrap());
    let frames = unsafe { decode_luma(enc.extradata(), &packets) };
    assert_eq!(frames.len(), 2);
    let diff = frames[0]
        .y
        .iter()
        .zip(&frames[1].y)
        .map(|(a, b)| a.abs_diff(*b) as f64)
        .sum::<f64>()
        / frames[0].y.len() as f64;
    assert!(diff > 10.0, "second producer's picture not encoded: {diff}");
}

/// A scaled encoder (a live stream) follows its source's pictures, repeats
/// and keyframes at its own size and rate control, also after the source
/// rebuilt its graph for another input size.
#[test]
fn scaled_encoder_encodes_the_source_pictures_at_its_own_size() {
    if !vesper_core::test_gpu() {
        return;
    }
    let (w, h) = (640, 360);
    let mut enc = Encoder::new(Codec::Hevc, w, h, 30, RateControl::ConstantQp(30)).unwrap();
    // Opened on another thread, as the pipeline does.
    let source = enc.scaled_source().unwrap();
    let mut live = std::thread::spawn(move || {
        source.open(
            Codec::Hevc,
            480,
            270,
            30,
            RateControl::ConstantBitrate(1_000_000),
        )
    })
    .join()
    .unwrap()
    .unwrap();
    assert!(
        live.encode_scaled(&enc, Duration::ZERO).is_err(),
        "no picture yet"
    );
    let ms = |t: u64| Duration::from_millis(1_000 + t * 33);
    let mut packets = Vec::new();
    let mut t = 0;
    for (generation, (pw, ph)) in [(640, 360), (1280, 720)].into_iter().enumerate() {
        let producer = unsafe { Producer::new(pw, ph) };
        for id in 0..3 {
            let frame = unsafe { producer.frame(id, t as u32, generation as u32, ms(t)) };
            enc.encode(frame).unwrap();
            if t == 4 {
                live.force_keyframe();
            }
            packets.extend(live.encode_scaled(&enc, ms(t)).unwrap());
            t += 1;
        }
        enc.repeat(ms(t)).unwrap();
        packets.extend(live.repeat(ms(t)).unwrap());
        t += 1;
    }
    packets.extend(live.flush().unwrap());
    let got: Vec<Duration> = packets.iter().map(|p| p.pts).collect();
    assert_eq!(got, (0..t).map(ms).collect::<Vec<_>>());
    let keys: Vec<_> = (0..packets.len())
        .filter(|&i| packets[i].keyframe)
        .collect();
    assert_eq!(keys, [0, 4]);
    let frames = unsafe { decode_luma(live.extradata(), &packets) };
    assert_eq!(frames.len(), t as usize);
    for f in &frames {
        assert_eq!((f.w, f.h), (480, 270));
        let mean = f.y.iter().map(|&v| f64::from(v)).sum::<f64>() / f.y.len() as f64;
        assert!(mean > 30.0, "the pattern, not black: {mean}");
    }
}

struct Luma {
    w: usize,
    h: usize,
    y: Vec<u8>,
    /// U (Cb) sample at the top-left corner: 128 when the corner is gray.
    corner_u: u8,
}

/// Decodes HEVC packets in software and returns each frame's luma plane.
unsafe fn decode_luma(extradata: &[u8], packets: &[vesper_core::EncodedPacket]) -> Vec<Luma> {
    use ffmpeg_next::ffi as ff;
    unsafe {
        let codec = ff::avcodec_find_decoder(ff::AVCodecID::AV_CODEC_ID_HEVC);
        let mut ctx = ff::avcodec_alloc_context3(codec);
        let pad = ff::AV_INPUT_BUFFER_PADDING_SIZE as usize;
        let extra = ff::av_mallocz(extradata.len() + pad) as *mut u8;
        std::ptr::copy_nonoverlapping(extradata.as_ptr(), extra, extradata.len());
        (*ctx).extradata = extra;
        (*ctx).extradata_size = extradata.len() as i32;
        common::ok(
            ff::avcodec_open2(ctx, codec, std::ptr::null_mut()),
            "open decoder",
        );
        let mut pkt = ff::av_packet_alloc();
        let mut frame = ff::av_frame_alloc();
        let mut out = Vec::new();
        let receive = |out: &mut Vec<Luma>| {
            while ff::avcodec_receive_frame(ctx, frame) >= 0 {
                let f = &*frame;
                let (w, h) = (f.width as usize, f.height as usize);
                let mut y = Vec::with_capacity(w * h);
                for r in 0..h {
                    let line = f.data[0].add(r * f.linesize[0] as usize);
                    y.extend_from_slice(std::slice::from_raw_parts(line, w));
                }
                let corner_u = *f.data[1];
                out.push(Luma { w, h, y, corner_u });
                ff::av_frame_unref(frame);
            }
        };
        for p in packets {
            common::ok(ff::av_new_packet(pkt, p.data.len() as i32), "packet");
            std::ptr::copy_nonoverlapping(p.data.as_ptr(), (*pkt).data, p.data.len());
            common::ok(ff::avcodec_send_packet(ctx, pkt), "decode");
            ff::av_packet_unref(pkt);
            receive(&mut out);
        }
        ff::avcodec_send_packet(ctx, std::ptr::null());
        receive(&mut out);
        ff::av_frame_free(&mut frame);
        ff::av_packet_free(&mut pkt);
        ff::avcodec_free_context(&mut ctx);
        out
    }
}

#[test]
fn quality_rate_control_trades_size_for_quality() {
    if !vesper_core::test_gpu() {
        return;
    }
    // radeonsi's H.264 QVBR ignores the quality value at 640x360 (measured
    // with the ffmpeg CLI too); it follows it from 1280x720 up.
    let (w, h) = (1280, 720);
    let producer = unsafe { Producer::new(w, h) };
    let bytes = |codec, quality| {
        let rate = RateControl::Quality {
            quality,
            max_bitrate: 50_000_000,
        };
        let mut enc = Encoder::new(codec, w, h, 30, rate).unwrap();
        let mut packets = Vec::new();
        for t in 0..4 {
            let pts = Duration::from_millis(1_000 + t as u64 * 33);
            packets.extend(
                enc.encode(unsafe { producer.frame(t as usize % POOL, t, 0, pts) })
                    .unwrap(),
            );
        }
        packets.extend(enc.flush().unwrap());
        assert_eq!(packets.len(), 4, "{codec:?}");
        assert!(packets[0].keyframe, "{codec:?}");
        packets.iter().map(|p| p.data.len()).sum::<usize>()
    };
    // AV1 quality is on a 0-255 scale: 80 / 160 is the same step as 20 / 40.
    for (codec, good, poor) in [
        (Codec::H264, 20, 40),
        (Codec::Hevc, 20, 40),
        (Codec::Av1, 80, 160),
    ] {
        let (big, small) = (bytes(codec, good), bytes(codec, poor));
        assert!(small < big, "{codec:?}: {small} >= {big} bytes");
    }
}
