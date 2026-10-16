//! MJPEG decoding. On the GPU: FFmpeg's `mjpeg` decoder with the VAAPI
//! hwaccel (the VCN JPEG engine), VA-VPP (`scale_vaapi`) to an RGB surface,
//! exported as a DMA-BUF `VideoFrame` (`av_hwframe_map` to DRM PRIME runs
//! `vaExportSurfaceHandle`). On the CPU, the last resort: FFmpeg's software
//! decoder, the planes repacked (not converted) into a [`CpuFrame`].

use std::collections::HashMap;
use std::ffi::{CStr, c_int};
use std::os::fd::BorrowedFd;
use std::ptr;
use std::time::Duration;

use ffmpeg_next::ffi as ff;
use vesper_core::{DmaBufPlane, ReleaseGuard, VideoFrame};

use super::modes::{Matrix, Range};
use super::{CameraFrame, CpuFrame, CpuLayout, CpuPlane};
use crate::Error;

/// The discrete GPU (`renderD129` is the iGPU on the target machine).
const RENDER_NODE: &CStr = c"/dev/dri/renderD128";

/// A JPEG the CPU decoder cannot repack (4:4:4, grey...): every picture of
/// the camera will be the same, so the stream ends at once.
#[derive(Debug)]
pub struct Unsupported(String);

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unsupported {}

fn check(ret: c_int, what: &str) -> Result<c_int, Error> {
    if ret < 0 {
        Err(format!("{what}: {}", ffmpeg_next::Error::from(ret)).into())
    } else {
        Ok(ret)
    }
}

/// Owned `AVFrame`.
struct Frame(*mut ff::AVFrame);

// SAFETY: an AVFrame's buffers are reference counted with atomics, and VA
// surfaces return to their pool under its lock: unref from any thread is fine.
unsafe impl Send for Frame {}

impl Frame {
    fn new() -> Result<Self, Error> {
        let f = unsafe { ff::av_frame_alloc() };
        if f.is_null() {
            return Err("av_frame_alloc failed".into());
        }
        Ok(Self(f))
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { ff::av_frame_free(&mut self.0) };
    }
}

/// The VPP graph from the decoder's YUV surfaces to RGB ones, built for one
/// decoder frames context and size.
struct Graph {
    graph: *mut ff::AVFilterGraph,
    src: *mut ff::AVFilterContext,
    sink: *mut ff::AVFilterContext,
    key: (*mut u8, c_int, c_int),
}

impl Drop for Graph {
    fn drop(&mut self) {
        unsafe { ff::avfilter_graph_free(&mut self.graph) };
    }
}

pub struct Decoder {
    ctx: *mut ff::AVCodecContext,
    /// VAAPI device; null on the CPU.
    device: *mut ff::AVBufferRef,
    packet: *mut ff::AVPacket,
    decoded: Frame,
    graph: Option<Graph>,
    /// `buffer_id` of each RGB VA surface of the current graph.
    ids: HashMap<u32, u32>,
    generation: u32,
}

// SAFETY: the FFmpeg/VA objects are owned here and only used through `&mut self`.
unsafe impl Send for Decoder {}

unsafe extern "C" fn vaapi_format(
    _: *mut ff::AVCodecContext,
    mut formats: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    unsafe {
        while *formats != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            if *formats == ff::AVPixelFormat::AV_PIX_FMT_VAAPI {
                return *formats;
            }
            formats = formats.add(1);
        }
    }
    // No VAAPI JPEG profile for this picture: the decode fails, and the
    // camera falls back to the CPU.
    ff::AVPixelFormat::AV_PIX_FMT_NONE
}

impl Decoder {
    /// A VAAPI decoder on [`RENDER_NODE`].
    pub fn gpu() -> Result<Self, Error> {
        Self::new(true)
    }

    /// The software decoder.
    pub fn cpu() -> Result<Self, Error> {
        Self::new(false)
    }

    pub fn is_gpu(&self) -> bool {
        !self.device.is_null()
    }

    fn new(gpu: bool) -> Result<Self, Error> {
        let mut d = Self {
            ctx: ptr::null_mut(),
            device: ptr::null_mut(),
            packet: ptr::null_mut(),
            decoded: Frame::new()?,
            graph: None,
            ids: HashMap::new(),
            generation: 0,
        };
        // On error, `d` is dropped and frees whatever was made so far.
        unsafe {
            let codec = ff::avcodec_find_decoder(ff::AVCodecID::AV_CODEC_ID_MJPEG);
            if codec.is_null() {
                return Err("FFmpeg has no MJPEG decoder".into());
            }
            d.ctx = ff::avcodec_alloc_context3(codec);
            d.packet = ff::av_packet_alloc();
            if d.ctx.is_null() || d.packet.is_null() {
                return Err("allocating the MJPEG decoder failed".into());
            }
            // One picture in, one out: no frame threads holding pictures back.
            (*d.ctx).thread_count = 1;
            if gpu {
                check(
                    ff::av_hwdevice_ctx_create(
                        &mut d.device,
                        ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                        RENDER_NODE.as_ptr(),
                        ptr::null_mut(),
                        0,
                    ),
                    "open VAAPI device",
                )?;
                (*d.ctx).hw_device_ctx = ff::av_buffer_ref(d.device);
                if (*d.ctx).hw_device_ctx.is_null() {
                    return Err("referencing the VAAPI device failed".into());
                }
                (*d.ctx).get_format = Some(vaapi_format);
            }
            check(
                ff::avcodec_open2(d.ctx, codec, ptr::null_mut()),
                "open MJPEG decoder",
            )?;
        }
        Ok(d)
    }

    /// Decodes one JPEG picture.
    pub fn decode(&mut self, jpeg: &[u8], pts: Duration) -> Result<CameraFrame, Error> {
        unsafe {
            ff::av_packet_unref(self.packet);
            let len = c_int::try_from(jpeg.len()).map_err(|_| "JPEG too big")?;
            check(ff::av_new_packet(self.packet, len), "av_new_packet")?;
            ptr::copy_nonoverlapping(jpeg.as_ptr(), (*self.packet).data, jpeg.len());
            check(
                ff::avcodec_send_packet(self.ctx, self.packet),
                "decode JPEG",
            )?;
            ff::av_frame_unref(self.decoded.0);
            check(
                ff::avcodec_receive_frame(self.ctx, self.decoded.0),
                "decode JPEG",
            )?;
            if self.is_gpu() {
                self.vpp_export(pts).map(CameraFrame::Gpu)
            } else {
                repack(&*self.decoded.0, pts).map(CameraFrame::Cpu)
            }
        }
    }

    /// VA-VPP of the decoded surface to RGB, exported as a DMA-BUF.
    unsafe fn vpp_export(&mut self, pts: Duration) -> Result<VideoFrame, Error> {
        unsafe {
            let dec = self.decoded.0;
            if (*dec).format != ff::AVPixelFormat::AV_PIX_FMT_VAAPI as c_int
                || (*dec).hw_frames_ctx.is_null()
            {
                return Err("the MJPEG decoder did not use VAAPI".into());
            }
            // JFIF: BT.601 full range, whatever the file leaves unsaid.
            if (*dec).colorspace == ff::AVColorSpace::AVCOL_SPC_UNSPECIFIED {
                (*dec).colorspace = ff::AVColorSpace::AVCOL_SPC_BT470BG;
            }
            if (*dec).color_range == ff::AVColorRange::AVCOL_RANGE_UNSPECIFIED {
                (*dec).color_range = ff::AVColorRange::AVCOL_RANGE_JPEG;
            }
            let key = ((*(*dec).hw_frames_ctx).data, (*dec).width, (*dec).height);
            if self.graph.as_ref().is_none_or(|g| g.key != key) {
                // A new size (or decoder pool): new RGB surfaces, new ids.
                self.graph = None;
                self.ids.clear();
                self.generation = self.generation.wrapping_add(1);
                self.graph = Some(vpp_graph(dec, key)?);
            }
            let graph = self.graph.as_ref().expect("built above");
            // Takes the reference: `decoded` is left blank.
            check(
                ff::av_buffersrc_add_frame_flags(graph.src, dec, 0),
                "feed VPP",
            )?;
            let rgb = Frame::new()?;
            check(ff::av_buffersink_get_frame(graph.sink, rgb.0), "run VPP")?;
            let surface = (*rgb.0).data[3] as usize as u32;
            // A read map syncs the surface before exporting it (FFmpeg 9,
            // vaExportSurfaceHandle path): the consumer gets a finished picture.
            let drm = Frame::new()?;
            (*drm.0).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
            check(
                ff::av_hwframe_map(drm.0, rgb.0, ff::AV_HWFRAME_MAP_READ as c_int),
                "export the RGB surface",
            )?;
            let desc = &*((*drm.0).data[0] as *const ff::AVDRMFrameDescriptor);
            // One RGB layer in one buffer object; more than one plane only
            // for the modifier's metadata (DCC), in that same object.
            let layer = &desc.layers[0];
            let planes_ok = (1..=4).contains(&layer.nb_planes)
                && layer.planes[..layer.nb_planes as usize]
                    .iter()
                    .all(|p| p.object_index == 0);
            if desc.nb_layers != 1 || desc.nb_objects != 1 || !planes_ok {
                return Err(format!(
                    "unexpected RGB export: {} layers, {} objects, {} planes",
                    desc.nb_layers, desc.nb_objects, layer.nb_planes
                )
                .into());
            }
            let mut planes = Vec::with_capacity(layer.nb_planes as usize);
            for p in &layer.planes[..layer.nb_planes as usize] {
                let object = &desc.objects[0];
                // The mapping closes its fds when `drm` drops: keep our own.
                let fd = BorrowedFd::borrow_raw(object.fd)
                    .try_clone_to_owned()
                    .map_err(|e| format!("dup of the exported fd failed: {e}"))?;
                planes.push(DmaBufPlane {
                    fd,
                    offset: p.offset as u32,
                    stride: p.pitch as u32,
                });
            }
            let next = self.ids.len() as u32;
            let buffer_id = *self.ids.entry(surface).or_insert(next);
            Ok(VideoFrame {
                width: (*rgb.0).width as u32,
                height: (*rgb.0).height as u32,
                fourcc: layer.format,
                modifier: desc.objects[0].format_modifier,
                planes,
                buffer_id,
                pool_generation: self.generation,
                pts,
                // The surface goes back to the VPP pool once the consumer is done.
                release: ReleaseGuard::new(move || drop(rgb)),
            })
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.graph = None;
        unsafe {
            ff::av_frame_unref(self.decoded.0);
            ff::av_packet_free(&mut self.packet);
            ff::avcodec_free_context(&mut self.ctx);
            ff::av_buffer_unref(&mut self.device);
        }
    }
}

unsafe fn filter(
    graph: *mut ff::AVFilterGraph,
    name: &CStr,
    args: Option<&CStr>,
) -> Result<*mut ff::AVFilterContext, Error> {
    let mut f = ptr::null_mut();
    unsafe {
        check(
            ff::avfilter_graph_create_filter(
                &mut f,
                ff::avfilter_get_by_name(name.as_ptr()),
                name.as_ptr(),
                args.map_or(ptr::null(), CStr::as_ptr),
                ptr::null_mut(),
                graph,
            ),
            "create filter",
        )?;
    }
    Ok(f)
}

/// `buffer` (the decoder's surfaces) -> `scale_vaapi=format=bgr0` -> sink.
unsafe fn vpp_graph(dec: *const ff::AVFrame, key: (*mut u8, c_int, c_int)) -> Result<Graph, Error> {
    unsafe {
        let mut g = Graph {
            graph: ff::avfilter_graph_alloc(),
            src: ptr::null_mut(),
            sink: ptr::null_mut(),
            key,
        };
        if g.graph.is_null() {
            return Err("avfilter_graph_alloc failed".into());
        }
        g.src = ff::avfilter_graph_alloc_filter(
            g.graph,
            ff::avfilter_get_by_name(c"buffer".as_ptr()),
            c"in".as_ptr(),
        );
        let par = ff::av_buffersrc_parameters_alloc();
        if g.src.is_null() || par.is_null() {
            ff::av_free(par.cast());
            return Err("allocating the VPP source failed".into());
        }
        (*par).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI as c_int;
        (*par).time_base = ff::AVRational { num: 1, den: 1 };
        (*par).width = (*dec).width;
        (*par).height = (*dec).height;
        (*par).hw_frames_ctx = (*dec).hw_frames_ctx;
        (*par).color_space = (*dec).colorspace;
        (*par).color_range = (*dec).color_range;
        let ret = ff::av_buffersrc_parameters_set(g.src, par);
        ff::av_free(par.cast());
        check(ret, "VPP source parameters")?;
        check(
            ff::avfilter_init_dict(g.src, ptr::null_mut()),
            "init VPP source",
        )?;
        let scale = filter(g.graph, c"scale_vaapi", Some(c"format=bgr0"))?;
        g.sink = filter(g.graph, c"buffersink", None)?;
        check(ff::avfilter_link(g.src, 0, scale, 0), "link VPP")?;
        check(ff::avfilter_link(scale, 0, g.sink, 0), "link VPP")?;
        check(
            ff::avfilter_graph_config(g.graph, ptr::null_mut()),
            "configure VPP",
        )?;
        Ok(g)
    }
}

/// A software-decoded picture as NV12 (4:2:0) or YUY2 (4:2:2): the planes
/// interleaved, the samples untouched.
unsafe fn repack(f: &ff::AVFrame, pts: Duration) -> Result<CpuFrame, Error> {
    use ff::AVPixelFormat as P;
    let (w, h) = (f.width as usize, f.height as usize);
    if w == 0 || h == 0 {
        return Err("empty JPEG picture".into());
    }
    let format = f.format;
    let is = |p: P| format == p as c_int;
    let range = if f.color_range == ff::AVColorRange::AVCOL_RANGE_MPEG {
        Range::Limited
    } else {
        Range::Full
    };
    // SAFETY: plane `i` of a decoded frame has `linesize[i]` bytes per row
    // and as many rows as its subsampling gives.
    let row = |plane: usize, y: usize, len: usize| unsafe {
        std::slice::from_raw_parts(
            f.data[plane].offset(f.linesize[plane] as isize * y as isize),
            len,
        )
    };
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (layout, data, planes) = if is(P::AV_PIX_FMT_YUVJ420P) || is(P::AV_PIX_FMT_YUV420P) {
        let mut data = Vec::with_capacity(w * h + cw * 2 * ch);
        for y in 0..h {
            data.extend_from_slice(row(0, y, w));
        }
        for y in 0..ch {
            for (u, v) in row(1, y, cw).iter().zip(row(2, y, cw)) {
                data.extend_from_slice(&[*u, *v]);
            }
        }
        let planes = vec![
            CpuPlane {
                offset: 0,
                stride: w,
            },
            CpuPlane {
                offset: w * h,
                stride: cw * 2,
            },
        ];
        (CpuLayout::Nv12, data, planes)
    } else if is(P::AV_PIX_FMT_YUVJ422P) || is(P::AV_PIX_FMT_YUV422P) {
        let stride = cw * 4;
        let mut data = Vec::with_capacity(stride * h);
        for y in 0..h {
            let (ys, us, vs) = (row(0, y, w), row(1, y, cw), row(2, y, cw));
            for x in 0..cw {
                // An odd last column repeats its Y.
                let y1 = ys.get(2 * x + 1).copied().unwrap_or(ys[2 * x]);
                data.extend_from_slice(&[ys[2 * x], us[x], y1, vs[x]]);
            }
        }
        (CpuLayout::Yuy2, data, vec![CpuPlane { offset: 0, stride }])
    } else {
        return Err(Box::new(Unsupported(format!(
            "the camera's JPEG sampling (FFmpeg pixel format {format}) needs GPU decoding"
        ))));
    };
    Ok(CpuFrame {
        width: w as u32,
        height: h as u32,
        layout,
        planes,
        data,
        matrix: Matrix::Bt601,
        range,
        pts,
    })
}
