//! Compositor tests on the real GPU. Skipped without `/dev/dri/renderD128`.

#[allow(dead_code)]
#[path = "../../vesper-encode/tests/common/mod.rs"]
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::Producer;
use vesper_compose::{
    Compositor, Draw, Fit, OUTPUT_FOURCC, PREVIEW_HEIGHT, PREVIEW_WIDTH, Source, YuvLayout,
    YuvMatrix, YuvPicture,
};
use vesper_core::{Codec, ReleaseGuard, VideoFrame};
use vesper_encode::{Encoder, RateControl};

fn gpu() -> bool {
    vesper_core::test_gpu()
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

/// RGBA test picture a bad downscale gets most wrong: 1-pixel checks, colour
/// noise and thin lines (text-like detail), premultiplied (opaque).
fn detailed(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let n = common::noise;
            let px = match x * 3 / w {
                0 => [if (x + y) % 2 == 0 { 250 } else { 5 }, 40, 200],
                1 => [n(x, y), n(y, x), n(x ^ y, x)],
                _ if x % 3 == 0 || y % 5 == 0 => [230, 230, 230],
                _ => [30, 30, 46],
            };
            v.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
    }
    v
}

/// Box (area) filter of the `sw` x `sh` region at (x0, y0) of an RGBA
/// picture `w` wide into `ow` x `oh`: each output pixel is the average of the
/// source area it covers, partial texels weighted by their overlap.
fn area(src: &[u8], w: u32, region: [u32; 4], ow: u32, oh: u32) -> Vec<[f64; 3]> {
    let [x0, y0, sw, sh] = region;
    let weights = |o: u32, n: u32, on: u32| {
        let k = f64::from(n) / f64::from(on);
        let (lo, hi) = (f64::from(o) * k, f64::from(o + 1) * k);
        (lo.floor() as u32..hi.ceil() as u32)
            .map(move |i| (i, (hi.min(f64::from(i + 1)) - lo.max(f64::from(i))) / k))
    };
    let mut out = Vec::with_capacity((ow * oh) as usize);
    for oy in 0..oh {
        for ox in 0..ow {
            let mut c = [0.0; 3];
            for (y, wy) in weights(oy, sh, oh) {
                for (x, wx) in weights(ox, sw, ow) {
                    let i = (((y0 + y) * w + x0 + x) * 4) as usize;
                    for (k, c) in c.iter_mut().enumerate() {
                        *c += wx * wy * f64::from(src[i + k]);
                    }
                }
            }
            out.push(c);
        }
    }
    out
}

/// Largest difference between an RGBA picture and a reference, in 8-bit steps.
fn max_error(px: &[u8], reference: &[[f64; 3]]) -> f64 {
    px.as_chunks::<4>()
        .0
        .iter()
        .zip(reference)
        .flat_map(|(p, r)| (0..3).map(move |k| (f64::from(p[k]) - r[k]).abs()))
        .fold(0.0, f64::max)
}

/// A main capture drawn smaller than its size is box filtered: 4K into
/// the 480x270 preview canvas (8:1), and crops at 2:1 (4K into 1080p) and
/// 4:3 (1440p into 1080p, not a whole ratio) and 8:1 by 2:1, each within rounding of an
/// exact area filter (bilinear sampling was off by more than 100).
#[test]
fn downscaled_main_matches_an_area_filter() {
    if !gpu() {
        return;
    }
    let (w, h) = (3840, 2160);
    let src = detailed(w, h);
    let mut big = Compositor::new(w, h).unwrap();
    big.set_texture(1, w, h, &src).unwrap();
    let mut small = Compositor::new(PREVIEW_WIDTH, PREVIEW_HEIGHT).unwrap();
    // Source regions (x, y, width, height) drawn over the whole canvas.
    for region in [
        [0, 0, w, h],
        [960, 540, 960, 540],
        [1000, 600, 640, 360],
        // Anisotropic: 8:1 across, 2:1 down.
        [0, 1000, w, 540],
    ] {
        let main = big
            .compose(None, &[full(Source::Texture(1), Fit::Stretch, 1.0)])
            .unwrap();
        let [x, y, rw, rh] = region.map(|v| v as f32);
        let (w, h) = (w as f32, h as f32);
        let draw = Draw {
            crop: [x / w, y / h, 1.0 - (x + rw) / w, 1.0 - (y + rh) / h],
            ..full(Source::Main, Fit::Stretch, 1.0)
        };
        let (_, px) = compose_read(&mut small, Some(main), &[draw]);
        let reference = area(&src, 3840, region, PREVIEW_WIDTH, PREVIEW_HEIGHT);
        let e = max_error(&px, &reference);
        assert!(e <= 1.0, "region {region:?}: off by {e}");
    }
}

/// A texture drawn much smaller reads the mip where a pixel spans 2 to 4
/// texels and box filters it: within the mips' own rounding of an area
/// filter of the full picture.
#[test]
fn downscaled_textures_match_an_area_filter() {
    if !gpu() {
        return;
    }
    let src = detailed(800, 800);
    let mut c = Compositor::new(PREVIEW_WIDTH, PREVIEW_HEIGHT).unwrap();
    c.set_texture(1, 800, 800, &src).unwrap();
    // 100x100 pixels at (40, 20): 8:1, filtered on mip 2.
    let (cw, ch) = (PREVIEW_WIDTH as f32, PREVIEW_HEIGHT as f32);
    let draw = Draw {
        rect: [40.0 / cw, 20.0 / ch, 100.0 / cw, 100.0 / ch],
        ..full(Source::Texture(1), Fit::Stretch, 1.0)
    };
    let (_, px) = compose_read(&mut c, None, &[draw]);
    let reference = area(&src, 800, [0, 0, 800, 800], 100, 100);
    let mut drawn = Vec::new();
    for y in 20..120 {
        let row = (y * PREVIEW_WIDTH + 40) as usize * 4;
        drawn.extend_from_slice(&px[row..row + 400]);
    }
    let e = max_error(&drawn, &reference);
    assert!(e <= 2.0, "off by {e}");
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

/// A texture removed (or replaced) right after its copy was queued, with or
/// without a composition in flight, is freed only once the copy is done:
/// the GPU never writes freed memory (that would fault or lose the device).
#[test]
fn textures_removed_while_their_copy_runs_are_freed_after_it() {
    if !gpu() {
        return;
    }
    let (w, h) = (PREVIEW_WIDTH, PREVIEW_HEIGHT);
    let mut c = Compositor::new(w, h).unwrap();
    let big = solid(2048, 2048, [9, 9, 9, 255]);
    let draws = [full(Source::Texture(2), Fit::Stretch, 1.0)];
    c.set_texture(2, 4, 4, &solid(4, 4, [0, 0, 255, 255]))
        .unwrap();
    for round in 0..20 {
        // A: queued, then removed at once.
        let s = c.uploader().prepare(2048, 2048, &big).unwrap();
        c.set_staged(1, s).unwrap();
        c.remove_texture(1);
        // Kept while its copy may still write it (checked in this order:
        // nothing frees it between the two calls).
        if !c.copies_done() {
            assert!(c.garbage() > 0, "round {round}: freed during its copy");
        }
        // B: a composition in flight, a copy queued behind it, removed,
        // then the composition polled out (which collects garbage).
        c.submit(None, &draws, false).unwrap();
        let s = c.uploader().prepare(2048, 2048, &big).unwrap();
        c.set_staged(3, s).unwrap();
        c.remove_texture(3);
        let f = loop {
            if let Some(f) = c.poll(Duration::from_millis(5)).unwrap() {
                break f;
            }
        };
        if !c.copies_done() {
            assert!(c.garbage() > 0, "round {round}: freed during its copy");
        }
        drop(f);
        assert!(!c.is_lost(), "round {round}");
    }
    let (_, px) = compose_read(&mut c, None, &draws);
    assert_eq!(px[..4], [0, 0, 255, 255]);
}

/// Y'CbCr bytes to R'G'B' bytes, in f64: the reference for the shader.
fn yuv_to_rgb(yuv: [f64; 3], matrix: YuvMatrix, full_range: bool) -> [f64; 3] {
    let (kr, kb) = match matrix {
        YuvMatrix::Bt601 => (0.299, 0.114),
        YuvMatrix::Bt709 => (0.2126, 0.0722),
        YuvMatrix::Bt2020 => (0.2627, 0.0593),
    };
    let kg = 1.0 - kr - kb;
    let [y, u, v] = yuv;
    let (y, cb, cr) = if full_range {
        (y / 255.0, (u - 128.0) / 255.0, (v - 128.0) / 255.0)
    } else {
        ((y - 16.0) / 219.0, (u - 128.0) / 224.0, (v - 128.0) / 224.0)
    };
    let r = y + 2.0 * (1.0 - kr) * cr;
    let b = y + 2.0 * (1.0 - kb) * cb;
    let g = (y - kr * r - kb * b) / kg;
    [r, g, b].map(|c| (c.clamp(0.0, 1.0) * 255.0).round())
}

/// A `w` x `h` picture coloured by `colour(x, y)` (constant over 2 x 2
/// blocks), with odd plane offsets and padded rows: (planes, data).
fn yuv_picture(
    layout: YuvLayout,
    w: usize,
    h: usize,
    colour: impl Fn(usize, usize) -> [u8; 3],
) -> (Vec<(usize, usize)>, Vec<u8>) {
    let pad = 16;
    match layout {
        YuvLayout::Nv12 => {
            let (ys, cs) = (w + pad, w + pad);
            let uv_at = 7 + ys * h + 5;
            let mut data = vec![0xaa; uv_at + cs * h / 2 + 3];
            for y in 0..h {
                for x in 0..w {
                    let [l, u, v] = colour(x, y);
                    data[7 + y * ys + x] = l;
                    let c = uv_at + (y / 2) * cs + (x / 2) * 2;
                    (data[c], data[c + 1]) = (u, v);
                }
            }
            (vec![(7, ys), (uv_at, cs)], data)
        }
        YuvLayout::Yuy2 => {
            let s = w * 2 + pad;
            let mut data = vec![0xaa; 3 + s * h];
            for y in 0..h {
                for x in 0..w {
                    let [l, u, v] = colour(x, y);
                    let i = 3 + y * s + (x / 2) * 4;
                    data[i + (x & 1) * 2] = l;
                    (data[i + 1], data[i + 3]) = (u, v);
                }
            }
            (vec![(3, s)], data)
        }
    }
}

/// NV12 and YUY2 pictures in BT.601 full and limited and BT.709 limited
/// come out as the reference conversion (±2), pure red as published.
#[test]
fn yuv_camera_pictures_convert_like_the_reference() {
    if !gpu() {
        return;
    }
    let (w, h) = (PREVIEW_WIDTH, PREVIEW_HEIGHT);
    let mut c = Compositor::new(w, h).unwrap();
    let (cw, ch) = (128usize, 32usize);
    // Pure red in each matrix and range.
    let anchors = [
        (YuvMatrix::Bt601, false, [81, 90, 240]),
        (YuvMatrix::Bt709, false, [63, 102, 240]),
        (YuvMatrix::Bt601, true, [76, 85, 255]),
    ];
    let mut seed = 0x1234_5678u32;
    let mut next = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 24) as u8
    };
    let mut colours = vec![
        [0, 128, 128],
        [255, 128, 128],
        [16, 16, 16],
        [235, 240, 240],
    ];
    colours.extend((0..60).map(|_| [next(), next(), next()]));
    let blocks = cw / 8;
    for layout in [YuvLayout::Nv12, YuvLayout::Yuy2] {
        for (matrix, full_range, red) in anchors {
            let colour = |x: usize, y: usize| {
                let b = (y / 8) * blocks + x / 8;
                if b == 0 {
                    red
                } else {
                    colours[b % colours.len()]
                }
            };
            let (planes, data) = yuv_picture(layout, cw, ch, colour);
            let staged = c
                .uploader()
                .prepare_yuv(&YuvPicture {
                    width: cw as u32,
                    height: ch as u32,
                    layout,
                    planes: &planes,
                    data: &data,
                    matrix,
                    full_range,
                })
                .unwrap();
            c.set_camera_yuv(3, staged).unwrap();
            let at_native = Draw {
                rect: [0.0, 0.0, cw as f32 / w as f32, ch as f32 / h as f32],
                ..full(Source::Camera(3), Fit::Stretch, 1.0)
            };
            let (_, px) = compose_read(&mut c, None, &[at_native]);
            let what = format!("{layout:?} {matrix:?} full {full_range}");
            let i = ((4 * w + 4) * 4) as usize;
            for (k, want) in [255, 0, 0].into_iter().enumerate() {
                let got = i32::from(px[i + k]);
                assert!(
                    (got - want).abs() <= 2,
                    "{what}: red is {:?}",
                    &px[i..i + 3]
                );
            }
            for by in 0..ch / 8 {
                for bx in 0..blocks {
                    let (x, y) = (bx * 8 + 3, by * 8 + 5);
                    let want = yuv_to_rgb(colour(x, y).map(f64::from), matrix, full_range);
                    let i = (y * w as usize + x) * 4;
                    for k in 0..3 {
                        assert!(
                            (f64::from(px[i + k]) - want[k]).abs() <= 2.0,
                            "{what}: block ({bx}, {by}) yuv {:?} is {:?}, want {want:?}",
                            colour(x, y),
                            &px[i..i + 3]
                        );
                    }
                }
            }
            // The last column of a block sits halfway between its chroma
            // and the next block's: bilinear, not nearest.
            for bx in 0..blocks - 1 {
                let (x, y) = (bx * 8 + 7, 13);
                let (a, b) = (colour(x, y), colour(x + 1, y));
                let mid = |k: usize| (f64::from(a[k]) + f64::from(b[k])) / 2.0;
                let want = yuv_to_rgb([f64::from(a[0]), mid(1), mid(2)], matrix, full_range);
                let i = (y * w as usize + x) * 4;
                for k in 0..3 {
                    assert!(
                        (f64::from(px[i + k]) - want[k]).abs() <= 2.0,
                        "{what}: edge of block {bx} is {:?}, want {want:?}",
                        &px[i..i + 3]
                    );
                }
            }
            assert_eq!(
                px[(cw * 4) + 4 * 4 * w as usize..][..4],
                [0, 0, 0, 255],
                "outside"
            );
        }
    }
}

/// A camera's DMA-BUF is copied 1:1, released by the composition that
/// copied it, and redrawn from the copy; one the GPU cannot import hides
/// the camera without failing the composition.
#[test]
fn camera_frames_are_copied_then_released_and_bad_ones_hide_the_camera() {
    if !gpu() {
        return;
    }
    let (w, h) = (PREVIEW_WIDTH, PREVIEW_HEIGHT);
    let producer = unsafe { Producer::new(w, h) };
    let mut c = Compositor::new(w, h).unwrap();
    let released = Arc::new(AtomicUsize::new(0));
    let frame = |t: u32| {
        let mut f = unsafe { producer.frame(t as usize % common::POOL, t, 7, Duration::ZERO) };
        let r = released.clone();
        f.release = ReleaseGuard::new(move || {
            r.fetch_add(1, Ordering::SeqCst);
        });
        f
    };
    let draws = [full(Source::Camera(1), Fit::Stretch, 1.0)];
    let check = |px: &[u8], t: u32| {
        for (x, y) in [(0, 0), (17, 200), (300, 40), (w - 1, h - 1)] {
            let i = ((y * w + x) * 4) as usize;
            let [b, g, r] = pattern(w, h, t, x, y);
            assert_eq!(px[i..i + 4], [r, g, b, 255], "picture {t} at ({x}, {y})");
        }
    };
    // Latest wins: the first picture goes back unused.
    c.set_camera_frame(1, frame(1)).unwrap();
    c.set_camera_frame(1, frame(2)).unwrap();
    assert_eq!(released.load(Ordering::SeqCst), 1);
    let (_, px) = compose_read(&mut c, None, &draws);
    assert_eq!(released.load(Ordering::SeqCst), 2, "copied, then released");
    check(&px, 2);
    // Redrawn from the copy: nothing held.
    let (_, px) = compose_read(&mut c, None, &draws);
    check(&px, 2);
    c.set_camera_frame(1, frame(3)).unwrap();
    let (_, px) = compose_read(&mut c, None, &draws);
    check(&px, 3);
    assert_eq!(released.load(Ordering::SeqCst), 3);

    // A picture the GPU cannot take: the camera is hidden, the rest drawn.
    c.set_texture(9, 4, 4, &solid(4, 4, [0, 0, 255, 255]))
        .unwrap();
    let mut bad = frame(4);
    bad.fourcc = u32::from_le_bytes(*b"NV12");
    c.set_camera_frame(1, bad).unwrap();
    let both = [full(Source::Texture(9), Fit::Stretch, 1.0), draws[0]];
    let (_, px) = compose_read(&mut c, None, &both);
    assert_eq!(released.load(Ordering::SeqCst), 4);
    assert_eq!(px[..4], [0, 0, 255, 255], "camera hidden, texture drawn");
    let failures = c.take_camera_failures();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0].0, 1);
    // Back with its next good picture.
    c.set_camera_frame(1, frame(5)).unwrap();
    let (_, px) = compose_read(&mut c, None, &both);
    check(&px, 5);
    assert!(c.take_camera_failures().is_empty());
    c.remove_camera(1);
    let (_, px) = compose_read(&mut c, None, &both);
    assert_eq!(px[..4], [0, 0, 255, 255], "removed");
    assert_eq!(released.load(Ordering::SeqCst), 5);
}
