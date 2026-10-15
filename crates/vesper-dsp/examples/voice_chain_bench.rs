//! CPU cost of the voice chain per 20 ms stereo block.
//!
//! `cargo run --release -p vesper-dsp --example voice_chain_bench`

use std::time::Instant;

use vesper_dsp::{Chain, FilterSpec, SAMPLE_RATE};

/// 20 ms at 48 kHz.
const BLOCK: usize = 960;

fn main() {
    let (mut chain, _) = Chain::new(&FilterSpec::voice_chain(), 2);
    let mut seed = 1u32;
    let input: Vec<f32> = (0..BLOCK * 2 * 50)
        .map(|i| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 8) as f32 / (1 << 24) as f32 - 0.5;
            0.3 * (i as f32 * 0.02).sin() + 0.05 * noise
        })
        .collect();
    let mut buf = vec![0.0; BLOCK * 2];
    let blocks = 50 * 60; // one minute of audio
    let run = |chain: &mut Chain, buf: &mut [f32], n: usize| {
        for i in 0..n {
            let at = (i % 50) * BLOCK * 2;
            buf.copy_from_slice(&input[at..at + BLOCK * 2]);
            chain.process(buf);
        }
    };
    run(&mut chain, &mut buf, 100);
    let start = Instant::now();
    run(&mut chain, &mut buf, blocks);
    let per_block = start.elapsed() / blocks as u32;
    let block_time = BLOCK as f64 / f64::from(SAMPLE_RATE);
    println!(
        "voice chain, stereo, {BLOCK}-frame blocks: {:.1} us per block, {:.3}% of one core",
        per_block.as_secs_f64() * 1e6,
        per_block.as_secs_f64() / block_time * 100.0
    );
    for spec in FilterSpec::voice_chain() {
        let (mut one, _) = Chain::new(&[spec], 2);
        run(&mut one, &mut buf, 100);
        let start = Instant::now();
        run(&mut one, &mut buf, blocks);
        let per_block = start.elapsed() / blocks as u32;
        println!(
            "  {:<18} {:6.1} us",
            spec.name(),
            per_block.as_secs_f64() * 1e6
        );
    }
}
