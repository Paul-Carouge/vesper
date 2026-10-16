//! `libvesper_glcapture.so`: hands each swapped OpenGL frame to Vesper as a
//! DMA-BUF, for native OpenGL games (OpenGL has no layer mechanism).
//!
//! Loaded with `LD_PRELOAD` by the `vesper-gl` wrapper, one game at a time
//! (`vesper-gl %command%` in Steam's launch options), never globally. It
//! interposes `glXSwapBuffers` and `eglSwapBuffers` (and the `WithDamage`
//! variants), and `dlsym`, `glXGetProcAddress(ARB)` and `eglGetProcAddress`
//! so that games loading OpenGL themselves (SDL, GLFW: `dlopen` + `dlsym`)
//! get the hooks too. Everything else goes through untouched.
//!
//! Dormant until Vesper asks for frames: a swap costs one atomic load. When
//! the server selects this game, each swap first blits the back buffer into
//! a small ring of exported linear images (GBM buffers imported with
//! `GL_EXT_memory_object_fd`), paced to the recording rate, with the same
//! protocol, consent and blocklist as the Vulkan layer. See `capture`.
//!
//! Safety rules: every entry point catches panics, the game's GL state is
//! restored after each blit, and any failure turns capture off for that
//! context and passes calls through unchanged.

mod capture;

use std::ffi::{CStr, c_char, c_int, c_ulong, c_void};
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use vesper_gamecapture::client::guard;

use capture::Api;

// --- Real functions ---

type Dlsym = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
type GlxSwap = unsafe extern "C" fn(*mut c_void, c_ulong);
type GlxDestroy = unsafe extern "C" fn(*mut c_void, *mut c_void);
type GetProcAddress = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type EglSwap = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type EglSwapDamage = unsafe extern "C" fn(*mut c_void, *mut c_void, *const i32, i32) -> u32;
type EglDestroy = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;

/// The functions this library interposes, by index into `REAL`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hook {
    GlxSwap,
    GlxGetProcAddress,
    GlxGetProcAddressArb,
    GlxDestroyContext,
    EglSwap,
    EglSwapDamageKhr,
    EglSwapDamageExt,
    EglGetProcAddress,
    EglDestroyContext,
}

const HOOKS: usize = 9;

impl Hook {
    fn from_name(name: &[u8]) -> Option<Hook> {
        Some(match name {
            b"glXSwapBuffers" => Hook::GlxSwap,
            b"glXGetProcAddress" => Hook::GlxGetProcAddress,
            b"glXGetProcAddressARB" => Hook::GlxGetProcAddressArb,
            b"glXDestroyContext" => Hook::GlxDestroyContext,
            b"eglSwapBuffers" => Hook::EglSwap,
            b"eglSwapBuffersWithDamageKHR" => Hook::EglSwapDamageKhr,
            b"eglSwapBuffersWithDamageEXT" => Hook::EglSwapDamageExt,
            b"eglGetProcAddress" => Hook::EglGetProcAddress,
            b"eglDestroyContext" => Hook::EglDestroyContext,
            _ => return None,
        })
    }

    fn name(self) -> &'static CStr {
        match self {
            Hook::GlxSwap => c"glXSwapBuffers",
            Hook::GlxGetProcAddress => c"glXGetProcAddress",
            Hook::GlxGetProcAddressArb => c"glXGetProcAddressARB",
            Hook::GlxDestroyContext => c"glXDestroyContext",
            Hook::EglSwap => c"eglSwapBuffers",
            Hook::EglSwapDamageKhr => c"eglSwapBuffersWithDamageKHR",
            Hook::EglSwapDamageExt => c"eglSwapBuffersWithDamageEXT",
            Hook::EglGetProcAddress => c"eglGetProcAddress",
            Hook::EglDestroyContext => c"eglDestroyContext",
        }
    }

    /// This library's replacement.
    fn ours(self) -> *mut c_void {
        let f: *const () = match self {
            Hook::GlxSwap => glXSwapBuffers as *const (),
            Hook::GlxGetProcAddress => glXGetProcAddress as *const (),
            Hook::GlxGetProcAddressArb => glXGetProcAddressARB as *const (),
            Hook::GlxDestroyContext => glXDestroyContext as *const (),
            Hook::EglSwap => eglSwapBuffers as *const (),
            Hook::EglSwapDamageKhr => eglSwapBuffersWithDamageKHR as *const (),
            Hook::EglSwapDamageExt => eglSwapBuffersWithDamageEXT as *const (),
            Hook::EglGetProcAddress => eglGetProcAddress as *const (),
            Hook::EglDestroyContext => eglDestroyContext as *const (),
        };
        f as *mut c_void
    }

    fn is_ours(p: *mut c_void) -> bool {
        ALL.iter().any(|h| h.ours() == p)
    }
}

const ALL: [Hook; HOOKS] = [
    Hook::GlxSwap,
    Hook::GlxGetProcAddress,
    Hook::GlxGetProcAddressArb,
    Hook::GlxDestroyContext,
    Hook::EglSwap,
    Hook::EglSwapDamageKhr,
    Hook::EglSwapDamageExt,
    Hook::EglGetProcAddress,
    Hook::EglDestroyContext,
];

/// The implementation each hook forwards to: the first one found, from the
/// game's own `dlsym`/`GetProcAddress` lookups or from the global scope.
static REAL: [AtomicPtr<c_void>; HOOKS] = [const { AtomicPtr::new(ptr::null_mut()) }; HOOKS];

static REAL_DLSYM: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// glibc's `dlsym` (versioned: it moved from libdl to libc in 2.34).
extern "C" fn real_dlsym_ptr() -> *mut c_void {
    let mut p = REAL_DLSYM.load(Ordering::Relaxed);
    if p.is_null() {
        for v in [c"GLIBC_2.34", c"GLIBC_2.2.5", c"GLIBC_2.0"] {
            p = unsafe { libc::dlvsym(libc::RTLD_NEXT, c"dlsym".as_ptr(), v.as_ptr()) };
            if !p.is_null() {
                break;
            }
        }
        REAL_DLSYM.store(p, Ordering::Relaxed);
    }
    p
}

fn real_dlsym(handle: *mut c_void, name: &CStr) -> *mut c_void {
    let p = real_dlsym_ptr();
    if p.is_null() {
        return ptr::null_mut();
    }
    let f: Dlsym = unsafe { std::mem::transmute(p) };
    unsafe { f(handle, name.as_ptr()) }
}

/// Records `p` as the implementation behind `hook` (the first one wins),
/// never one of ours (that would loop).
fn remember(hook: Hook, p: *mut c_void) {
    if !p.is_null() && !Hook::is_ours(p) {
        let _ = REAL[hook as usize].compare_exchange(
            ptr::null_mut(),
            p,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

/// The real function behind `hook`, as a `F` (a function pointer type).
fn real<F: Copy>(hook: Hook) -> Option<F> {
    let mut p = REAL[hook as usize].load(Ordering::Acquire);
    if p.is_null() {
        // Linked directly: next in the global scope. Loaded with dlopen
        // (RTLD_LOCAL): ask the libraries already loaded, without loading any.
        remember(hook, real_dlsym(libc::RTLD_NEXT, hook.name()));
        for lib in [c"libGLX.so.0", c"libGL.so.1", c"libEGL.so.1"] {
            if !REAL[hook as usize].load(Ordering::Acquire).is_null() {
                break;
            }
            let h = unsafe { libc::dlopen(lib.as_ptr(), libc::RTLD_LAZY | libc::RTLD_NOLOAD) };
            if !h.is_null() {
                remember(hook, real_dlsym(h, hook.name()));
                unsafe { libc::dlclose(h) };
            }
        }
        p = REAL[hook as usize].load(Ordering::Acquire);
    }
    (!p.is_null()).then(|| unsafe { std::mem::transmute_copy::<*mut c_void, F>(&p) })
}

/// What a lookup of `name` that found `found` returns: our hook for the
/// functions we interpose (remembering `found` as the real one).
fn intercept(name: *const c_char, found: *mut c_void) -> *mut c_void {
    if name.is_null() || found.is_null() {
        return found;
    }
    match Hook::from_name(unsafe { CStr::from_ptr(name) }.to_bytes()) {
        Some(hook) => {
            remember(hook, found);
            hook.ours()
        }
        None => found,
    }
}

// --- dlsym ---

/// Our part of `dlsym`: our hook for interposed names, null for all others
/// (the trampoline then tail-calls glibc's `dlsym`, which must see the
/// caller's return address: `RTLD_NEXT` and the lookup scope depend on it).
/// Only compares bytes for other names: it runs for every `dlsym` call of
/// the process, also during start-up.
extern "C" fn dlsym_hook(handle: *mut c_void, name: *const c_char) -> *mut c_void {
    guard(ptr::null_mut(), || {
        // RTLD_NEXT: an interposer after us wants the next real function,
        // looked up from its own place in the chain, which only glibc
        // (seeing the caller's address) can do.
        if name.is_null() || handle == libc::RTLD_NEXT {
            return ptr::null_mut();
        }
        let Some(hook) = Hook::from_name(unsafe { CStr::from_ptr(name) }.to_bytes()) else {
            return ptr::null_mut();
        };
        let found = real_dlsym(handle, hook.name());
        // Not found, or found in the global scope (RTLD_DEFAULT, a
        // dlopen(NULL) handle): glibc answers, with the caller's scope. The
        // global scope's function is ours or that of an interposer preloaded
        // before us, which reaches us through RTLD_NEXT; remembering it as
        // the real one would loop.
        if found.is_null() || found == real_dlsym(libc::RTLD_DEFAULT, hook.name()) {
            return ptr::null_mut();
        }
        remember(hook, found);
        hook.ours()
    })
}

/// `dlsym` itself: calls `dlsym_hook`, and if that returns null, jumps to
/// glibc's `dlsym` with the caller's stack and return address untouched.
///
/// # Safety
///
/// Same contract as `dlsym(3)`.
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dlsym(_handle: *mut c_void, _name: *const c_char) -> *mut c_void {
    core::arch::naked_asm!(
        "endbr64",
        // Keep the arguments; 16-byte stack alignment for the calls.
        "push rdi",
        "push rsi",
        "sub rsp, 8",
        "call {hook}",
        "test rax, rax",
        "jnz 2f",
        "call {real}",
        "add rsp, 8",
        "pop rsi",
        "pop rdi",
        "test rax, rax",
        "jz 3f",
        "jmp rax",
        "2:",
        "add rsp, 8",
        "pop rsi",
        "pop rdi",
        "3:",
        "ret",
        hook = sym dlsym_hook,
        real = sym real_dlsym_ptr,
    )
}

/// See the x86_64 version (cdecl: arguments on the stack).
///
/// # Safety
///
/// Same contract as `dlsym(3)`.
#[cfg(target_arch = "x86")]
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dlsym(_handle: *mut c_void, _name: *const c_char) -> *mut c_void {
    core::arch::naked_asm!(
        "endbr32",
        // Copy the two arguments; 16-byte stack alignment for the calls.
        "sub esp, 4",
        "push dword ptr [esp + 12]",
        "push dword ptr [esp + 12]",
        "call {hook}",
        "test eax, eax",
        "jnz 2f",
        "call {real}",
        "add esp, 12",
        "test eax, eax",
        "jz 3f",
        "jmp eax",
        "2:",
        "add esp, 12",
        "3:",
        "ret",
        hook = sym dlsym_hook,
        real = sym real_dlsym_ptr,
    )
}

// --- GLX ---

/// # Safety
///
/// Same contract as `glXSwapBuffers`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glXSwapBuffers(dpy: *mut c_void, drawable: c_ulong) {
    let Some(next) = real::<GlxSwap>(Hook::GlxSwap) else {
        return;
    };
    guard((), || {
        capture::before_swap(Api::Glx, dpy, drawable as usize)
    });
    unsafe { next(dpy, drawable) }
}

fn glx_get_proc_address(hook: Hook, name: *const c_char) -> *mut c_void {
    let Some(next) = real::<GetProcAddress>(hook) else {
        return ptr::null_mut();
    };
    let found = unsafe { next(name) };
    guard(found, || intercept(name, found))
}

/// # Safety
///
/// Same contract as `glXGetProcAddress`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glXGetProcAddress(name: *const c_char) -> *mut c_void {
    glx_get_proc_address(Hook::GlxGetProcAddress, name)
}

/// # Safety
///
/// Same contract as `glXGetProcAddressARB`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glXGetProcAddressARB(name: *const c_char) -> *mut c_void {
    glx_get_proc_address(Hook::GlxGetProcAddressArb, name)
}

/// # Safety
///
/// Same contract as `glXDestroyContext`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glXDestroyContext(dpy: *mut c_void, ctx: *mut c_void) {
    guard((), || capture::context_destroyed(Api::Glx, ctx as usize));
    if let Some(next) = real::<GlxDestroy>(Hook::GlxDestroyContext) {
        unsafe { next(dpy, ctx) }
    }
}

// --- EGL ---

/// # Safety
///
/// Same contract as `eglSwapBuffers`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglSwapBuffers(dpy: *mut c_void, surface: *mut c_void) -> u32 {
    let Some(next) = real::<EglSwap>(Hook::EglSwap) else {
        return 0;
    };
    guard((), || capture::before_swap(Api::Egl, dpy, surface as usize));
    unsafe { next(dpy, surface) }
}

fn egl_swap_damage(
    hook: Hook,
    dpy: *mut c_void,
    surface: *mut c_void,
    rects: *const i32,
    n: c_int,
) -> u32 {
    let Some(next) = real::<EglSwapDamage>(hook) else {
        return 0;
    };
    guard((), || capture::before_swap(Api::Egl, dpy, surface as usize));
    unsafe { next(dpy, surface, rects, n) }
}

/// # Safety
///
/// Same contract as `eglSwapBuffersWithDamageKHR`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglSwapBuffersWithDamageKHR(
    dpy: *mut c_void,
    surface: *mut c_void,
    rects: *const i32,
    n: c_int,
) -> u32 {
    egl_swap_damage(Hook::EglSwapDamageKhr, dpy, surface, rects, n)
}

/// # Safety
///
/// Same contract as `eglSwapBuffersWithDamageEXT`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglSwapBuffersWithDamageEXT(
    dpy: *mut c_void,
    surface: *mut c_void,
    rects: *const i32,
    n: c_int,
) -> u32 {
    egl_swap_damage(Hook::EglSwapDamageExt, dpy, surface, rects, n)
}

/// # Safety
///
/// Same contract as `eglGetProcAddress`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglGetProcAddress(name: *const c_char) -> *mut c_void {
    let Some(next) = real::<GetProcAddress>(Hook::EglGetProcAddress) else {
        return ptr::null_mut();
    };
    let found = unsafe { next(name) };
    guard(found, || intercept(name, found))
}

/// # Safety
///
/// Same contract as `eglDestroyContext`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglDestroyContext(dpy: *mut c_void, ctx: *mut c_void) -> u32 {
    guard((), || capture::context_destroyed(Api::Egl, ctx as usize));
    match real::<EglDestroy>(Hook::EglDestroyContext) {
        Some(next) => unsafe { next(dpy, ctx) },
        None => 0,
    }
}

/// The real `GetProcAddress` of `api`, for the capture's own GL calls.
pub(crate) fn get_proc_address(api: Api) -> Option<GetProcAddress> {
    match api {
        Api::Glx => real(Hook::GlxGetProcAddressArb).or_else(|| real(Hook::GlxGetProcAddress)),
        Api::Egl => real(Hook::EglGetProcAddress),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_names_round_trip() {
        for h in ALL {
            assert_eq!(Hook::from_name(h.name().to_bytes()), Some(h));
            assert!(Hook::is_ours(h.ours()));
        }
        assert_eq!(Hook::from_name(b"glXSwapBuffersMscOML"), None);
        assert_eq!(Hook::from_name(b"glClear"), None);
    }

    #[test]
    fn intercepts_only_our_names() {
        let fake = 0x1234usize as *mut c_void;
        assert_eq!(intercept(c"glClear".as_ptr(), fake), fake);
        assert_eq!(
            intercept(c"glClear".as_ptr(), ptr::null_mut()),
            ptr::null_mut()
        );
        // A name we hook that the GL library does not have stays missing.
        assert_eq!(
            intercept(c"eglSwapBuffersWithDamageEXT".as_ptr(), ptr::null_mut()),
            ptr::null_mut()
        );
        assert_eq!(
            intercept(c"eglSwapBuffersWithDamageKHR".as_ptr(), fake),
            Hook::EglSwapDamageKhr.ours()
        );
        assert_eq!(
            REAL[Hook::EglSwapDamageKhr as usize].load(Ordering::Acquire),
            fake
        );
        // Our own hook is never taken for the real function.
        remember(Hook::GlxSwap, Hook::GlxSwap.ours());
        assert!(
            REAL[Hook::GlxSwap as usize]
                .load(Ordering::Acquire)
                .is_null()
        );
    }

    #[test]
    fn dlsym_trampoline_forwards_with_the_caller_scope() {
        // Not interposed: glibc answers, from this (the caller's) scope.
        let p = unsafe { dlsym(libc::RTLD_DEFAULT, c"getpid".as_ptr()) };
        assert_eq!(p, libc::getpid as *mut c_void);
        let none = unsafe { dlsym(libc::RTLD_DEFAULT, c"vesper_no_such_symbol".as_ptr()) };
        assert!(none.is_null());
        let next = unsafe { dlsym(libc::RTLD_NEXT, c"getpid".as_ptr()) };
        assert!(!next.is_null());
    }
}
