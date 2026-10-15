//! Test-pattern producer shared by the integration tests and `encode_bench`:
//! a pool of VA surfaces, each exported once as a DMA-BUF.

use std::ffi::c_int;
use std::os::fd::BorrowedFd;
use std::ptr;
use std::time::Duration;

use ffmpeg_next::ffi as ff;
use vesper_core::{DmaBufPlane, ReleaseGuard, VideoFrame};
use vesper_encode::RENDER_NODE;

/// Number of surfaces in the producer pool.
pub const POOL: usize = 4;

pub fn ok(ret: c_int, what: &str) {
    assert!(ret >= 0, "{what}: {}", ffmpeg_next::Error::from(ret));
}

/// A pool of VA surfaces (the "producer"), each exported once as a DMA-BUF.
pub struct Producer {
    w: u32,
    h: u32,
    surfaces: Vec<*mut ff::AVFrame>,
    exports: Vec<*mut ff::AVFrame>,
    upload: *mut ff::AVFrame,
}

impl Producer {
    pub unsafe fn new(w: u32, h: u32) -> Self {
        unsafe {
            let mut dev = ptr::null_mut();
            ok(
                ff::av_hwdevice_ctx_create(
                    &mut dev,
                    ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    RENDER_NODE.as_ptr(),
                    ptr::null_mut(),
                    0,
                ),
                "producer device",
            );
            let mut frames = ff::av_hwframe_ctx_alloc(dev);
            let fc = (*frames).data as *mut ff::AVHWFramesContext;
            (*fc).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*fc).sw_format = ff::AVPixelFormat::AV_PIX_FMT_BGR0;
            (*fc).width = w as c_int;
            (*fc).height = h as c_int;
            (*fc).initial_pool_size = POOL as c_int;
            ok(ff::av_hwframe_ctx_init(frames), "producer frames");

            let mut surfaces = Vec::new();
            let mut exports = Vec::new();
            for _ in 0..POOL {
                let s = ff::av_frame_alloc();
                ok(ff::av_hwframe_get_buffer(frames, s, 0), "get surface");
                let e = ff::av_frame_alloc();
                (*e).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
                ok(
                    ff::av_hwframe_map(e, s, ff::AV_HWFRAME_MAP_READ as c_int),
                    "export DMA-BUF",
                );
                surfaces.push(s);
                exports.push(e);
            }
            ff::av_buffer_unref(&mut frames);
            ff::av_buffer_unref(&mut dev);

            let upload = ff::av_frame_alloc();
            (*upload).format = ff::AVPixelFormat::AV_PIX_FMT_BGR0 as c_int;
            (*upload).width = w as c_int;
            (*upload).height = h as c_int;
            ok(ff::av_frame_get_buffer(upload, 0), "upload frame");
            Self {
                w,
                h,
                surfaces,
                exports,
                upload,
            }
        }
    }

    /// Draws frame `t` into surface `id` and wraps it as a `VideoFrame`.
    pub unsafe fn frame(&self, id: usize, t: u32, generation: u32, pts: Duration) -> VideoFrame {
        let (w, h) = (self.w, self.h);
        unsafe {
            let up = &*self.upload;
            let stride = up.linesize[0] as usize;
            let pixels = std::slice::from_raw_parts_mut(up.data[0], stride * h as usize);
            let (bx, by) = ((t * 9) % (w - 256), (t * 5) % (h - 256));
            for (y, row) in pixels.chunks_exact_mut(stride).enumerate() {
                let y = y as u32;
                for (x, px) in row[..w as usize * 4]
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .enumerate()
                {
                    let x = x as u32;
                    let in_box = (bx..bx + 256).contains(&x) && (by..by + 256).contains(&y);
                    let v = if in_box {
                        [255, 255, 255, 0]
                    } else {
                        [
                            (x + t * 4) as u8,
                            (y + t * 2) as u8,
                            ((x ^ y) >> 3) as u8,
                            0,
                        ]
                    };
                    *px = v;
                }
            }
            ok(
                ff::av_hwframe_transfer_data(self.surfaces[id], self.upload, 0),
                "upload",
            );

            let desc = &*((*self.exports[id]).data[0] as *const ff::AVDRMFrameDescriptor);
            let layer = &desc.layers[0];
            let planes = layer.planes[..layer.nb_planes as usize]
                .iter()
                .map(|p| DmaBufPlane {
                    fd: BorrowedFd::borrow_raw(desc.objects[p.object_index as usize].fd)
                        .try_clone_to_owned()
                        .unwrap(),
                    offset: p.offset as u32,
                    stride: p.pitch as u32,
                })
                .collect();
            VideoFrame {
                width: w,
                height: h,
                fourcc: layer.format,
                modifier: desc.objects[0].format_modifier,
                planes,
                buffer_id: id as u32,
                pool_generation: generation,
                pts,
                release: ReleaseGuard::new(|| {}),
            }
        }
    }
}
