//! Starts a capture, prints frame properties and fps for ~5 s, then exits.
//!
//! Usage: `cargo run -p vesper-capture --example capture_probe [RESTORE_TOKEN]`
//! (or set `VESPER_RESTORE_TOKEN`). Set `RUST_LOG=debug` for more logs.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use vesper_capture::Capture;

fn main() -> vesper_capture::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let token = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("VESPER_RESTORE_TOKEN").ok());

    let (tx, rx) = mpsc::sync_channel(4);
    let capture = Capture::start(token.as_deref(), move |event| match event {
        // Full channel: drop the frame, which returns its buffer to PipeWire.
        Ok(frame) => drop(tx.try_send(Ok(frame))),
        Err(e) => drop(tx.send(Err(e))),
    })?;
    println!("capture started");

    let end = Instant::now() + Duration::from_secs(5);
    let mut window = (Instant::now(), 0u32);
    let mut total = 0u32;
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        let frame = match rx.recv_timeout(left) {
            Ok(Ok(frame)) => frame,
            Ok(Err(e)) => {
                println!("capture ended: {e}");
                break;
            }
            Err(_) => continue,
        };
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
    }
    println!("{total} frames in total");
    match capture.restore_token() {
        Some(t) => println!("restore token: {t}"),
        None => println!("no restore token"),
    }
    capture.stop();
    Ok(())
}
