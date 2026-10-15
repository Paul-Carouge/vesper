//! Types shared between the capture, encode and app crates.

use std::os::fd::OwnedFd;
use std::time::Duration;

/// One plane of a DMA-BUF.
#[derive(Debug)]
pub struct DmaBufPlane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

/// A captured frame that lives in GPU memory.
///
/// The fds are duplicates owned by the frame. The producer may reuse the
/// underlying buffer once the frame is dropped, so a consumer must copy it
/// on the GPU (e.g. VA-VPP to NV12) before dropping it.
#[derive(Debug)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    /// DRM fourcc, e.g. `XR24` (`DRM_FORMAT_XRGB8888`).
    pub fourcc: u32,
    /// DRM format modifier.
    pub modifier: u64,
    pub planes: Vec<DmaBufPlane>,
    /// Presentation time, relative to the start of the recording.
    pub pts: Duration,
}

/// Video codecs supported by the hardware encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

/// One compressed access unit produced by an encoder.
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub pts: Duration,
    pub keyframe: bool,
}
