//! Hardware video encoding through VAAPI (FFmpeg `*_vaapi` encoders).
//!
//! Pipeline, all on the GPU:
//! DMA-BUF (DRM PRIME) -> VA surface (imported once per producer buffer) ->
//! `scale_vaapi` (VA-VPP, RGB to NV12) -> `av1_vaapi` / `hevc_vaapi` / `h264_vaapi`.

use std::collections::HashMap;
use std::ffi::{CStr, c_int, c_void};
use std::fmt;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::ptr;
use std::sync::Arc;
use std::time::Duration;

use ffmpeg_next::ffi as ff;
use vesper_core::{Codec, EncodedPacket, VideoFrame};

/// The discrete GPU. `renderD129` is the iGPU on the target machine.
pub const RENDER_NODE: &CStr = c"/dev/dri/renderD128";

/// Timestamps go through FFmpeg in nanoseconds so `pts` comes back unchanged.
const TIME_BASE: ff::AVRational = ff::AVRational {
    num: 1,
    den: 1_000_000_000,
};

#[link(name = "va")]
unsafe extern "C" {
    fn vaSyncSurface(dpy: *mut c_void, surface: u32) -> c_int;
}

/// Rate control of the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateControl {
    /// Target bitrate in bits per second (VBR).
    Bitrate(u64),
    /// Constant QP. Range depends on the codec: 0-51 for H.264/HEVC, 0-255 for AV1.
    ConstantQp(u32),
}

#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn check(ret: c_int, what: &str) -> Result<c_int, Error> {
    if ret < 0 {
        Err(Error(format!("{what}: {}", ffmpeg_next::Error::from(ret))))
    } else {
        Ok(ret)
    }
}

fn not_null<T>(p: *mut T, what: &str) -> Result<*mut T, Error> {
    if p.is_null() {
        Err(Error(format!("{what}: allocation failed")))
    } else {
        Ok(p)
    }
}

/// Owned `AVFrame`.
struct Frame(*mut ff::AVFrame);

impl Frame {
    fn new() -> Result<Self, Error> {
        Ok(Self(not_null(
            unsafe { ff::av_frame_alloc() },
            "av_frame_alloc",
        )?))
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { ff::av_frame_free(&mut self.0) };
    }
}

/// Borrows a raw fd as a `File` without taking ownership.
fn borrow_file(fd: RawFd) -> ManuallyDrop<File> {
    ManuallyDrop::new(unsafe { File::from_raw_fd(fd) })
}

/// Hardware encoder for DMA-BUF frames.
///
/// Uses the GPU at [`RENDER_NODE`]. Frames must have the encoder's size.
pub struct Encoder {
    width: u32,
    height: u32,
    device: *mut ff::AVBufferRef,
    /// VAAPI display of `device`, for `vaSyncSurface`.
    display: *mut c_void,
    /// RGB VA frames context the DMA-BUFs are imported into.
    import_frames: *mut ff::AVBufferRef,
    graph: *mut ff::AVFilterGraph,
    src: *mut ff::AVFilterContext,
    sink: *mut ff::AVFilterContext,
    enc: *mut ff::AVCodecContext,
    filtered: Frame,
    packet: *mut ff::AVPacket,
    /// Imported VA surfaces by producer `buffer_id`, valid for `generation`.
    imports: HashMap<u32, Frame>,
    generation: Option<u32>,
    /// Whether a frame reached the encoder (FFmpeg 9 vaapi crashes when
    /// flushing an encoder that never got one).
    started: bool,
}

// SAFETY: the FFmpeg/VA objects are owned by this struct and only used through
// `&mut self`, so moving the encoder to another thread is fine.
unsafe impl Send for Encoder {}

impl Encoder {
    /// Opens the encoder. `fps` is the nominal frame rate used for rate
    /// control and the GOP length (one keyframe every 2 s).
    pub fn new(
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        rate: RateControl,
    ) -> Result<Self, Error> {
        if width == 0 || height == 0 || fps == 0 {
            return Err(Error("width, height and fps must be non-zero".into()));
        }
        let mut e = Self {
            width,
            height,
            device: ptr::null_mut(),
            display: ptr::null_mut(),
            import_frames: ptr::null_mut(),
            graph: ptr::null_mut(),
            src: ptr::null_mut(),
            sink: ptr::null_mut(),
            enc: ptr::null_mut(),
            filtered: Frame::new()?,
            packet: ptr::null_mut(),
            imports: HashMap::new(),
            generation: None,
            started: false,
        };
        // On error, `e` is dropped and frees whatever was created so far.
        unsafe {
            e.packet = not_null(ff::av_packet_alloc(), "av_packet_alloc")?;
            check(
                ff::av_hwdevice_ctx_create(
                    &mut e.device,
                    ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    RENDER_NODE.as_ptr(),
                    ptr::null_mut(),
                    0,
                ),
                "open VAAPI device",
            )?;
            let dev = (*e.device).data as *mut ff::AVHWDeviceContext;
            e.display = (*((*dev).hwctx as *mut ff::AVVAAPIDeviceContext)).display;

            e.import_frames = not_null(ff::av_hwframe_ctx_alloc(e.device), "av_hwframe_ctx_alloc")?;
            let fc = (*e.import_frames).data as *mut ff::AVHWFramesContext;
            (*fc).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
            // Only describes the frames for VPP; imports take their real
            // format (XRGB/ARGB/...) from the DRM fourcc.
            (*fc).sw_format = ff::AVPixelFormat::AV_PIX_FMT_BGR0;
            (*fc).width = width as c_int;
            (*fc).height = height as c_int;
            check(ff::av_hwframe_ctx_init(e.import_frames), "init VA frames")?;

            e.build_graph()?;
            e.open_encoder(codec, fps, rate)?;
        }
        Ok(e)
    }

    unsafe fn build_graph(&mut self) -> Result<(), Error> {
        unsafe {
            self.graph = not_null(ff::avfilter_graph_alloc(), "avfilter_graph_alloc")?;
            self.src = not_null(
                ff::avfilter_graph_alloc_filter(
                    self.graph,
                    ff::avfilter_get_by_name(c"buffer".as_ptr()),
                    c"in".as_ptr(),
                ),
                "alloc buffer filter",
            )?;
            let par = not_null(ff::av_buffersrc_parameters_alloc(), "buffersrc params")?;
            (*par).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI as c_int;
            (*par).time_base = TIME_BASE;
            (*par).width = self.width as c_int;
            (*par).height = self.height as c_int;
            (*par).hw_frames_ctx = self.import_frames;
            let ret = ff::av_buffersrc_parameters_set(self.src, par);
            ff::av_free(par.cast());
            check(ret, "buffersrc params")?;
            check(
                ff::avfilter_init_dict(self.src, ptr::null_mut()),
                "init buffersrc",
            )?;

            let mut vpp = ptr::null_mut();
            check(
                ff::avfilter_graph_create_filter(
                    &mut vpp,
                    ff::avfilter_get_by_name(c"scale_vaapi".as_ptr()),
                    c"vpp".as_ptr(),
                    c"format=nv12:out_color_matrix=bt709:out_range=tv".as_ptr(),
                    ptr::null_mut(),
                    self.graph,
                ),
                "create scale_vaapi",
            )?;
            check(
                ff::avfilter_graph_create_filter(
                    &mut self.sink,
                    ff::avfilter_get_by_name(c"buffersink".as_ptr()),
                    c"out".as_ptr(),
                    ptr::null(),
                    ptr::null_mut(),
                    self.graph,
                ),
                "create buffersink",
            )?;
            check(ff::avfilter_link(self.src, 0, vpp, 0), "link")?;
            check(ff::avfilter_link(vpp, 0, self.sink, 0), "link")?;
            check(
                ff::avfilter_graph_config(self.graph, ptr::null_mut()),
                "configure graph",
            )?;
        }
        Ok(())
    }

    unsafe fn open_encoder(
        &mut self,
        codec: Codec,
        fps: u32,
        rate: RateControl,
    ) -> Result<(), Error> {
        let name = match codec {
            Codec::H264 => c"h264_vaapi",
            Codec::Hevc => c"hevc_vaapi",
            Codec::Av1 => c"av1_vaapi",
        };
        unsafe {
            let c = ff::avcodec_find_encoder_by_name(name.as_ptr());
            if c.is_null() {
                return Err(Error(format!("FFmpeg has no {name:?} encoder")));
            }
            self.enc = not_null(ff::avcodec_alloc_context3(c), "avcodec_alloc_context3")?;
            let enc = &mut *self.enc;
            enc.width = self.width as c_int;
            enc.height = self.height as c_int;
            enc.time_base = TIME_BASE;
            enc.framerate = ff::AVRational {
                num: fps as c_int,
                den: 1,
            };
            enc.pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
            enc.gop_size = (fps * 2) as c_int;
            // No B-frames: decode order == presentation order (vesper-core contract).
            enc.max_b_frames = 0;
            enc.color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
            enc.colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
            enc.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
            enc.color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_IEC61966_2_1;
            enc.hw_frames_ctx = not_null(
                ff::av_buffer_ref(ff::av_buffersink_get_hw_frames_ctx(self.sink)),
                "sink frames",
            )?;
            match rate {
                RateControl::Bitrate(bps) => {
                    enc.bit_rate = bps as i64;
                    check(
                        ff::av_opt_set(
                            self.enc.cast(),
                            c"rc_mode".as_ptr(),
                            c"VBR".as_ptr(),
                            ff::AV_OPT_SEARCH_CHILDREN,
                        ),
                        "rc_mode",
                    )?;
                }
                RateControl::ConstantQp(qp) => {
                    enc.global_quality = qp as c_int;
                    check(
                        ff::av_opt_set(
                            self.enc.cast(),
                            c"rc_mode".as_ptr(),
                            c"CQP".as_ptr(),
                            ff::AV_OPT_SEARCH_CHILDREN,
                        ),
                        "rc_mode",
                    )?;
                }
            }
            check(
                ff::avcodec_open2(self.enc, c, ptr::null_mut()),
                "open encoder",
            )?;
        }
        Ok(())
    }

    /// Imports `frame`'s DMA-BUF as a VA surface (no copy).
    unsafe fn import(&self, frame: &VideoFrame) -> Result<Frame, Error> {
        let n = frame.planes.len();
        if n == 0 || n > 4 {
            return Err(Error(format!("unsupported plane count {n}")));
        }
        // FFmpeg's VAAPI import takes exactly one DRM object. Planes of RGB
        // frames (incl. AMD DCC metadata planes) live in the same BO.
        let fd0 = frame.planes[0].fd.as_raw_fd();
        let ino = |fd| borrow_file(fd).metadata().map(|m| m.ino());
        let ino0 = ino(fd0).map_err(|e| Error(format!("fstat dmabuf: {e}")))?;
        for p in &frame.planes[1..] {
            if ino(p.fd.as_raw_fd()).ok() != Some(ino0) {
                return Err(Error(
                    "planes in separate buffer objects are not supported".into(),
                ));
            }
        }
        let size = borrow_file(fd0)
            .seek(SeekFrom::End(0))
            .map_err(|e| Error(format!("seek dmabuf: {e}")))? as usize;

        unsafe {
            let src = Frame::new()?;
            let desc_buf = not_null(
                ff::av_buffer_allocz(size_of::<ff::AVDRMFrameDescriptor>()),
                "av_buffer_allocz",
            )?;
            (*src.0).buf[0] = desc_buf;
            (*src.0).data[0] = (*desc_buf).data;
            let desc = &mut *((*desc_buf).data as *mut ff::AVDRMFrameDescriptor);
            desc.nb_objects = 1;
            desc.objects[0].fd = fd0;
            desc.objects[0].size = size;
            desc.objects[0].format_modifier = frame.modifier;
            desc.nb_layers = 1;
            desc.layers[0].format = frame.fourcc;
            desc.layers[0].nb_planes = n as c_int;
            for (dst, p) in desc.layers[0].planes.iter_mut().zip(&frame.planes) {
                dst.object_index = 0;
                dst.offset = p.offset as isize;
                dst.pitch = p.stride as isize;
            }
            (*src.0).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
            (*src.0).width = frame.width as c_int;
            (*src.0).height = frame.height as c_int;

            let dst = Frame::new()?;
            (*dst.0).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI as c_int;
            (*dst.0).hw_frames_ctx = not_null(ff::av_buffer_ref(self.import_frames), "frames ref")?;
            check(
                ff::av_hwframe_map(dst.0, src.0, ff::AV_HWFRAME_MAP_READ as c_int),
                "import DMA-BUF into VA surface",
            )?;
            // The VA surface now holds its own reference to the BO. The mapping
            // keeps `src` alive, so clear the fd: the frame owning it is released.
            desc.objects[0].fd = -1;
            Ok(dst)
        }
    }

    /// Encodes one frame. The RGB->NV12 GPU copy is complete when this
    /// returns, and `frame` is dropped (released to the producer) right after it.
    pub fn encode(&mut self, frame: VideoFrame) -> Result<Vec<EncodedPacket>, Error> {
        if frame.width != self.width || frame.height != self.height {
            // shortcut: no scaling of mismatched sizes; rebuild the encoder or add scale_vaapi w/h if capture resizes.
            return Err(Error(format!(
                "frame is {}x{}, encoder is {}x{}",
                frame.width, frame.height, self.width, self.height
            )));
        }
        if self.generation != Some(frame.pool_generation) {
            self.imports.clear();
            self.generation = Some(frame.pool_generation);
        }
        let pts = i64::try_from(frame.pts.as_nanos()).map_err(|_| Error("pts overflow".into()))?;

        unsafe {
            let surface = match self.imports.get(&frame.buffer_id) {
                Some(s) => s.0,
                None => {
                    let s = self.import(&frame)?;
                    self.imports.entry(frame.buffer_id).or_insert(s).0
                }
            };
            (*surface).pts = pts;
            check(
                ff::av_buffersrc_add_frame_flags(
                    self.src,
                    surface,
                    ff::AV_BUFFERSRC_FLAG_KEEP_REF as c_int,
                ),
                "feed VPP",
            )?;
            // On error, wait for any VPP job still reading the producer's
            // buffer before `frame` drops and releases it.
            let sync_input = || vaSyncSurface(self.display, (*surface).data[3] as usize as u32);
            let ret = ff::av_buffersink_get_frame(self.sink, self.filtered.0);
            if ret < 0 {
                sync_input();
                check(ret, "run VPP")?;
            }
            // Wait for VPP so the producer's buffer is no longer read, then release it.
            let nv12 = (*self.filtered.0).data[3] as usize as u32;
            let status = vaSyncSurface(self.display, nv12);
            if status != 0 {
                sync_input();
                ff::av_frame_unref(self.filtered.0);
                return Err(Error(format!("vaSyncSurface failed: VA status {status}")));
            }
            drop(frame);
            let ret = ff::avcodec_send_frame(self.enc, self.filtered.0);
            ff::av_frame_unref(self.filtered.0);
            check(ret, "send frame to encoder")?;
            self.started = true;
        }
        self.drain()
    }

    /// Flushes the encoder and returns the remaining packets. The encoder
    /// cannot take frames afterwards.
    pub fn flush(&mut self) -> Result<Vec<EncodedPacket>, Error> {
        if !self.started {
            return Ok(Vec::new());
        }
        let ret = unsafe { ff::avcodec_send_frame(self.enc, ptr::null()) };
        if ret != ff::AVERROR_EOF {
            check(ret, "flush encoder")?;
        }
        self.drain()
    }

    fn drain(&mut self) -> Result<Vec<EncodedPacket>, Error> {
        let mut out = Vec::new();
        loop {
            let ret = unsafe { ff::avcodec_receive_packet(self.enc, self.packet) };
            if ret == ff::AVERROR(ffmpeg_next::util::error::EAGAIN) || ret == ff::AVERROR_EOF {
                return Ok(out);
            }
            check(ret, "receive packet")?;
            unsafe {
                let p = &*self.packet;
                debug_assert!(p.pts >= 0, "negative packet pts {}", p.pts);
                let data = if p.data.is_null() {
                    &[][..]
                } else {
                    std::slice::from_raw_parts(p.data, p.size as usize)
                };
                out.push(EncodedPacket {
                    data: Arc::from(data),
                    pts: Duration::from_nanos(p.pts.max(0) as u64),
                    keyframe: p.flags & ff::AV_PKT_FLAG_KEY != 0,
                });
                ff::av_packet_unref(self.packet);
            }
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.imports.clear();
        unsafe {
            ff::avcodec_free_context(&mut self.enc);
            ff::avfilter_graph_free(&mut self.graph);
            ff::av_packet_free(&mut self.packet);
            ff::av_buffer_unref(&mut self.import_frames);
            ff::av_buffer_unref(&mut self.device);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use vesper_core::{DmaBufPlane, ReleaseGuard};

    #[test]
    fn rejects_frame_of_wrong_size_and_releases_it() {
        if !Path::new("/dev/dri/renderD128").exists() {
            eprintln!("no GPU, skipping");
            return;
        }
        let mut enc = Encoder::new(Codec::Hevc, 640, 360, 30, RateControl::ConstantQp(30)).unwrap();
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let r = released.clone();
        let frame = VideoFrame {
            width: 128,
            height: 128,
            fourcc: u32::from_le_bytes(*b"XR24"),
            modifier: 0,
            planes: vec![DmaBufPlane {
                fd: File::open("/dev/null").unwrap().into(),
                offset: 0,
                stride: 512,
            }],
            buffer_id: 0,
            pool_generation: 0,
            pts: Duration::ZERO,
            release: ReleaseGuard::new(move || r.store(true, std::sync::atomic::Ordering::SeqCst)),
        };
        assert!(enc.encode(frame).is_err());
        assert!(released.load(std::sync::atomic::Ordering::SeqCst));
        assert!(enc.flush().unwrap().is_empty());
    }
}
