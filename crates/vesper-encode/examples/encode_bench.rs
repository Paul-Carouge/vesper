//! Headless encode benchmark: renders a moving test pattern into a small pool
//! of VA surfaces, exports them as DMA-BUFs, and encodes them with
//! `vesper_encode::Encoder` as if they came from screen capture.
//!
//! `cargo run --release -p vesper-encode --example encode_bench [frames]`
//!
//! Writes `vesper-bench.ivf` (AV1) and `vesper-bench.hevc` (Annex B) to the
//! system temp dir. The CPU upload of the pattern is outside the timed region.

#[path = "../tests/common/mod.rs"]
mod common;

use std::collections::HashMap;
use std::fs;
use std::time::{Duration, Instant};

use common::{POOL, Producer};
use ffmpeg_next::ffi as ff;
use vesper_core::{Codec, EncodedPacket};
use vesper_encode::{Encoder, RateControl};

const W: u32 = 2560;
const H: u32 = 1440;
const FPS: u32 = 60;

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
    let producer = unsafe { Producer::new(W, H) };
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
