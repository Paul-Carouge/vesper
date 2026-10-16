//! Compositor GPU cost, measured with timestamp queries, at 1440p and 4K:
//! the main capture alone (1:1 copy) and with a typical overlay set
//! (webcam corner, logo with alpha, half-transparent text banner), and with
//! the copy of the main frame kept for redraws without it.
//!
//! `cargo run --release -p vesper-compose --example compose_bench [frames]`
//! (default 120 per case). Short on purpose: the GPU may be busy with a game.

#[allow(dead_code)]
#[path = "../../vesper-encode/tests/common/mod.rs"]
mod common;

use std::time::{Duration, Instant};

use common::{POOL, Producer};
use vesper_compose::{Compositor, Draw, Fit, Source};

fn draw(source: Source, rect: [f32; 4], fit: Fit, opacity: f32) -> Draw {
    Draw {
        source,
        rect,
        crop: [0.0; 4],
        fit,
        opacity,
    }
}

/// Premultiplied RGBA test texture: a gradient with the given alpha.
fn texture(w: u32, h: u32, alpha: u8) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let a = u32::from(alpha);
            let c = |n: u32| (n % 256 * a / 255) as u8;
            v.extend_from_slice(&[c(x), c(y), c(x + y), alpha]);
        }
    }
    v
}

fn stats(mut v: Vec<Duration>) -> String {
    v.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let p = |q: f64| ms(v[((v.len() - 1) as f64 * q).round() as usize]);
    format!(
        "median {:.3} ms, p95 {:.3} ms, max {:.3} ms",
        p(0.5),
        p(0.95),
        p(1.0)
    )
}

fn main() {
    tracing_subscriber::fmt().init();
    let frames: u32 = std::env::args()
        .nth(1)
        .map_or(120, |a| a.parse().expect("frame count"));
    for (w, h) in [(2560, 1440), (3840, 2160)] {
        let producer = unsafe { Producer::new(w, h) };
        let mut c = Compositor::new(w, h).unwrap();
        c.set_texture(1, 1280, 720, &texture(1280, 720, 255))
            .unwrap();
        c.set_texture(2, 512, 256, &texture(512, 256, 200)).unwrap();
        c.set_texture(3, 1600, 160, &texture(1600, 160, 255))
            .unwrap();
        let main = draw(Source::Main, [0.0, 0.0, 1.0, 1.0], Fit::Contain, 1.0);
        let overlays = [
            main,
            draw(
                Source::Texture(1),
                [0.74, 0.70, 0.24, 0.27],
                Fit::Cover,
                1.0,
            ),
            draw(
                Source::Texture(2),
                [0.02, 0.02, 0.15, 0.08],
                Fit::Contain,
                1.0,
            ),
            draw(
                Source::Texture(3),
                [0.02, 0.88, 0.6, 0.06],
                Fit::Contain,
                0.8,
            ),
        ];
        for (name, draws, keep_base) in [
            ("main only", &overlays[..1], false),
            ("main + 3 overlays", &overlays[..], false),
            ("main + 3 overlays + kept copy", &overlays[..], true),
        ] {
            let (mut gpu, mut wall) = (Vec::new(), Vec::new());
            for t in 0..frames {
                let f = unsafe { producer.frame(t as usize % POOL, t, 0, Duration::ZERO) };
                let start = Instant::now();
                c.submit(Some(f), draws, keep_base).unwrap();
                let out = c.poll(Duration::from_secs(2)).unwrap().expect("done");
                wall.push(start.elapsed());
                gpu.push(c.gpu_time().expect("timestamps"));
                drop(out);
            }
            println!("{w}x{h} {name}: GPU {}", stats(gpu));
            println!("{w}x{h} {name}: compose call {}", stats(wall));
        }
    }
}
