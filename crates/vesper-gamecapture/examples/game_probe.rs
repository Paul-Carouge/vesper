//! Waits for a game to present through the Vesper layer, then prints frame
//! properties and fps every second. With `--encode PATH`, also encodes the
//! frames to HEVC (Annex B) at PATH. Non-Steam games need `--allow EXE`
//! (their process name); until then they are listed as pending consent.
//!
//! Usage: `cargo run -p vesper-gamecapture --example game_probe -- [SECONDS] [--encode PATH] [--allow EXE]...`
//! (default 5 s of frames). Set `RUST_LOG=debug` for more logs.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use vesper_core::Codec;
use vesper_encode::{Encoder, RateControl};
use vesper_gamecapture::GameCapture;

fn main() -> vesper_gamecapture::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let mut args = std::env::args().skip(1);
    let mut seconds = 5;
    let mut out = None;
    let mut allow = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--encode" => out = Some(args.next().ok_or("--encode needs a path")?),
            "--allow" => allow.push(args.next().ok_or("--allow needs a process name")?),
            s => seconds = s.parse()?,
        }
    }

    let (tx, rx) = mpsc::sync_channel(4);
    let capture = GameCapture::start(move |event| match event {
        // Full channel: drop the frame, which releases it to the game.
        Ok(frame) => drop(tx.try_send(Ok(frame))),
        Err(e) => drop(tx.send(Err(e))),
    })?;
    for exe in &allow {
        capture.allow(exe);
    }
    // Paced far above any game's rate: every frame is copied.
    capture.set_active(Some(1000));
    println!("waiting for a game (VESPER_CAPTURE_DISABLE unset, layer installed)");
    let mut listed = Vec::new();

    let mut end = None;
    let mut window = (Instant::now(), 0u32);
    let mut total = 0u32;
    let mut encoder: Option<Encoder> = None;
    let mut stream = Vec::new();
    loop {
        if end.is_some_and(|e| Instant::now() >= e) {
            break;
        }
        let frame = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(frame)) => frame,
            Ok(Err(e)) => {
                println!("capture ended: {e}");
                break;
            }
            Err(_) => {
                let games = capture.games();
                if games != listed {
                    println!("games: {games:?}");
                    listed = games;
                }
                continue;
            }
        };
        if end.is_none() {
            println!("captured: {:?}", capture.captured());
            end = Some(Instant::now() + Duration::from_secs(seconds));
            window = (Instant::now(), 0);
        }
        total += 1;
        window.1 += 1;
        let elapsed = window.0.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let fourcc = String::from_utf8_lossy(&frame.fourcc.to_le_bytes()).into_owned();
            println!(
                "{}x{} {fourcc} modifier={:#018x} planes={} buffer={} gen={} pts={:?} fps={:.1}",
                frame.width,
                frame.height,
                frame.modifier,
                frame.planes.len(),
                frame.buffer_id,
                frame.pool_generation,
                frame.pts,
                window.1 as f64 / elapsed.as_secs_f64(),
            );
            window = (Instant::now(), 0);
        }
        if out.is_some() {
            if encoder.is_none() {
                encoder = Some(Encoder::new(
                    Codec::Hevc,
                    frame.width,
                    frame.height,
                    60,
                    RateControl::ConstantQp(24),
                )?);
            }
            let enc = encoder.as_mut().expect("encoder created above");
            for p in enc.encode(frame)? {
                stream.extend_from_slice(&p.data);
            }
        }
    }
    println!("{total} frames in total");
    if let (Some(path), Some(mut enc)) = (out, encoder) {
        for p in enc.flush()? {
            stream.extend_from_slice(&p.data);
        }
        std::fs::write(&path, &stream)?;
        println!("wrote {path}");
    }
    capture.stop();
    Ok(())
}
