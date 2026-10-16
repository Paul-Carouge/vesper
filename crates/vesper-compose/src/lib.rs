//! GPU compositor for scenes: draws the main capture and overlays (images,
//! text, webcam) into an exported DMA-BUF ring that `vesper-encode` takes as
//! any other [`VideoFrame`].
//!
//! Vulkan compute on an async compute queue of the discrete GPU
//! (`/dev/dri/renderD128`, never the iGPU), at low global priority when the
//! driver allows it so a game keeps the GPU first. One compute shader
//! (`shaders/compose.comp`, SPIR-V committed) draws up to [`MAX_LAYERS`]
//! layers per frame. `compose` waits its fence before returning: VA has no
//! implicit sync with Vulkan on radeonsi.

use std::collections::HashMap;
use std::fmt;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use ash::vk;
use vesper_core::{DmaBufPlane, ReleaseGuard, VideoFrame};

/// Layers drawn per frame, the main capture included.
pub const MAX_LAYERS: usize = 16;
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

const fn fourcc(c: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*c)
}

/// Every pool of composed frames gets its own generation, so an encoder
/// never reuses an import from a previous compositor's ring.
static GENERATION: AtomicU32 = AtomicU32::new(0);

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
    Some(GpuLayer {
        dst: [dx0, dy0, dx1, dy1],
        src: [sx0, sy0, kx, ky],
        misc: [opacity, kx.max(ky).log2().max(0.0), 1.0 / tw, 1.0 / th],
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
    /// Size, fourcc, modifier and the DMA-BUF's inode: a recycled
    /// (buffer_id, generation) on another buffer never hits.
    key: (u32, u32, u32, u64, u64),
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
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    memory_fd: ash::khr::external_memory_fd::Device,
    semaphore_fd: ash::khr::external_semaphore_fd::Device,
    /// Waited by a compose: takes the main frame's pending writes (its
    /// implicit fence), which Vulkan on RADV does not wait for by itself.
    acquire: vk::Semaphore,
    memory: vk::PhysicalDeviceMemoryProperties,
    physical: vk::PhysicalDevice,
    /// DRM modifiers the device supports per import format.
    modifiers: HashMap<vk::Format, Vec<vk::DrmFormatModifierPropertiesEXT>>,
    queue: vk::Queue,
    family: u32,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    queries: Option<(vk::QueryPool, f32)>,
    gpu_time: Option<Duration>,
    sampler: vk::Sampler,
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    descriptor_pool: vk::DescriptorPool,
    /// Compose pass, preview pass.
    sets: [vk::DescriptorSet; 2],
    params: [Buffer; 2],
    dummy: Texture,
    textures: HashMap<u32, Texture>,
    imports: Vec<Import>,
    import_generation: Option<u32>,
    ring: Vec<Slot>,
    generation: u32,
    preview: Option<Preview>,
    /// A fence wait timed out: the GPU state is unknown, refuse further work.
    lost: bool,
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
        // From here `Drop` frees whatever exists, the device and instance included.
        let mut c = unsafe {
            Self {
                width,
                height,
                memory_fd: ash::khr::external_memory_fd::Device::new(&instance, &device),
                semaphore_fd: ash::khr::external_semaphore_fd::Device::new(&instance, &device),
                acquire: vk::Semaphore::null(),
                memory: instance.get_physical_device_memory_properties(physical),
                physical,
                modifiers: HashMap::new(),
                queue: device.get_device_queue(family, 0),
                _entry: entry,
                instance,
                device,
                family,
                pool: vk::CommandPool::null(),
                cmd: vk::CommandBuffer::null(),
                fence: vk::Fence::null(),
                queries: timestamps.map(|period| (vk::QueryPool::null(), period)),
                gpu_time: None,
                sampler: vk::Sampler::null(),
                set_layout: vk::DescriptorSetLayout::null(),
                pipeline_layout: vk::PipelineLayout::null(),
                pipeline: vk::Pipeline::null(),
                descriptor_pool: vk::DescriptorPool::null(),
                sets: [vk::DescriptorSet::null(); 2],
                params: [Buffer::NULL, Buffer::NULL],
                dummy: Texture {
                    image: Image::NULL,
                    width: 1,
                    height: 1,
                },
                textures: HashMap::new(),
                imports: Vec::new(),
                import_generation: None,
                ring: Vec::new(),
                generation: GENERATION.fetch_add(1, Ordering::Relaxed),
                preview: None,
                lost: false,
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
            self.cmd = d
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(self.pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .map_err(err("allocate command buffer"))?[0];
            self.acquire = d
                .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
                .map_err(err("create semaphore"))?;
            self.fence = d
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
            let code = ash::util::read_spv(&mut std::io::Cursor::new(SPIRV))
                .map_err(|e| Error(format!("read SPIR-V: {e}")))?;
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
                    .layout(self.pipeline_layout)],
                None,
            );
            d.destroy_shader_module(module, None);
            self.pipeline = pipeline.map_err(|(_, e)| err("create pipeline")(e))?[0];
            let sizes = [
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::UNIFORM_BUFFER,
                    descriptor_count: 2,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::STORAGE_IMAGE,
                    descriptor_count: 2,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                    descriptor_count: 2 * MAX_LAYERS as u32,
                },
            ];
            self.descriptor_pool = d
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(2)
                        .pool_sizes(&sizes),
                    None,
                )
                .map_err(err("create descriptor pool"))?;
            let sets = d
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(self.descriptor_pool)
                        .set_layouts(&[self.set_layout; 2]),
                )
                .map_err(err("allocate descriptor sets"))?;
            self.sets = [sets[0], sets[1]];
            for i in 0..2 {
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
            self.dummy = self.upload(1, 1, &[0; 4])?;
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

    /// GPU time of the last composition (timestamp queries), when the queue
    /// supports them.
    pub fn gpu_time(&self) -> Option<Duration> {
        self.gpu_time
    }

    /// Sets (or replaces) texture `id` from `width` x `height` premultiplied
    /// RGBA8 pixels, with a full mip chain for downscaled draws. Blocks until
    /// the upload is done and builds the mips on the calling thread (a few
    /// ms per megapixel): call it when the content changes, not per frame,
    /// and off the pipeline thread for large images. Levels at least twice
    /// the canvas size are dropped.
    pub fn set_texture(&mut self, id: u32, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
        self.check_alive()?;
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
        // Drawn at most canvas-sized: levels at least twice the canvas are
        // never sampled, so they are not built or uploaded.
        // shortcut: a crop zooming into a huge image loses that detail; keep
        // more levels if the editor ever allows such zooms.
        let (mut w, mut h) = (width as usize, height as usize);
        let mut px = std::borrow::Cow::Borrowed(rgba);
        while w / 2 >= self.width as usize && h / 2 >= self.height as usize {
            let (p, nw, nh) = half(&px, w, h);
            (px, w, h) = (std::borrow::Cow::Owned(p), nw, nh);
        }
        let texture = unsafe { self.upload(w as u32, h as u32, &px)? };
        if let Some(old) = self.textures.insert(id, texture) {
            // `upload` waited for the queue: nothing uses the old one.
            unsafe { old.image.destroy(&self.device) };
        }
        Ok(())
    }

    /// Frees texture `id`. Draws of it are skipped from then on.
    pub fn remove_texture(&mut self, id: u32) {
        if let Some(old) = self.textures.remove(&id) {
            unsafe { old.image.destroy(&self.device) };
        }
    }

    /// Requests a [`PREVIEW_WIDTH`] x [`PREVIEW_HEIGHT`] RGBA8 downscale of
    /// the next composition and returns the latest finished one not returned
    /// yet. Never waits for the GPU; the small extra pass runs inside the
    /// next `compose` only after a request. A plain bilinear downscale of the
    /// canvas (no mips): fine details may alias.
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

    /// Composes `draws` over black into the next free ring image and returns
    /// it, ready for the encoder (the GPU is done with it). `main` is released
    /// as soon as the GPU has read it. Draws of a missing source are skipped.
    /// The frame's pts is `main`'s, or zero without one: set it as needed.
    /// `main` is sampled without mips: a large downscale of it may alias.
    /// Without `main`, the previous producer's imports are freed.
    pub fn compose(&mut self, main: Option<VideoFrame>, draws: &[Draw]) -> Result<VideoFrame> {
        self.check_alive()?;
        if draws.len() > MAX_LAYERS {
            return Err(Error(format!(
                "{} layers, at most {MAX_LAYERS}",
                draws.len()
            )));
        }
        let slot = self
            .ring
            .iter()
            .position(|s| !s.busy.load(Ordering::Acquire))
            .ok_or_else(|| Error("every composed frame is still in use".into()))?;
        let main_import = match &main {
            Some(f) => Some(unsafe { self.import(f)? }),
            None => {
                // No main (e.g. the game quit): free its pool's images now
                // rather than pin them until another producer shows up.
                self.drop_imports();
                None
            }
        };

        let mut params = Params {
            head: [0, 1, 0, 0],
            layers: [GpuLayer::default(); MAX_LAYERS],
        };
        let mut views = [self.dummy.image.view; MAX_LAYERS];
        let mut n = 0;
        for d in draws {
            let (view, w, h, flags) = match d.source {
                Source::Main => {
                    let (Some(i), Some(f)) = (main_import, &main) else {
                        continue;
                    };
                    let flags = import_format(f.fourcc).map_or(0, |(_, fl)| fl);
                    (self.imports[i].image.view, f.width, f.height, flags)
                }
                Source::Texture(id) => match self.textures.get(&id) {
                    Some(t) => (t.image.view, t.width, t.height, 0),
                    None => continue,
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
            let wait = match &main {
                Some(f) => self.import_implicit_fence(f),
                None => false,
            };
            self.record(slot, main_import, want_preview)?;
            self.submit_and_wait(wait)?;
        }
        // The GPU has read it: hand the producer its buffer back now.
        let pts = main.as_ref().map_or(Duration::ZERO, |f| f.pts);
        drop(main);

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
        if want_preview && let Some(p) = &mut self.preview {
            p.ready = Some(p.next);
            p.next ^= 1;
            p.wanted = false;
        }

        let s = &self.ring[slot];
        let fd =
            s.fd.try_clone()
                .map_err(|e| Error(format!("dup output fd: {e}")))?;
        s.busy.store(true, Ordering::Release);
        let busy = s.busy.clone();
        Ok(VideoFrame {
            width: self.width,
            height: self.height,
            fourcc: OUTPUT_FOURCC,
            modifier: DRM_FORMAT_MOD_LINEAR,
            planes: vec![DmaBufPlane {
                fd,
                offset: s.offset,
                stride: s.stride,
            }],
            buffer_id: slot as u32,
            pool_generation: self.generation,
            pts,
            release: ReleaseGuard::new(move || busy.store(false, Ordering::Release)),
        })
    }

    /// Frees every imported main frame (nothing is in flight between calls).
    pub fn drop_imports(&mut self) {
        for i in self.imports.drain(..) {
            unsafe { i.image.destroy(&self.device) };
        }
        self.import_generation = None;
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

    unsafe fn record(&self, slot: usize, main: Option<usize>, preview: bool) -> Result<()> {
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
                    self.imports[i].image.image,
                    vk::ImageLayout::GENERAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::SHADER_READ,
                    foreign,
                    self.family,
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
            if let Some((pool, _)) = self.queries {
                d.cmd_write_timestamp(cmd, vk::PipelineStageFlags::BOTTOM_OF_PIPE, pool, 1);
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
            if let Some(i) = main {
                release.push(barrier(
                    self.imports[i].image.image,
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

    /// Puts the pending writes to `frame`'s DMA-BUF into `acquire`. False
    /// when there is nothing to wait for or the kernel cannot tell
    /// (before Linux 6.0): then the producer must be done already.
    unsafe fn import_implicit_fence(&self, frame: &VideoFrame) -> bool {
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
                    "compositor: no implicit fence from the main frame: {}",
                    std::io::Error::last_os_error()
                );
            }
            return false;
        }
        let info = vk::ImportSemaphoreFdInfoKHR::default()
            .semaphore(self.acquire)
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

    unsafe fn submit_and_wait(&mut self, wait_acquire: bool) -> Result<()> {
        let d = &self.device;
        let cmds = [self.cmd];
        let sems = [self.acquire];
        let stages = [vk::PipelineStageFlags::COMPUTE_SHADER];
        let mut submit = vk::SubmitInfo::default().command_buffers(&cmds);
        if wait_acquire {
            submit = submit.wait_semaphores(&sems).wait_dst_stage_mask(&stages);
        }
        unsafe {
            d.queue_submit(self.queue, &[submit], self.fence)
                .map_err(err("submit"))?;
            let r = d.wait_for_fences(&[self.fence], true, FENCE_TIMEOUT.as_nanos() as u64);
            if let Err(e) = r {
                self.lost = true;
                return Err(Error(format!("wait for the GPU: {e}")));
            }
            d.reset_fences(&[self.fence]).map_err(err("reset fence"))
        }
    }

    /// Index in `imports` of `frame`'s image, importing it on first sight.
    unsafe fn import(&mut self, frame: &VideoFrame) -> Result<usize> {
        if self.import_generation != Some(frame.pool_generation) {
            // A new producer pool: the old buffers are gone (nothing is in
            // flight, `compose` waits for the GPU).
            self.drop_imports();
            self.import_generation = Some(frame.pool_generation);
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
            inode(&first.fd)?,
        );
        if let Some(i) = self
            .imports
            .iter()
            .position(|i| i.buffer_id == frame.buffer_id)
        {
            if self.imports[i].key == key {
                return Ok(i);
            }
            let old = self.imports.swap_remove(i);
            unsafe { old.image.destroy(&self.device) };
        }
        unsafe { self.check_importable(frame)? };
        let image = unsafe { self.import_dmabuf(frame)? };
        self.imports.push(Import {
            buffer_id: frame.buffer_id,
            key,
            image,
        });
        Ok(self.imports.len() - 1)
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
        let types = &self.memory.memory_types[..self.memory.memory_type_count as usize];
        (0..types.len() as u32)
            .find(|&i| bits & (1 << i) != 0 && types[i as usize].property_flags.contains(flags))
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
                        self.memory_type(
                            reqs.memory_type_bits,
                            host | vk::MemoryPropertyFlags::HOST_CACHED,
                        )
                    })
                    .flatten()
                    .or_else(|| self.memory_type(reqs.memory_type_bits, host))
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

    /// Uploads premultiplied RGBA with a CPU-built mip chain (a compute
    /// queue cannot blit).
    unsafe fn upload(&mut self, width: u32, height: u32, rgba: &[u8]) -> Result<Texture> {
        let levels = 32 - width.max(height).leading_zeros();
        let mut chain = vec![(rgba.to_vec(), width as usize, height as usize)];
        while chain.len() < levels as usize {
            let (p, w, h) = chain.last().expect("level 0 is there");
            chain.push(half(p, *w, *h));
        }
        let total: usize = chain.iter().map(|(p, _, _)| p.len()).sum();
        let ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(levels)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        unsafe {
            let staging = self.buffer(total as u64, vk::BufferUsageFlags::TRANSFER_SRC, false)?;
            let image = match self.image(&ci, false) {
                Ok(i) => i,
                Err(e) => {
                    staging.destroy(&self.device);
                    return Err(e);
                }
            };
            let mut regions = Vec::new();
            let mut offset = 0;
            for (level, (p, w, h)) in chain.iter().enumerate() {
                std::ptr::copy_nonoverlapping(p.as_ptr(), staging.ptr.add(offset), p.len());
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
            let r = self.record_upload(staging.buffer, image.image, levels, &regions);
            let r = r.and_then(|()| self.submit_and_wait(false));
            staging.destroy(&self.device);
            match r {
                Ok(()) => Ok(Texture {
                    image,
                    width,
                    height,
                }),
                Err(e) => {
                    if !self.lost {
                        image.destroy(&self.device);
                    }
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
        let cmd = self.cmd;
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
            if let Some(p) = &self.preview {
                p.image.destroy(d);
                for b in &p.buffers {
                    b.destroy(d);
                }
            }
            for s in &self.ring {
                s.image.destroy(d);
            }
            for i in &self.imports {
                i.image.destroy(d);
            }
            for t in self.textures.values() {
                t.image.destroy(d);
            }
            self.dummy.image.destroy(d);
            for b in &self.params {
                b.destroy(d);
            }
            d.destroy_descriptor_pool(self.descriptor_pool, None);
            d.destroy_pipeline(self.pipeline, None);
            d.destroy_pipeline_layout(self.pipeline_layout, None);
            d.destroy_descriptor_set_layout(self.set_layout, None);
            d.destroy_sampler(self.sampler, None);
            if let Some((pool, _)) = self.queries {
                d.destroy_query_pool(pool, None);
            }
            d.destroy_fence(self.fence, None);
            d.destroy_semaphore(self.acquire, None);
            d.destroy_command_pool(self.pool, None);
            d.destroy_device(None);
            self.instance.destroy_instance(None);
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
        assert_eq!(l.misc[1], 2.0);
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
}
