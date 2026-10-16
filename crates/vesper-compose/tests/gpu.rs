//! Compositor tests on the real GPU. Skipped without `/dev/dri/renderD128`.

#[allow(dead_code)]
#[path = "../../vesper-encode/tests/common/mod.rs"]
mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::Producer;
use vesper_compose::{Compositor, Draw, Fit, OUTPUT_FOURCC, PREVIEW_HEIGHT, PREVIEW_WIDTH, Source};
use vesper_core::{Codec, ReleaseGuard, VideoFrame};
use vesper_encode::{Encoder, RateControl};

fn gpu() -> bool {
    let ok = Path::new("/dev/dri/renderD128").exists();
    if !ok {
        eprintln!("no GPU, skipping");
    }
    ok
}

fn full(source: Source, fit: Fit, opacity: f32) -> Draw {
    Draw {
        source,
        rect: [0.0, 0.0, 1.0, 1.0],
        crop: [0.0; 4],
        fit,
        opacity,
    }
}

/// Composes and reads the result back as RGBA through the preview pass,
/// which copies the frame 1:1 when the canvas is the preview size (the
/// output DMA-BUF itself is in VRAM without CPU access).
fn compose_read(
    c: &mut Compositor,
    main: Option<VideoFrame>,
    draws: &[Draw],
) -> (VideoFrame, Vec<u8>) {
    assert_eq!((c.width(), c.height()), (PREVIEW_WIDTH, PREVIEW_HEIGHT));
    let _ = c.preview();
    let f = c.compose(main, draws).unwrap();
    assert_eq!(f.fourcc, OUTPUT_FOURCC);
    assert_eq!(f.modifier, 0, "LINEAR");
    let px = c.preview().expect("preview of this compose").to_vec();
    (f, px)
}

/// The producer's pattern for frame `t` at (x, y), as BGR bytes.
fn pattern(w: u32, h: u32, t: u32, x: u32, y: u32) -> [u8; 3] {
    let (bx, by) = ((t * 9) % (w - 256), (t * 5) % (h - 256));
    if (bx..bx + 256).contains(&x) && (by..by + 256).contains(&y) {
        [255, 255, 255]
    } else {
        [(x + t * 4) as u8, (y + t * 2) as u8, ((x ^ y) >> 3) as u8]
    }
}

fn solid(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
    rgba.repeat((w * h) as usize)
}

#[test]
fn passthrough_is_bit_exact_and_releases_main_before_returning() {
    if !gpu() {
        return;
    }
    let (w, h) = (PREVIEW_WIDTH, PREVIEW_HEIGHT);
    let producer = unsafe { Producer::new(w, h) };
    let mut c = Compositor::new(w, h).unwrap();
    let released = Arc::new(AtomicUsize::new(0));
    for t in 0..6u32 {
        let pts = Duration::from_millis(1_000 + u64::from(t) * 16);
        let mut main = unsafe { producer.frame(t as usize % common::POOL, t, 0, pts) };
        if t == 0 {
            eprintln!(
                "producer modifier {:#x}, {} plane(s)",
                main.modifier,
                main.planes.len()
            );
        }
        let r = released.clone();
        main.release = ReleaseGuard::new(move || {
            r.fetch_add(1, Ordering::SeqCst);
        });
        let (out, px) = compose_read(&mut c, Some(main), &[full(Source::Main, Fit::Contain, 1.0)]);
        assert_eq!(
            released.load(Ordering::SeqCst),
            t as usize + 1,
            "main released"
        );
        assert_eq!(out.pts, pts);
        assert_eq!((out.width, out.height), (w, h));
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let [b, g, r] = pattern(w, h, t, x, y);
                assert_eq!(px[i..i + 4], [r, g, b, 255], "frame {t} pixel ({x}, {y})");
            }
        }
    }
    assert!(c.gpu_time().is_some(), "timestamps on the compute queue");
}

/// sRGB-encoded byte to linear and back, as the shader does.
fn lin(v: f64) -> f64 {
    if v > 0.04045 {
        ((v + 0.055) / 1.055).powf(2.4)
    } else {
        v / 12.92
    }
}

fn srgb(v: f64) -> f64 {
    if v > 0.0031308 {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    } else {
        v * 12.92
    }
}

#[test]
fn half_transparent_layer_blends_in_linear_light() {
    if !gpu() {
        return;
    }
    let (w, h) = (PREVIEW_WIDTH, PREVIEW_HEIGHT);
    let mut c = Compositor::new(w, h).unwrap();
    let bg = [20u8, 120, 240];
    c.set_texture(1, w, h, &solid(w, h, [bg[0], bg[1], bg[2], 255]))
        .unwrap();
    // Straight (200, 100, 50) at alpha 128, premultiplied.
    let fg = [200u8, 100, 50];
    let a = 128.0 / 255.0;
    let pre = fg.map(|v| (f64::from(v) * a).round() as u8);
    c.set_texture(2, 32, 32, &solid(32, 32, [pre[0], pre[1], pre[2], 128]))
        .unwrap();
    let overlay = Draw {
        rect: [0.0, 0.0, 0.5, 1.0],
        ..full(Source::Texture(2), Fit::Stretch, 1.0)
    };
    // Same layer at half opacity on the right: alpha 64.
    let faded = Draw {
        rect: [0.5, 0.0, 0.5, 1.0],
        ..full(Source::Texture(2), Fit::Stretch, 0.5)
    };
    let (out, px) = compose_read(
        &mut c,
        None,
        &[full(Source::Texture(1), Fit::Stretch, 1.0), overlay, faded],
    );
    assert_eq!(out.pts, Duration::ZERO);
    let expect = |alpha: f64| {
        (0..3).map(move |i| {
            let s = f64::from(pre[i]) / 255.0 / a;
            let d = f64::from(bg[i]) / 255.0;
            (srgb(lin(s) * alpha + lin(d) * (1.0 - alpha)) * 255.0).round()
        })
    };
    for (x, alpha) in [(100u32, a), (400, a * 0.5)] {
        let i = ((135 * w + x) * 4) as usize;
        let got = [px[i], px[i + 1], px[i + 2]];
        for (g, e) in got.iter().zip(expect(alpha)) {
            assert!(
                (f64::from(*g) - e).abs() <= 1.0,
                "x {x}: got {got:?}, want {:?}",
                expect(alpha).collect::<Vec<_>>()
            );
        }
        assert_eq!(px[i + 3], 255, "opaque output");
    }
}

#[test]
fn ring_slots_come_back_when_frames_drop_and_preview_follows() {
    if !gpu() {
        return;
    }
    let (w, h) = (PREVIEW_WIDTH, PREVIEW_HEIGHT);
    let mut c = Compositor::new(w, h).unwrap();
    c.set_texture(7, 4, 4, &solid(4, 4, [255, 0, 0, 255]))
        .unwrap();
    let draws = [full(Source::Texture(7), Fit::Stretch, 1.0)];
    assert!(c.preview().is_none(), "nothing composed yet");
    let held: Vec<_> = (0..3).map(|_| c.compose(None, &draws).unwrap()).collect();
    let ids: Vec<_> = held.iter().map(|f| f.buffer_id).collect();
    assert_eq!(ids, [0, 1, 2]);
    assert!(c.compose(None, &draws).is_err(), "ring exhausted");
    drop(held);
    let f = c.compose(None, &draws).unwrap();
    assert_eq!(f.buffer_id, 0);
    drop(f);

    // Requested before the first compose, which ran the preview pass.
    let p = c.preview().expect("a finished preview");
    assert_eq!(p.len(), (PREVIEW_WIDTH * PREVIEW_HEIGHT * 4) as usize);
    let mid = ((135 * PREVIEW_WIDTH + 240) * 4) as usize;
    assert_eq!(p[mid..mid + 4], [255, 0, 0, 255], "RGBA, red");
    assert!(c.preview().is_none(), "handed out once");

    // Missing sources are skipped, not errors.
    c.remove_texture(7);
    let (_, px) = compose_read(
        &mut c,
        None,
        &[full(Source::Main, Fit::Contain, 1.0), draws[0]],
    );
    assert_eq!(px[..4], [0, 0, 0, 255], "black");
}

#[test]
fn composed_frames_encode_with_the_right_colours() {
    if !gpu() {
        return;
    }
    let (w, h) = (640, 360);
    let mut c = Compositor::new(w, h).unwrap();
    let left = [200u8, 40, 60];
    let right = [30u8, 90, 220];
    c.set_texture(1, 8, 8, &solid(8, 8, [left[0], left[1], left[2], 255]))
        .unwrap();
    c.set_texture(2, 8, 8, &solid(8, 8, [right[0], right[1], right[2], 255]))
        .unwrap();
    let draws = [
        Draw {
            rect: [0.0, 0.0, 0.5, 1.0],
            ..full(Source::Texture(1), Fit::Stretch, 1.0)
        },
        Draw {
            rect: [0.5, 0.0, 0.5, 1.0],
            ..full(Source::Texture(2), Fit::Stretch, 1.0)
        },
    ];
    let mut enc = Encoder::new(Codec::Hevc, w, h, 30, RateControl::ConstantQp(20)).unwrap();
    let mut packets = Vec::new();
    for t in 0..3u64 {
        let mut f = c.compose(None, &draws).unwrap();
        f.pts = Duration::from_millis(1_000 + t * 33);
        packets.extend(enc.encode(f).unwrap());
    }
    packets.extend(enc.flush().unwrap());
    let frames = unsafe { decode(enc.extradata(), &packets) };
    assert_eq!(frames.len(), 3);
    // BT.709 limited range, as the encoder converts.
    let yuv = |rgb: [u8; 3]| {
        let [r, g, b] = rgb.map(|v| f64::from(v) / 255.0);
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        [
            16.0 + 219.0 * y,
            128.0 + 224.0 * (b - y) / 1.8556,
            128.0 + 224.0 * (r - y) / 1.5748,
        ]
    };
    for f in &frames {
        for (x, rgb) in [(160usize, left), (480, right)] {
            let got = f.at(x, 180);
            let want = yuv(rgb);
            for k in 0..3 {
                assert!(
                    (f64::from(got[k]) - want[k]).abs() <= 6.0,
                    "x {x}: got YUV {got:?}, want {want:?}"
                );
            }
        }
    }
}

struct Yuv {
    w: usize,
    planes: [Vec<u8>; 3],
}

impl Yuv {
    fn at(&self, x: usize, y: usize) -> [u8; 3] {
        let cw = self.w / 2;
        [
            self.planes[0][y * self.w + x],
            self.planes[1][(y / 2) * cw + x / 2],
            self.planes[2][(y / 2) * cw + x / 2],
        ]
    }
}

/// Decodes HEVC packets in software to 4:2:0 planes.
unsafe fn decode(extradata: &[u8], packets: &[vesper_core::EncodedPacket]) -> Vec<Yuv> {
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
        let receive = |out: &mut Vec<Yuv>| {
            while ff::avcodec_receive_frame(ctx, frame) >= 0 {
                let f = &*frame;
                assert_eq!(f.format, ff::AVPixelFormat::AV_PIX_FMT_YUV420P as i32);
                let (w, h) = (f.width as usize, f.height as usize);
                let plane = |i: usize, pw: usize, ph: usize| {
                    let mut v = Vec::with_capacity(pw * ph);
                    for r in 0..ph {
                        let line = f.data[i].add(r * f.linesize[i] as usize);
                        v.extend_from_slice(std::slice::from_raw_parts(line, pw));
                    }
                    v
                };
                out.push(Yuv {
                    w,
                    planes: [
                        plane(0, w, h),
                        plane(1, w / 2, h / 2),
                        plane(2, w / 2, h / 2),
                    ],
                });
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

/// A producer's frame description is not trusted: a plane count or a
/// modifier the driver does not have is an error, never an import.
#[test]
fn rejects_frames_the_driver_cannot_import() {
    if !gpu() {
        return;
    }
    let (w, h) = (PREVIEW_WIDTH, PREVIEW_HEIGHT);
    let producer = unsafe { Producer::new(w, h) };
    let mut c = Compositor::new(w, h).unwrap();
    let draws = [full(Source::Main, Fit::Contain, 1.0)];

    let mut extra = unsafe { producer.frame(0, 0, 0, Duration::ZERO) };
    let dup = extra.planes[0].fd.try_clone().unwrap();
    extra.planes.push(vesper_core::DmaBufPlane {
        fd: dup,
        offset: 0,
        stride: extra.planes[0].stride,
    });
    let e = c.compose(Some(extra), &draws).unwrap_err();
    assert!(e.to_string().contains("plane"), "{e}");

    let mut odd = unsafe { producer.frame(1, 0, 0, Duration::ZERO) };
    odd.modifier = 0x00de_adbe_ef00_0000;
    assert!(c.compose(Some(odd), &draws).is_err());

    let mut huge = unsafe { producer.frame(2, 0, 0, Duration::ZERO) };
    huge.height = 100_000;
    assert!(c.compose(Some(huge), &draws).is_err());

    // Still fine with a real frame afterwards.
    let ok = unsafe { producer.frame(3, 2, 0, Duration::ZERO) };
    let (_, px) = compose_read(&mut c, Some(ok), &draws);
    let [b, g, r] = pattern(w, h, 2, 300, 200);
    let i = ((200 * w + 300) * 4) as usize;
    assert_eq!(px[i..i + 4], [r, g, b, 255]);
}

/// Two producers reusing the same buffer id and generation (another pool)
/// must not hit each other's cached import.
#[test]
fn same_buffer_id_and_generation_on_another_buffer_is_reimported() {
    if !gpu() {
        return;
    }
    let (w, h) = (PREVIEW_WIDTH, PREVIEW_HEIGHT);
    let a = unsafe { Producer::new(w, h) };
    let b = unsafe { Producer::new(w, h) };
    let mut c = Compositor::new(w, h).unwrap();
    let draws = [full(Source::Main, Fit::Contain, 1.0)];
    for (producer, t) in [(&a, 0), (&b, 3), (&a, 5)] {
        let f = unsafe { producer.frame(0, t, 7, Duration::ZERO) };
        let (_, px) = compose_read(&mut c, Some(f), &draws);
        // Outside every box of these frames.
        let (x, y) = (470, 265);
        let [bb, g, r] = pattern(w, h, t, x, y);
        let i = ((y * w + x) * 4) as usize;
        assert_eq!(px[i..i + 4], [r, g, bb, 255], "frame t={t}");
    }
}

/// Textures far bigger than the canvas lose their unused top levels but
/// still draw right.
#[test]
fn huge_textures_are_reduced_and_still_drawn() {
    if !gpu() {
        return;
    }
    let (w, h) = (PREVIEW_WIDTH, PREVIEW_HEIGHT);
    let mut c = Compositor::new(w, h).unwrap();
    c.set_texture(1, 4000, 2000, &solid(4000, 2000, [10, 200, 30, 255]))
        .unwrap();
    let (_, px) = compose_read(&mut c, None, &[full(Source::Texture(1), Fit::Cover, 1.0)]);
    let i = ((135 * w + 240) * 4) as usize;
    assert_eq!(px[i..i + 4], [10, 200, 30, 255]);
}

/// A kept copy of the main frame stands in for it, bit-exact, once the
/// frame itself went back to its producer.
#[test]
fn the_kept_copy_redraws_main_after_its_release() {
    if !gpu() {
        return;
    }
    let (w, h) = (PREVIEW_WIDTH, PREVIEW_HEIGHT);
    let producer = unsafe { Producer::new(w, h) };
    let mut c = Compositor::new(w, h).unwrap();
    let main_draw = [full(Source::Main, Fit::Contain, 1.0)];
    let main = unsafe { producer.frame(0, 3, 0, Duration::ZERO) };
    c.submit(Some(main), &main_draw, true).unwrap();
    let first = loop {
        if let Some(f) = c.poll(Duration::from_millis(5)).unwrap() {
            break f;
        }
    };
    drop(first);
    assert!(c.has_base());
    // No main: the copy, plus an overlay drawn over it.
    c.set_texture(1, 8, 8, &solid(8, 8, [0, 0, 255, 255]))
        .unwrap();
    let mut draws = main_draw.to_vec();
    draws.push(Draw {
        rect: [0.0, 0.0, 0.25, 0.25],
        ..full(Source::Texture(1), Fit::Stretch, 1.0)
    });
    let (_, px) = compose_read(&mut c, None, &draws);
    for (x, y) in [(200u32, 200u32), (479, 269), (130, 10)] {
        let i = ((y * w + x) * 4) as usize;
        let [b, g, r] = pattern(w, h, 3, x, y);
        assert_eq!(px[i..i + 4], [r, g, b, 255], "pixel ({x}, {y})");
    }
    assert_eq!(px[..4], [0, 0, 255, 255], "overlay");
    c.drop_base();
    assert!(!c.has_base());
}

/// `submit` returns at once; one composition at a time; `poll` hands the
/// frame over and releases main by then.
#[test]
fn submit_does_not_wait_and_poll_releases_main() {
    if !gpu() {
        return;
    }
    let (w, h) = (640, 360);
    let producer = unsafe { Producer::new(w, h) };
    let mut c = Compositor::new(w, h).unwrap();
    let released = Arc::new(AtomicUsize::new(0));
    let mut main = unsafe { producer.frame(0, 0, 0, Duration::from_secs(7)) };
    let r = released.clone();
    main.release = ReleaseGuard::new(move || {
        r.fetch_add(1, Ordering::SeqCst);
    });
    let draws = [full(Source::Main, Fit::Contain, 1.0)];
    c.submit(Some(main), &draws, false).unwrap();
    assert!(c.in_flight());
    assert!(c.submit(None, &draws, false).is_err(), "one at a time");
    let f = loop {
        if let Some(f) = c.poll(Duration::ZERO).unwrap() {
            break f;
        }
        std::thread::sleep(Duration::from_micros(200));
    };
    assert_eq!(released.load(Ordering::SeqCst), 1);
    assert_eq!(f.pts, Duration::from_secs(7));
    assert!(!c.in_flight());
    assert!(c.poll(Duration::ZERO).is_err(), "nothing in flight");
}

/// Textures are staged on another thread and queued while a composition
/// is in flight; the replaced one stays valid for it.
#[test]
fn textures_staged_elsewhere_are_set_without_waiting() {
    if !gpu() {
        return;
    }
    let (w, h) = (PREVIEW_WIDTH, PREVIEW_HEIGHT);
    let mut c = Compositor::new(w, h).unwrap();
    let up = c.uploader();
    let staged = std::thread::spawn(move || up.prepare(4, 4, &solid(4, 4, [255, 0, 0, 255])))
        .join()
        .unwrap()
        .unwrap();
    c.set_staged(1, staged).unwrap();
    let draws = [full(Source::Texture(1), Fit::Stretch, 1.0)];
    c.submit(None, &draws, false).unwrap();
    let green = c
        .uploader()
        .prepare(4, 4, &solid(4, 4, [0, 255, 0, 255]))
        .unwrap();
    c.set_staged(1, green).unwrap();
    let f = loop {
        if let Some(f) = c.poll(Duration::from_millis(5)).unwrap() {
            break f;
        }
    };
    drop(f);
    let (_, px) = compose_read(&mut c, None, &draws);
    assert_eq!(px[..4], [0, 255, 0, 255]);
}
