//! Audit round 2: the letterbox bars of a scaled picture must be black.
//!
//! Regression tests for a bug (found by `encode.rs`'s
//! `scales_frames_of_other_sizes_to_the_encoder_size` failing about half the
//! time when another process loads the GPU): when the source went back to a
//! size it already had (a game toggling windowed and fullscreen, each size a
//! new graph and pad pool) the bars showed picture pixels instead of black,
//! for as long as the size lasted. radeonsi never paints `pad_vaapi`'s
//! background, so the bars kept whatever the recycled VRAM held.
//!
//! `R2_SIZES=1280x720,1920x800 cargo test -p vesper-encode --test busy_gpu
//! -- --nocapture` tries another sequence of source sizes.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{POOL, Producer};
use vesper_core::{Codec, EncodedPacket};
use vesper_encode::{Encoder, RateControl};

const OUT: (usize, usize) = (640, 360);

/// A decoded frame's luma plane.
struct Luma {
    w: usize,
    h: usize,
    y: Vec<u8>,
}

impl Luma {
    fn mean(&self, x0: usize, x1: usize, y0: usize, y1: usize) -> f64 {
        let mut sum = 0.0;
        for y in y0..y1 {
            for x in x0..x1 {
                sum += f64::from(self.y[y * self.w + x]);
            }
        }
        sum / ((x1 - x0) * (y1 - y0)) as f64
    }
}

/// Decodes HEVC packets in software.
unsafe fn decode(extradata: &[u8], packets: &[EncodedPacket]) -> Vec<Luma> {
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
                out.push(Luma { w, h, y });
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

/// `n` encoders of 1080p busy with test patterns until `stop`.
fn load(n: usize, stop: &Arc<AtomicBool>) -> Vec<std::thread::JoinHandle<()>> {
    (0..n)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let (w, h) = (1920, 1080);
                let producer = unsafe { Producer::new(w, h) };
                let mut enc =
                    Encoder::new(Codec::Hevc, w, h, 60, RateControl::ConstantQp(30)).unwrap();
                let mut t = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    let pts = Duration::from_millis(1_000 + u64::from(t) * 16);
                    let frame = unsafe { producer.frame(t as usize % POOL, t, 0, pts) };
                    enc.encode(frame).unwrap();
                    t += 1;
                }
            })
        })
        .collect()
}

/// The bar rectangles `(x0, x1, y0, y1)` of a `pw`x`ph` source fitted in
/// [`OUT`], 4 px inside the picture edge; none when it fills the frame.
fn bars(pw: usize, ph: usize) -> Vec<(usize, usize, usize, usize)> {
    let (w, h) = OUT;
    if pw * h > ph * w {
        // Wider than the output: bars above and below.
        let bar = (h - ((w * ph / pw) & !1)) / 2;
        if bar < 8 {
            return vec![];
        }
        vec![(0, w, 0, bar - 4), (0, w, h - bar + 4, h)]
    } else if pw * h < ph * w {
        let bar = (w - ((h * pw / ph) & !1)) / 2;
        if bar < 8 {
            return vec![];
        }
        vec![(0, bar - 4, 0, h), (w - bar + 4, w, 0, h)]
    } else {
        vec![]
    }
}

type Lit = Vec<(usize, (usize, usize), f64)>;

/// Encodes 3 frames per size, `rounds` times over the list, each size a new
/// producer pool and generation (a game resize). Returns the frame count and,
/// for the frames whose bars are above luma 20, `(frame, size, bar luma)`.
fn lit_bars(sizes: &[(usize, usize)], rounds: usize, busy: usize) -> (usize, Lit) {
    let stop = Arc::new(AtomicBool::new(false));
    let loaders = load(busy, &stop);
    let (w, h) = (OUT.0 as u32, OUT.1 as u32);
    let mut enc = Encoder::new(Codec::Hevc, w, h, 30, RateControl::ConstantQp(30)).unwrap();
    let mut packets = Vec::new();
    let mut plan = Vec::new();
    let (mut t, mut generation) = (0u32, 0u32);
    for _ in 0..rounds {
        for &(pw, ph) in sizes {
            let producer = unsafe { Producer::new(pw as u32, ph as u32) };
            for id in 0..3 {
                let pts = Duration::from_millis(1_000 + u64::from(t) * 33);
                let frame = unsafe { producer.frame(id, t, generation, pts) };
                packets.extend(enc.encode(frame).unwrap());
                plan.push((pw, ph));
                t += 1;
            }
            generation += 1;
        }
    }
    packets.extend(enc.flush().unwrap());
    stop.store(true, Ordering::Relaxed);
    for l in loaders {
        l.join().unwrap();
    }
    let frames = unsafe { decode(enc.extradata(), &packets) };
    assert_eq!(frames.len(), plan.len());
    let mut lit = Vec::new();
    for (i, (f, &(pw, ph))) in frames.iter().zip(&plan).enumerate() {
        assert_eq!((f.w, f.h), OUT);
        let (w, h) = OUT;
        let centre = f.mean(w / 4, 3 * w / 4, h / 2 - 8, h / 2 + 8);
        assert!(centre > 40.0, "frame {i}: no picture (luma {centre})");
        let worst = bars(pw, ph)
            .into_iter()
            .map(|(x0, x1, y0, y1)| f.mean(x0, x1, y0, y1))
            .fold(0.0, f64::max);
        if worst > 20.0 {
            lit.push((i, (pw, ph), worst));
        }
    }
    (frames.len(), lit)
}

fn sizes() -> Vec<(usize, usize)> {
    match std::env::var("R2_SIZES") {
        Ok(s) => s
            .split(',')
            .map(|p| {
                let (w, h) = p.split_once('x').unwrap();
                (w.parse().unwrap(), h.parse().unwrap())
            })
            .collect(),
        Err(_) => vec![(400, 300), (1920, 800)],
    }
}

fn has_gpu() -> bool {
    vesper_core::test_gpu()
}

#[test]
fn bars_are_black_after_a_size_change() {
    if !has_gpu() {
        return;
    }
    let (n, lit) = lit_bars(&sizes(), 4, 0);
    eprintln!("{} of {n} frames have lit bars: {lit:?}", lit.len());
    assert!(
        lit.is_empty(),
        "lit bars (frame, source size, luma): {lit:?}"
    );
}

#[test]
fn bars_stay_black_while_the_gpu_is_busy() {
    if !has_gpu() {
        return;
    }
    let (n, lit) = lit_bars(&sizes(), 4, 3);
    eprintln!("{} of {n} frames have lit bars: {lit:?}", lit.len());
    assert!(
        lit.is_empty(),
        "lit bars (frame, source size, luma): {lit:?}"
    );
}

/// What works today, to keep a fix honest: one size from the start.
#[test]
fn bars_are_black_for_a_single_size_from_the_start() {
    if !has_gpu() {
        return;
    }
    for size in [(1920, 800), (400, 300)] {
        let (_, lit) = lit_bars(&[size], 1, 0);
        assert!(lit.is_empty(), "{size:?}: {lit:?}");
    }
}
