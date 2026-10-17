//! GPU compositor for scenes: draws the main capture and overlays (images,
//! text, webcam) into an exported DMA-BUF ring that `vesper-encode` takes as
//! any other [`VideoFrame`].
//!
//! Vulkan compute on an async compute queue of the discrete GPU
//! (`/dev/dri/renderD128`, never the iGPU), at low global priority when the
//! driver allows it so a game keeps the GPU first. One compute shader
//! (`shaders/compose.comp`, SPIR-V committed) draws up to [`MAX_LAYERS`]
//! layers per frame; a layer drawn smaller than its source is box filtered
//! (no aliasing). [`Compositor::submit`] queues a composition and
//! [`Compositor::poll`] hands the frame over once its fence signalled (VA
//! has no implicit sync with Vulkan on radeonsi): a caller on a frame clock
//! never waits longer than it chooses for a GPU busy with a game.
//! Textures are prepared (mips, staging copy) by an [`Uploader`] on any
//! thread; [`Compositor::set_staged`] only queues the GPU copy.
//!
//! Cameras are inputs of their own ([`Source::Camera`]): the latest
//! picture of each, an RGB DMA-BUF ([`Compositor::set_camera_frame`]) or
//! YUV bytes staged by an [`Uploader`] ([`Compositor::set_camera_yuv`]), is
//! copied (and converted to RGB) into a compositor-owned image by the next
//! composition, then released: redraws never pin a camera's buffers.

use std::collections::HashMap;
use std::fmt;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ash::vk;
use vesper_core::{DmaBufPlane, ReleaseGuard, VideoFrame};

/// Layers drawn per frame, the main capture included.
pub const MAX_LAYERS: usize = 16;
/// Cameras one compositor takes.
pub const MAX_CAMERAS: usize = 4;
/// Size of the [`Compositor::preview`] image.
pub const PREVIEW_WIDTH: u32 = 480;
pub const PREVIEW_HEIGHT: u32 = 270;
/// DRM fourcc of the composed frames (`DRM_FORMAT_XRGB8888`).
pub const OUTPUT_FOURCC: u32 = fourcc(b"XR24");

const RENDER_NODE: &str = "/dev/dri/renderD128";
const RING: usize = 3;
/// Longest wait for one composition: past it the GPU is considered hung.
const FENCE_TIMEOUT: Duration = Duration::from_secs(2);
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const FLAG_NO_ALPHA: u32 = 1;
const FLAG_SWAP_RB: u32 = 2;
const FLAG_EXACT: u32 = 4;
const SPIRV: &[u8] = include_bytes!("../shaders/compose.spv");
const CAMERA_SPIRV: &[u8] = include_bytes!("../shaders/camera.spv");
const MODE_RGB: u32 = 0;
const MODE_NV12: u32 = 1;
const MODE_YUY2: u32 = 2;

const fn fourcc(c: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*c)
}

#[derive(Debug, Clone)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn err(what: &'static str) -> impl FnOnce(vk::Result) -> Error {
    move |r| Error(format!("{what}: {r}"))
}

/// What a layer shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The `main` frame passed to [`Compositor::compose`].
    Main,
    /// A texture set with [`Compositor::set_texture`].
    Texture(u32),
    /// The latest picture of a camera set with
    /// [`Compositor::set_camera_frame`] or [`Compositor::set_camera_yuv`].
    Camera(u32),
}

/// Layout of a YUV camera picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YuvLayout {
    /// A Y plane, then interleaved UV at half width and height.
    Nv12,
    /// One plane, `Y0 U Y1 V` per two pixels.
    Yuy2,
}

/// The YUV to RGB matrix of a camera picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YuvMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

/// A YUV camera picture in system memory, for [`Uploader::prepare_yuv`].
#[derive(Debug, Clone, Copy)]
pub struct YuvPicture<'a> {
    pub width: u32,
    pub height: u32,
    pub layout: YuvLayout,
    /// `(offset, stride)` in `data` of each plane: one for YUY2, two for NV12.
    pub planes: &'a [(usize, usize)],
    pub data: &'a [u8],
    pub matrix: YuvMatrix,
    /// 0-255 (JPEG) rather than 16-235.
    pub full_range: bool,
}

/// The rows of `f` (byte ranges of `data`) in tightly packed order: Y rows
/// (`width` bytes) then UV rows for NV12, `Y0 U Y1 V` rows for YUY2. Errs
/// when a plane does not fit in `data`.
fn yuv_rows(f: &YuvPicture<'_>) -> Result<Vec<std::ops::Range<usize>>> {
    let (w, h) = (f.width as usize, f.height as usize);
    if w == 0 || h == 0 || w > 16384 || h > 16384 {
        return Err(Error(format!("bad camera picture size {w}x{h}")));
    }
    let pairs = w.div_ceil(2);
    // (bytes per row, rows) of each plane.
    let planes = match f.layout {
        YuvLayout::Nv12 => vec![(w, h), (pairs * 2, h.div_ceil(2))],
        YuvLayout::Yuy2 => vec![(pairs * 4, h)],
    };
    if f.planes.len() != planes.len() {
        return Err(Error(format!(
            "{:?} has {} plane(s), the picture {}",
            f.layout,
            planes.len(),
            f.planes.len()
        )));
    }
    let mut rows = Vec::new();
    for (&(offset, stride), (bytes, n)) in f.planes.iter().zip(planes) {
        if stride < bytes {
            return Err(Error(format!("stride {stride} under {bytes} bytes")));
        }
        for r in 0..n {
            let range = r
                .checked_mul(stride)
                .and_then(|o| o.checked_add(offset))
                .and_then(|s| Some(s..s.checked_add(bytes)?))
                .filter(|r| r.end <= f.data.len())
                .ok_or_else(|| Error("camera plane out of its buffer".into()))?;
            rows.push(range);
        }
    }
    Ok(rows)
}

/// The camera pass's parameters (std140).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct CameraParams {
    /// Mode, unused, chroma offset (NV12), chroma (NV12) or row (YUY2) stride.
    head: [u32; 4],
    /// Luma offset and scale, unused, chroma scale.
    range: [f32; 4],
    /// R from Cr, G from Cb, G from Cr, B from Cb.
    coef: [f32; 4],
}

fn yuv_params(
    layout: YuvLayout,
    matrix: YuvMatrix,
    full_range: bool,
    w: u32,
    h: u32,
) -> CameraParams {
    let (kr, kb) = match matrix {
        YuvMatrix::Bt601 => (0.299, 0.114),
        YuvMatrix::Bt709 => (0.2126, 0.0722),
        YuvMatrix::Bt2020 => (0.2627, 0.0593),
    };
    let kg = 1.0 - kr - kb;
    let pairs = w.div_ceil(2);
    let (mode, chroma, stride) = match layout {
        YuvLayout::Nv12 => (MODE_NV12, w * h, pairs * 2),
        YuvLayout::Yuy2 => (MODE_YUY2, 0, pairs * 4),
    };
    CameraParams {
        head: [mode, 0, chroma, stride],
        range: if full_range {
            [0.0, 1.0 / 255.0, 0.0, 1.0 / 255.0]
        } else {
            [16.0, 1.0 / 219.0, 0.0, 1.0 / 224.0]
        },
        coef: [
            2.0 * (1.0 - kr),
            2.0 * kb * (1.0 - kb) / kg,
            2.0 * kr * (1.0 - kr) / kg,
            2.0 * (1.0 - kb),
        ],
    }
}

/// How the cropped source fills its rect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fit {
    /// Whole source visible, aspect kept, centred (bars stay transparent).
    #[default]
    Contain,
    /// Rect filled, aspect kept, source cropped around its centre.
    Cover,
    /// Rect filled, aspect ignored.
    Stretch,
}

/// One layer of a composition. Draws are listed bottom to top.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Draw {
    pub source: Source,
    /// x, y, width, height as fractions of the canvas.
    pub rect: [f32; 4],
    /// Left, top, right, bottom fractions of the source cut away.
    pub crop: [f32; 4],
    pub fit: Fit,
    /// 0 (invisible) to 1.
    pub opacity: f32,
}

/// One layer as the shader reads it (std140).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct GpuLayer {
    dst: [f32; 4],
    src: [f32; 4],
    misc: [f32; 4],
    flags: [u32; 4],
}

#[repr(C)]
struct Params {
    head: [u32; 4],
    layers: [GpuLayer; MAX_LAYERS],
}

/// Places a `tw` x `th` source on a `cw` x `ch` canvas: crop and fit become
/// whole-pixel destination bounds and a source mapping. `None` when nothing
/// would be visible.
fn place(d: &Draw, tw: u32, th: u32, cw: u32, ch: u32) -> Option<GpuLayer> {
    let (tw, th, cw, ch) = (tw as f32, th as f32, cw as f32, ch as f32);
    let [l, t, r, b] = d.crop.map(|v| v.clamp(0.0, 1.0));
    let (mut sx0, mut sy0, mut sx1, mut sy1) = (l * tw, t * th, (1.0 - r) * tw, (1.0 - b) * th);
    let [x, y, w, h] = d.rect;
    let (mut dx0, mut dy0, mut dx1, mut dy1) = (x * cw, y * ch, (x + w) * cw, (y + h) * ch);
    let (sw, sh, dw, dh) = (sx1 - sx0, sy1 - sy0, dx1 - dx0, dy1 - dy0);
    let opacity = d.opacity.clamp(0.0, 1.0);
    let finite = d.rect.iter().chain(&d.crop).all(|v| v.is_finite()) && opacity.is_finite();
    // Written so NaN fails too.
    if !(finite && sw > 0.0 && sh > 0.0 && dw > 0.0 && dh > 0.0 && opacity > 0.0) {
        return None;
    }
    match d.fit {
        Fit::Contain => {
            let s = (dw / sw).min(dh / sh);
            dx0 += (dw - sw * s) / 2.0;
            dy0 += (dh - sh * s) / 2.0;
            (dx1, dy1) = (dx0 + sw * s, dy0 + sh * s);
        }
        Fit::Cover => {
            let s = (dw / sw).max(dh / sh);
            sx0 += (sw - dw / s) / 2.0;
            sy0 += (sh - dh / s) / 2.0;
            (sx1, sy1) = (sx0 + dw / s, sy0 + dh / s);
        }
        Fit::Stretch => {}
    }
    let [dx0, dy0, dx1, dy1] = [dx0, dy0, dx1, dy1].map(f32::round);
    if dx1 <= dx0 || dy1 <= dy0 {
        return None;
    }
    let (kx, ky) = ((sx1 - sx0) / (dx1 - dx0), (sy1 - sy0) / (dy1 - dy0));
    if ![dx0, dy0, dx1, dy1, sx0, sy0, kx, ky]
        .iter()
        .all(|v| v.is_finite())
    {
        return None;
    }
    let exact = kx == 1.0 && ky == 1.0 && sx0.fract() == 0.0 && sy0.fract() == 0.0;
    // The box filter reads the mip where a pixel spans 2 to 4 texels, or a
    // lower one where a crop edge would cut a mip texel: no cropped-away
    // content may enter the edge pixels.
    let mut level = (kx.min(ky).log2().floor() - 1.0).max(0.0);
    let cuts = |v: f32, size: f32, step: f32| v != 0.0 && v != size && (v / step).fract() != 0.0;
    while level > 0.0
        && [(sx0, tw), (sx1, tw), (sy0, th), (sy1, th)]
            .iter()
            .any(|&(v, size)| cuts(v, size, level.exp2()))
    {
        level -= 1.0;
    }
    Some(GpuLayer {
        dst: [dx0, dy0, dx1, dy1],
        src: [sx0, sy0, kx, ky],
        misc: [opacity, level, 1.0 / tw, 1.0 / th],
        flags: [if exact { FLAG_EXACT } else { 0 }, 0, 0, 0],
    })
}

/// Vulkan format and shader flags of a supported DRM fourcc.
fn import_format(fourcc_code: u32) -> Option<(vk::Format, u32)> {
    match &fourcc_code.to_le_bytes() {
        b"XR24" => Some((vk::Format::B8G8R8A8_UNORM, FLAG_NO_ALPHA)),
        b"AR24" => Some((vk::Format::B8G8R8A8_UNORM, 0)),
        b"XB24" => Some((vk::Format::R8G8B8A8_UNORM, FLAG_NO_ALPHA)),
        b"AB24" => Some((vk::Format::R8G8B8A8_UNORM, 0)),
        _ => None,
    }
}

/// Halves an RGBA image with a 2x2 box filter (edges clamped), for mipmaps.
/// Premultiplied alpha averages correctly as is.
fn half(src: &[u8], w: usize, h: usize) -> (Vec<u8>, usize, usize) {
    let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
    let mut out = vec![0; nw * nh * 4];
    let at =
        |x: usize, y: usize, c: usize| u32::from(src[(y.min(h - 1) * w + x.min(w - 1)) * 4 + c]);
    for y in 0..nh {
        for x in 0..nw {
            for c in 0..4 {
                let (sx, sy) = (2 * x, 2 * y);
                let s =
                    at(sx, sy, c) + at(sx + 1, sy, c) + at(sx, sy + 1, c) + at(sx + 1, sy + 1, c);
                out[(y * nw + x) * 4 + c] = ((s + 2) / 4) as u8;
            }
        }
    }
    (out, nw, nh)
}

/// `major:minor` of a device number, as glibc's `major()` / `minor()`.
fn dev_numbers(rdev: u64) -> (i64, i64) {
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff);
    let minor = (rdev & 0xff) | ((rdev >> 12) & !0xff);
    (major as i64, minor as i64)
}

/// The Vulkan instance and device, alive while the compositor or a staged
/// texture needs them.
struct Gpu {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    memory: vk::PhysicalDeviceMemoryProperties,
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

impl Gpu {
    /// A host-visible buffer, persistently mapped.
    unsafe fn buffer(
        &self,
        size: u64,
        usage: vk::BufferUsageFlags,
        cached: bool,
    ) -> Result<Buffer> {
        let d = &self.device;
        let mut b = Buffer::NULL;
        let r = unsafe {
            (|| {
                b.buffer = d
                    .create_buffer(
                        &vk::BufferCreateInfo::default()
                            .size(size)
                            .usage(usage)
                            .sharing_mode(vk::SharingMode::EXCLUSIVE),
                        None,
                    )
                    .map_err(err("create buffer"))?;
                let reqs = d.get_buffer_memory_requirements(b.buffer);
                let host =
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
                let type_index = cached
                    .then(|| {
                        memory_type(
                            &self.memory,
                            reqs.memory_type_bits,
                            host | vk::MemoryPropertyFlags::HOST_CACHED,
                        )
                    })
                    .flatten()
                    .or_else(|| memory_type(&self.memory, reqs.memory_type_bits, host))
                    .ok_or_else(|| Error("no host-visible memory".into()))?;
                b.memory = d
                    .allocate_memory(
                        &vk::MemoryAllocateInfo::default()
                            .allocation_size(reqs.size)
                            .memory_type_index(type_index),
                        None,
                    )
                    .map_err(err("allocate buffer memory"))?;
                d.bind_buffer_memory(b.buffer, b.memory, 0)
                    .map_err(err("bind buffer memory"))?;
                b.ptr = d
                    .map_memory(b.memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
                    .map_err(err("map buffer"))? as *mut u8;
                Ok(())
            })()
        };
        match r {
            Ok(()) => Ok(b),
            Err(e) => {
                unsafe { b.destroy(d) };
                Err(e)
            }
        }
    }
}

fn memory_type(
    memory: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    flags: vk::MemoryPropertyFlags,
) -> Option<u32> {
    let types = &memory.memory_types[..memory.memory_type_count as usize];
    (0..types.len() as u32)
        .find(|&i| bits & (1 << i) != 0 && types[i as usize].property_flags.contains(flags))
}

/// Prepares textures for one compositor on any thread, without its lock:
/// the CPU mip chain and the staging copy. Cheap to clone.
#[derive(Clone)]
pub struct Uploader {
    gpu: Arc<Gpu>,
    width: u32,
    height: u32,
}

// The Vulkan calls it makes (create, allocate, map) are thread-safe.
unsafe impl Send for Uploader {}
unsafe impl Sync for Uploader {}

/// A texture's pixels in a staging buffer, ready for
/// [`Compositor::set_staged`].
pub struct Staged {
    gpu: Arc<Gpu>,
    buffer: Buffer,
    width: u32,
    height: u32,
    levels: u32,
    regions: Vec<vk::BufferImageCopy>,
}

// The mapping is only written by `Uploader::prepare`.
unsafe impl Send for Staged {}

impl Drop for Staged {
    fn drop(&mut self) {
        unsafe { self.buffer.destroy(&self.gpu.device) };
    }
}

impl Uploader {
    /// Stages `width` x `height` premultiplied RGBA8 pixels with a full mip
    /// chain for downscaled draws. Levels at least twice the canvas size
    /// are dropped: they are never sampled. A few ms per megapixel.
    pub fn prepare(&self, width: u32, height: u32, rgba: &[u8]) -> Result<Staged> {
        if width == 0 || height == 0 || width > 16384 || height > 16384 {
            return Err(Error(format!("bad texture size {width}x{height}")));
        }
        if rgba.len() != width as usize * height as usize * 4 {
            return Err(Error(format!(
                "texture {width}x{height} needs {} bytes, got {}",
                width as usize * height as usize * 4,
                rgba.len()
            )));
        }
        // shortcut: a crop zooming into a huge image loses that detail; keep
        // more levels if the editor ever allows such zooms.
        let (mut w, mut h) = (width as usize, height as usize);
        let mut px = std::borrow::Cow::Borrowed(rgba);
        while w / 2 >= self.width as usize && h / 2 >= self.height as usize {
            let (p, nw, nh) = half(&px, w, h);
            (px, w, h) = (std::borrow::Cow::Owned(p), nw, nh);
        }
        let levels = usize::BITS - w.max(h).leading_zeros();
        let mut chain = vec![(px.into_owned(), w, h)];
        while chain.len() < levels as usize {
            let (p, w, h) = chain.last().expect("level 0 is there");
            chain.push(half(p, *w, *h));
        }
        let total: usize = chain.iter().map(|(p, _, _)| p.len()).sum();
        let buffer = unsafe {
            self.gpu
                .buffer(total as u64, vk::BufferUsageFlags::TRANSFER_SRC, false)?
        };
        let mut regions = Vec::new();
        let mut offset = 0;
        for (level, (p, w, h)) in chain.iter().enumerate() {
            unsafe { std::ptr::copy_nonoverlapping(p.as_ptr(), buffer.ptr.add(offset), p.len()) };
            regions.push(
                vk::BufferImageCopy::default()
                    .buffer_offset(offset as u64)
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .mip_level(level as u32)
                            .layer_count(1),
                    )
                    .image_extent(vk::Extent3D {
                        width: *w as u32,
                        height: *h as u32,
                        depth: 1,
                    }),
            );
            offset += p.len();
        }
        Ok(Staged {
            gpu: self.gpu.clone(),
            buffer,
            width: w as u32,
            height: h as u32,
            levels,
            regions,
        })
    }

    /// Stages a YUV camera picture for [`Compositor::set_camera_yuv`]: its
    /// rows packed into a GPU-readable buffer (one copy, about 0.3 ms per
    /// 1080p NV12 picture). The conversion to RGB runs on the GPU.
    pub fn prepare_yuv(&self, f: &YuvPicture<'_>) -> Result<StagedYuv> {
        let rows = yuv_rows(f)?;
        let total: usize = rows.iter().map(ExactSizeIterator::len).sum();
        // shortcut: one allocation per picture (30 per second); recycle the
        // buffers if this ever shows up in profiles.
        // The shader reads whole 32-bit words.
        let buffer = unsafe {
            self.gpu.buffer(
                total.next_multiple_of(4) as u64,
                vk::BufferUsageFlags::STORAGE_BUFFER,
                false,
            )?
        };
        let mut offset = 0;
        for r in rows {
            let row = &f.data[r];
            unsafe {
                std::ptr::copy_nonoverlapping(row.as_ptr(), buffer.ptr.add(offset), row.len())
            };
            offset += row.len();
        }
        Ok(StagedYuv {
            gpu: self.gpu.clone(),
            buffer,
            width: f.width,
            height: f.height,
            params: yuv_params(f.layout, f.matrix, f.full_range, f.width, f.height),
        })
    }
}

/// A camera's YUV picture in a GPU-readable buffer, ready for
/// [`Compositor::set_camera_yuv`].
pub struct StagedYuv {
    gpu: Arc<Gpu>,
    buffer: Buffer,
    width: u32,
    height: u32,
    params: CameraParams,
}

// The mapping is only written by `Uploader::prepare_yuv`.
unsafe impl Send for StagedYuv {}

impl StagedYuv {
    /// Staged for `c` (else [`Compositor::set_camera_yuv`] refuses it).
    pub fn is_for(&self, c: &Compositor) -> bool {
        Arc::ptr_eq(&self.gpu, &c.gpu)
    }
}

impl Drop for StagedYuv {
    fn drop(&mut self) {
        unsafe { self.buffer.destroy(&self.gpu.device) };
    }
}

/// A camera picture waiting for the next composition, then for the GPU to
/// be done with it.
enum Input {
    Frame(VideoFrame),
    Yuv(StagedYuv),
}

/// A camera: the copy of its latest picture, and its imported DMA-BUFs.
#[derive(Default)]
struct Camera {
    image: Option<Texture>,
    /// A copy was queued into `image`.
    valid: bool,
    imports: Imports,
    pending: Option<Input>,
}

/// The copy of one camera picture, as `record` queues it.
struct CameraPass {
    /// The imported DMA-BUF, acquired from its producer for the pass.
    import: Option<vk::Image>,
    image: vk::Image,
    width: u32,
    height: u32,
}

/// Imported DMA-BUFs of one producer pool.
#[derive(Default)]
struct Imports {
    generation: Option<u32>,
    list: Vec<Import>,
}

/// A copy of the last main frame kept on request (see
/// [`Compositor::submit`]), drawn in its place when a composition has none.
struct Base {
    image: Image,
    width: u32,
    height: u32,
    /// A copy was queued into it.
    valid: bool,
}

/// A queued composition.
struct InFlight {
    slot: usize,
    /// Released when the GPU is done with it.
    main: Option<VideoFrame>,
    /// Camera pictures copied by it, released with `main`.
    inputs: Vec<Input>,
    preview: bool,
    since: Instant,
}

struct Image {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
}

struct Buffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    ptr: *mut u8,
}

/// A main frame imported from its DMA-BUF, kept while its producer's pool lives.
struct Import {
    buffer_id: u32,
    /// Size, fourcc, modifier, plane 0 offset and stride, and the DMA-BUF's
    /// inode: a recycled (buffer_id, generation) on another buffer never hits.
    key: (u32, u32, u32, u64, u32, u32, u64),
    image: Image,
}

/// Inode of a DMA-BUF: unique per buffer while it lives.
fn inode(fd: &OwnedFd) -> Result<u64> {
    let mut st = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(Error(format!(
            "stat DMA-BUF: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(st.st_ino)
}

struct Texture {
    image: Image,
    width: u32,
    height: u32,
}

struct Slot {
    image: Image,
    fd: OwnedFd,
    offset: u32,
    stride: u32,
    /// Set while the frame is out; its `ReleaseGuard` clears it.
    busy: Arc<AtomicBool>,
}

struct Preview {
    image: Image,
    buffers: [Buffer; 2],
    /// Buffer the next preview pass writes.
    next: usize,
    /// Buffer holding a finished preview not handed out yet.
    ready: Option<usize>,
    wanted: bool,
}

/// Composes frames on the discrete GPU. Not thread-safe: one owner calls it.
pub struct Compositor {
    width: u32,
    height: u32,
    instance: ash::Instance,
    device: ash::Device,
    memory_fd: ash::khr::external_memory_fd::Device,
    semaphore_fd: ash::khr::external_semaphore_fd::Device,
    /// Waited by a compose: takes the main frame's pending writes (its
    /// implicit fence), which Vulkan on RADV does not wait for by itself.
    /// Index 0 for the main frame, 1 + k for camera pass k.
    acquire: [vk::Semaphore; 1 + MAX_CAMERAS],
    memory: vk::PhysicalDeviceMemoryProperties,
    physical: vk::PhysicalDevice,
    /// DRM modifiers the device supports per import format.
    modifiers: HashMap<vk::Format, Vec<vk::DrmFormatModifierPropertiesEXT>>,
    queue: vk::Queue,
    family: u32,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    /// Texture copies: their own command buffer and fence, so they queue
    /// while a composition is in flight.
    upload_cmd: vk::CommandBuffer,
    upload_fence: vk::Fence,
    /// The staging buffer of the last queued copy, freed once it is done.
    upload_pending: Option<Staged>,
    /// Images freed once no queued work may read them.
    graveyard: Vec<Image>,
    base: Option<Base>,
    in_flight: Option<InFlight>,
    queries: Option<(vk::QueryPool, f32)>,
    gpu_time: Option<Duration>,
    sampler: vk::Sampler,
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    descriptor_pool: vk::DescriptorPool,
    /// Compose pass, preview pass, base copy pass.
    sets: [vk::DescriptorSet; 3],
    params: [Buffer; 3],
    dummy: Texture,
    textures: HashMap<u32, Texture>,
    /// The main frame's.
    imports: Imports,
    /// Camera copy passes: one descriptor set and parameter buffer per
    /// camera copied in a composition.
    camera_set_layout: vk::DescriptorSetLayout,
    camera_pipeline_layout: vk::PipelineLayout,
    camera_pipeline: vk::Pipeline,
    camera_sets: [vk::DescriptorSet; MAX_CAMERAS],
    camera_params: [Buffer; MAX_CAMERAS],
    /// Bound as the YUV buffer of RGB copies.
    dummy_storage: Buffer,
    cameras: HashMap<u32, Camera>,
    /// Cameras whose picture could not be used, and why.
    camera_failures: Vec<(u32, String)>,
    ring: Vec<Slot>,
    generation: u32,
    preview: Option<Preview>,
    /// A fence wait timed out: the GPU state is unknown, refuse further work.
    lost: bool,
    /// Last field: the device outlives every handle above.
    gpu: Arc<Gpu>,
}

// Vulkan handles are externally synchronised; `&mut self` provides that.
// The raw pointers are persistent mappings owned by the compositor.
unsafe impl Send for Compositor {}

impl Compositor {
    /// Opens the discrete GPU and allocates a `width` x `height` output ring.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 || width > 16384 || height > 16384 {
            return Err(Error(format!("bad canvas size {width}x{height}")));
        }
        let entry =
            unsafe { ash::Entry::load() }.map_err(|e| Error(format!("load Vulkan: {e}")))?;
        let app = vk::ApplicationInfo::default()
            .application_name(c"vesper")
            .api_version(vk::API_VERSION_1_3);
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app),
                None,
            )
        }
        .map_err(err("create instance"))?;
        let (device, physical, family, timestamps) = match unsafe { create_device(&instance) } {
            Ok(d) => d,
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                return Err(e);
            }
        };
        let memory = unsafe { instance.get_physical_device_memory_properties(physical) };
        // From here `Gpu` frees the device and instance, `Drop` the rest.
        let gpu = Arc::new(Gpu {
            _entry: entry,
            instance: instance.clone(),
            device: device.clone(),
            memory,
        });
        let mut c = unsafe {
            Self {
                width,
                height,
                memory_fd: ash::khr::external_memory_fd::Device::new(&instance, &device),
                semaphore_fd: ash::khr::external_semaphore_fd::Device::new(&instance, &device),
                acquire: [vk::Semaphore::null(); 1 + MAX_CAMERAS],
                memory,
                physical,
                modifiers: HashMap::new(),
                queue: device.get_device_queue(family, 0),
                instance,
                device,
                family,
                pool: vk::CommandPool::null(),
                cmd: vk::CommandBuffer::null(),
                fence: vk::Fence::null(),
                upload_cmd: vk::CommandBuffer::null(),
                upload_fence: vk::Fence::null(),
                upload_pending: None,
                graveyard: Vec::new(),
                base: None,
                in_flight: None,
                queries: timestamps.map(|period| (vk::QueryPool::null(), period)),
                gpu_time: None,
                sampler: vk::Sampler::null(),
                set_layout: vk::DescriptorSetLayout::null(),
                pipeline_layout: vk::PipelineLayout::null(),
                pipeline: vk::Pipeline::null(),
                descriptor_pool: vk::DescriptorPool::null(),
                sets: [vk::DescriptorSet::null(); 3],
                params: [Buffer::NULL, Buffer::NULL, Buffer::NULL],
                dummy: Texture {
                    image: Image::NULL,
                    width: 1,
                    height: 1,
                },
                textures: HashMap::new(),
                imports: Imports::default(),
                camera_set_layout: vk::DescriptorSetLayout::null(),
                camera_pipeline_layout: vk::PipelineLayout::null(),
                camera_pipeline: vk::Pipeline::null(),
                camera_sets: [vk::DescriptorSet::null(); MAX_CAMERAS],
                camera_params: [const { Buffer::NULL }; MAX_CAMERAS],
                dummy_storage: Buffer::NULL,
                cameras: HashMap::new(),
                camera_failures: Vec::new(),
                ring: Vec::new(),
                // Unique in the process, so an encoder never reuses an import
                // from another pool.
                generation: vesper_core::next_pool_generation(),
                preview: None,
                lost: false,
                gpu,
            }
        };
        unsafe { c.init()? };
        Ok(c)
    }
    unsafe fn init(&mut self) -> Result<()> {
        unsafe {
            let d = &self.device;
            self.pool = d
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(self.family)
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                    None,
                )
                .map_err(err("create command pool"))?;
            let cmds = d
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(self.pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(2),
                )
                .map_err(err("allocate command buffer"))?;
            (self.cmd, self.upload_cmd) = (cmds[0], cmds[1]);
            for sem in &mut self.acquire {
                *sem = d
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
                    .map_err(err("create semaphore"))?;
            }
            self.fence = d
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .map_err(err("create fence"))?;
            self.upload_fence = d
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .map_err(err("create fence"))?;
            if let Some((pool, _)) = &mut self.queries {
                *pool = d
                    .create_query_pool(
                        &vk::QueryPoolCreateInfo::default()
                            .query_type(vk::QueryType::TIMESTAMP)
                            .query_count(2),
                        None,
                    )
                    .map_err(err("create query pool"))?;
            }
            self.sampler = d
                .create_sampler(
                    &vk::SamplerCreateInfo::default()
                        .mag_filter(vk::Filter::LINEAR)
                        .min_filter(vk::Filter::LINEAR)
                        .mipmap_mode(vk::SamplerMipmapMode::LINEAR)
                        .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .max_lod(vk::LOD_CLAMP_NONE),
                    None,
                )
                .map_err(err("create sampler"))?;
            let bindings = [
                vk::DescriptorSetLayoutBinding::default()
                    .binding(0)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(2)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(MAX_LAYERS as u32)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
            ];
            self.set_layout = d
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                    None,
                )
                .map_err(err("create descriptor set layout"))?;
            self.pipeline_layout = d
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default().set_layouts(&[self.set_layout]),
                    None,
                )
                .map_err(err("create pipeline layout"))?;
            self.pipeline = self.compute_pipeline(SPIRV, self.pipeline_layout)?;
            let camera_bindings = [
                vk::DescriptorSetLayoutBinding::default()
                    .binding(0)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(2)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(3)
                    .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
            ];
            self.camera_set_layout = d
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&camera_bindings),
                    None,
                )
                .map_err(err("create descriptor set layout"))?;
            self.camera_pipeline_layout = d
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default().set_layouts(&[self.camera_set_layout]),
                    None,
                )
                .map_err(err("create pipeline layout"))?;
            self.camera_pipeline =
                self.compute_pipeline(CAMERA_SPIRV, self.camera_pipeline_layout)?;
            let cams = MAX_CAMERAS as u32;
            let sizes = [
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::UNIFORM_BUFFER,
                    descriptor_count: 3 + cams,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::STORAGE_IMAGE,
                    descriptor_count: 3 + cams,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                    descriptor_count: 3 * MAX_LAYERS as u32 + cams,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::STORAGE_BUFFER,
                    descriptor_count: cams,
                },
            ];
            self.descriptor_pool = d
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(3 + cams)
                        .pool_sizes(&sizes),
                    None,
                )
                .map_err(err("create descriptor pool"))?;
            let sets = d
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(self.descriptor_pool)
                        .set_layouts(&[self.set_layout; 3]),
                )
                .map_err(err("allocate descriptor sets"))?;
            self.sets = [sets[0], sets[1], sets[2]];
            for i in 0..3 {
                self.params[i] = self.buffer(
                    size_of::<Params>() as u64,
                    vk::BufferUsageFlags::UNIFORM_BUFFER,
                    false,
                )?;
                let info = [vk::DescriptorBufferInfo::default()
                    .buffer(self.params[i].buffer)
                    .range(vk::WHOLE_SIZE)];
                self.device.update_descriptor_sets(
                    &[vk::WriteDescriptorSet::default()
                        .dst_set(self.sets[i])
                        .dst_binding(0)
                        .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                        .buffer_info(&info)],
                    &[],
                );
            }
            let sets = self
                .device
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(self.descriptor_pool)
                        .set_layouts(&[self.camera_set_layout; MAX_CAMERAS]),
                )
                .map_err(err("allocate descriptor sets"))?;
            self.camera_sets.copy_from_slice(&sets);
            for i in 0..MAX_CAMERAS {
                self.camera_params[i] = self.buffer(
                    size_of::<CameraParams>() as u64,
                    vk::BufferUsageFlags::UNIFORM_BUFFER,
                    false,
                )?;
                let info = [vk::DescriptorBufferInfo::default()
                    .buffer(self.camera_params[i].buffer)
                    .range(vk::WHOLE_SIZE)];
                self.device.update_descriptor_sets(
                    &[vk::WriteDescriptorSet::default()
                        .dst_set(self.camera_sets[i])
                        .dst_binding(0)
                        .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                        .buffer_info(&info)],
                    &[],
                );
            }
            self.dummy_storage = self.buffer(4, vk::BufferUsageFlags::STORAGE_BUFFER, false)?;
            let staged = self.uploader().prepare(1, 1, &[0; 4])?;
            self.dummy = self.upload(staged)?;
            self.wait_upload()?;
            for _ in 0..RING {
                let slot = self.create_slot(self.width, self.height)?;
                self.ring.push(slot);
            }
            Ok(())
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// GPU time of the last composition (timestamp queries), preview pass
    /// included, when the queue supports them.
    pub fn gpu_time(&self) -> Option<Duration> {
        self.gpu_time
    }

    /// Prepares textures for this compositor on other threads.
    pub fn uploader(&self) -> Uploader {
        Uploader {
            gpu: self.gpu.clone(),
            width: self.width,
            height: self.height,
        }
    }

    /// Sets (or replaces) texture `id` from `width` x `height` premultiplied
    /// RGBA8 pixels and waits for the copy: [`Uploader::prepare`] then
    /// [`Compositor::set_staged`], on this thread.
    pub fn set_texture(&mut self, id: u32, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
        self.check_alive()?;
        let staged = self.uploader().prepare(width, height, rgba)?;
        self.set_staged(id, staged)?;
        self.wait_upload()
    }

    /// Sets (or replaces) texture `id`: queues the GPU copy of `staged`
    /// and returns. Compositions queued after it draw the new texture. Waits
    /// only for the previous copy, if still running.
    pub fn set_staged(&mut self, id: u32, staged: Staged) -> Result<()> {
        self.check_alive()?;
        if !Arc::ptr_eq(&staged.gpu, &self.gpu) {
            return Err(Error("staged for another compositor".into()));
        }
        self.wait_upload()?;
        let texture = unsafe { self.upload(staged)? };
        if let Some(old) = self.textures.insert(id, texture) {
            self.graveyard.push(old.image);
        }
        self.collect_garbage();
        Ok(())
    }

    /// Frees texture `id` (once no queued work reads it). Draws of it are
    /// skipped from then on.
    pub fn remove_texture(&mut self, id: u32) {
        if let Some(old) = self.textures.remove(&id) {
            self.graveyard.push(old.image);
        }
        self.collect_garbage();
    }

    /// Camera `id`'s latest picture, an RGB DMA-BUF (`XR24`, `AR24`, `XB24`,
    /// `AB24`) the producer finished writing: the next composition copies
    /// it into the compositor's own image, then releases it. A newer picture
    /// set before then replaces (releases) this one. Fails past
    /// [`MAX_CAMERAS`] cameras. A picture that cannot be imported hides the
    /// camera ([`Compositor::take_camera_failures`]), the composition goes on.
    pub fn set_camera_frame(&mut self, id: u32, frame: VideoFrame) -> Result<()> {
        self.set_camera(id, Input::Frame(frame))
    }

    /// Camera `id`'s latest picture, YUV staged by [`Uploader::prepare_yuv`];
    /// as [`Compositor::set_camera_frame`].
    pub fn set_camera_yuv(&mut self, id: u32, staged: StagedYuv) -> Result<()> {
        if !Arc::ptr_eq(&staged.gpu, &self.gpu) {
            return Err(Error("staged for another compositor".into()));
        }
        self.set_camera(id, Input::Yuv(staged))
    }

    fn set_camera(&mut self, id: u32, input: Input) -> Result<()> {
        self.check_alive()?;
        if !self.cameras.contains_key(&id) && self.cameras.len() >= MAX_CAMERAS {
            return Err(Error(format!("more than {MAX_CAMERAS} cameras")));
        }
        // An older picture not copied yet goes back now.
        self.cameras.entry(id).or_default().pending = Some(input);
        Ok(())
    }

    /// Forgets camera `id`: its draws are skipped until it has a picture
    /// again, its images are freed once no queued work reads them.
    pub fn remove_camera(&mut self, id: u32) {
        if let Some(c) = self.cameras.remove(&id) {
            self.graveyard.extend(c.image.map(|t| t.image));
            self.graveyard
                .extend(c.imports.list.into_iter().map(|i| i.image));
        }
        self.collect_garbage();
    }

    /// Cameras hidden since the last call because their picture could not
    /// be used, with the reason.
    pub fn take_camera_failures(&mut self) -> Vec<(u32, String)> {
        std::mem::take(&mut self.camera_failures)
    }

    /// Waits for the last texture copy (at most the GPU timeout) and frees
    /// its staging buffer.
    fn wait_upload(&mut self) -> Result<()> {
        if self.upload_pending.is_none() {
            return Ok(());
        }
        let d = &self.device;
        let r = unsafe {
            d.wait_for_fences(&[self.upload_fence], true, FENCE_TIMEOUT.as_nanos() as u64)
        };
        if let Err(e) = r {
            self.lost = true;
            return Err(Error(format!("wait for a texture copy: {e}")));
        }
        unsafe { d.reset_fences(&[self.upload_fence]) }.map_err(err("reset fence"))?;
        self.upload_pending = None;
        Ok(())
    }

    /// Tests: images waiting to be freed.
    #[doc(hidden)]
    pub fn garbage(&self) -> usize {
        self.graveyard.len()
    }

    /// Tests: the last texture copy is done (or none was queued).
    #[doc(hidden)]
    pub fn copies_done(&self) -> bool {
        self.upload_pending.is_none()
            || unsafe { self.device.get_fence_status(self.upload_fence) } == Ok(true)
    }

    /// Frees replaced images once no queued work uses them: no composition
    /// in flight, and the last texture copy (which may write one) done.
    fn collect_garbage(&mut self) {
        if self.in_flight.is_some() || self.graveyard.is_empty() {
            return;
        }
        if self.upload_pending.is_some() {
            let d = &self.device;
            if unsafe { d.get_fence_status(self.upload_fence) } != Ok(true)
                || unsafe { d.reset_fences(&[self.upload_fence]) }.is_err()
            {
                return;
            }
            self.upload_pending = None;
        }
        for i in self.graveyard.drain(..) {
            unsafe { i.destroy(&self.device) };
        }
    }

    /// Requests a [`PREVIEW_WIDTH`] x [`PREVIEW_HEIGHT`] RGBA8 downscale of
    /// the next composition and returns the latest finished one not returned
    /// yet. Never waits for the GPU; the small extra pass runs inside the
    /// next `compose` only after a request. A box-filtered downscale of the
    /// canvas.
    pub fn preview(&mut self) -> Option<&[u8]> {
        if self.lost {
            return None;
        }
        if self.preview.is_none() {
            match unsafe { self.create_preview() } {
                Ok(p) => self.preview = Some(p),
                Err(e) => {
                    tracing::warn!("compositor: no preview: {e}");
                    return None;
                }
            }
        }
        let p = self.preview.as_mut()?;
        p.wanted = true;
        let i = p.ready.take()?;
        let len = (PREVIEW_WIDTH * PREVIEW_HEIGHT * 4) as usize;
        Some(unsafe { std::slice::from_raw_parts(p.buffers[i].ptr, len) })
    }

    /// Composes `draws` over black and waits for it: [`Compositor::submit`]
    /// then [`Compositor::poll`] up to the GPU timeout. For tests and tools:
    /// a frame clock should not wait.
    pub fn compose(&mut self, main: Option<VideoFrame>, draws: &[Draw]) -> Result<VideoFrame> {
        self.submit(main, draws, false)?;
        match self.poll(FENCE_TIMEOUT)? {
            Some(f) => Ok(f),
            None => {
                self.lost = true;
                Err(Error("the GPU stopped answering".into()))
            }
        }
    }

    /// Whether a composition is queued and not polled out yet.
    pub fn in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Whether a copy of a main frame is kept (see `keep_base`).
    pub fn has_base(&self) -> bool {
        self.base.as_ref().is_some_and(|b| b.valid)
    }

    /// Queues the composition of `draws` over black into the next free ring
    /// image; [`Compositor::poll`] returns it. `main` is released once the
    /// GPU has read it. Draws of a missing source are skipped. Without
    /// `main`, `Source::Main` draws the kept copy of an earlier one, if any
    /// (else the previous producer's imports are freed). `keep_base`: also
    /// copy `main` for that (one more pass over it). Fails while another
    /// composition is in flight.
    pub fn submit(
        &mut self,
        main: Option<VideoFrame>,
        draws: &[Draw],
        keep_base: bool,
    ) -> Result<()> {
        self.check_alive()?;
        if self.in_flight.is_some() {
            return Err(Error("a composition is already in flight".into()));
        }
        if draws.len() > MAX_LAYERS {
            return Err(Error(format!(
                "{} layers, at most {MAX_LAYERS}",
                draws.len()
            )));
        }
        self.collect_garbage();
        let slot = self
            .ring
            .iter()
            .position(|s| !s.busy.load(Ordering::Acquire))
            .ok_or_else(|| Error("every composed frame is still in use".into()))?;
        let main_import = match &main {
            Some(f) => Some(unsafe { self.import(f)? }),
            None => {
                // No main (e.g. the game quit) and no copy to stand in:
                // free its pool's images now rather than pin them.
                if !self.has_base() {
                    self.drop_imports();
                }
                None
            }
        };
        let base_pass = match (&main, keep_base) {
            (Some(f), true) => {
                unsafe { self.ensure_base(f.width, f.height)? };
                true
            }
            _ => false,
        };
        // Camera pictures new since the last composition: copied first.
        let mut passes = Vec::new();
        let mut inputs = Vec::new();
        let mut copied = Vec::new();
        let ids: Vec<u32> = self
            .cameras
            .iter()
            .filter(|(_, c)| c.pending.is_some())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let Some(input) = self.cameras.get_mut(&id).and_then(|c| c.pending.take()) else {
                continue;
            };
            match unsafe { self.camera_pass(id, &input, passes.len()) } {
                Ok(pass) => {
                    passes.push(pass);
                    inputs.push(input);
                    copied.push(id);
                }
                Err(e) => {
                    tracing::warn!("compositor: camera {id} hidden: {e}");
                    self.camera_failures.push((id, e.to_string()));
                    // Its picture goes back with it.
                    drop(input);
                    self.remove_camera(id);
                }
            }
        }

        let mut params = Params {
            head: [0, 1, 0, 0],
            layers: [GpuLayer::default(); MAX_LAYERS],
        };
        let mut views = [self.dummy.image.view; MAX_LAYERS];
        let mut n = 0;
        for d in draws {
            let (view, w, h, flags) = match d.source {
                Source::Main => match (main_import, &main, &self.base) {
                    (Some(i), Some(f), _) => {
                        let flags = import_format(f.fourcc).map_or(0, |(_, fl)| fl);
                        (self.imports.list[i].image.view, f.width, f.height, flags)
                    }
                    (None, _, Some(b)) if b.valid => {
                        (b.image.view, b.width, b.height, FLAG_NO_ALPHA)
                    }
                    _ => continue,
                },
                Source::Texture(id) => match self.textures.get(&id) {
                    Some(t) => (t.image.view, t.width, t.height, 0),
                    None => continue,
                },
                Source::Camera(id) => match self
                    .cameras
                    .get(&id)
                    .filter(|c| c.valid || copied.contains(&id))
                {
                    Some(Camera { image: Some(t), .. }) => (t.image.view, t.width, t.height, 0),
                    _ => continue,
                },
            };
            if let Some(mut l) = place(d, w, h, self.width, self.height) {
                l.flags[0] |= flags;
                params.layers[n] = l;
                views[n] = view;
                n += 1;
            }
        }
        params.head[0] = n as u32;

        let want_preview = self.preview.as_ref().is_some_and(|p| p.wanted);
        unsafe {
            (self.params[0].ptr as *mut Params).write(params);
            self.bind(
                0,
                self.ring[slot].image.view,
                &views,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            );
            if want_preview {
                let p = self.preview.as_ref().expect("checked above");
                let whole = Draw {
                    source: Source::Main,
                    rect: [0.0, 0.0, 1.0, 1.0],
                    crop: [0.0; 4],
                    fit: Fit::Contain,
                    opacity: 1.0,
                };
                let mut l = place(
                    &whole,
                    self.width,
                    self.height,
                    PREVIEW_WIDTH,
                    PREVIEW_HEIGHT,
                )
                .expect("the canvas is not empty");
                // The ring holds BGRX bytes in an RGBA image.
                l.flags[0] |= FLAG_SWAP_RB | FLAG_NO_ALPHA;
                let mut pp = Params {
                    head: [1, 0, 0, 0],
                    layers: [GpuLayer::default(); MAX_LAYERS],
                };
                pp.layers[0] = l;
                (self.params[1].ptr as *mut Params).write(pp);
                let mut pv = [self.dummy.image.view; MAX_LAYERS];
                pv[0] = self.ring[slot].image.view;
                // The ring image stays in GENERAL while sampled.
                self.bind(1, p.image.view, &pv, vk::ImageLayout::GENERAL);
            }
            if base_pass && let (Some(i), Some(f), Some(b)) = (main_import, &main, &self.base) {
                // A 1:1 fetch of `main` into the RGBA copy (alpha dropped).
                let whole = Draw {
                    source: Source::Main,
                    rect: [0.0, 0.0, 1.0, 1.0],
                    crop: [0.0; 4],
                    fit: Fit::Stretch,
                    opacity: 1.0,
                };
                let mut l = place(&whole, f.width, f.height, b.width, b.height)
                    .expect("the main frame is not empty");
                l.flags[0] |= import_format(f.fourcc).map_or(0, |(_, fl)| fl);
                let mut bp = Params {
                    head: [1, 0, 0, 0],
                    layers: [GpuLayer::default(); MAX_LAYERS],
                };
                bp.layers[0] = l;
                (self.params[2].ptr as *mut Params).write(bp);
                let mut bv = [self.dummy.image.view; MAX_LAYERS];
                bv[0] = self.imports.list[i].image.view;
                self.bind(
                    2,
                    b.image.view,
                    &bv,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                );
            }
            // The producers' pending writes (their implicit fences): the
            // main frame's, and the camera pictures' (a VA-VPP picture may
            // still be drawn when handed over).
            let mut wait = Vec::new();
            if let Some(f) = &main
                && self.import_implicit_fence(f, self.acquire[0])
            {
                wait.push(self.acquire[0]);
            }
            let frames = inputs.iter().enumerate().filter_map(|(k, i)| match i {
                Input::Frame(f) => Some((k, f)),
                Input::Yuv(_) => None,
            });
            for (k, f) in frames {
                if self.import_implicit_fence(f, self.acquire[1 + k]) {
                    wait.push(self.acquire[1 + k]);
                }
            }
            let base = base_pass
                .then(|| {
                    self.base
                        .as_ref()
                        .map(|b| (b.image.image, b.width, b.height))
                })
                .flatten();
            self.record(slot, main_import, want_preview, base, &passes)?;
            self.submit_compose(&wait)?;
        }
        if base_pass && let Some(b) = &mut self.base {
            b.valid = true;
        }
        for id in &copied {
            if let Some(c) = self.cameras.get_mut(id) {
                c.valid = true;
            }
        }
        self.in_flight = Some(InFlight {
            slot,
            main,
            inputs,
            preview: want_preview,
            since: Instant::now(),
        });
        Ok(())
    }

    /// The queued composition's frame once the GPU is done, waiting at most
    /// `wait` for it; `None` if not done yet. Past the GPU timeout since it
    /// was queued, the compositor is lost.
    pub fn poll(&mut self, wait: Duration) -> Result<Option<VideoFrame>> {
        self.check_alive()?;
        let Some(f) = &self.in_flight else {
            return Err(Error("no composition in flight".into()));
        };
        let since = f.since;
        let r = unsafe {
            self.device.wait_for_fences(
                &[self.fence],
                true,
                wait.as_nanos().min(u128::from(u64::MAX)) as u64,
            )
        };
        match r {
            Ok(()) => {}
            Err(vk::Result::TIMEOUT) if since.elapsed() < FENCE_TIMEOUT => return Ok(None),
            Err(e) => {
                self.lost = true;
                return Err(Error(format!("wait for the GPU: {e}")));
            }
        }
        unsafe { self.device.reset_fences(&[self.fence]) }.map_err(err("reset fence"))?;
        let done = self.in_flight.take().expect("checked above");
        // The GPU has read it: hand the producer its buffer back now.
        let pts = done.main.as_ref().map_or(Duration::ZERO, |f| f.pts);
        drop(done.main);
        drop(done.inputs);
        self.collect_garbage();

        if let Some((pool, period)) = self.queries {
            let mut ts = [0u64; 2];
            let r = unsafe {
                self.device
                    .get_query_pool_results(pool, 0, &mut ts, vk::QueryResultFlags::TYPE_64)
            };
            self.gpu_time = r.ok().map(|()| {
                Duration::from_nanos(
                    (ts[1].saturating_sub(ts[0]) as f64 * f64::from(period)) as u64,
                )
            });
        }
        if done.preview
            && let Some(p) = &mut self.preview
        {
            p.ready = Some(p.next);
            p.next ^= 1;
            p.wanted = false;
        }

        let s = &self.ring[done.slot];
        let fd =
            s.fd.try_clone()
                .map_err(|e| Error(format!("dup output fd: {e}")))?;
        s.busy.store(true, Ordering::Release);
        let busy = s.busy.clone();
        Ok(Some(VideoFrame {
            width: self.width,
            height: self.height,
            fourcc: OUTPUT_FOURCC,
            modifier: DRM_FORMAT_MOD_LINEAR,
            planes: vec![DmaBufPlane {
                fd,
                offset: s.offset,
                stride: s.stride,
            }],
            buffer_id: done.slot as u32,
            pool_generation: self.generation,
            pts,
            release: ReleaseGuard::new(move || busy.store(false, Ordering::Release)),
        }))
    }

    /// The kept copy of main frames, `width` x `height`.
    unsafe fn ensure_base(&mut self, width: u32, height: u32) -> Result<()> {
        if self
            .base
            .as_ref()
            .is_some_and(|b| (b.width, b.height) == (width, height))
        {
            return Ok(());
        }
        if let Some(old) = self.base.take() {
            self.graveyard.push(old.image);
        }
        let ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { self.image(&ci, false)? };
        self.base = Some(Base {
            image,
            width,
            height,
            valid: false,
        });
        Ok(())
    }

    /// Frees the kept copy of main frames.
    pub fn drop_base(&mut self) {
        if let Some(old) = self.base.take() {
            self.graveyard.push(old.image);
        }
        self.collect_garbage();
    }

    /// Frees every imported main frame (once no queued work reads them).
    pub fn drop_imports(&mut self) {
        for i in self.imports.list.drain(..) {
            self.graveyard.push(i.image);
        }
        self.imports.generation = None;
        self.collect_garbage();
    }

    /// True once a composition timed out (the GPU hung or was reset). Every
    /// call fails from then on: drop this compositor and create another.
    pub fn is_lost(&self) -> bool {
        self.lost
    }

    fn check_alive(&self) -> Result<()> {
        if self.lost {
            return Err(Error("the GPU stopped answering".into()));
        }
        Ok(())
    }

    /// Points descriptor set `set` at its output image and textures. The
    /// first texture is in `first_layout`, the others in SHADER_READ_ONLY.
    unsafe fn bind(
        &self,
        set: usize,
        output: vk::ImageView,
        views: &[vk::ImageView; MAX_LAYERS],
        first_layout: vk::ImageLayout,
    ) {
        let out = [vk::DescriptorImageInfo::default()
            .image_view(output)
            .image_layout(vk::ImageLayout::GENERAL)];
        let mut tex = views.map(|v| {
            vk::DescriptorImageInfo::default()
                .sampler(self.sampler)
                .image_view(v)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        });
        tex[0].image_layout = first_layout;
        unsafe {
            self.device.update_descriptor_sets(
                &[
                    vk::WriteDescriptorSet::default()
                        .dst_set(self.sets[set])
                        .dst_binding(1)
                        .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                        .image_info(&out),
                    vk::WriteDescriptorSet::default()
                        .dst_set(self.sets[set])
                        .dst_binding(2)
                        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .image_info(&tex),
                ],
                &[],
            );
        }
    }

    unsafe fn record(
        &self,
        slot: usize,
        main: Option<usize>,
        preview: bool,
        base: Option<(vk::Image, u32, u32)>,
        cameras: &[CameraPass],
    ) -> Result<()> {
        let d = &self.device;
        let cmd = self.cmd;
        let color = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let barrier = |image, old, new, src_access, dst_access, src_family, dst_family| {
            vk::ImageMemoryBarrier::default()
                .image(image)
                .old_layout(old)
                .new_layout(new)
                .src_access_mask(src_access)
                .dst_access_mask(dst_access)
                .src_queue_family_index(src_family)
                .dst_queue_family_index(dst_family)
                .subresource_range(color)
        };
        let ignored = vk::QUEUE_FAMILY_IGNORED;
        let foreign = vk::QUEUE_FAMILY_FOREIGN_EXT;
        let out = self.ring[slot].image.image;
        let stage = vk::PipelineStageFlags::COMPUTE_SHADER;
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .map_err(err("reset command buffer"))?;
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(err("begin command buffer"))?;
            if let Some((pool, _)) = self.queries {
                d.cmd_reset_query_pool(cmd, pool, 0, 2);
                d.cmd_write_timestamp(cmd, vk::PipelineStageFlags::TOP_OF_PIPE, pool, 0);
            }
            // The whole image is rewritten: its old content can go.
            let mut acquire = vec![barrier(
                out,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::GENERAL,
                vk::AccessFlags::empty(),
                vk::AccessFlags::SHADER_WRITE,
                ignored,
                ignored,
            )];
            // External images live in GENERAL; acquiring from the foreign
            // family keeps compression metadata (DCC) valid.
            if let Some(i) = main {
                acquire.push(barrier(
                    self.imports.list[i].image.image,
                    vk::ImageLayout::GENERAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::SHADER_READ,
                    foreign,
                    self.family,
                ));
            }
            // The copy is rewritten whole; earlier passes sampling it are
            // earlier on this queue (execution dependency below).
            if let Some((image, _, _)) = base {
                acquire.push(barrier(
                    image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::GENERAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::SHADER_WRITE,
                    ignored,
                    ignored,
                ));
            }
            // Camera copies: their whole image is rewritten (earlier passes
            // sampling it are earlier on this queue).
            for c in cameras {
                if let Some(import) = c.import {
                    acquire.push(barrier(
                        import,
                        vk::ImageLayout::GENERAL,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        vk::AccessFlags::empty(),
                        vk::AccessFlags::SHADER_READ,
                        foreign,
                        self.family,
                    ));
                }
                acquire.push(barrier(
                    c.image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::GENERAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::SHADER_WRITE,
                    ignored,
                    ignored,
                ));
            }
            // Source stage = the semaphore's wait stage: the layout
            // transitions run after the implicit fence.
            d.cmd_pipeline_barrier(
                cmd,
                stage,
                stage,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &acquire,
            );
            if !cameras.is_empty() {
                d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.camera_pipeline);
                for (k, c) in cameras.iter().enumerate() {
                    d.cmd_bind_descriptor_sets(
                        cmd,
                        vk::PipelineBindPoint::COMPUTE,
                        self.camera_pipeline_layout,
                        0,
                        &[self.camera_sets[k]],
                        &[],
                    );
                    d.cmd_dispatch(cmd, c.width.div_ceil(16), c.height.div_ceil(16), 1);
                }
                let written: Vec<_> = cameras
                    .iter()
                    .map(|c| {
                        barrier(
                            c.image,
                            vk::ImageLayout::GENERAL,
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            vk::AccessFlags::SHADER_WRITE,
                            vk::AccessFlags::SHADER_READ,
                            ignored,
                            ignored,
                        )
                    })
                    .collect();
                d.cmd_pipeline_barrier(
                    cmd,
                    stage,
                    stage,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &written,
                );
            }
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            d.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.pipeline_layout,
                0,
                &[self.sets[0]],
                &[],
            );
            d.cmd_dispatch(cmd, self.width.div_ceil(16), self.height.div_ceil(16), 1);
            if let Some((image, w, h)) = base {
                d.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::COMPUTE,
                    self.pipeline_layout,
                    0,
                    &[self.sets[2]],
                    &[],
                );
                d.cmd_dispatch(cmd, w.div_ceil(16), h.div_ceil(16), 1);
                d.cmd_pipeline_barrier(
                    cmd,
                    stage,
                    stage,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[barrier(
                        image,
                        vk::ImageLayout::GENERAL,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        vk::AccessFlags::SHADER_WRITE,
                        vk::AccessFlags::SHADER_READ,
                        ignored,
                        ignored,
                    )],
                );
            }
            if let Some(p) = self.preview.as_ref().filter(|_| preview) {
                let to_read = [
                    barrier(
                        out,
                        vk::ImageLayout::GENERAL,
                        vk::ImageLayout::GENERAL,
                        vk::AccessFlags::SHADER_WRITE,
                        vk::AccessFlags::SHADER_READ,
                        ignored,
                        ignored,
                    ),
                    barrier(
                        p.image.image,
                        vk::ImageLayout::UNDEFINED,
                        vk::ImageLayout::GENERAL,
                        vk::AccessFlags::empty(),
                        vk::AccessFlags::SHADER_WRITE,
                        ignored,
                        ignored,
                    ),
                ];
                d.cmd_pipeline_barrier(
                    cmd,
                    stage,
                    stage,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &to_read,
                );
                d.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::COMPUTE,
                    self.pipeline_layout,
                    0,
                    &[self.sets[1]],
                    &[],
                );
                d.cmd_dispatch(
                    cmd,
                    PREVIEW_WIDTH.div_ceil(16),
                    PREVIEW_HEIGHT.div_ceil(16),
                    1,
                );
                d.cmd_pipeline_barrier(
                    cmd,
                    stage,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[barrier(
                        p.image.image,
                        vk::ImageLayout::GENERAL,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        vk::AccessFlags::SHADER_WRITE,
                        vk::AccessFlags::TRANSFER_READ,
                        ignored,
                        ignored,
                    )],
                );
                let region = vk::BufferImageCopy::default()
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .layer_count(1),
                    )
                    .image_extent(vk::Extent3D {
                        width: PREVIEW_WIDTH,
                        height: PREVIEW_HEIGHT,
                        depth: 1,
                    });
                let buffer = p.buffers[p.next].buffer;
                d.cmd_copy_image_to_buffer(
                    cmd,
                    p.image.image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    buffer,
                    &[region],
                );
                d.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::HOST,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[vk::BufferMemoryBarrier::default()
                        .buffer(buffer)
                        .size(vk::WHOLE_SIZE)
                        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                        .dst_access_mask(vk::AccessFlags::HOST_READ)
                        .src_queue_family_index(ignored)
                        .dst_queue_family_index(ignored)],
                    &[],
                );
            }

            // After the preview pass: its cost counts.
            if let Some((pool, _)) = self.queries {
                d.cmd_write_timestamp(cmd, vk::PipelineStageFlags::BOTTOM_OF_PIPE, pool, 1);
            }

            // Hand both DMA-BUFs back to their external users (VA, the producer).
            let mut release = vec![barrier(
                out,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::GENERAL,
                vk::AccessFlags::SHADER_WRITE,
                vk::AccessFlags::empty(),
                self.family,
                foreign,
            )];
            let imports = main
                .map(|i| self.imports.list[i].image.image)
                .into_iter()
                .chain(cameras.iter().filter_map(|c| c.import));
            for image in imports {
                release.push(barrier(
                    image,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::ImageLayout::GENERAL,
                    vk::AccessFlags::SHADER_READ,
                    vk::AccessFlags::empty(),
                    self.family,
                    foreign,
                ));
            }
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &release,
            );
            d.end_command_buffer(cmd).map_err(err("end command buffer"))
        }
    }

    /// Puts the pending writes to `frame`'s DMA-BUF into `sem`. False
    /// when there is nothing to wait for or the kernel cannot tell
    /// (before Linux 6.0): then the producer must be done already.
    unsafe fn import_implicit_fence(&self, frame: &VideoFrame, sem: vk::Semaphore) -> bool {
        // struct dma_buf_export_sync_file, DMA_BUF_IOCTL_EXPORT_SYNC_FILE.
        #[repr(C)]
        struct ExportSyncFile {
            flags: u32,
            fd: i32,
        }
        const EXPORT_SYNC_FILE: libc::c_ulong = 0xc008_6202;
        const DMA_BUF_SYNC_READ: u32 = 1;
        let mut arg = ExportSyncFile {
            flags: DMA_BUF_SYNC_READ,
            fd: -1,
        };
        let fd = frame.planes[0].fd.as_raw_fd();
        if unsafe { libc::ioctl(fd, EXPORT_SYNC_FILE, &mut arg) } != 0 || arg.fd < 0 {
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "compositor: no implicit fence from an imported frame: {}",
                    std::io::Error::last_os_error()
                );
            }
            return false;
        }
        let info = vk::ImportSemaphoreFdInfoKHR::default()
            .semaphore(sem)
            .flags(vk::SemaphoreImportFlags::TEMPORARY)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD)
            .fd(arg.fd);
        match unsafe { self.semaphore_fd.import_semaphore_fd(&info) } {
            Ok(()) => true,
            Err(e) => {
                // Vulkan owns the fd only on success.
                drop(unsafe { OwnedFd::from_raw_fd(arg.fd) });
                tracing::warn!("compositor: import implicit fence: {e}");
                false
            }
        }
    }

    unsafe fn submit_compose(&mut self, wait: &[vk::Semaphore]) -> Result<()> {
        let cmds = [self.cmd];
        let stages = vec![vk::PipelineStageFlags::COMPUTE_SHADER; wait.len()];
        let mut submit = vk::SubmitInfo::default().command_buffers(&cmds);
        if !wait.is_empty() {
            submit = submit.wait_semaphores(wait).wait_dst_stage_mask(&stages);
        }
        unsafe { self.device.queue_submit(self.queue, &[submit], self.fence) }
            .map_err(err("submit"))
    }

    /// Index in `imports` of `frame`'s image, importing it on first sight.
    unsafe fn import(&mut self, frame: &VideoFrame) -> Result<usize> {
        if self.imports.generation != Some(frame.pool_generation) {
            // A new producer pool: the kept copy is of the old source.
            if let Some(old) = self.base.take() {
                self.graveyard.push(old.image);
            }
        }
        let mut cache = std::mem::take(&mut self.imports);
        let r = unsafe { self.import_into(&mut cache, frame) };
        self.imports = cache;
        r
    }

    /// Index in `cache` of `frame`'s image, importing it on first sight.
    unsafe fn import_into(&mut self, cache: &mut Imports, frame: &VideoFrame) -> Result<usize> {
        if cache.generation != Some(frame.pool_generation) {
            // A new producer pool: the old buffers are gone (freed once no
            // queued work reads them).
            self.graveyard.extend(cache.list.drain(..).map(|i| i.image));
            cache.generation = Some(frame.pool_generation);
        }
        let first = frame
            .planes
            .first()
            .ok_or_else(|| Error("frame without planes".into()))?;
        let key = (
            frame.width,
            frame.height,
            frame.fourcc,
            frame.modifier,
            first.offset,
            first.stride,
            inode(&first.fd)?,
        );
        if let Some(i) = cache
            .list
            .iter()
            .position(|i| i.buffer_id == frame.buffer_id)
        {
            if cache.list[i].key == key {
                return Ok(i);
            }
            let old = cache.list.swap_remove(i);
            unsafe { old.image.destroy(&self.device) };
        }
        unsafe { self.check_importable(frame)? };
        let image = unsafe { self.import_dmabuf(frame)? };
        cache.list.push(Import {
            buffer_id: frame.buffer_id,
            key,
            image,
        });
        Ok(cache.list.len() - 1)
    }

    /// Prepares the copy of camera `id`'s `input` into its image, with
    /// descriptor set `index`.
    unsafe fn camera_pass(&mut self, id: u32, input: &Input, index: usize) -> Result<CameraPass> {
        let (import, view, buffer, params, width, height) = match input {
            Input::Frame(f) => {
                let cam = self.cameras.get_mut(&id).expect("listed");
                let mut cache = std::mem::take(&mut cam.imports);
                let r = unsafe { self.import_into(&mut cache, f) };
                let cam = self.cameras.get_mut(&id).expect("listed");
                cam.imports = cache;
                let image = &cam.imports.list[r?].image;
                let params = CameraParams {
                    head: [MODE_RGB, 0, 0, 0],
                    ..CameraParams::default()
                };
                let buffer = self.dummy_storage.buffer;
                (
                    Some(image.image),
                    image.view,
                    buffer,
                    params,
                    f.width,
                    f.height,
                )
            }
            Input::Yuv(s) => {
                let view = self.dummy.image.view;
                (None, view, s.buffer.buffer, s.params, s.width, s.height)
            }
        };
        unsafe { self.ensure_camera_image(id, width, height)? };
        let cam = self.cameras.get_mut(&id).expect("listed");
        let target = cam.image.as_ref().expect("made above");
        let (image, target_view) = (target.image.image, target.image.view);
        unsafe {
            (self.camera_params[index].ptr as *mut CameraParams).write(params);
            let buffers = [vk::DescriptorBufferInfo::default()
                .buffer(buffer)
                .range(vk::WHOLE_SIZE)];
            let source = [vk::DescriptorImageInfo::default()
                .sampler(self.sampler)
                .image_view(view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let out = [vk::DescriptorImageInfo::default()
                .image_view(target_view)
                .image_layout(vk::ImageLayout::GENERAL)];
            let set = self.camera_sets[index];
            self.device.update_descriptor_sets(
                &[
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(1)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(&buffers),
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(2)
                        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .image_info(&source),
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(3)
                        .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                        .image_info(&out),
                ],
                &[],
            );
        }
        Ok(CameraPass {
            import,
            image,
            width,
            height,
        })
    }

    /// Camera `id`'s image, `width` x `height`.
    unsafe fn ensure_camera_image(&mut self, id: u32, width: u32, height: u32) -> Result<()> {
        let cam = self.cameras.get_mut(&id).expect("listed");
        if cam
            .image
            .as_ref()
            .is_some_and(|t| (t.width, t.height) == (width, height))
        {
            return Ok(());
        }
        if let Some(old) = cam.image.take() {
            cam.valid = false;
            self.graveyard.push(old.image);
        }
        let ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { self.image(&ci, false)? };
        self.cameras.get_mut(&id).expect("listed").image = Some(Texture {
            image,
            width,
            height,
        });
        Ok(())
    }

    /// Rejects what the driver cannot import, before it reads the frame's
    /// plane layouts: the producer (a game, through the layer) is not
    /// trusted to send a modifier with its real plane count.
    unsafe fn check_importable(&mut self, f: &VideoFrame) -> Result<()> {
        let (format, _) = import_format(f.fourcc).ok_or_else(|| {
            Error(format!(
                "unsupported format {:?}",
                String::from_utf8_lossy(&f.fourcc.to_le_bytes())
            ))
        })?;
        let (instance, physical) = (&self.instance, self.physical);
        let list = self.modifiers.entry(format).or_insert_with(|| unsafe {
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            instance.get_physical_device_format_properties2(physical, format, &mut props);
            let mut out = vec![
                vk::DrmFormatModifierPropertiesEXT::default();
                list.drm_format_modifier_count as usize
            ];
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
                .drm_format_modifier_properties(&mut out);
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            instance.get_physical_device_format_properties2(physical, format, &mut props);
            let n = list.drm_format_modifier_count as usize;
            out.truncate(n);
            out
        });
        let m = list
            .iter()
            .find(|m| m.drm_format_modifier == f.modifier)
            .ok_or_else(|| Error(format!("modifier {:#x} not supported", f.modifier)))?;
        if m.drm_format_modifier_plane_count as usize != f.planes.len() {
            return Err(Error(format!(
                "modifier {:#x} has {} plane(s), the frame {}",
                f.modifier,
                m.drm_format_modifier_plane_count,
                f.planes.len()
            )));
        }
        if !m
            .drm_format_modifier_tiling_features
            .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
        {
            return Err(Error(format!(
                "modifier {:#x} cannot be sampled",
                f.modifier
            )));
        }
        let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
            .drm_format_modifier(f.modifier)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let mut ext = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(format)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .push_next(&mut ext)
            .push_next(&mut drm);
        let mut ext_props = vk::ExternalImageFormatProperties::default();
        let mut props = vk::ImageFormatProperties2::default().push_next(&mut ext_props);
        unsafe {
            self.instance.get_physical_device_image_format_properties2(
                self.physical,
                &info,
                &mut props,
            )
        }
        .map_err(|e| Error(format!("modifier {:#x} not importable: {e}", f.modifier)))?;
        let max = props.image_format_properties.max_extent;
        if f.width == 0 || f.height == 0 || f.width > max.width || f.height > max.height {
            return Err(Error(format!(
                "frame {}x{} out of {}x{}",
                f.width, f.height, max.width, max.height
            )));
        }
        if !ext_props
            .external_memory_properties
            .external_memory_features
            .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
        {
            return Err(Error(format!("modifier {:#x} not importable", f.modifier)));
        }
        Ok(())
    }

    unsafe fn import_dmabuf(&self, f: &VideoFrame) -> Result<Image> {
        let (format, _) = import_format(f.fourcc).ok_or_else(|| {
            Error(format!(
                "unsupported format {:?}",
                String::from_utf8_lossy(&f.fourcc.to_le_bytes())
            ))
        })?;
        let first = f
            .planes
            .first()
            .ok_or_else(|| Error("frame without planes".into()))?;
        if f.planes.len() > 1 {
            // Planes (e.g. DCC metadata) must share one buffer object: each fd
            // is a duplicate. Disjoint planes are not supported.
            let a = inode(&first.fd)?;
            for p in &f.planes[1..] {
                if inode(&p.fd)? != a {
                    return Err(Error("DMA-BUF planes in separate buffers".into()));
                }
            }
        }
        let layouts: Vec<_> = f
            .planes
            .iter()
            .map(|p| vk::SubresourceLayout {
                offset: p.offset.into(),
                row_pitch: p.stride.into(),
                ..Default::default()
            })
            .collect();
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(f.modifier)
            .plane_layouts(&layouts);
        let ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: f.width,
                height: f.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external)
            .push_next(&mut explicit);
        let d = &self.device;
        let mut img = Image::NULL;
        let r = unsafe {
            (|| {
                img.image = d.create_image(&ci, None).map_err(|e| {
                    Error(format!(
                        "import {}x{} modifier {:#x}: {e}",
                        f.width, f.height, f.modifier
                    ))
                })?;
                let reqs = d.get_image_memory_requirements(img.image);
                let fd = first
                    .fd
                    .try_clone()
                    .map_err(|e| Error(format!("dup DMA-BUF: {e}")))?;
                // A DMA-BUF's size is its end offset. Never bind memory the
                // image would overrun (bogus offsets or strides).
                let size = libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END);
                if size < 0 || (size as u64) < reqs.size {
                    return Err(Error(format!(
                        "DMA-BUF of {size} bytes, the image needs {}",
                        reqs.size
                    )));
                }
                let mut props = vk::MemoryFdPropertiesKHR::default();
                self.memory_fd
                    .get_memory_fd_properties(
                        vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                        fd.as_raw_fd(),
                        &mut props,
                    )
                    .map_err(err("DMA-BUF memory properties"))?;
                let type_index = self
                    .memory_type(
                        reqs.memory_type_bits & props.memory_type_bits,
                        vk::MemoryPropertyFlags::empty(),
                    )
                    .ok_or_else(|| Error("no memory type for the DMA-BUF".into()))?;
                let raw = fd.into_raw_fd();
                let mut import = vk::ImportMemoryFdInfoKHR::default()
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                    .fd(raw);
                let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(img.image);
                let ai = vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(type_index)
                    .push_next(&mut import)
                    .push_next(&mut dedicated);
                img.memory = d.allocate_memory(&ai, None).map_err(|e| {
                    // Vulkan owns the fd only on success.
                    drop(OwnedFd::from_raw_fd(raw));
                    err("import DMA-BUF memory")(e)
                })?;
                d.bind_image_memory(img.image, img.memory, 0)
                    .map_err(err("bind DMA-BUF memory"))?;
                img.view = self.view(img.image, format, 1)?;
                Ok(())
            })()
        };
        match r {
            Ok(()) => Ok(img),
            Err(e) => {
                unsafe { img.destroy(d) };
                Err(e)
            }
        }
    }

    fn memory_type(&self, bits: u32, flags: vk::MemoryPropertyFlags) -> Option<u32> {
        memory_type(&self.memory, bits, flags)
    }

    unsafe fn compute_pipeline(
        &self,
        spirv: &[u8],
        layout: vk::PipelineLayout,
    ) -> Result<vk::Pipeline> {
        let d = &self.device;
        let code = ash::util::read_spv(&mut std::io::Cursor::new(spirv))
            .map_err(|e| Error(format!("read SPIR-V: {e}")))?;
        unsafe {
            let module = d
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)
                .map_err(err("create shader module"))?;
            let stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(module)
                .name(c"main");
            let pipeline = d.create_compute_pipelines(
                vk::PipelineCache::null(),
                &[vk::ComputePipelineCreateInfo::default()
                    .stage(stage)
                    .layout(layout)],
                None,
            );
            d.destroy_shader_module(module, None);
            Ok(pipeline.map_err(|(_, e)| err("create pipeline")(e))?[0])
        }
    }

    unsafe fn view(
        &self,
        image: vk::Image,
        format: vk::Format,
        levels: u32,
    ) -> Result<vk::ImageView> {
        unsafe {
            self.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(levels)
                            .layer_count(1),
                    ),
                None,
            )
        }
        .map_err(err("create image view"))
    }

    /// A device-local image with its memory and view.
    unsafe fn image(&self, ci: &vk::ImageCreateInfo<'_>, export: bool) -> Result<Image> {
        let d = &self.device;
        let mut img = Image::NULL;
        let r = unsafe {
            (|| {
                img.image = d.create_image(ci, None).map_err(err("create image"))?;
                let reqs = d.get_image_memory_requirements(img.image);
                let type_index = self
                    .memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                    .or_else(|| {
                        self.memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::empty())
                    })
                    .ok_or_else(|| Error("no memory type for an image".into()))?;
                let mut exp = vk::ExportMemoryAllocateInfo::default()
                    .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
                let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(img.image);
                let mut ai = vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(type_index);
                if export {
                    ai = ai.push_next(&mut exp).push_next(&mut dedicated);
                }
                img.memory = d
                    .allocate_memory(&ai, None)
                    .map_err(err("allocate image memory"))?;
                d.bind_image_memory(img.image, img.memory, 0)
                    .map_err(err("bind image memory"))?;
                img.view = self.view(img.image, ci.format, ci.mip_levels)?;
                Ok(())
            })()
        };
        match r {
            Ok(()) => Ok(img),
            Err(e) => {
                unsafe { img.destroy(d) };
                Err(e)
            }
        }
    }

    /// One exported XR24 LINEAR output image.
    unsafe fn create_slot(&self, width: u32, height: u32) -> Result<Slot> {
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        // RGBA image the shader fills with BGRX bytes: XR24 in memory.
        let ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::LINEAR)
            .usage(vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external);
        unsafe {
            let image = self.image(&ci, true)?;
            let fd = self
                .memory_fd
                .get_memory_fd(
                    &vk::MemoryGetFdInfoKHR::default()
                        .memory(image.memory)
                        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
                )
                .map(|fd| OwnedFd::from_raw_fd(fd));
            let layout = self.device.get_image_subresource_layout(
                image.image,
                vk::ImageSubresource::default().aspect_mask(vk::ImageAspectFlags::COLOR),
            );
            let fail = |e: Error| {
                image.destroy(&self.device);
                Err(e)
            };
            let fd = match fd {
                Ok(fd) => fd,
                Err(e) => return fail(err("export output DMA-BUF")(e)),
            };
            let (Ok(offset), Ok(stride)) = (
                u32::try_from(layout.offset),
                u32::try_from(layout.row_pitch),
            ) else {
                return fail(Error("output layout does not fit 32 bits".into()));
            };
            Ok(Slot {
                image,
                fd,
                offset,
                stride,
                busy: Arc::new(AtomicBool::new(false)),
            })
        }
    }

    unsafe fn buffer(
        &self,
        size: u64,
        usage: vk::BufferUsageFlags,
        cached: bool,
    ) -> Result<Buffer> {
        unsafe { self.gpu.buffer(size, usage, cached) }
    }

    /// Queues the copy of `staged` into a new texture (a compute queue
    /// cannot blit: the mips were built on the CPU). The previous copy must
    /// be done ([`Compositor::wait_upload`]).
    unsafe fn upload(&mut self, staged: Staged) -> Result<Texture> {
        let ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D {
                width: staged.width,
                height: staged.height,
                depth: 1,
            })
            .mip_levels(staged.levels)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        unsafe {
            let image = self.image(&ci, false)?;
            let r = self
                .record_upload(
                    staged.buffer.buffer,
                    image.image,
                    staged.levels,
                    &staged.regions,
                )
                .and_then(|()| {
                    let cmds = [self.upload_cmd];
                    self.device
                        .queue_submit(
                            self.queue,
                            &[vk::SubmitInfo::default().command_buffers(&cmds)],
                            self.upload_fence,
                        )
                        .map_err(err("submit texture copy"))
                });
            match r {
                Ok(()) => {
                    let texture = Texture {
                        image,
                        width: staged.width,
                        height: staged.height,
                    };
                    self.upload_pending = Some(staged);
                    Ok(texture)
                }
                Err(e) => {
                    image.destroy(&self.device);
                    Err(e)
                }
            }
        }
    }

    unsafe fn record_upload(
        &self,
        staging: vk::Buffer,
        image: vk::Image,
        levels: u32,
        regions: &[vk::BufferImageCopy],
    ) -> Result<()> {
        let d = &self.device;
        let cmd = self.upload_cmd;
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(levels)
            .layer_count(1);
        let barrier = |old, new, src, dst| {
            vk::ImageMemoryBarrier::default()
                .image(image)
                .old_layout(old)
                .new_layout(new)
                .src_access_mask(src)
                .dst_access_mask(dst)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .subresource_range(range)
        };
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .map_err(err("reset command buffer"))?;
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(err("begin command buffer"))?;
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier(
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_WRITE,
                )],
            );
            d.cmd_copy_buffer_to_image(
                cmd,
                staging,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                regions,
            );
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier(
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::TRANSFER_WRITE,
                    vk::AccessFlags::SHADER_READ,
                )],
            );
            d.end_command_buffer(cmd).map_err(err("end command buffer"))
        }
    }

    unsafe fn create_preview(&self) -> Result<Preview> {
        let ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D {
                width: PREVIEW_WIDTH,
                height: PREVIEW_HEIGHT,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let size = u64::from(PREVIEW_WIDTH * PREVIEW_HEIGHT * 4);
        let usage = vk::BufferUsageFlags::TRANSFER_DST;
        unsafe {
            let image = self.image(&ci, false)?;
            let a = self.buffer(size, usage, true);
            let b = self.buffer(size, usage, true);
            match (a, b) {
                (Ok(a), Ok(b)) => Ok(Preview {
                    image,
                    buffers: [a, b],
                    next: 0,
                    ready: None,
                    wanted: false,
                }),
                (a, b) => {
                    image.destroy(&self.device);
                    let mut e = None;
                    for r in [a, b] {
                        match r {
                            Ok(buf) => buf.destroy(&self.device),
                            Err(x) => e = Some(x),
                        }
                    }
                    Err(e.expect("one failed"))
                }
            }
        }
    }
}

impl Image {
    const NULL: Self = Self {
        image: vk::Image::null(),
        memory: vk::DeviceMemory::null(),
        view: vk::ImageView::null(),
    };

    unsafe fn destroy(&self, d: &ash::Device) {
        unsafe {
            d.destroy_image_view(self.view, None);
            d.destroy_image(self.image, None);
            d.free_memory(self.memory, None);
        }
    }
}

impl Buffer {
    const NULL: Self = Self {
        buffer: vk::Buffer::null(),
        memory: vk::DeviceMemory::null(),
        ptr: std::ptr::null_mut(),
    };

    unsafe fn destroy(&self, d: &ash::Device) {
        unsafe {
            d.destroy_buffer(self.buffer, None);
            // Freeing mapped memory unmaps it.
            d.free_memory(self.memory, None);
        }
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        let d = &self.device;
        unsafe {
            // Frames still out keep their DMA-BUF (their own fd) alive.
            let _ = d.device_wait_idle();
            self.upload_pending = None;
            for i in self.graveyard.drain(..) {
                i.destroy(d);
            }
            if let Some(b) = &self.base {
                b.image.destroy(d);
            }
            if let Some(p) = &self.preview {
                p.image.destroy(d);
                for b in &p.buffers {
                    b.destroy(d);
                }
            }
            for s in &self.ring {
                s.image.destroy(d);
            }
            for i in &self.imports.list {
                i.image.destroy(d);
            }
            for c in self.cameras.values() {
                if let Some(t) = &c.image {
                    t.image.destroy(d);
                }
                for i in &c.imports.list {
                    i.image.destroy(d);
                }
            }
            // Pictures still pending go back to their producers.
            self.cameras.clear();
            for t in self.textures.values() {
                t.image.destroy(d);
            }
            self.dummy.image.destroy(d);
            for b in self.params.iter().chain(&self.camera_params) {
                b.destroy(d);
            }
            self.dummy_storage.destroy(d);
            d.destroy_descriptor_pool(self.descriptor_pool, None);
            d.destroy_pipeline(self.pipeline, None);
            d.destroy_pipeline_layout(self.pipeline_layout, None);
            d.destroy_descriptor_set_layout(self.set_layout, None);
            d.destroy_pipeline(self.camera_pipeline, None);
            d.destroy_pipeline_layout(self.camera_pipeline_layout, None);
            d.destroy_descriptor_set_layout(self.camera_set_layout, None);
            d.destroy_sampler(self.sampler, None);
            if let Some((pool, _)) = self.queries {
                d.destroy_query_pool(pool, None);
            }
            d.destroy_fence(self.fence, None);
            d.destroy_fence(self.upload_fence, None);
            for sem in self.acquire {
                d.destroy_semaphore(sem, None);
            }
            d.destroy_command_pool(self.pool, None);
            // `gpu` (dropped after this) destroys the device and instance.
        }
    }
}

/// Creates the logical device on [`RENDER_NODE`]'s GPU with one compute
/// queue, at low global priority when the driver allows it.
unsafe fn create_device(
    instance: &ash::Instance,
) -> Result<(ash::Device, vk::PhysicalDevice, u32, Option<f32>)> {
    unsafe {
        let (physical, family, priority, timestamps) = pick_device(instance)?;
        let mut extensions = vec![
            c"VK_KHR_external_memory_fd".as_ptr(),
            c"VK_EXT_external_memory_dma_buf".as_ptr(),
            c"VK_EXT_image_drm_format_modifier".as_ptr(),
            c"VK_EXT_queue_family_foreign".as_ptr(),
            c"VK_KHR_external_semaphore_fd".as_ptr(),
        ];
        let features =
            vk::PhysicalDeviceFeatures::default().shader_sampled_image_array_dynamic_indexing(true);
        let create = |extensions: &[*const std::ffi::c_char], low: bool| {
            let mut global = vk::DeviceQueueGlobalPriorityCreateInfoKHR::default()
                .global_priority(vk::QueueGlobalPriorityKHR::LOW);
            let mut queue = vk::DeviceQueueCreateInfo::default()
                .queue_family_index(family)
                .queue_priorities(&[1.0]);
            if low {
                queue = queue.push_next(&mut global);
            }
            let queues = [queue];
            let ci = vk::DeviceCreateInfo::default()
                .queue_create_infos(&queues)
                .enabled_extension_names(extensions)
                .enabled_features(&features);
            instance.create_device(physical, &ci, None)
        };
        let device = match priority {
            Some(ext) => {
                extensions.push(ext.as_ptr());
                match create(&extensions, true) {
                    Ok(d) => {
                        tracing::info!("compositor: compute queue family {family}, low priority");
                        d
                    }
                    Err(e) => {
                        tracing::info!(
                            "compositor: compute queue family {family}, default priority (low refused: {e})"
                        );
                        extensions.pop();
                        create(&extensions, false).map_err(err("create device"))?
                    }
                }
            }
            None => {
                tracing::info!(
                    "compositor: compute queue family {family}, default priority (low not offered)"
                );
                create(&extensions, false).map_err(err("create device"))?
            }
        };
        Ok((device, physical, family, timestamps))
    }
}

/// The physical device behind [`RENDER_NODE`], its compute queue family, the
/// global priority extension to enable if the family allows LOW, and the
/// timestamp period when the family has timestamps.
unsafe fn pick_device(
    instance: &ash::Instance,
) -> Result<(
    vk::PhysicalDevice,
    u32,
    Option<&'static std::ffi::CStr>,
    Option<f32>,
)> {
    let rdev = std::fs::metadata(RENDER_NODE)
        .map_err(|e| Error(format!("{RENDER_NODE}: {e}")))?
        .rdev();
    let (major, minor) = dev_numbers(rdev);
    unsafe {
        let devices = instance
            .enumerate_physical_devices()
            .map_err(err("enumerate devices"))?;
        for pd in devices {
            let exts = instance
                .enumerate_device_extension_properties(pd)
                .unwrap_or_default();
            let has = |name: &std::ffi::CStr| {
                exts.iter()
                    .any(|e| e.extension_name_as_c_str().is_ok_and(|n| n == name))
            };
            let required = [
                c"VK_EXT_physical_device_drm",
                c"VK_KHR_external_memory_fd",
                c"VK_EXT_external_memory_dma_buf",
                c"VK_EXT_image_drm_format_modifier",
                c"VK_EXT_queue_family_foreign",
                c"VK_KHR_external_semaphore_fd",
            ];
            if !required.iter().all(|n| has(n)) {
                continue;
            }
            let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
            let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
            instance.get_physical_device_properties2(pd, &mut props);
            let timestamp_period = props.properties.limits.timestamp_period;
            if !(drm.has_render == vk::TRUE
                && drm.render_major == major
                && drm.render_minor == minor)
            {
                continue;
            }
            let priority_ext = [c"VK_KHR_global_priority", c"VK_EXT_global_priority"]
                .into_iter()
                .find(|n| has(n));
            let queryable = has(c"VK_KHR_global_priority") || has(c"VK_EXT_global_priority_query");
            let n = instance.get_physical_device_queue_family_properties2_len(pd);
            let mut prios = vec![vk::QueueFamilyGlobalPriorityPropertiesKHR::default(); n];
            let mut families: Vec<_> = prios
                .iter_mut()
                .map(|p| {
                    let f = vk::QueueFamilyProperties2::default();
                    if queryable { f.push_next(p) } else { f }
                })
                .collect();
            instance.get_physical_device_queue_family_properties2(pd, &mut families);
            let flags: Vec<_> = families
                .iter()
                .map(|f| {
                    (
                        f.queue_family_properties.queue_flags,
                        f.queue_family_properties.timestamp_valid_bits,
                    )
                })
                .collect();
            drop(families);
            // An async compute family (no graphics) first, else any compute one.
            let family = (0..n)
                .find(|&i| {
                    flags[i].0.contains(vk::QueueFlags::COMPUTE)
                        && !flags[i].0.contains(vk::QueueFlags::GRAPHICS)
                })
                .or_else(|| (0..n).find(|&i| flags[i].0.contains(vk::QueueFlags::COMPUTE)))
                .ok_or_else(|| Error("no compute queue".into()))?;
            let low = queryable
                && prios[family].priorities[..prios[family].priority_count as usize]
                    .contains(&vk::QueueGlobalPriorityKHR::LOW);
            let priority = priority_ext.filter(|_| low);
            let timestamps = (flags[family].1 > 0).then_some(timestamp_period);
            return Ok((pd, family as u32, priority, timestamps));
        }
    }
    Err(Error(format!("no Vulkan device for {RENDER_NODE}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(rect: [f32; 4], crop: [f32; 4], fit: Fit) -> Draw {
        Draw {
            source: Source::Main,
            rect,
            crop,
            fit,
            opacity: 1.0,
        }
    }

    #[test]
    fn full_canvas_at_native_size_is_exact() {
        let l = place(
            &draw([0.0, 0.0, 1.0, 1.0], [0.0; 4], Fit::Contain),
            1920,
            1080,
            1920,
            1080,
        )
        .unwrap();
        assert_eq!(l.dst, [0.0, 0.0, 1920.0, 1080.0]);
        assert_eq!(l.src, [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(l.flags[0], FLAG_EXACT);
        assert_eq!(l.misc[1], 0.0, "no mip level");
    }

    #[test]
    fn contain_letterboxes_and_cover_crops() {
        // 4:3 source in a 16:9 canvas.
        let c = place(
            &draw([0.0, 0.0, 1.0, 1.0], [0.0; 4], Fit::Contain),
            800,
            600,
            1600,
            900,
        )
        .unwrap();
        assert_eq!(c.dst, [200.0, 0.0, 1400.0, 900.0]);
        assert_eq!(c.flags[0], 0, "scaled");
        let v = place(
            &draw([0.0, 0.0, 1.0, 1.0], [0.0; 4], Fit::Cover),
            800,
            600,
            1600,
            900,
        )
        .unwrap();
        assert_eq!(v.dst, [0.0, 0.0, 1600.0, 900.0]);
        // 800x450 of the source shown, centred: rows 75..525.
        assert_eq!(v.src, [0.0, 75.0, 0.5, 0.5]);
        let s = place(
            &draw([0.0, 0.0, 1.0, 1.0], [0.0; 4], Fit::Stretch),
            800,
            600,
            1600,
            900,
        )
        .unwrap();
        assert_eq!(s.src, [0.0, 0.0, 0.5, 600.0 / 900.0]);
    }

    #[test]
    fn crop_and_downscale_pick_a_mip_level() {
        // Right half of a 4000x1000 source into a 500x250 rect: 4x smaller.
        let l = place(
            &draw([0.0, 0.0, 0.25, 0.25], [0.5, 0.0, 0.0, 0.0], Fit::Stretch),
            4000,
            1000,
            2000,
            1000,
        )
        .unwrap();
        assert_eq!(l.dst, [0.0, 0.0, 500.0, 250.0]);
        assert_eq!(l.src[..2], [2000.0, 0.0]);
        assert_eq!(l.misc[1], 1.0, "2 texels per pixel on mip 1");
    }

    #[test]
    fn invisible_draws_are_dropped() {
        let full = [0.0, 0.0, 1.0, 1.0];
        assert!(
            place(
                &draw([0.0, 0.0, 0.0, 1.0], [0.0; 4], Fit::Contain),
                10,
                10,
                100,
                100
            )
            .is_none()
        );
        assert!(
            place(
                &draw(full, [0.6, 0.0, 0.5, 0.0], Fit::Contain),
                10,
                10,
                100,
                100
            )
            .is_none()
        );
        assert!(
            place(
                &draw([f32::NAN, 0.0, 1.0, 1.0], [0.0; 4], Fit::Contain),
                10,
                10,
                100,
                100
            )
            .is_none()
        );
        let mut d = draw(full, [0.0; 4], Fit::Contain);
        d.opacity = 0.0;
        assert!(place(&d, 10, 10, 100, 100).is_none());
        // Non-finite input never reaches the shader.
        let inf = f32::INFINITY;
        for (rect, crop) in [
            ([-inf, 0.0, inf, 1.0], [0.0; 4]),
            ([0.0, 0.0, 1.0, inf], [0.0; 4]),
            (full, [0.0, f32::NAN, 0.0, 0.0]),
            (full, [-inf, 0.0, 0.0, 0.0]),
        ] {
            for fit in [Fit::Contain, Fit::Cover, Fit::Stretch] {
                assert!(place(&draw(rect, crop, fit), 10, 10, 100, 100).is_none());
            }
        }
        let mut d = draw(full, [0.0; 4], Fit::Contain);
        d.opacity = f32::NAN;
        assert!(place(&d, 10, 10, 100, 100).is_none());
    }

    #[test]
    fn mip_halving_averages_and_clamps_odd_edges() {
        let px = |v: u8| [v, v, v, 255];
        let src: Vec<u8> = [px(0), px(100), px(200), px(50), px(150), px(250)].concat();
        let (out, w, h) = half(&src, 3, 2);
        assert_eq!((w, h), (1, 1));
        assert_eq!(out, [75, 75, 75, 255]);
    }

    #[test]
    fn device_numbers_match_glibc() {
        // makedev(226, 128) as glibc encodes it.
        assert_eq!(dev_numbers(0xe280), (226, 128));
    }

    #[test]
    fn formats() {
        assert_eq!(OUTPUT_FOURCC, 0x3432_5258);
        assert!(import_format(fourcc(b"XR24")).is_some());
        assert!(import_format(fourcc(b"NV12")).is_none());
    }

    #[test]
    fn yuv_rows_pack_tightly_and_stay_in_the_buffer() {
        let data = [0u8; 100];
        let pic = |layout, w, h, planes: &'static [(usize, usize)]| YuvPicture {
            width: w,
            height: h,
            layout,
            planes,
            data: &data,
            matrix: YuvMatrix::Bt601,
            full_range: false,
        };
        // 3 x 3 NV12: Y rows of 3, UV rows of 4 (2 pairs), 2 of them.
        let rows = yuv_rows(&pic(YuvLayout::Nv12, 3, 3, &[(1, 5), (40, 6)])).unwrap();
        assert_eq!(rows, [1..4, 6..9, 11..14, 40..44, 46..50]);
        // 3 x 2 YUY2: rows of 8 bytes.
        let rows = yuv_rows(&pic(YuvLayout::Yuy2, 3, 2, &[(0, 10)])).unwrap();
        assert_eq!(rows, [0..8, 10..18]);
        for bad in [
            pic(YuvLayout::Yuy2, 3, 2, &[(0, 7)]),
            pic(YuvLayout::Yuy2, 3, 2, &[(90, 8)]),
            pic(YuvLayout::Yuy2, 3, 2, &[(usize::MAX - 4, 8)]),
            pic(YuvLayout::Yuy2, 3, 2, &[(0, 8), (0, 8)]),
            pic(YuvLayout::Nv12, 3, 3, &[(0, 3)]),
            pic(YuvLayout::Yuy2, 0, 2, &[(0, 8)]),
        ] {
            assert!(yuv_rows(&bad).is_err(), "{bad:?}");
        }
    }
}
