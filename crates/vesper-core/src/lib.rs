//! Types shared between the capture, encode and app crates.
//!
//! Timestamps are `CLOCK_MONOTONIC` instants (the clock of PipeWire's
//! `spa_meta_header.pts`), so video and audio streams share one time base.
//! The muxer rebases them to the start of the recording.

use std::fmt;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// One plane of a DMA-BUF. Several planes may share the same buffer object
/// (e.g. AMD DCC modifiers), each with its own fd duplicate.
#[derive(Debug)]
pub struct DmaBufPlane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

/// Runs a callback once when dropped. The producer uses it to learn when a
/// consumer is done with a buffer.
pub struct ReleaseGuard(Option<Box<dyn FnOnce() + Send>>);

impl ReleaseGuard {
    pub fn new(release: impl FnOnce() + Send + 'static) -> Self {
        Self(Some(Box::new(release)))
    }
}

impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        if let Some(release) = self.0.take() {
            release();
        }
    }
}

impl fmt::Debug for ReleaseGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReleaseGuard")
    }
}

/// A captured frame that lives in GPU memory.
///
/// The producer does not reuse the underlying buffer until the frame is
/// dropped (`release` fires). Consumers should copy it on the GPU (e.g.
/// VA-VPP to NV12) and drop it quickly: the producer's pool is small.
#[derive(Debug)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    /// DRM fourcc, e.g. `XR24` (`DRM_FORMAT_XRGB8888`).
    pub fourcc: u32,
    /// DRM format modifier.
    pub modifier: u64,
    pub planes: Vec<DmaBufPlane>,
    /// Index of the buffer in the producer's pool. Stable for the lifetime of
    /// a pool, so consumers can cache imports (one VA surface per buffer).
    pub buffer_id: u32,
    /// Identifies the pool: a new one whenever the pool is renegotiated
    /// (size, format, modifier) or another producer takes over. Taken from
    /// [`next_pool_generation`], so it is unique across all producers in the
    /// process: one consumer can be fed by several (portal window, game).
    /// Consumers must drop cached imports when it changes.
    pub pool_generation: u32,
    /// `CLOCK_MONOTONIC` presentation time.
    pub pts: Duration,
    pub release: ReleaseGuard,
}

/// A pool generation no producer in this process used yet. Never 0.
pub fn next_pool_generation() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    loop {
        let g = NEXT.fetch_add(1, Ordering::Relaxed);
        if g != 0 {
            return g;
        }
    }
}

/// Whether `VESPER_REQUIRE_GPU=1` is set: GPU tests must then fail, not skip,
/// when the GPU is missing (the dev machine runs the suite this way).
pub fn gpu_required() -> bool {
    std::env::var_os("VESPER_REQUIRE_GPU").is_some_and(|v| v == "1")
}

/// For GPU tests: whether `/dev/dri/renderD128` exists. Without it the test
/// should return early, unless [`gpu_required`], which panics instead so a
/// skipped GPU test can't pass for a run one.
pub fn test_gpu() -> bool {
    let ok = std::path::Path::new("/dev/dri/renderD128").exists();
    if !ok {
        assert!(
            !gpu_required(),
            "VESPER_REQUIRE_GPU=1 but /dev/dri/renderD128 is missing"
        );
        eprintln!("no GPU, skipping");
    }
    ok
}

/// Video codecs supported by the hardware encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

/// One compressed access unit produced by an encoder.
///
/// Encoders emit no B-frames, so decode order equals presentation order and
/// no separate dts is needed.
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    /// Shared so the replay buffer and the muxer can hold it without copying.
    pub data: Arc<[u8]>,
    /// `CLOCK_MONOTONIC` presentation time, copied from the source frame.
    pub pts: Duration,
    pub keyframe: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_generations_are_unique_across_threads_and_never_0() {
        let threads: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    (0..1000)
                        .map(|_| next_pool_generation())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut all: Vec<u32> = threads
            .into_iter()
            .flat_map(|t| t.join().unwrap())
            .collect();
        assert!(!all.contains(&0));
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 4000);
    }

    #[test]
    fn release_guard_fires_once_on_drop() {
        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        let guard = ReleaseGuard::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(guard);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
