//! Dumps the compositor's downscale of a picture, to compare it with a
//! reference filter (ffmpeg `scale=flags=area`, SSIM, VMAF).
//!
//! `cargo run --release -p vesper-compose --example downscale_dump --
//! in.rgba 3840 2160 1920 1080 out.rgba`
//!
//! The picture goes through the main-capture path: drawn 1:1 into a
//! full-size canvas whose exported frame is the `Source::Main` of a
//! preview-size compositor, read back through the preview. Outputs larger
//! than the preview are assembled from preview-size tiles, each a crop of
//! the source (exact when the output is a whole number of tiles and the
//! source a whole number of pixels per tile).

use vesper_compose::{Compositor, Draw, Fit, PREVIEW_HEIGHT, PREVIEW_WIDTH, Source};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    assert_eq!(a.len(), 7, "usage: in.rgba w h ow oh out.rgba");
    let n = |i: usize| a[i].parse::<u32>().expect("a number");
    let (w, h, ow, oh) = (n(2), n(3), n(4), n(5));
    let rgba = std::fs::read(&a[1]).expect("input picture");
    let (tx, ty) = (ow / PREVIEW_WIDTH, oh / PREVIEW_HEIGHT);
    assert_eq!(
        (tx * PREVIEW_WIDTH, ty * PREVIEW_HEIGHT),
        (ow, oh),
        "whole tiles"
    );

    let mut big = Compositor::new(w, h).unwrap();
    big.set_texture(1, w, h, &rgba).unwrap();
    let whole = Draw {
        source: Source::Texture(1),
        rect: [0.0, 0.0, 1.0, 1.0],
        crop: [0.0; 4],
        fit: Fit::Stretch,
        opacity: 1.0,
    };
    let mut small = Compositor::new(PREVIEW_WIDTH, PREVIEW_HEIGHT).unwrap();
    let mut out = vec![0u8; (ow * oh * 4) as usize];
    for j in 0..ty {
        for i in 0..tx {
            let main = big.compose(None, &[whole]).unwrap();
            let (fx, fy) = (tx as f32, ty as f32);
            let tile = Draw {
                source: Source::Main,
                crop: [
                    i as f32 / fx,
                    j as f32 / fy,
                    1.0 - (i + 1) as f32 / fx,
                    1.0 - (j + 1) as f32 / fy,
                ],
                ..whole
            };
            let _ = small.preview();
            drop(small.compose(Some(main), &[tile]).unwrap());
            let px = small.preview().expect("preview");
            let row = (PREVIEW_WIDTH * 4) as usize;
            for y in 0..PREVIEW_HEIGHT as usize {
                let dst = ((j * PREVIEW_HEIGHT) as usize + y) * (ow * 4) as usize
                    + (i * PREVIEW_WIDTH * 4) as usize;
                out[dst..dst + row].copy_from_slice(&px[y * row..(y + 1) * row]);
            }
        }
    }
    std::fs::write(&a[6], out).expect("output picture");
}
