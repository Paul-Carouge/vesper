//! Per-swap capture: ownership, the export ring and the blit.
//!
//! One GL context per process is captured (the first to swap a window of
//! 640x360 or more; another one takes over when it stops swapping). On each
//! swap of that context, while Vesper captures this game and paced to the
//! recording rate (`protocol::Pacer`), the back buffer is blitted (flipped:
//! GL is bottom-up) into a free ring image, the GL state the blit touches
//! is restored, and the frame goes to Vesper with a sync_file that signals
//! when the blit is done. No ring image free (Vesper is slow): the frame is
//! skipped, never waited.
//!
//! The sync_file: each ring image has a DRM syncobj, imported into the
//! context as a GL semaphore (`GL_EXT_semaphore_fd`) and signaled after the
//! blit. The driver attaches the blit's fence to it when it submits the
//! work (its own thread, Mesa): the frame is sent once that happened,
//! checked without waiting on this swap (Mesa has always submitted by then)
//! or else on a later swap. (The
//! DMA-BUF's own fences cannot be used: Mesa's radeonsi leaves none there.)
//!
//! The ring images are linear GBM buffers on the context's GPU, imported into
//! the game's context as GL memory objects (`GL_EXT_memory_object_fd`), which
//! works the same for GLX and EGL. Their width is rounded up to 64 pixels so
//! that the GL driver and GBM agree on the row pitch without being told.

use std::ffi::{CStr, c_char, c_int, c_uint, c_ulong, c_void};
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use vesper_gamecapture::client::{self as ipc, IPC, LockExt, debug, now_ns, warn};
use vesper_gamecapture::protocol::{self, Msg, Plane};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Api {
    Glx,
    Egl,
}

/// Smallest window worth capturing (skips launchers' splash windows and the
/// like).
const MIN_SIZE: (u32, u32) = (640, 360);
const RING: usize = protocol::RING as usize;
/// At most one ring (re)creation per context in this time.
const RING_RETRY: Duration = Duration::from_secs(1);
/// A captured context that has not swapped for this long gives way.
const STALE: Duration = Duration::from_secs(2);
/// Ring image width alignment, in pixels: 256 bytes, the largest linear
/// pitch alignment of the GPUs we know, so no driver pads the rows.
const WIDTH_ALIGN: u32 = 64;
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const FALLBACK_NODE: &str = "/dev/dri/renderD128";
/// A copy not submitted after this long is dropped.
const PENDING_MAX: Duration = Duration::from_millis(100);

// --- GL, GLX and EGL entry points ---

const GL_VERSION: u32 = 0x1F02;
const GL_EXTENSIONS: u32 = 0x1F03;
const GL_NUM_EXTENSIONS: u32 = 0x821D;
const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_TEXTURE_BINDING_2D: u32 = 0x8069;
const GL_RGBA8: u32 = 0x8058;
const GL_SRGB8_ALPHA8: u32 = 0x8C43;
const GL_READ_FRAMEBUFFER: u32 = 0x8CA8;
const GL_DRAW_FRAMEBUFFER: u32 = 0x8CA9;
const GL_READ_FRAMEBUFFER_BINDING: u32 = 0x8CAA;
const GL_DRAW_FRAMEBUFFER_BINDING: u32 = 0x8CA6;
const GL_READ_BUFFER: u32 = 0x0C02;
const GL_BACK: u32 = 0x0405;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_NEAREST: u32 = 0x2600;
const GL_SCISSOR_TEST: u32 = 0x0C11;
const GL_FRAMEBUFFER_SRGB: u32 = 0x8DB9;
const GL_SAMPLE_BUFFERS: u32 = 0x80A8;
const GL_TEXTURE_TILING_EXT: u32 = 0x9580;
const GL_LINEAR_TILING_EXT: i32 = 0x9585;
const GL_HANDLE_TYPE_OPAQUE_FD_EXT: u32 = 0x9586;
const GL_LAYOUT_GENERAL_EXT: u32 = 0x958D;

const GLX_WIDTH: c_int = 0x801D;
const GLX_HEIGHT: c_int = 0x801E;
const GLX_RENDERER_VENDOR_ID_MESA: c_int = 0x8183;
const GLX_RENDERER_DEVICE_ID_MESA: c_int = 0x8184;

const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_DRAW: i32 = 0x3059;
const EGL_GL_COLORSPACE: i32 = 0x309D;
const EGL_GL_COLORSPACE_SRGB: i32 = 0x3089;
const EGL_DEVICE_EXT: i32 = 0x322C;
const EGL_DRM_RENDER_NODE_FILE_EXT: i32 = 0x3377;

/// Declares a table of function pointers loaded by name, all required.
macro_rules! fns {
    ($table:ident { $($field:ident: fn($($arg:ty),*) $(-> $ret:ty)? = $name:literal;)* }) => {
        struct $table {
            $($field: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }
        impl $table {
            fn load(get: &dyn Fn(&CStr) -> *mut c_void) -> Option<Self> {
                Some(Self {
                    $($field: {
                        let p = get($name);
                        if p.is_null() {
                            debug(&format!("missing {:?}", $name));
                            return None;
                        }
                        unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) $(-> $ret)?>(p) }
                    },)*
                })
            }
        }
    };
}

fns!(Gl {
    get_string: fn(u32) -> *const c_char = c"glGetString";
    get_stringi: fn(u32, u32) -> *const c_char = c"glGetStringi";
    get_integerv: fn(u32, *mut i32) = c"glGetIntegerv";
    is_enabled: fn(u32) -> u8 = c"glIsEnabled";
    enable: fn(u32) = c"glEnable";
    disable: fn(u32) = c"glDisable";
    flush: fn() = c"glFlush";
    read_buffer: fn(u32) = c"glReadBuffer";
    gen_textures: fn(i32, *mut u32) = c"glGenTextures";
    delete_textures: fn(i32, *const u32) = c"glDeleteTextures";
    bind_texture: fn(u32, u32) = c"glBindTexture";
    tex_parameteri: fn(u32, u32, i32) = c"glTexParameteri";
    gen_framebuffers: fn(i32, *mut u32) = c"glGenFramebuffers";
    delete_framebuffers: fn(i32, *const u32) = c"glDeleteFramebuffers";
    bind_framebuffer: fn(u32, u32) = c"glBindFramebuffer";
    framebuffer_texture_2d: fn(u32, u32, u32, u32, i32) = c"glFramebufferTexture2D";
    check_framebuffer_status: fn(u32) -> u32 = c"glCheckFramebufferStatus";
    blit_framebuffer: fn(i32, i32, i32, i32, i32, i32, i32, i32, u32, u32) = c"glBlitFramebuffer";
    create_memory_objects: fn(i32, *mut u32) = c"glCreateMemoryObjectsEXT";
    delete_memory_objects: fn(i32, *const u32) = c"glDeleteMemoryObjectsEXT";
    import_memory_fd: fn(u32, u64, u32, c_int) = c"glImportMemoryFdEXT";
    tex_storage_mem_2d: fn(u32, i32, u32, i32, i32, u32, u64) = c"glTexStorageMem2DEXT";
    gen_semaphores: fn(i32, *mut u32) = c"glGenSemaphoresEXT";
    delete_semaphores: fn(i32, *const u32) = c"glDeleteSemaphoresEXT";
    import_semaphore_fd: fn(u32, u32, c_int) = c"glImportSemaphoreFdEXT";
    signal_semaphore: fn(u32, u32, *const u32, u32, *const u32, *const u32) = c"glSignalSemaphoreEXT";
});

fns!(Glx {
    current_context: fn() -> *mut c_void = c"glXGetCurrentContext";
    current_drawable: fn() -> c_ulong = c"glXGetCurrentDrawable";
    query_drawable: fn(*mut c_void, c_ulong, c_int, *mut c_uint) = c"glXQueryDrawable";
});

fns!(Egl {
    current_context: fn() -> *mut c_void = c"eglGetCurrentContext";
    current_surface: fn(i32) -> *mut c_void = c"eglGetCurrentSurface";
    query_surface: fn(*mut c_void, *mut c_void, i32, *mut i32) -> u32 = c"eglQuerySurface";
});

type GlxQueryRenderer = unsafe extern "C" fn(c_int, *mut c_uint) -> c_int;
type EglQueryDisplayAttrib = unsafe extern "C" fn(*mut c_void, i32, *mut isize) -> u32;
type EglQueryDeviceString = unsafe extern "C" fn(*mut c_void, i32) -> *const c_char;

enum Wsi {
    Glx(Glx, Option<GlxQueryRenderer>),
    Egl(Egl, Option<(EglQueryDisplayAttrib, EglQueryDeviceString)>),
}

struct Funcs {
    gl: Gl,
    wsi: Wsi,
}

// SAFETY: function pointers only.
unsafe impl Send for Funcs {}
unsafe impl Sync for Funcs {}

impl Funcs {
    fn load(api: Api) -> Option<Funcs> {
        let gpa = crate::get_proc_address(api)?;
        let get = |name: &CStr| unsafe { gpa(name.as_ptr()) };
        let opt = |name: &CStr| {
            let p = get(name);
            (!p.is_null()).then_some(p)
        };
        let wsi = match api {
            Api::Glx => Wsi::Glx(
                Glx::load(&get)?,
                opt(c"glXQueryCurrentRendererIntegerMESA")
                    .map(|p| unsafe { std::mem::transmute::<*mut c_void, GlxQueryRenderer>(p) }),
            ),
            Api::Egl => Wsi::Egl(
                Egl::load(&get)?,
                opt(c"eglQueryDisplayAttribEXT")
                    .zip(opt(c"eglQueryDeviceStringEXT"))
                    .map(|(a, b)| unsafe {
                        (
                            std::mem::transmute::<*mut c_void, EglQueryDisplayAttrib>(a),
                            std::mem::transmute::<*mut c_void, EglQueryDeviceString>(b),
                        )
                    }),
            ),
        };
        Some(Funcs {
            gl: Gl::load(&get)?,
            wsi,
        })
    }

    fn get(api: Api) -> Option<&'static Funcs> {
        static GLX: OnceLock<Option<Funcs>> = OnceLock::new();
        static EGL: OnceLock<Option<Funcs>> = OnceLock::new();
        let cell = match api {
            Api::Glx => &GLX,
            Api::Egl => &EGL,
        };
        cell.get_or_init(|| Funcs::load(api)).as_ref()
    }

    fn current_context(&self) -> usize {
        unsafe {
            match &self.wsi {
                Wsi::Glx(f, _) => (f.current_context)() as usize,
                Wsi::Egl(f, _) => (f.current_context)() as usize,
            }
        }
    }

    /// Whether `drawable` is the current context's draw surface (the one
    /// whose back buffer the blit reads).
    fn is_current(&self, drawable: usize) -> bool {
        unsafe {
            match &self.wsi {
                Wsi::Glx(f, _) => (f.current_drawable)() as usize == drawable,
                Wsi::Egl(f, _) => (f.current_surface)(EGL_DRAW) as usize == drawable,
            }
        }
    }

    fn size(&self, dpy: *mut c_void, drawable: usize) -> Option<(u32, u32)> {
        unsafe {
            match &self.wsi {
                Wsi::Glx(f, _) => {
                    let (mut w, mut h) = (0, 0);
                    (f.query_drawable)(dpy, drawable as c_ulong, GLX_WIDTH, &mut w);
                    (f.query_drawable)(dpy, drawable as c_ulong, GLX_HEIGHT, &mut h);
                    Some((w, h))
                }
                Wsi::Egl(f, _) => {
                    let (mut w, mut h) = (0, 0);
                    let s = drawable as *mut c_void;
                    let ok = (f.query_surface)(dpy, s, EGL_WIDTH, &mut w) != 0
                        && (f.query_surface)(dpy, s, EGL_HEIGHT, &mut h) != 0;
                    ok.then(|| (w.max(0) as u32, h.max(0) as u32))
                }
            }
        }
    }

    /// Whether `drawable` is an sRGB EGL surface.
    fn srgb_surface(&self, dpy: *mut c_void, drawable: usize) -> bool {
        let Wsi::Egl(f, _) = &self.wsi else {
            return false;
        };
        let mut cs = 0;
        let ok =
            unsafe { (f.query_surface)(dpy, drawable as *mut c_void, EGL_GL_COLORSPACE, &mut cs) };
        ok != 0 && cs == EGL_GL_COLORSPACE_SRGB
    }

    /// Render node of the GPU the current context renders on.
    fn render_node(&self, dpy: *mut c_void) -> PathBuf {
        let node = unsafe {
            match &self.wsi {
                Wsi::Glx(_, Some(query)) => {
                    let (mut vendor, mut device) = (0, 0);
                    if query(GLX_RENDERER_VENDOR_ID_MESA, &mut vendor) != 0
                        && query(GLX_RENDERER_DEVICE_ID_MESA, &mut device) != 0
                    {
                        node_for_pci(Path::new("/sys/class/drm"), vendor, device)
                    } else {
                        None
                    }
                }
                Wsi::Egl(_, Some((attrib, string))) => {
                    let mut dev = 0isize;
                    if attrib(dpy, EGL_DEVICE_EXT, &mut dev) != 0 && dev != 0 {
                        let s = string(dev as *mut c_void, EGL_DRM_RENDER_NODE_FILE_EXT);
                        (!s.is_null()).then(|| {
                            PathBuf::from(CStr::from_ptr(s).to_string_lossy().into_owned())
                        })
                    } else {
                        None
                    }
                }
                _ => None,
            }
        };
        node.unwrap_or_else(|| PathBuf::from(FALLBACK_NODE))
    }
}

/// The render node (`/dev/dri/renderD*`) of the PCI device `vendor:device`,
/// from `drm` (`/sys/class/drm`). The first one if several match (two
/// identical GPUs): only a matter of copy cost, a linear buffer works on any.
fn node_for_pci(drm: &Path, vendor: u32, device: u32) -> Option<PathBuf> {
    let read = |p: PathBuf| {
        let s = std::fs::read_to_string(p).ok()?;
        u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()
    };
    let mut names: Vec<String> = std::fs::read_dir(drm)
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("renderD"))
        .collect();
    names.sort();
    names
        .into_iter()
        .find(|n| {
            let dev = drm.join(n).join("device");
            read(dev.join("vendor")) == Some(vendor) && read(dev.join("device")) == Some(device)
        })
        .map(|n| Path::new("/dev/dri").join(n))
}

/// Mesa names itself in `GL_VERSION` ("4.6 (Compatibility Profile) Mesa 26.2.4").
fn is_mesa(version: &str) -> bool {
    version.split_whitespace().any(|w| w == "Mesa")
}

/// `(major, ES)` from `GL_VERSION`: "4.6 (Compatibility Profile) Mesa ...",
/// "OpenGL ES 3.2 Mesa ...".
fn parse_version(v: &str) -> Option<(u32, bool)> {
    let (rest, es) = match v.strip_prefix("OpenGL ES") {
        Some(r) => (r.trim_start_matches(['-', 'C', 'M', ' ']), true),
        None => (v, false),
    };
    let major = rest.split('.').next()?.trim().parse().ok()?;
    Some((major, es))
}

// --- GBM ---

fns!(GbmLib {
    create_device: fn(c_int) -> *mut c_void = c"gbm_create_device";
    bo_create: fn(*mut c_void, u32, u32, u32, u32) -> *mut c_void = c"gbm_bo_create";
    bo_get_fd: fn(*mut c_void) -> c_int = c"gbm_bo_get_fd";
    bo_get_stride: fn(*mut c_void) -> u32 = c"gbm_bo_get_stride";
    bo_get_offset: fn(*mut c_void, c_int) -> u32 = c"gbm_bo_get_offset";
    bo_destroy: fn(*mut c_void) = c"gbm_bo_destroy";
});

// SAFETY: function pointers only.
unsafe impl Send for GbmLib {}
unsafe impl Sync for GbmLib {}

const GBM_BO_USE_RENDERING: u32 = 1 << 2;
const GBM_BO_USE_LINEAR: u32 = 1 << 4;

/// The process's GBM device, kept for its lifetime. Opened before `STATE`
/// is taken, never under it: loading libgbm and `gbm_create_device` (which
/// loads Mesa's backend) take the loader lock, which a thread in a library
/// destructor may hold while it destroys a context (and waits on `STATE`).
///
/// shortcut: one device per process, on the GPU of the first context
/// captured; a context on another GPU still works (linear buffers import
/// anywhere), only with a cross-GPU copy.
static DEVICE: OnceLock<Result<GbmDevice, String>> = OnceLock::new();

fn open_device(f: &Funcs, dpy: *mut c_void) {
    DEVICE.get_or_init(|| GbmDevice::open(&f.render_node(dpy)));
}

/// A GBM device on a render node.
struct GbmDevice {
    lib: &'static GbmLib,
    dev: *mut c_void,
    /// The render node: also the DRM device of the syncobjs.
    node: File,
    path: PathBuf,
}

impl GbmDevice {
    fn open(node: &Path) -> Result<GbmDevice, String> {
        static LIB: OnceLock<Option<GbmLib>> = OnceLock::new();
        let lib = LIB
            .get_or_init(|| {
                // Kept loaded (never dlclosed).
                let h = unsafe {
                    libc::dlopen(c"libgbm.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL)
                };
                if h.is_null() {
                    warn("cannot load libgbm.so.1: capture off");
                    return None;
                }
                GbmLib::load(&|name| crate::real_dlsym(h, name))
            })
            .as_ref()
            .ok_or("no libgbm")?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(node)
            .map_err(|e| format!("open {}: {e}", node.display()))?;
        let dev = unsafe { (lib.create_device)(file.as_raw_fd()) };
        if dev.is_null() {
            return Err(format!("no GBM device on {}", node.display()));
        }
        Ok(GbmDevice {
            lib,
            dev,
            node: file,
            path: node.to_owned(),
        })
    }
}

// --- Ring ---

struct Slot {
    bo: *mut c_void,
    dmabuf: OwnedFd,
    /// GL names, in the owning context.
    mem: u32,
    tex: u32,
    fbo: u32,
    /// DRM syncobj (on the GBM device's node), and its GL semaphore.
    syncobj: u32,
    sem: u32,
    /// Not held by Vesper.
    free: bool,
    /// Its copy was never submitted: never used again in this ring.
    retired: bool,
}

struct Ring {
    /// `Start` it serves: the server forgets every buffer on a new `Start`.
    start_id: u64,
    generation: u32,
    size: (u32, u32),
    /// OpenGL ES context (no `GL_FRAMEBUFFER_SRGB`).
    es: bool,
    /// Storage format of the ring textures: `GL_SRGB8_ALPHA8` for an ES
    /// context on an sRGB EGL surface (ES blits decode an sRGB source; an
    /// sRGB target encodes back, so the bytes are copied unchanged).
    format: u32,
    gbm: &'static GbmDevice,
    slots: Vec<Slot>,
    pacer: protocol::Pacer,
    /// Oldest copy not yet seen done: (its sync_file, swap time), for the pacer.
    timing: Option<(OwnedFd, u64)>,
    /// A copy waiting for the driver to submit it.
    pending: Option<Pending>,
    /// A frame went out: the semaphore path works.
    sent_any: bool,
}

#[derive(Clone, Copy)]
struct Pending {
    index: usize,
    /// Swap time, for the pacer.
    at: u64,
    pts_ns: u64,
}

/// Ring generation, unique in the process.
static GENERATION: AtomicU32 = AtomicU32::new(0);

impl Ring {
    /// Frees the ring. `gl`: the owning context is current, so its GL
    /// objects are deleted too; otherwise they go with the context.
    fn destroy(self, gl: Option<&Gl>) {
        if let Some(gl) = gl {
            for s in &self.slots {
                unsafe {
                    (gl.delete_framebuffers)(1, &s.fbo);
                    (gl.delete_textures)(1, &s.tex);
                    (gl.delete_memory_objects)(1, &s.mem);
                    (gl.delete_semaphores)(1, &s.sem);
                }
            }
        }
    }

    /// Sends the frame of `p` if the driver submitted its blit, waiting up
    /// to `wait_ns` for that. False: not submitted yet. True: sent, or
    /// dropped (no sync_file, or the socket full).
    fn finish(&mut self, p: Pending, wait_ns: u64) -> bool {
        let node = &self.gbm.node;
        let slot = &mut self.slots[p.index];
        if !syncobj_wait_submitted(node, slot.syncobj, wait_ns) {
            return false;
        }
        let Some(sync) = syncobj_sync_file(node, slot.syncobj) else {
            debug("cannot export the copy's sync_file: frame dropped");
            return true;
        };
        let msg = Msg::Frame {
            generation: self.generation,
            index: p.index as u32,
            pts_ns: p.pts_ns,
        };
        if ipc::send(&msg, Some(sync.as_fd())) {
            slot.free = false;
            self.sent_any = true;
            self.pacer.copied(p.at);
            if self.timing.is_none() {
                self.timing = Some((sync, p.at));
            }
        }
        true
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // Before the GBM device (a field, dropped after this).
        for s in self.slots.drain(..) {
            unsafe { (self.gbm.lib.bo_destroy)(s.bo) };
            if s.syncobj != 0 {
                syncobj_destroy(&self.gbm.node, s.syncobj);
            }
        }
    }
}

/// The captured context.
struct Owner {
    api: Api,
    ctx: usize,
    size: (u32, u32),
    /// Connection the size was announced on (0: never).
    announced: u64,
    last_swap: Instant,
    ring: Option<Ring>,
    /// Earliest next ring (re)creation (rate limit, also after failures).
    next_ring: Option<Instant>,
}

#[derive(Default)]
struct State {
    owner: Option<Owner>,
    /// Contexts that cannot be captured (missing extension, multisampled
    /// window, failed import): never tried again.
    disabled: Vec<(Api, usize)>,
}

static STATE: Mutex<State> = Mutex::new(State {
    owner: None,
    disabled: Vec::new(),
});
/// A ring exists: checked on the dormant path so a ring never outlives the
/// connection in VRAM.
static HAS_RING: AtomicBool = AtomicBool::new(false);

fn announce(size: (u32, u32)) -> bool {
    ipc::send(
        &Msg::Swapchain {
            width: size.0,
            height: size.1,
        },
        None,
    )
}

/// Runs before the real swap, on the game's render thread.
pub fn before_swap(api: Api, dpy: *mut c_void, drawable: usize) {
    if ipc::passive() {
        return;
    }
    ipc::ensure_thread();
    // Dormant: Vesper is not running.
    if !IPC.connected.load(Ordering::Relaxed) {
        if HAS_RING.load(Ordering::Relaxed) {
            drop_ring_if_current(api);
        }
        return;
    }
    let Some(f) = Funcs::get(api) else { return };
    let ctx = f.current_context();
    if ctx == 0 || !f.is_current(drawable) {
        return;
    }
    if IPC.capturing.load(Ordering::Relaxed) {
        open_device(f, dpy);
    }
    let mut st = STATE.locked();
    if st.disabled.contains(&(api, ctx)) {
        return;
    }
    let Some(size) = f.size(dpy, drawable) else {
        return;
    };
    let conn = IPC.conn_id.load(Ordering::SeqCst);
    let small = size.0 < MIN_SIZE.0 || size.1 < MIN_SIZE.1;
    let now = Instant::now();
    let mine = st
        .owner
        .as_ref()
        .is_some_and(|o| (o.api, o.ctx) == (api, ctx));
    if !mine {
        let busy = st
            .owner
            .as_ref()
            .is_some_and(|o| now.duration_since(o.last_swap) < STALE);
        if small || busy {
            return;
        }
        if let Some(old) = st.owner.take() {
            // shortcut: the old context is not current, so its ring's GL
            // objects (and the buffers they import) stay until it is
            // destroyed; fine for the rare game that swaps a new context.
            if let Some(ring) = old.ring {
                ring.destroy(None);
            }
        }
        st.owner = Some(Owner {
            api,
            ctx,
            size: (0, 0),
            announced: 0,
            last_swap: now,
            ring: None,
            next_ring: None,
        });
    }
    let owner = st.owner.as_mut().expect("set above");
    owner.last_swap = now;
    if small {
        // The window shrank below capture size: give way.
        let owner = st.owner.take().expect("set above");
        if let Some(ring) = owner.ring {
            ring.destroy(Some(&f.gl));
        }
        if owner.announced == conn {
            announce((0, 0));
        }
        return;
    }
    if owner.announced != conn || owner.size != size {
        if !announce(size) {
            return;
        }
        owner.announced = conn;
        owner.size = size;
    }
    if !IPC.capturing.load(Ordering::SeqCst) {
        if let Some(ring) = owner.ring.take() {
            ring.destroy(Some(&f.gl));
        }
        return;
    }
    if let Err(e) = capture(f, owner, dpy, drawable) {
        warn(&format!("capture off for this OpenGL context: {e}"));
        let owner = st.owner.take().expect("set above");
        if let Some(ring) = owner.ring {
            ring.destroy(Some(&f.gl));
        }
        if owner.announced == conn {
            announce((0, 0));
        }
        st.disabled.push((api, ctx));
    }
    HAS_RING.store(
        st.owner.as_ref().is_some_and(|o| o.ring.is_some()),
        Ordering::Relaxed,
    );
}

/// Vesper went away: frees the ring once its context is current again.
fn drop_ring_if_current(api: Api) {
    let Some(f) = Funcs::get(api) else { return };
    let ctx = f.current_context();
    let mut st = STATE.locked();
    if let Some(o) = st.owner.as_mut().filter(|o| (o.api, o.ctx) == (api, ctx))
        && let Some(ring) = o.ring.take()
    {
        ring.destroy(Some(&f.gl));
    }
    HAS_RING.store(
        st.owner.as_ref().is_some_and(|o| o.ring.is_some()),
        Ordering::Relaxed,
    );
}

/// Before the game destroys a context: forget it (the context frees its GL
/// objects).
pub fn context_destroyed(api: Api, ctx: usize) {
    let mut st = STATE.locked();
    st.disabled.retain(|d| *d != (api, ctx));
    if st
        .owner
        .as_ref()
        .is_some_and(|o| (o.api, o.ctx) == (api, ctx))
    {
        let owner = st.owner.take().expect("checked above");
        if let Some(ring) = owner.ring {
            ring.destroy(None);
        }
        HAS_RING.store(false, Ordering::Relaxed);
        if owner.announced == IPC.conn_id.load(Ordering::SeqCst) {
            announce((0, 0));
        }
    }
}

/// Copies this swap's frame if it is due. Err disables capture for the
/// context.
fn capture(f: &Funcs, owner: &mut Owner, dpy: *mut c_void, drawable: usize) -> Result<(), String> {
    let start = IPC.start_id.load(Ordering::SeqCst);
    if owner
        .ring
        .as_ref()
        .is_some_and(|r| r.start_id != start || r.size != owner.size)
    {
        // Restarted by the server, or the window was resized.
        let ring = owner.ring.take().expect("checked above");
        ring.destroy(Some(&f.gl));
    }
    if owner.ring.is_none() {
        let now = Instant::now();
        if owner.next_ring.is_some_and(|t| now < t) {
            return Ok(());
        }
        owner.next_ring = Some(now + RING_RETRY);
        match create_ring(f, owner.size, start, dpy, drawable)? {
            Some(r) => owner.ring = Some(r),
            // Socket full: try again later.
            None => return Ok(()),
        }
    }
    let ring = owner.ring.as_mut().expect("created above");
    let now = now_ns();
    // A copy from an earlier swap still waiting for its submission.
    if let Some(p) = ring.pending.take()
        && !ring.finish(p, 0)
    {
        if now.saturating_sub(p.at) < PENDING_MAX.as_nanos() as u64 {
            ring.pending = Some(p);
            return Ok(());
        }
        if !ring.sent_any {
            return Err("the driver never signaled the copy's semaphore".into());
        }
        // Its syncobj may still get this copy's fence: not reused in this ring.
        let slot = &mut ring.slots[p.index];
        (slot.free, slot.retired) = (false, true);
        debug("copy never submitted: frame dropped, ring image retired");
        if ring.slots.iter().all(|s| s.retired) {
            // Nothing left to copy into: a new ring after RING_RETRY.
            let ring = owner.ring.take().expect("checked above");
            ring.destroy(Some(&f.gl));
            return Ok(());
        }
    }
    if let Some((sync, at)) = &ring.timing
        && signaled(sync.as_fd())
    {
        ring.pacer.latency(now - at);
        ring.timing = None;
    }
    if !ring.pacer.due(now) {
        return Ok(());
    }
    for (g, i) in ipc::take_releases() {
        if g == ring.generation
            && let Some(s) = ring.slots.get_mut(i as usize)
        {
            s.free = true;
        }
    }
    let Some(index) = ring.slots.iter().position(|s| s.free) else {
        debug("no free ring image: frame skipped");
        return Ok(());
    };
    let (w, h) = (ring.size.0 as i32, ring.size.1 as i32);
    let slot = &ring.slots[index];
    // Drops the fence of this slot's previous copy (Vesper waited for it
    // before releasing the slot), so "available" means this copy's.
    syncobj_reset(&ring.gbm.node, slot.syncobj)?;
    unsafe {
        blit(&f.gl, ring.es, slot.fbo, w, h);
        let layout = GL_LAYOUT_GENERAL_EXT;
        (f.gl.signal_semaphore)(slot.sem, 0, std::ptr::null(), 1, &slot.tex, &layout);
        (f.gl.flush)();
    }
    let p = Pending {
        index,
        at: now,
        pts_ns: now_ns(),
    };
    // Never waited for: the kernel rounds any timeout up to a jiffy (ms).
    if !ring.finish(p, 0) {
        // Sent on a later swap.
        ring.pending = Some(p);
    }
    Ok(())
}

/// Blits the default framebuffer's back buffer into `fbo`, flipped, and
/// restores every piece of GL state it changes.
unsafe fn blit(gl: &Gl, es: bool, fbo: u32, w: i32, h: i32) {
    unsafe {
        let (mut read_fb, mut draw_fb, mut read_buf) = (0, 0, 0);
        (gl.get_integerv)(GL_READ_FRAMEBUFFER_BINDING, &mut read_fb);
        (gl.get_integerv)(GL_DRAW_FRAMEBUFFER_BINDING, &mut draw_fb);
        let scissor = (gl.is_enabled)(GL_SCISSOR_TEST) != 0;
        let srgb = !es && (gl.is_enabled)(GL_FRAMEBUFFER_SRGB) != 0;
        (gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, 0);
        (gl.get_integerv)(GL_READ_BUFFER, &mut read_buf);
        (gl.read_buffer)(GL_BACK);
        (gl.bind_framebuffer)(GL_DRAW_FRAMEBUFFER, fbo);
        if scissor {
            (gl.disable)(GL_SCISSOR_TEST);
        }
        if srgb {
            (gl.disable)(GL_FRAMEBUFFER_SRGB);
        }
        (gl.blit_framebuffer)(0, 0, w, h, 0, h, w, 0, GL_COLOR_BUFFER_BIT, GL_NEAREST);
        if srgb {
            (gl.enable)(GL_FRAMEBUFFER_SRGB);
        }
        if scissor {
            (gl.enable)(GL_SCISSOR_TEST);
        }
        (gl.read_buffer)(read_buf as u32);
        (gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, read_fb as u32);
        (gl.bind_framebuffer)(GL_DRAW_FRAMEBUFFER, draw_fb as u32);
    }
}

/// Whether the current context can be captured: GL 3 or ES 3 (for
/// glBlitFramebuffer), `GL_EXT_memory_object_fd` and `GL_EXT_semaphore_fd`, a single-sampled window
/// (a multisampled one cannot be blitted flipped). Ok(whether it is ES).
unsafe fn check_context(gl: &Gl) -> Result<bool, String> {
    unsafe {
        let v = (gl.get_string)(GL_VERSION);
        if v.is_null() {
            return Err("no GL version".into());
        }
        let v = CStr::from_ptr(v).to_string_lossy();
        // Importing a DMA-BUF and a DRM syncobj as "opaque" fds is what
        // Mesa does; other drivers (NVIDIA) mean something else by them.
        if !is_mesa(&v) {
            return Err(format!("not a Mesa driver ({v})"));
        }
        let (major, es) = parse_version(&v).ok_or(format!("unknown GL version {v}"))?;
        if major < 3 {
            return Err(format!("GL {v} is older than 3.0"));
        }
        let mut n = 0;
        (gl.get_integerv)(GL_NUM_EXTENSIONS, &mut n);
        let has = |name: &[u8]| {
            (0..n.max(0) as u32).any(|i| {
                let e = (gl.get_stringi)(GL_EXTENSIONS, i);
                !e.is_null() && CStr::from_ptr(e).to_bytes() == name
            })
        };
        for ext in ["GL_EXT_memory_object_fd", "GL_EXT_semaphore_fd"] {
            if !has(ext.as_bytes()) {
                return Err(format!("no {ext}"));
            }
        }
        let mut draw_fb = 0;
        (gl.get_integerv)(GL_DRAW_FRAMEBUFFER_BINDING, &mut draw_fb);
        (gl.bind_framebuffer)(GL_DRAW_FRAMEBUFFER, 0);
        let mut samples = 0;
        (gl.get_integerv)(GL_SAMPLE_BUFFERS, &mut samples);
        (gl.bind_framebuffer)(GL_DRAW_FRAMEBUFFER, draw_fb as u32);
        if samples != 0 {
            return Err("multisampled window".into());
        }
        Ok(es)
    }
}

/// `Ok(None)` when Vesper could not take the buffers right now.
fn create_ring(
    f: &Funcs,
    size: (u32, u32),
    start: u64,
    dpy: *mut c_void,
    drawable: usize,
) -> Result<Option<Ring>, String> {
    let es = unsafe { check_context(&f.gl)? };
    let format = if es && f.srgb_surface(dpy, drawable) {
        GL_SRGB8_ALPHA8
    } else {
        GL_RGBA8
    };
    let gbm = match DEVICE.get() {
        // Opened on a later swap, before `STATE`.
        None => return Ok(None),
        Some(Err(e)) => return Err(e.clone()),
        Some(Ok(d)) => d,
    };
    let mut ring = Ring {
        start_id: start,
        generation: GENERATION.fetch_add(1, Ordering::SeqCst).wrapping_add(1),
        size,
        es,
        format,
        gbm,
        slots: Vec::new(),
        pacer: protocol::Pacer::new(ipc::fps()),
        timing: None,
        pending: None,
        sent_any: false,
    };
    for index in 0..RING {
        let msg = match create_slot(&f.gl, &mut ring, index as u32) {
            Ok(m) => m,
            Err(e) => {
                ring.destroy(Some(&f.gl));
                return Err(e);
            }
        };
        let fd = ring.slots[index].dmabuf.as_fd();
        if !ipc::send(&msg, Some(fd)) {
            ring.destroy(Some(&f.gl));
            return Ok(None);
        }
    }
    debug(&format!(
        "ring of {RING} {}x{} linear images on {}",
        size.0,
        size.1,
        gbm.path.display()
    ));
    Ok(Some(ring))
}

/// Creates one ring image, exported and imported into the current context.
/// Returns its `Buffer` message.
fn create_slot(gl: &Gl, ring: &mut Ring, index: u32) -> Result<Msg, String> {
    let (w, h) = ring.size;
    let width = w.div_ceil(WIDTH_ALIGN) * WIDTH_ALIGN;
    let lib = ring.gbm.lib;
    let bo = unsafe {
        (lib.bo_create)(
            ring.gbm.dev,
            width,
            h,
            protocol::XB24,
            GBM_BO_USE_LINEAR | GBM_BO_USE_RENDERING,
        )
    };
    if bo.is_null() {
        return Err(format!("cannot allocate a {width}x{h} linear buffer"));
    }
    let fd = unsafe { (lib.bo_get_fd)(bo) };
    if fd < 0 {
        unsafe { (lib.bo_destroy)(bo) };
        return Err("cannot export the buffer".into());
    }
    let dmabuf = unsafe { OwnedFd::from_raw_fd(fd) };
    // Pushed now so `Ring::destroy` frees whatever exists on error.
    ring.slots.push(Slot {
        bo,
        dmabuf,
        mem: 0,
        tex: 0,
        fbo: 0,
        syncobj: 0,
        sem: 0,
        free: true,
        retired: false,
    });
    let slot = ring.slots.last_mut().expect("pushed above");
    let (stride, offset) = unsafe { ((lib.bo_get_stride)(bo), (lib.bo_get_offset)(bo, 0)) };
    if stride != width * 4 {
        return Err(format!(
            "linear pitch {stride} is not {} (width {width})",
            width * 4
        ));
    }
    let len = unsafe { libc::lseek(slot.dmabuf.as_raw_fd(), 0, libc::SEEK_END) };
    if len <= 0 {
        return Err("cannot size the buffer".into());
    }
    // The GL driver takes ownership of its own copy on success.
    let import_fd = slot
        .dmabuf
        .try_clone()
        .map_err(|e| format!("cannot duplicate the buffer fd: {e}"))?;
    slot.syncobj = syncobj_create(&ring.gbm.node)?;
    let sem_fd =
        syncobj_fd(&ring.gbm.node, slot.syncobj, false).ok_or("cannot export a syncobj")?;
    let complete = unsafe {
        (gl.gen_semaphores)(1, &mut slot.sem);
        // GL takes ownership of the fd.
        (gl.import_semaphore_fd)(slot.sem, GL_HANDLE_TYPE_OPAQUE_FD_EXT, sem_fd.into_raw_fd());
        (gl.create_memory_objects)(1, &mut slot.mem);
        (gl.import_memory_fd)(
            slot.mem,
            len as u64,
            GL_HANDLE_TYPE_OPAQUE_FD_EXT,
            import_fd.into_raw_fd(),
        );
        let (mut tex_binding, mut draw_fb) = (0, 0);
        (gl.get_integerv)(GL_TEXTURE_BINDING_2D, &mut tex_binding);
        (gl.gen_textures)(1, &mut slot.tex);
        (gl.bind_texture)(GL_TEXTURE_2D, slot.tex);
        (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_TILING_EXT, GL_LINEAR_TILING_EXT);
        (gl.tex_storage_mem_2d)(
            GL_TEXTURE_2D,
            1,
            ring.format,
            width as i32,
            h as i32,
            slot.mem,
            offset as u64,
        );
        (gl.bind_texture)(GL_TEXTURE_2D, tex_binding as u32);
        (gl.get_integerv)(GL_DRAW_FRAMEBUFFER_BINDING, &mut draw_fb);
        (gl.gen_framebuffers)(1, &mut slot.fbo);
        (gl.bind_framebuffer)(GL_DRAW_FRAMEBUFFER, slot.fbo);
        (gl.framebuffer_texture_2d)(
            GL_DRAW_FRAMEBUFFER,
            GL_COLOR_ATTACHMENT0,
            GL_TEXTURE_2D,
            slot.tex,
            0,
        );
        // Incomplete if the import or the storage failed: no glGetError,
        // which would eat the game's own pending error.
        let status = (gl.check_framebuffer_status)(GL_DRAW_FRAMEBUFFER);
        (gl.bind_framebuffer)(GL_DRAW_FRAMEBUFFER, draw_fb as u32);
        status == GL_FRAMEBUFFER_COMPLETE
    };
    if !complete {
        return Err("the GL driver cannot render to the imported buffer".into());
    }
    Ok(Msg::Buffer {
        generation: ring.generation,
        index,
        width: w,
        height: h,
        fourcc: protocol::XB24,
        modifier: DRM_FORMAT_MOD_LINEAR,
        planes: vec![Plane { offset, stride }],
    })
}

// --- DRM syncobjs (drm.h) ---

const DRM_IOCTL_SYNCOBJ_CREATE: c_ulong = 0xC008_64BF;
const DRM_IOCTL_SYNCOBJ_DESTROY: c_ulong = 0xC008_64C0;
const DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD: c_ulong = 0xC010_64C1;
/// The plain wait refuses `WAIT_AVAILABLE`; on a binary syncobj, point 0.
const DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT: c_ulong = 0xC030_64CA;
const DRM_IOCTL_SYNCOBJ_RESET: c_ulong = 0xC010_64C4;
const DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE: u32 = 1;
const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT: u32 = 1 << 1;
const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE: u32 = 1 << 2;

#[repr(C)]
struct SyncobjHandle {
    handle: u32,
    flags: u32,
    fd: i32,
    pad: u32,
}

#[repr(C)]
struct SyncobjTimelineWait {
    handles: u64,
    points: u64,
    timeout_nsec: i64,
    count_handles: u32,
    flags: u32,
    first_signaled: u32,
    pad: u32,
    deadline_nsec: u64,
}

#[repr(C)]
struct SyncobjArray {
    handles: u64,
    count_handles: u32,
    pad: u32,
}

fn drm_ioctl<T>(node: &File, request: c_ulong, arg: &mut T) -> bool {
    loop {
        if unsafe { libc::ioctl(node.as_raw_fd(), request as _, arg as *mut T) } == 0 {
            return true;
        }
        let e = std::io::Error::last_os_error().raw_os_error();
        if !matches!(e, Some(libc::EINTR) | Some(libc::EAGAIN)) {
            return false;
        }
    }
}

fn syncobj_create(node: &File) -> Result<u32, String> {
    let mut arg = [0u32; 2];
    drm_ioctl(node, DRM_IOCTL_SYNCOBJ_CREATE, &mut arg)
        .then_some(arg[0])
        .ok_or_else(|| "cannot create a DRM syncobj".into())
}

fn syncobj_destroy(node: &File, handle: u32) {
    let mut arg = [handle, 0];
    drm_ioctl(node, DRM_IOCTL_SYNCOBJ_DESTROY, &mut arg);
}

fn syncobj_reset(node: &File, handle: u32) -> Result<(), String> {
    let mut arg = SyncobjArray {
        handles: &handle as *const u32 as u64,
        count_handles: 1,
        pad: 0,
    };
    drm_ioctl(node, DRM_IOCTL_SYNCOBJ_RESET, &mut arg)
        .then_some(())
        .ok_or_else(|| "cannot reset a DRM syncobj".into())
}

/// The syncobj as an fd: itself (opaque), or its current fence as a
/// sync_file.
fn syncobj_fd(node: &File, handle: u32, sync_file: bool) -> Option<OwnedFd> {
    let mut arg = SyncobjHandle {
        handle,
        flags: if sync_file {
            DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE
        } else {
            0
        },
        fd: -1,
        pad: 0,
    };
    (drm_ioctl(node, DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &mut arg) && arg.fd >= 0)
        .then(|| unsafe { OwnedFd::from_raw_fd(arg.fd) })
}

fn syncobj_sync_file(node: &File, handle: u32) -> Option<OwnedFd> {
    syncobj_fd(node, handle, true)
}

/// Whether a fence was attached to the syncobj (the work signaling it was
/// submitted, not necessarily done), waiting up to `wait_ns` for it.
fn syncobj_wait_submitted(node: &File, handle: u32, wait_ns: u64) -> bool {
    let point = 0u64;
    let mut arg = SyncobjTimelineWait {
        handles: &handle as *const u32 as u64,
        points: &point as *const u64 as u64,
        // Absolute CLOCK_MONOTONIC.
        timeout_nsec: (now_ns() + wait_ns) as i64,
        count_handles: 1,
        flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE,
        first_signaled: 0,
        pad: 0,
        deadline_nsec: 0,
    };
    drm_ioctl(node, DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, &mut arg)
}

fn signaled(sync: BorrowedFd) -> bool {
    let mut pfd = libc::pollfd {
        fd: sync.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut pfd, 1, 0) == 1 }
}

// SAFETY: GBM buffer pointers, only used under `STATE`'s lock.
unsafe impl Send for Slot {}
unsafe impl Send for GbmDevice {}
// SAFETY: only used under `STATE`'s lock.
unsafe impl Sync for GbmDevice {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_mesa_is_captured() {
        assert!(is_mesa("4.6 (Compatibility Profile) Mesa 26.2.4-arch3.1"));
        assert!(is_mesa("OpenGL ES 3.2 Mesa 26.2.4"));
        assert!(!is_mesa("4.6.0 NVIDIA 580.95.05"));
        assert!(!is_mesa("OpenGL ES 3.2 NVIDIA 580.95.05"));
    }

    #[test]
    fn parses_gl_versions() {
        assert_eq!(
            parse_version("4.6 (Compatibility Profile) Mesa 25.2.0"),
            Some((4, false))
        );
        assert_eq!(parse_version("3.3 (Core Profile) Mesa"), Some((3, false)));
        assert_eq!(parse_version("2.1 Mesa 25.2.0"), Some((2, false)));
        assert_eq!(parse_version("OpenGL ES 3.2 Mesa 25.2.0"), Some((3, true)));
        assert_eq!(parse_version("OpenGL ES-CM 1.1 Mesa"), Some((1, true)));
        assert_eq!(parse_version("garbage"), None);
    }

    #[test]
    fn finds_the_render_node_of_a_pci_device() {
        let drm = std::env::temp_dir().join(format!("vesper-drm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&drm);
        for (name, vendor, device) in [
            ("card1", "0x1002", "0x7550"),
            ("renderD128", "0x1002", "0x7550"),
            ("renderD129", "0x1002", "0x13c0"),
        ] {
            let dev = drm.join(name).join("device");
            std::fs::create_dir_all(&dev).unwrap();
            std::fs::write(dev.join("vendor"), format!("{vendor}\n")).unwrap();
            std::fs::write(dev.join("device"), format!("{device}\n")).unwrap();
        }
        assert_eq!(
            node_for_pci(&drm, 0x1002, 0x13c0),
            Some(PathBuf::from("/dev/dri/renderD129"))
        );
        assert_eq!(
            node_for_pci(&drm, 0x1002, 0x7550),
            Some(PathBuf::from("/dev/dri/renderD128"))
        );
        assert_eq!(node_for_pci(&drm, 0x10de, 0x7550), None);
        std::fs::remove_dir_all(&drm).unwrap();
    }

    #[test]
    fn syncobjs() {
        let not_drm = File::open("/dev/null").unwrap();
        assert!(syncobj_create(&not_drm).is_err());
        // The rest needs a GPU (not in CI).
        let Ok(node) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(FALLBACK_NODE)
        else {
            return;
        };
        let h = syncobj_create(&node).unwrap();
        // No fence attached: not submitted, nothing to export.
        assert!(!syncobj_wait_submitted(&node, h, 1_000_000));
        assert!(syncobj_sync_file(&node, h).is_none());
        assert!(syncobj_fd(&node, h, false).is_some());
        // Signaled by the kernel (a stub fence): submitted, and exported.
        let mut arg = SyncobjArray {
            handles: &h as *const u32 as u64,
            count_handles: 1,
            pad: 0,
        };
        // DRM_IOCTL_SYNCOBJ_SIGNAL
        assert!(drm_ioctl(&node, 0xC010_64C5, &mut arg));
        assert!(syncobj_wait_submitted(&node, h, 0));
        let sync = syncobj_sync_file(&node, h).unwrap();
        assert!(signaled(sync.as_fd()));
        syncobj_reset(&node, h).unwrap();
        assert!(!syncobj_wait_submitted(&node, h, 0));
        syncobj_destroy(&node, h);
        assert!(syncobj_reset(&node, h).is_err());
    }
}
