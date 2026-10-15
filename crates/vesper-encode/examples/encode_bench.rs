//! Headless encode benchmark: renders a moving test pattern into a small pool
//! of VA surfaces, exports them as DMA-BUFs, and encodes them with
//! `vesper_encode::Encoder` as if they came from screen capture.
//!
//! `cargo run --release -p vesper-encode --example encode_bench [frames]`
//!
//! Writes `vesper-bench.ivf` (AV1) and `vesper-bench.hevc` (Annex B) to the
//! system temp dir. The CPU upload of the pattern is outside the timed region.

use std::collections::HashMap;
use std::ffi::c_int;
use std::fs;
use std::os::fd::BorrowedFd;
use std::ptr;
use std::time::{Duration, Instant};

use ffmpeg_next::ffi as ff;
use vesper_core::{Codec, DmaBufPlane, EncodedPacket, ReleaseGuard, VideoFrame};
use vesper_encode::{Encoder, RENDER_NODE, RateControl};

const W: u32 = 2560;
const H: u32 = 1440;
const FPS: u32 = 60;
const POOL: usize = 4;

fn ok(ret: c_int, what: &str) {
    assert!(ret >= 0, "{what}: {}", ffmpeg_next::Error::from(ret));
}

/// A pool of VA surfaces (the "producer"), each exported once as a DMA-BUF.
struct Producer {
    surfaces: Vec<*mut ff::AVFrame>,
    exports: Vec<*mut ff::AVFrame>,
    upload: *mut ff::AVFrame,
}

impl Producer {
    unsafe fn new() -> Self {
        unsafe {
            let mut dev = ptr::null_mut();
            ok(
                ff::av_hwdevice_ctx_create(
                    &mut dev,
                    ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    RENDER_NODE.as_ptr(),
                    ptr::null_mut(),
                    0,
                ),
                "producer device",
            );
            let mut frames = ff::av_hwframe_ctx_alloc(dev);
            let fc = (*frames).data as *mut ff::AVHWFramesContext;
            (*fc).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*fc).sw_format = ff::AVPixelFormat::AV_PIX_FMT_BGR0;
            (*fc).width = W as c_int;
            (*fc).height = H as c_int;
            (*fc).initial_pool_size = POOL as c_int;
            ok(ff::av_hwframe_ctx_init(frames), "producer frames");

            let mut surfaces = Vec::new();
            let mut exports = Vec::new();
            for _ in 0..POOL {
                let s = ff::av_frame_alloc();
                ok(ff::av_hwframe_get_buffer(frames, s, 0), "get surface");
                let e = ff::av_frame_alloc();
                (*e).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
                ok(
                    ff::av_hwframe_map(e, s, ff::AV_HWFRAME_MAP_READ as c_int),
                    "export DMA-BUF",
                );
                surfaces.push(s);
                exports.push(e);
            }
            ff::av_buffer_unref(&mut frames);
            ff::av_buffer_unref(&mut dev);

            let upload = ff::av_frame_alloc();
            (*upload).format = ff::AVPixelFormat::AV_PIX_FMT_BGR0 as c_int;
            (*upload).width = W as c_int;
            (*upload).height = H as c_int;
            ok(ff::av_frame_get_buffer(upload, 0), "upload frame");
            Self {
                surfaces,
                exports,
                upload,
            }
        }
    }

    /// Draws frame `t` into surface `id` and wraps it as a `VideoFrame`.
    unsafe fn frame(&self, id: usize, t: u32, generation: u32, pts: Duration) -> VideoFrame {
        unsafe {
            let up = &*self.upload;
            let stride = up.linesize[0] as usize;
            let pixels = std::slice::from_raw_parts_mut(up.data[0], stride * H as usize);
            let (bx, by) = ((t * 9) % (W - 256), (t * 5) % (H - 256));
            for (y, row) in pixels.chunks_exact_mut(stride).enumerate() {
                let y = y as u32;
                for (x, px) in row[..W as usize * 4]
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .enumerate()
                {
                    let x = x as u32;
                    let in_box = (bx..bx + 256).contains(&x) && (by..by + 256).contains(&y);
                    let v = if in_box {
                        [255, 255, 255, 0]
                    } else {
                        [
                            (x + t * 4) as u8,
                            (y + t * 2) as u8,
                            ((x ^ y) >> 3) as u8,
                            0,
                        ]
                    };
                    *px = v;
                }
            }
            ok(
                ff::av_hwframe_transfer_data(self.surfaces[id], self.upload, 0),
                "upload",
            );

            let desc = &*((*self.exports[id]).data[0] as *const ff::AVDRMFrameDescriptor);
            let layer = &desc.layers[0];
            let planes = layer.planes[..layer.nb_planes as usize]
                .iter()
                .map(|p| DmaBufPlane {
                    fd: BorrowedFd::borrow_raw(desc.objects[p.object_index as usize].fd)
                        .try_clone_to_owned()
                        .unwrap(),
                    offset: p.offset as u32,
                    stride: p.pitch as u32,
                })
                .collect();
            VideoFrame {
                width: W,
                height: H,
                fourcc: layer.format,
                modifier: desc.objects[0].format_modifier,
                planes,
                buffer_id: id as u32,
                pool_generation: generation,
                pts,
                release: ReleaseGuard::new(|| {}),
            }
        }
    }
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn bench(producer: &Producer, codec: Codec, frames: u32) -> Vec<EncodedPacket> {
    let mut enc = Encoder::new(codec, W, H, FPS, RateControl::Bitrate(20_000_000)).unwrap();
    // CLOCK_MONOTONIC, like PipeWire timestamps.
    let base = Duration::from_micros(unsafe { ff::av_gettime_relative() } as u64);
    let mut packets = Vec::new();
    let mut started: HashMap<Duration, Instant> = HashMap::new();
    let mut call_times = Vec::new();
    let mut latencies = Vec::new();
    let mut encode_total = Duration::ZERO;

    let mut collect = |out: Vec<EncodedPacket>,
                       packets: &mut Vec<EncodedPacket>,
                       started: &mut HashMap<Duration, Instant>| {
        let now = Instant::now();
        for p in out {
            latencies.push(now - started.remove(&p.pts).expect("packet pts matches a frame"));
            packets.push(p);
        }
    };

    for t in 0..frames {
        let pts = base + Duration::from_nanos(t as u64 * 1_000_000_000 / FPS as u64);
        // Renegotiate the pool halfway to exercise import-cache invalidation.
        let generation = (t >= frames / 2) as u32;
        let frame = unsafe { producer.frame(t as usize % POOL, t, generation, pts) };

        let t0 = Instant::now();
        started.insert(pts, t0);
        let out = enc.encode(frame).unwrap();
        let dt = t0.elapsed();
        call_times.push(dt);
        encode_total += dt;
        collect(out, &mut packets, &mut started);
    }
    let t0 = Instant::now();
    let out = enc.flush().unwrap();
    encode_total += t0.elapsed();
    collect(out, &mut packets, &mut started);

    assert_eq!(packets.len(), frames as usize, "one packet per frame");
    assert!(
        packets.windows(2).all(|w| w[0].pts < w[1].pts),
        "packets in pts order (no B-frames)"
    );
    assert!(packets[0].keyframe);

    call_times.sort();
    latencies.sort();
    let bytes: usize = packets.iter().map(|p| p.data.len()).sum();
    println!(
        "{codec:?} {W}x{H}: {frames} frames, {:.0} fps encode throughput, {:.1} Mbit/s at {FPS} fps",
        frames as f64 / encode_total.as_secs_f64(),
        bytes as f64 * 8.0 * FPS as f64 / frames as f64 / 1e6,
    );
    println!(
        "  encode() call: avg {:.2} ms, p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms",
        ms(encode_total / frames),
        ms(percentile(&call_times, 0.5)),
        ms(percentile(&call_times, 0.99)),
        ms(*call_times.last().unwrap()),
    );
    println!(
        "  frame in -> packet out: p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms",
        ms(percentile(&latencies, 0.5)),
        ms(percentile(&latencies, 0.99)),
        ms(*latencies.last().unwrap()),
    );
    packets
}

fn write_ivf(path: &std::path::Path, packets: &[EncodedPacket]) {
    let mut out = Vec::new();
    out.extend_from_slice(b"DKIF");
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(b"AV01");
    out.extend_from_slice(&(W as u16).to_le_bytes());
    out.extend_from_slice(&(H as u16).to_le_bytes());
    out.extend_from_slice(&FPS.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(packets.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for (i, p) in packets.iter().enumerate() {
        out.extend_from_slice(&(p.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(i as u64).to_le_bytes());
        out.extend_from_slice(&p.data);
    }
    fs::write(path, out).unwrap();
}

fn main() {
    let frames: u32 = std::env::args()
        .nth(1)
        .map(|a| a.parse().expect("frame count"))
        .unwrap_or(600);
    let producer = unsafe { Producer::new() };
    let dir = std::env::temp_dir();

    let av1 = bench(&producer, Codec::Av1, frames);
    let path = dir.join("vesper-bench.ivf");
    write_ivf(&path, &av1);
    println!("  wrote {}", path.display());

    let hevc = bench(&producer, Codec::Hevc, frames);
    let path = dir.join("vesper-bench.hevc");
    fs::write(
        &path,
        hevc.iter()
            .flat_map(|p| p.data.iter().copied())
            .collect::<Vec<u8>>(),
    )
    .unwrap();
    println!("  wrote {}", path.display());
}
