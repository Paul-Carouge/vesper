//! Hardware video encoding through VAAPI (FFmpeg `*_vaapi` encoders).
//!
//! Pipeline, all on the GPU:
//! DMA-BUF (DRM PRIME) -> VA surface (imported once per producer buffer) ->
//! [letterbox: fitted onto an encoder-size canvas, VA-VPP, for other sizes] ->
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
    fn vaQueryVendorString(dpy: *mut c_void) -> *const std::ffi::c_char;
    fn vaCreateConfig(
        dpy: *mut c_void,
        profile: c_int,
        entrypoint: c_int,
        attribs: *mut c_void,
        num_attribs: c_int,
        config: *mut u32,
    ) -> c_int;
    fn vaDestroyConfig(dpy: *mut c_void, config: u32) -> c_int;
    fn vaCreateContext(
        dpy: *mut c_void,
        config: u32,
        width: c_int,
        height: c_int,
        flag: c_int,
        targets: *mut u32,
        num_targets: c_int,
        context: *mut u32,
    ) -> c_int;
    fn vaDestroyContext(dpy: *mut c_void, context: u32) -> c_int;
    fn vaCreateBuffer(
        dpy: *mut c_void,
        context: u32,
        kind: c_int,
        size: u32,
        num: u32,
        data: *mut c_void,
        buf: *mut u32,
    ) -> c_int;
    fn vaDestroyBuffer(dpy: *mut c_void, buf: u32) -> c_int;
    fn vaBeginPicture(dpy: *mut c_void, context: u32, target: u32) -> c_int;
    fn vaRenderPicture(dpy: *mut c_void, context: u32, bufs: *mut u32, num: c_int) -> c_int;
    fn vaEndPicture(dpy: *mut c_void, context: u32) -> c_int;
}

const VA_INVALID_ID: u32 = u32::MAX;

/// `VARectangle`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct VaRect {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}

/// `VAProcPipelineParameterBuffer` (libva 2.x, 64-bit): the fields after
/// `output_region` (filters, blending, colour properties, ...) stay zero.
#[repr(C)]
struct VppParams {
    surface: u32,
    surface_region: *const VaRect,
    surface_color_standard: u32,
    output_region: *const VaRect,
    rest: [u8; 192],
}
const _: () = assert!(size_of::<VppParams>() == 224);
const _: () = assert!(std::mem::offset_of!(VppParams, output_region) == 24);

fn va(status: c_int, what: &str) -> Result<(), Error> {
    if status == 0 {
        Ok(())
    } else {
        Err(Error(format!("{what} failed: VA status {status}")))
    }
}

/// VA quality level for [`RateControl::Quality`] on radeonsi, which reads it
/// as bit masks: bit 0 = set, bits 1-2 = preset (speed, balanced, quality,
/// high quality), bit 4 = VBAQ. Measured on VCN 5: quality + VBAQ (21 =
/// 0b10101) needs 15-30% fewer bits than the driver default for the same
/// VMAF, but HEVC/H.264 then encode only ~550 Mpx/s, so faster pixel rates
/// use balanced + VBAQ (19 = 0b10011). AV1 runs at the same speed with any
/// level. Other drivers (Intel: 1-7) read the level differently: `None`
/// keeps FFmpeg's default.
fn quality_level(vendor: &str, codec: Codec, px_rate: u64) -> Option<c_int> {
    if !vendor.contains("radeonsi") {
        return None;
    }
    Some(if codec == Codec::Av1 || px_rate <= 300_000_000 {
        21
    } else {
        19
    })
}

/// What to open instead when `rate` fails: drivers without QVBR get VBR at
/// half the cap.
fn fallback(rate: RateControl) -> Option<RateControl> {
    match rate {
        RateControl::Quality { max_bitrate, .. } => Some(RateControl::Bitrate(max_bitrate / 2)),
        _ => None,
    }
}

/// Rate control of the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateControl {
    /// Target bitrate in bits per second (VBR).
    Bitrate(u64),
    /// Constant QP. Range depends on the codec: 0-51 for H.264/HEVC, 0-255 for AV1.
    ConstantQp(u32),
    /// Constant quality capped at `max_bitrate` bits per second (VAAPI QVBR).
    /// `quality` is on the QP scale of [`RateControl::ConstantQp`]; lower is better.
    Quality { quality: u32, max_bitrate: u64 },
    /// Constant bitrate in bits per second with a one-second buffer (VBV),
    /// what streaming services ask for.
    ConstantBitrate(u64),
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
/// Uses the GPU at [`RENDER_NODE`]. Frames of another size than the encoder's
/// (a resized window or game) are scaled to fit it on the GPU, keeping their
/// aspect ratio (black bars).
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
    /// Last NV12 surface sent to the encoder, kept for [`Encoder::repeat`].
    last: Frame,
    /// Letterbox of frames of another size than the encoder's: where the
    /// picture goes on the canvas, `None` at native size.
    fit: Option<VaRect>,
    /// Encoder-size RGB surfaces the pictures are fitted onto (made on the
    /// first frame of another size), and those of them with black bars
    /// around `fit`.
    canvas: *mut ff::AVBufferRef,
    cleared: Vec<u32>,
    /// Small black RGB surface the bars are painted from.
    black: Frame,
    /// VA-VPP config and context of the letterbox.
    vpp: (u32, u32),
    packet: *mut ff::AVPacket,
    /// Imported VA surfaces by producer `buffer_id`, valid for `generation`.
    imports: HashMap<u32, Frame>,
    generation: Option<u32>,
    /// Whether a frame reached the encoder (FFmpeg 9 vaapi crashes when
    /// flushing an encoder that never got one).
    started: bool,
    /// Whether the next picture must be a keyframe ([`Encoder::force_keyframe`]).
    force_key: bool,
    /// Scaled encoders ([`Encoder::new_scaled`]): the frames context of the
    /// source pictures their graph was built for.
    scaled_from: *mut u8,
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
            last: Frame::new()?,
            fit: None,
            canvas: ptr::null_mut(),
            cleared: Vec::new(),
            black: Frame::new()?,
            vpp: (VA_INVALID_ID, VA_INVALID_ID),
            packet: ptr::null_mut(),
            imports: HashMap::new(),
            generation: None,
            started: false,
            force_key: false,
            scaled_from: ptr::null_mut(),
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

            e.set_import_size(width, height)?;
            e.open_with_fallback(codec, fps, rate)?;
        }
        Ok(e)
    }

    /// A second encoder fed with `self`'s pictures ([`Encoder::encode_scaled`]),
    /// scaled on the GPU to `width`x`height`: e.g. a live stream at another
    /// size, codec or rate control than the recording, without a second
    /// import or colour conversion. Same device as `self`.
    pub fn new_scaled(
        &self,
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        rate: RateControl,
    ) -> Result<Self, Error> {
        self.scaled_source()?.open(codec, width, height, fps, rate)
    }

    /// What [`ScaledSource::open`] needs from `self`, to open a scaled
    /// encoder on another thread (opening one takes tens of milliseconds).
    pub fn scaled_source(&self) -> Result<ScaledSource, Error> {
        unsafe {
            let mut source = ScaledSource {
                device: not_null(ff::av_buffer_ref(self.device), "device ref")?,
                frames: ptr::null_mut(),
                display: self.display,
                width: self.width,
                height: self.height,
            };
            source.frames = not_null(
                ff::av_buffer_ref(ff::av_buffersink_get_hw_frames_ctx(self.sink)),
                "frames ref",
            )?;
            Ok(source)
        }
    }
}

/// References to an encoder's device and picture pool: enough to open a
/// scaled encoder for it ([`Encoder::new_scaled`]) on any thread.
pub struct ScaledSource {
    device: *mut ff::AVBufferRef,
    frames: *mut ff::AVBufferRef,
    display: *mut c_void,
    width: u32,
    height: u32,
}

// SAFETY: reference-counted FFmpeg buffers (thread-safe counts) and a VA
// display, which libva lets several threads use.
unsafe impl Send for ScaledSource {}

impl Drop for ScaledSource {
    fn drop(&mut self) {
        unsafe {
            ff::av_buffer_unref(&mut self.frames);
            ff::av_buffer_unref(&mut self.device);
        }
    }
}

impl ScaledSource {
    /// Opens the scaled encoder; it then takes the source encoder's
    /// pictures through [`Encoder::encode_scaled`].
    pub fn open(
        mut self,
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        rate: RateControl,
    ) -> Result<Encoder, Error> {
        if width == 0 || height == 0 || fps == 0 {
            return Err(Error("width, height and fps must be non-zero".into()));
        }
        let mut e = Encoder {
            width,
            height,
            device: std::mem::replace(&mut self.device, ptr::null_mut()),
            display: self.display,
            import_frames: ptr::null_mut(),
            graph: ptr::null_mut(),
            src: ptr::null_mut(),
            sink: ptr::null_mut(),
            enc: ptr::null_mut(),
            filtered: Frame::new()?,
            last: Frame::new()?,
            fit: None,
            canvas: ptr::null_mut(),
            cleared: Vec::new(),
            black: Frame::new()?,
            vpp: (VA_INVALID_ID, VA_INVALID_ID),
            packet: ptr::null_mut(),
            imports: HashMap::new(),
            generation: None,
            started: false,
            force_key: false,
            scaled_from: ptr::null_mut(),
        };
        // On error, `e` is dropped and frees whatever was created so far.
        unsafe {
            e.packet = not_null(ff::av_packet_alloc(), "av_packet_alloc")?;
            e.build_scaled_graph(self.frames, self.width, self.height)?;
            e.open_with_fallback(codec, fps, rate)?;
        }
        Ok(e)
    }
}

impl Encoder {
    unsafe fn open_with_fallback(
        &mut self,
        codec: Codec,
        fps: u32,
        rate: RateControl,
    ) -> Result<(), Error> {
        unsafe {
            if let Err(err) = self.open_encoder(codec, fps, rate) {
                let Some(fb) = fallback(rate) else {
                    return Err(err);
                };
                tracing::warn!("{rate:?} failed ({err}), falling back to {fb:?}");
                ff::avcodec_free_context(&mut self.enc);
                self.open_encoder(codec, fps, fb)?;
            }
        }
        Ok(())
    }

    /// Builds `buffer` (`width`x`height` NV12 VA surfaces of `frames`) ->
    /// `scale_vaapi` -> `buffersink` for a scaled encoder. On error the
    /// previous graph stays.
    unsafe fn build_scaled_graph(
        &mut self,
        frames: *mut ff::AVBufferRef,
        width: u32,
        height: u32,
    ) -> Result<(), Error> {
        let args = std::ffi::CString::new(format!(
            "w={}:h={}:format=nv12:out_color_matrix=bt709:out_range=tv",
            self.width, self.height
        ))
        .expect("no NUL");
        unsafe {
            if frames.is_null() {
                return Err(Error("the source encoder has no frames context".into()));
            }
            let mut graph = not_null(ff::avfilter_graph_alloc(), "avfilter_graph_alloc")?;
            let built = (|| {
                let src = buffer_source(graph, frames, width, height, true)?;
                let vpp = create_filter(graph, c"scale_vaapi", c"scale", args.as_ptr())?;
                let sink = create_filter(graph, c"buffersink", c"out", ptr::null())?;
                check(ff::avfilter_link(src, 0, vpp, 0), "link")?;
                check(ff::avfilter_link(vpp, 0, sink, 0), "link")?;
                check(
                    ff::avfilter_graph_config(graph, ptr::null_mut()),
                    "configure graph",
                )?;
                Ok((src, sink))
            })();
            match built {
                Ok((src, sink)) => {
                    ff::avfilter_graph_free(&mut self.graph);
                    (self.graph, self.src, self.sink) = (graph, src, sink);
                    self.scaled_from = (*frames).data;
                    Ok(())
                }
                Err(e) => {
                    ff::avfilter_graph_free(&mut graph);
                    Err(e)
                }
            }
        }
    }

    /// Makes the import frames context and the filter graph take
    /// `width`x`height` frames: DMA-BUFs are imported at their own size, and
    /// a size other than the encoder's is scaled to fit it, keeping its
    /// aspect ratio (black bars, see [`Encoder::letterbox`]). Rebuilt only
    /// when the size changes; on error the previous graph stays. Imported
    /// surfaces and `last` keep references to the previous context and pool.
    unsafe fn set_import_size(&mut self, width: u32, height: u32) -> Result<(), Error> {
        unsafe {
            if !self.import_frames.is_null() {
                let fc = &*((*self.import_frames).data as *const ff::AVHWFramesContext);
                if (fc.width, fc.height) == (width as c_int, height as c_int) {
                    return Ok(());
                }
            }
            let mut frames = not_null(
                ff::av_hwframe_ctx_alloc(self.device),
                "av_hwframe_ctx_alloc",
            )?;
            let fc = (*frames).data as *mut ff::AVHWFramesContext;
            (*fc).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
            // Only describes the frames for VPP; imports take their real
            // format (XRGB/ARGB/...) from the DRM fourcc.
            (*fc).sw_format = ff::AVPixelFormat::AV_PIX_FMT_BGR0;
            (*fc).width = width as c_int;
            (*fc).height = height as c_int;
            let mut graph = ptr::null_mut();
            let native = (width, height) == (self.width, self.height);
            let built = check(ff::av_hwframe_ctx_init(frames), "init VA frames").and_then(|_| {
                if native {
                    self.build_graph(&mut graph, frames)
                } else {
                    self.open_letterbox()?;
                    self.build_graph(&mut graph, self.canvas)
                }
            });
            match built {
                Ok((src, sink)) => {
                    ff::avfilter_graph_free(&mut self.graph);
                    ff::av_buffer_unref(&mut self.import_frames);
                    (self.graph, self.src, self.sink, self.import_frames) =
                        (graph, src, sink, frames);
                    self.fit = (!native).then(|| fit(width, height, self.width, self.height));
                    self.cleared.clear();
                    Ok(())
                }
                Err(e) => {
                    ff::avfilter_graph_free(&mut graph);
                    ff::av_buffer_unref(&mut frames);
                    Err(e)
                }
            }
        }
    }

    /// Builds `buffer -> scale_vaapi (NV12) -> buffersink` into `graph`
    /// (freed by the caller on error) for encoder-size RGB surfaces of
    /// `frames`.
    unsafe fn build_graph(
        &self,
        graph: &mut *mut ff::AVFilterGraph,
        frames: *mut ff::AVBufferRef,
    ) -> Result<(*mut ff::AVFilterContext, *mut ff::AVFilterContext), Error> {
        unsafe {
            *graph = not_null(ff::avfilter_graph_alloc(), "avfilter_graph_alloc")?;
            let src = buffer_source(*graph, frames, self.width, self.height, false)?;
            let vpp = create_filter(
                *graph,
                c"scale_vaapi",
                c"vpp",
                c"format=nv12:out_color_matrix=bt709:out_range=tv".as_ptr(),
            )?;
            let sink = create_filter(*graph, c"buffersink", c"out", ptr::null())?;
            check(ff::avfilter_link(src, 0, vpp, 0), "link")?;
            check(ff::avfilter_link(vpp, 0, sink, 0), "link")?;
            check(
                ff::avfilter_graph_config(*graph, ptr::null_mut()),
                "configure graph",
            )?;
            Ok((src, sink))
        }
    }

    /// Makes what [`Encoder::letterbox`] needs, once: the canvas frames
    /// context, the black surface (uploaded from a zeroed frame, no
    /// readback) and the VA-VPP context.
    unsafe fn open_letterbox(&mut self) -> Result<(), Error> {
        unsafe {
            if !self.canvas.is_null() {
                return Ok(());
            }
            if (*self.black.0).buf[0].is_null() {
                const B: u32 = 16;
                let mut frames = rgb_frames(self.device, B, B)?;
                let made = (|| {
                    let zero = Frame::new()?;
                    (*zero.0).format = ff::AVPixelFormat::AV_PIX_FMT_BGR0 as c_int;
                    (*zero.0).width = B as c_int;
                    (*zero.0).height = B as c_int;
                    check(ff::av_frame_get_buffer(zero.0, 0), "black frame")?;
                    let len = (*zero.0).linesize[0] as usize * B as usize;
                    ptr::write_bytes((*zero.0).data[0], 0, len);
                    check(
                        ff::av_hwframe_get_buffer(frames, self.black.0, 0),
                        "black surface",
                    )?;
                    check(
                        ff::av_hwframe_transfer_data(self.black.0, zero.0, 0),
                        "upload black",
                    )
                })();
                // The surface holds its own reference to `frames`.
                ff::av_buffer_unref(&mut frames);
                if made.is_err() {
                    ff::av_frame_unref(self.black.0);
                }
                made?;
            }
            if self.vpp.0 == VA_INVALID_ID {
                const PROFILE_NONE: c_int = -1;
                const ENTRYPOINT_VIDEO_PROC: c_int = 10;
                va(
                    vaCreateConfig(
                        self.display,
                        PROFILE_NONE,
                        ENTRYPOINT_VIDEO_PROC,
                        ptr::null_mut(),
                        0,
                        &mut self.vpp.0,
                    ),
                    "vaCreateConfig (VPP)",
                )?;
            }
            if self.vpp.1 == VA_INVALID_ID {
                const PROGRESSIVE: c_int = 1;
                va(
                    vaCreateContext(
                        self.display,
                        self.vpp.0,
                        self.width as c_int,
                        self.height as c_int,
                        PROGRESSIVE,
                        ptr::null_mut(),
                        0,
                        &mut self.vpp.1,
                    ),
                    "vaCreateContext (VPP)",
                )?;
            }
            self.canvas = rgb_frames(self.device, self.width, self.height)?;
            Ok(())
        }
    }

    /// One VA-VPP job: surface `src` scaled into `region` of surface `dst`
    /// (all of it if null). Only that region of `dst` is written.
    unsafe fn blit(&self, src: u32, dst: u32, region: *const VaRect) -> Result<(), Error> {
        const PIPELINE_PARAMETER_BUFFER: c_int = 41;
        let mut params = VppParams {
            surface: src,
            surface_region: ptr::null(),
            surface_color_standard: 0,
            output_region: region,
            rest: [0; 192],
        };
        let (dpy, ctx) = (self.display, self.vpp.1);
        unsafe {
            va(vaBeginPicture(dpy, ctx, dst), "vaBeginPicture")?;
            let mut buf = VA_INVALID_ID;
            let rendered = va(
                vaCreateBuffer(
                    dpy,
                    ctx,
                    PIPELINE_PARAMETER_BUFFER,
                    size_of::<VppParams>() as u32,
                    1,
                    (&raw mut params).cast(),
                    &mut buf,
                ),
                "vaCreateBuffer",
            )
            .and_then(|_| va(vaRenderPicture(dpy, ctx, &mut buf, 1), "vaRenderPicture"));
            let ended = va(vaEndPicture(dpy, ctx), "vaEndPicture");
            if buf != VA_INVALID_ID {
                vaDestroyBuffer(dpy, buf);
            }
            rendered.and(ended)
        }
    }

    /// Fits `picture` (a surface of another size) at `fit` on a canvas
    /// surface, returned. Not `pad_vaapi`: radeonsi never paints its
    /// background colour, so its bars kept whatever the recycled VRAM held
    /// (a former picture after a size change, green in NV12). Here a canvas
    /// surface gets black bars once per `fit`, then only the picture area is
    /// written, in one scaling job (`scale_vaapi` + `pad_vaapi` took two).
    unsafe fn letterbox(&mut self, picture: *mut ff::AVFrame, fit: VaRect) -> Result<Frame, Error> {
        unsafe {
            let canvas = Frame::new()?;
            check(
                ff::av_hwframe_get_buffer(self.canvas, canvas.0, 0),
                "canvas surface",
            )?;
            let dst = (*canvas.0).data[3] as usize as u32;
            if !self.cleared.contains(&dst) {
                let black = (*self.black.0).data[3] as usize as u32;
                self.blit(black, dst, ptr::null())?;
                self.cleared.push(dst);
            }
            if let Err(e) = self.blit((*picture).data[3] as usize as u32, dst, &fit) {
                // The job may be running: wait for it before the producer's
                // buffer is released (radeonsi fences the target, not the source).
                vaSyncSurface(self.display, dst);
                return Err(e);
            }
            Ok(canvas)
        }
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
            // Parameter sets go to `extradata` for containers (see `extradata()`).
            enc.flags |= ff::AV_CODEC_FLAG_GLOBAL_HEADER as c_int;
            enc.color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
            enc.colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
            enc.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
            enc.color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_IEC61966_2_1;
            enc.hw_frames_ctx = not_null(
                ff::av_buffer_ref(ff::av_buffersink_get_hw_frames_ctx(self.sink)),
                "sink frames",
            )?;
            let mode = match rate {
                RateControl::Bitrate(bps) => {
                    enc.bit_rate = bps as i64;
                    c"VBR"
                }
                RateControl::ConstantQp(qp) => {
                    enc.global_quality = qp as c_int;
                    c"CQP"
                }
                RateControl::Quality {
                    quality,
                    max_bitrate,
                } => {
                    enc.global_quality = quality as c_int;
                    enc.bit_rate = max_bitrate as i64;
                    enc.rc_max_rate = max_bitrate as i64;
                    c"QVBR"
                }
                RateControl::ConstantBitrate(bps) => {
                    enc.bit_rate = bps as i64;
                    enc.rc_max_rate = bps as i64;
                    enc.rc_buffer_size = c_int::try_from(bps).unwrap_or(c_int::MAX);
                    c"CBR"
                }
            };
            if matches!(
                rate,
                RateControl::Quality { .. } | RateControl::ConstantBitrate(_)
            ) {
                let vendor = vaQueryVendorString(self.display);
                let vendor = if vendor.is_null() {
                    ""
                } else {
                    CStr::from_ptr(vendor).to_str().unwrap_or("")
                };
                let px_rate = u64::from(self.width) * u64::from(self.height) * u64::from(fps);
                if let Some(level) = quality_level(vendor, codec, px_rate) {
                    enc.compression_level = level;
                }
            }
            check(
                ff::av_opt_set(
                    self.enc.cast(),
                    c"rc_mode".as_ptr(),
                    mode.as_ptr(),
                    ff::AV_OPT_SEARCH_CHILDREN,
                ),
                "rc_mode",
            )?;
            check(
                ff::avcodec_open2(self.enc, c, ptr::null_mut()),
                "open encoder",
            )?;
        }
        Ok(())
    }

    /// Codec-level header (FFmpeg `extradata`) a muxer needs to declare the
    /// stream: Annex-B VPS/SPS/PPS for HEVC, SPS/PPS for H.264, the sequence
    /// header OBU for AV1. Empty if the encoder exposes none.
    pub fn extradata(&self) -> &[u8] {
        unsafe {
            let e = &*self.enc;
            if e.extradata.is_null() || e.extradata_size <= 0 {
                &[]
            } else {
                std::slice::from_raw_parts(e.extradata, e.extradata_size as usize)
            }
        }
    }

    /// Imports `frame`'s DMA-BUF as a VA surface (no copy).
    unsafe fn import(&mut self, frame: &VideoFrame) -> Result<Frame, Error> {
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
            self.set_import_size(frame.width, frame.height)?;
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
            // On error, wait for any VPP job still reading the producer's
            // buffer before `frame` drops and releases it.
            let (display, input) = (self.display, (*surface).data[3] as usize as u32);
            let sync_input = move || vaSyncSurface(display, input);
            let ret = match self.fit {
                None => {
                    (*surface).pts = pts;
                    ff::av_buffersrc_add_frame_flags(
                        self.src,
                        surface,
                        ff::AV_BUFFERSRC_FLAG_KEEP_REF as c_int,
                    )
                }
                Some(fit) => match self.letterbox(surface, fit) {
                    Ok(canvas) => {
                        (*canvas.0).pts = pts;
                        // Takes the reference.
                        ff::av_buffersrc_add_frame_flags(self.src, canvas.0, 0)
                    }
                    Err(e) => {
                        sync_input();
                        return Err(e);
                    }
                },
            };
            if ret < 0 {
                sync_input();
                check(ret, "feed VPP")?;
            }
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
            let ret = self.send_filtered();
            // Keep the NV12 surface (not the producer's buffer) for `repeat`.
            ff::av_frame_unref(self.last.0);
            ff::av_frame_move_ref(self.last.0, self.filtered.0);
            check(ret, "send frame to encoder")?;
            self.started = true;
        }
        self.drain()
    }

    /// Encodes the last picture again with a new `pts`, without a new
    /// capture: keeps the frame rate constant when the source sends no frame
    /// (Wayland only sends frames on damage). Fails before the first `encode`.
    pub fn repeat(&mut self, pts: Duration) -> Result<Vec<EncodedPacket>, Error> {
        let pts = i64::try_from(pts.as_nanos()).map_err(|_| Error("pts overflow".into()))?;
        unsafe {
            if (*self.last.0).buf[0].is_null() {
                return Err(Error("no frame to repeat".into()));
            }
            // Reuses `filtered` as a second reference to the same surface.
            check(
                ff::av_frame_ref(self.filtered.0, self.last.0),
                "ref last frame",
            )?;
            (*self.filtered.0).pts = pts;
            let ret = self.send_filtered();
            ff::av_frame_unref(self.filtered.0);
            check(ret, "send frame to encoder")?;
        }
        self.drain()
    }

    /// Encodes the picture `source` last encoded or repeated, scaled to this
    /// encoder's size, with `pts`. Only for encoders made by
    /// [`Encoder::new_scaled`] from `source`; fails before `source`'s first
    /// picture. For a picture already given here, [`Encoder::repeat`] is
    /// cheaper (no VPP job).
    pub fn encode_scaled(
        &mut self,
        source: &Encoder,
        pts: Duration,
    ) -> Result<Vec<EncodedPacket>, Error> {
        let pts = i64::try_from(pts.as_nanos()).map_err(|_| Error("pts overflow".into()))?;
        unsafe {
            let picture = source.last.0;
            if (*picture).buf[0].is_null() {
                return Err(Error("no picture to scale".into()));
            }
            let frames = (*picture).hw_frames_ctx;
            if frames.is_null() {
                return Err(Error("the picture has no frames context".into()));
            }
            // `source` rebuilt its graph (its input size changed): new pool.
            if (*frames).data != self.scaled_from {
                self.build_scaled_graph(frames, source.width, source.height)?;
            }
            check(ff::av_frame_ref(self.filtered.0, picture), "ref picture")?;
            (*self.filtered.0).pts = pts;
            // Takes the reference and resets `filtered`.
            let ret = ff::av_buffersrc_add_frame_flags(self.src, self.filtered.0, 0);
            if ret < 0 {
                ff::av_frame_unref(self.filtered.0);
                check(ret, "feed VPP")?;
            }
            check(
                ff::av_buffersink_get_frame(self.sink, self.filtered.0),
                "run VPP",
            )?;
            let ret = self.send_filtered();
            ff::av_frame_unref(self.last.0);
            ff::av_frame_move_ref(self.last.0, self.filtered.0);
            check(ret, "send frame to encoder")?;
            self.started = true;
        }
        self.drain()
    }

    /// Makes the next picture given to [`Encoder::encode`] or
    /// [`Encoder::repeat`] a keyframe (IDR), e.g. so a new file can start there.
    pub fn force_keyframe(&mut self) {
        self.force_key = true;
    }

    /// Sends `filtered` to the encoder, as a keyframe if one was forced.
    unsafe fn send_filtered(&mut self) -> c_int {
        unsafe {
            let f = &mut *self.filtered.0;
            if std::mem::take(&mut self.force_key) {
                f.pict_type = ff::AVPictureType::AV_PICTURE_TYPE_I;
                f.flags |= ff::AV_FRAME_FLAG_KEY as c_int;
            }
            let ret = ff::avcodec_send_frame(self.enc, f);
            // `filtered` becomes `last`, which `repeat` copies: never force twice.
            f.pict_type = ff::AVPictureType::AV_PICTURE_TYPE_NONE;
            f.flags &= !(ff::AV_FRAME_FLAG_KEY as c_int);
            ret
        }
    }

    /// Flushes the encoder and returns the remaining packets. The encoder
    /// cannot take frames afterwards.
    pub fn flush(&mut self) -> Result<Vec<EncodedPacket>, Error> {
        if !self.started {
            return Ok(Vec::new());
        }
        unsafe { ff::av_frame_unref(self.last.0) };
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

/// Where a `width`x`height` picture goes on a `w`x`h` canvas: as large as
/// fits with its aspect ratio, even-sized, centred.
fn fit(width: u32, height: u32, w: u32, h: u32) -> VaRect {
    let (iw, ih, w, h) = (
        u64::from(width),
        u64::from(height),
        u64::from(w),
        u64::from(h),
    );
    let (fw, fh) = if iw * h > ih * w {
        (w, (w * ih + iw / 2) / iw)
    } else {
        ((h * iw + ih / 2) / ih, h)
    };
    let (fw, fh) = ((fw & !1).max(2), (fh & !1).max(2));
    VaRect {
        x: ((w - fw) / 2) as i16,
        y: ((h - fh) / 2) as i16,
        width: fw as u16,
        height: fh as u16,
    }
}

/// An initialised frames context of `w`x`h` RGB VA surfaces.
unsafe fn rgb_frames(
    device: *mut ff::AVBufferRef,
    w: u32,
    h: u32,
) -> Result<*mut ff::AVBufferRef, Error> {
    unsafe {
        let mut frames = not_null(ff::av_hwframe_ctx_alloc(device), "av_hwframe_ctx_alloc")?;
        let fc = (*frames).data as *mut ff::AVHWFramesContext;
        (*fc).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*fc).sw_format = ff::AVPixelFormat::AV_PIX_FMT_BGR0;
        (*fc).width = w as c_int;
        (*fc).height = h as c_int;
        let ret = ff::av_hwframe_ctx_init(frames);
        if ret < 0 {
            ff::av_buffer_unref(&mut frames);
        }
        check(ret, "init VA frames")?;
        Ok(frames)
    }
}

/// A `buffer` source of `width`x`height` VA surfaces from `frames`; `yuv`:
/// NV12 pictures of our own (BT.709, limited range), else RGB imports.
unsafe fn buffer_source(
    graph: *mut ff::AVFilterGraph,
    frames: *mut ff::AVBufferRef,
    width: u32,
    height: u32,
    yuv: bool,
) -> Result<*mut ff::AVFilterContext, Error> {
    unsafe {
        let src = not_null(
            ff::avfilter_graph_alloc_filter(
                graph,
                ff::avfilter_get_by_name(c"buffer".as_ptr()),
                c"in".as_ptr(),
            ),
            "alloc buffer filter",
        )?;
        let par = not_null(ff::av_buffersrc_parameters_alloc(), "buffersrc params")?;
        (*par).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI as c_int;
        (*par).time_base = TIME_BASE;
        (*par).width = width as c_int;
        (*par).height = height as c_int;
        (*par).hw_frames_ctx = frames;
        if yuv {
            (*par).color_space = ff::AVColorSpace::AVCOL_SPC_BT709;
            (*par).color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
        }
        let ret = ff::av_buffersrc_parameters_set(src, par);
        ff::av_free(par.cast());
        check(ret, "buffersrc params")?;
        check(
            ff::avfilter_init_dict(src, ptr::null_mut()),
            "init buffersrc",
        )?;
        Ok(src)
    }
}

unsafe fn create_filter(
    graph: *mut ff::AVFilterGraph,
    name: &CStr,
    label: &CStr,
    args: *const std::ffi::c_char,
) -> Result<*mut ff::AVFilterContext, Error> {
    let mut f = ptr::null_mut();
    unsafe {
        check(
            ff::avfilter_graph_create_filter(
                &mut f,
                ff::avfilter_get_by_name(name.as_ptr()),
                label.as_ptr(),
                args,
                ptr::null_mut(),
                graph,
            ),
            "create filter",
        )?;
    }
    Ok(f)
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.imports.clear();
        unsafe { ff::av_frame_unref(self.last.0) };
        unsafe {
            ff::avcodec_free_context(&mut self.enc);
            ff::avfilter_graph_free(&mut self.graph);
            ff::av_packet_free(&mut self.packet);
            ff::av_buffer_unref(&mut self.import_frames);
            ff::av_frame_unref(self.black.0);
            ff::av_buffer_unref(&mut self.canvas);
            if self.vpp.1 != VA_INVALID_ID {
                vaDestroyContext(self.display, self.vpp.1);
            }
            if self.vpp.0 != VA_INVALID_ID {
                vaDestroyConfig(self.display, self.vpp.0);
            }
            ff::av_buffer_unref(&mut self.device);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vesper_core::{DmaBufPlane, ReleaseGuard};

    #[test]
    fn fit_keeps_the_aspect_ratio_centred_and_even() {
        let r = |x, y, width, height| VaRect {
            x,
            y,
            width,
            height,
        };
        assert_eq!(fit(1920, 800, 640, 360), r(0, 47, 640, 266));
        assert_eq!(fit(400, 300, 640, 360), r(80, 0, 480, 360));
        assert_eq!(fit(1280, 720, 640, 360), r(0, 0, 640, 360));
    }

    #[test]
    fn quality_level_only_on_radeonsi() {
        let amd = "Mesa Gallium driver 26.2.4 for AMD Radeon RX 9070 XT (radeonsi, gfx1201)";
        assert_eq!(quality_level(amd, Codec::Hevc, 2560 * 1440 * 60), Some(21));
        assert_eq!(quality_level(amd, Codec::Hevc, 3840 * 2160 * 60), Some(19));
        assert_eq!(quality_level(amd, Codec::Av1, 3840 * 2160 * 60), Some(21));
        assert_eq!(quality_level("Intel iHD driver", Codec::Hevc, 1), None);
    }

    #[test]
    fn only_quality_falls_back_to_vbr_at_half_the_cap() {
        let q = RateControl::Quality {
            quality: 28,
            max_bitrate: 20_000_000,
        };
        assert_eq!(fallback(q), Some(RateControl::Bitrate(10_000_000)));
        assert_eq!(fallback(RateControl::Bitrate(1)), None);
        assert_eq!(fallback(RateControl::ConstantQp(30)), None);
    }

    #[test]
    fn rejects_a_bad_dmabuf_and_releases_it() {
        if !vesper_core::test_gpu() {
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
