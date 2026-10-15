//! Encodes a 1440p60 test pattern with HEVC and AV1 and muxes it, with a
//! stand-in Opus track, to MKV files in the system temp dir.
//!
//! `cargo run --release -p vesper-mux --example mux_bench [frames]`
//! writes `vesper-mux-hevc.mkv` and `vesper-mux-av1.mkv` (600 frames = 10 s).
//!
//! `cargo run --release -p vesper-mux --example mux_bench abort [frames]`
//! writes `vesper-mux-killed.mkv` with HEVC and calls `std::process::abort`
//! after `frames` frames (default 300), without finishing the muxer.

#[path = "../tests/common/mod.rs"]
mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::producer::{POOL, Producer};
use common::{OPUS_FRAME, opus_packet, opus_track};
use ffmpeg_next::ffi as ff;
use vesper_core::Codec;
use vesper_encode::{Encoder, RateControl};
use vesper_mux::{Muxer, VideoTrack};

const W: u32 = 2560;
const H: u32 = 1440;
const FPS: u32 = 60;

fn run(producer: &Producer, codec: Codec, frames: u32, path: PathBuf, abort: bool) {
    let mut enc = Encoder::new(codec, W, H, FPS, RateControl::Bitrate(20_000_000)).unwrap();
    // CLOCK_MONOTONIC, like PipeWire timestamps.
    let base = Duration::from_micros(unsafe { ff::av_gettime_relative() } as u64);
    let video = VideoTrack {
        codec,
        width: W,
        height: H,
        fps: FPS,
        extradata: enc.extradata().to_vec(),
    };
    println!("{codec:?}: {} bytes of extradata", video.extradata.len());
    let mut mux = Muxer::create(&path, base, video, &[opus_track()]).unwrap();
    let mut audio_pts = base;
    let mut writes = Vec::new();

    for t in 0..frames {
        let pts = base + Duration::from_nanos(t as u64 * 1_000_000_000 / FPS as u64);
        let frame = unsafe { producer.frame(t as usize % POOL, t, 0, pts) };
        for p in enc.encode(frame).unwrap() {
            while audio_pts <= p.pts {
                mux.write_audio(0, &opus_packet(audio_pts)).unwrap();
                audio_pts += OPUS_FRAME;
            }
            let t0 = Instant::now();
            mux.write_video(&p).unwrap();
            writes.push(t0.elapsed());
        }
        if abort && t + 1 == frames {
            println!("aborting after {frames} frames: {}", path.display());
            std::process::abort();
        }
    }
    for p in enc.flush().unwrap() {
        mux.write_video(&p).unwrap();
    }
    let t0 = Instant::now();
    mux.finish().unwrap();
    let finish = t0.elapsed();

    writes.sort();
    println!(
        "  write_video(): p50 {:?}, p99 {:?}, max {:?}; finish() {finish:?}",
        writes[writes.len() / 2],
        writes[writes.len() * 99 / 100],
        writes.last().unwrap(),
    );
    println!("  wrote {}", path.display());
}

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    let abort = args.next_if(|a| a == "abort").is_some();
    let frames: u32 = args
        .next()
        .map(|a| a.parse().expect("frame count"))
        .unwrap_or(if abort { 300 } else { 600 });
    let producer = unsafe { Producer::new(W, H) };
    let dir = std::env::temp_dir();

    if abort {
        run(
            &producer,
            Codec::Hevc,
            frames,
            dir.join("vesper-mux-killed.mkv"),
            true,
        );
        return;
    }
    run(
        &producer,
        Codec::Hevc,
        frames,
        dir.join("vesper-mux-hevc.mkv"),
        false,
    );
    run(
        &producer,
        Codec::Av1,
        frames,
        dir.join("vesper-mux-av1.mkv"),
        false,
    );
}
