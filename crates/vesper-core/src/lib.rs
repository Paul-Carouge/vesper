//! Types shared between the capture, encode and app crates.
//!
//! Timestamps are `CLOCK_MONOTONIC` instants (the clock of PipeWire's
//! `spa_meta_header.pts`), so video and audio streams share one time base.
//! The muxer rebases them to the start of the recording.

use std::fmt;
use std::os::fd::OwnedFd;
use std::sync::Arc;
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
    /// Bumped whenever the pool is renegotiated (size, format, modifier).
    /// Consumers must drop cached imports when it changes.
    pub pool_generation: u32,
    /// `CLOCK_MONOTONIC` presentation time.
    pub pts: Duration,
    pub release: ReleaseGuard,
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
    use std::sync::atomic::{AtomicU32, Ordering};

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
