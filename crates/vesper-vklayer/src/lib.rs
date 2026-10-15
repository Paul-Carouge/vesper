//! `VK_LAYER_VESPER_capture`: an implicit Vulkan layer that hands each
//! presented swapchain image to Vesper as a DMA-BUF.
//!
//! The layer is dormant until Vesper asks for frames: it only adds
//! `TRANSFER_SRC` to swapchain usage and enables export extensions the
//! driver supports, and its present hook is a device lookup plus one atomic
//! load. When the server selects this process, each present first submits a
//! GPU copy of the swapchain image into a small ring of exported images,
//! chained between the app's wait semaphores and the present (the game never waits on the CPU).
//! See `vesper_gamecapture::protocol` for the IPC.
//!
//! Safety rules: every entry point catches panics, and any failure turns
//! capture off for that swapchain and passes calls through unchanged.

mod capture;
mod ipc;

use std::ffi::{CStr, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use ash::vk;

/// Process names never captured: compositors, browsers, Electron apps,
/// Vesper itself. The layer stays a pure pass-through there.
const BLOCKLIST: &[&str] = &[
    "kwin_wayland",
    "kwin_x11",
    "plasmashell",
    "xwayland",
    "gamescope",
    "gamescope-wl",
    "steam",
    "steamwebhelper",
    "vesper",
    "obs",
    "firefox",
    "firefox-bin",
    "librewolf",
    "chromium",
    "chrome",
    "brave",
    "msedge",
    "thunderbird",
    "electron",
    "code",
    "discord",
    "slack",
    "spotify",
    "signal-desktop",
    "obsidian",
];

fn is_blocked(name: &str) -> bool {
    BLOCKLIST.iter().any(|b| b.eq_ignore_ascii_case(name))
}

/// Pure pass-through for this process (blocklisted).
static PASSIVE: LazyLock<bool> =
    LazyLock::new(|| is_blocked(&vesper_gamecapture::protocol::process_name()));

fn debug_enabled() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| std::env::var_os("VESPER_LAYER_DEBUG").is_some());
    *ON
}

/// Writes a line to stderr only if that cannot block: these run on the
/// game's render thread, and a full pipe must not stall it.
fn log(msg: &str) {
    let line = format!("[vesper layer] {msg}\n");
    let mut pfd = libc::pollfd {
        fd: 2,
        events: libc::POLLOUT,
        revents: 0,
    };
    // POLLOUT on a pipe means at least PIPE_BUF bytes free: a short line fits.
    if line.len() <= libc::PIPE_BUF
        && unsafe { libc::poll(&mut pfd, 1, 0) } == 1
        && pfd.revents & libc::POLLOUT != 0
    {
        let _ = unsafe { libc::write(2, line.as_ptr().cast(), line.len()) };
    }
}

pub(crate) fn warn(msg: &str) {
    log(msg);
}

pub(crate) fn debug(msg: &str) {
    if debug_enabled() {
        log(msg);
    }
}

/// Locks, ignoring poisoning: a panic caught in one hook must not turn
/// every later call (and the destroy paths) into panics.
pub(crate) trait LockExt<T> {
    fn locked(&self) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn locked(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Runs `f`, turning a panic into `fallback`: unwinding into the loader or
/// the game would abort it.
fn guard<R>(fallback: R, f: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(fallback)
}

// --- Loader interface (vk_layer.h) ---

const LAYER_LINK_INFO: i32 = 0;
const LAYER_NEGOTIATE_INTERFACE_STRUCT: i32 = 1;

#[repr(C)]
struct LayerInstanceLink {
    next: *mut LayerInstanceLink,
    next_get_instance_proc_addr: vk::PFN_vkGetInstanceProcAddr,
    next_get_physical_device_proc_addr: *const c_void,
}

#[repr(C)]
struct LayerInstanceCreateInfo {
    s_type: vk::StructureType,
    p_next: *const c_void,
    function: i32,
    /// First member of a union; the only one used with `LAYER_LINK_INFO`.
    layer_info: *mut LayerInstanceLink,
}

#[repr(C)]
struct LayerDeviceLink {
    next: *mut LayerDeviceLink,
    next_get_instance_proc_addr: vk::PFN_vkGetInstanceProcAddr,
    next_get_device_proc_addr: vk::PFN_vkGetDeviceProcAddr,
}

#[repr(C)]
struct LayerDeviceCreateInfo {
    s_type: vk::StructureType,
    p_next: *const c_void,
    function: i32,
    layer_info: *mut LayerDeviceLink,
}

#[repr(C)]
pub struct NegotiateLayerInterface {
    s_type: i32,
    p_next: *mut c_void,
    loader_layer_interface_version: u32,
    get_instance_proc_addr: Option<vk::PFN_vkGetInstanceProcAddr>,
    get_device_proc_addr: Option<vk::PFN_vkGetDeviceProcAddr>,
    get_physical_device_proc_addr: *const c_void,
}

/// Finds the loader's link info in a create-info chain.
unsafe fn find_link<T>(mut p: *const c_void, s_type: vk::StructureType) -> *mut T {
    while !p.is_null() {
        let base = p as *const vk::BaseInStructure;
        // Both loader structs start with sType, pNext, function.
        let info = p as *const LayerInstanceCreateInfo;
        unsafe {
            if (*base).s_type == s_type && (*info).function == LAYER_LINK_INFO {
                return p as *mut T;
            }
            p = (*base).p_next.cast();
        }
    }
    ptr::null_mut()
}

/// Key of a dispatchable handle: the loader's dispatch table pointer, shared
/// by an instance and its physical devices, and by a device and its queues.
fn key<H: vk::Handle>(h: H) -> usize {
    unsafe { *(h.as_raw() as usize as *const usize) }
}

type Fn = unsafe extern "system" fn();

fn void_fn(f: *const ()) -> vk::PFN_vkVoidFunction {
    Some(unsafe { std::mem::transmute::<*const (), Fn>(f) })
}

// --- Instances ---

pub(crate) struct Instance {
    handle: vk::Instance,
    gipa: vk::PFN_vkGetInstanceProcAddr,
    pub fns: ash::Instance,
    pub format_properties2: Option<vk::PFN_vkGetPhysicalDeviceFormatProperties2>,
    pub image_format_properties2: Option<vk::PFN_vkGetPhysicalDeviceImageFormatProperties2>,
    pub surface_capabilities: Option<vk::PFN_vkGetPhysicalDeviceSurfaceCapabilitiesKHR>,
}

// SAFETY: handles and function pointers only.
unsafe impl Send for Instance {}
unsafe impl Sync for Instance {}

impl Instance {
    fn proc(&self, name: &CStr) -> vk::PFN_vkVoidFunction {
        unsafe { (self.gipa)(self.handle, name.as_ptr()) }
    }

    /// First of `names` the next layer resolves, as a typed pointer.
    unsafe fn load<F: Copy>(&self, names: &[&CStr]) -> Option<F> {
        names.iter().find_map(|n| {
            self.proc(n)
                .map(|f| unsafe { std::mem::transmute_copy::<Fn, F>(&f) })
        })
    }
}

static INSTANCES: Mutex<Vec<(usize, Arc<Instance>)>> = Mutex::new(Vec::new());
static DEVICES: Mutex<Vec<(usize, Arc<capture::Device>)>> = Mutex::new(Vec::new());

fn instance_for(k: usize) -> Option<Arc<Instance>> {
    INSTANCES
        .locked()
        .iter()
        .find(|(i, _)| *i == k)
        .map(|(_, d)| d.clone())
}

pub(crate) fn device_for(k: usize) -> Option<Arc<capture::Device>> {
    DEVICES
        .locked()
        .iter()
        .find(|(i, _)| *i == k)
        .map(|(_, d)| d.clone())
}

/// Instance extensions the device-side export extensions depend on before
/// Vulkan 1.1.
const INSTANCE_EXTENSIONS_1_0: &[&CStr] = &[
    c"VK_KHR_get_physical_device_properties2",
    c"VK_KHR_external_memory_capabilities",
    c"VK_KHR_external_semaphore_capabilities",
];

/// Appends `extra` names not already in `names`.
unsafe fn with_extensions(
    names: *const *const c_char,
    count: u32,
    extra: &[&CStr],
) -> Vec<*const c_char> {
    let mut out: Vec<*const c_char> = if names.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(names, count as usize) }.to_vec()
    };
    for e in extra {
        if !out.iter().any(|n| unsafe { CStr::from_ptr(*n) } == *e) {
            out.push(e.as_ptr());
        }
    }
    out
}

unsafe extern "system" fn create_instance(
    info: *const vk::InstanceCreateInfo<'_>,
    alloc: *const vk::AllocationCallbacks<'_>,
    out: *mut vk::Instance,
) -> vk::Result {
    guard(vk::Result::ERROR_INITIALIZATION_FAILED, || unsafe {
        let link: *mut LayerInstanceCreateInfo = find_link(
            (*info).p_next,
            vk::StructureType::LOADER_INSTANCE_CREATE_INFO,
        );
        if link.is_null() || (*link).layer_info.is_null() {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        }
        let gipa = (*(*link).layer_info).next_get_instance_proc_addr;
        (*link).layer_info = (*(*link).layer_info).next;
        let saved = (*link).layer_info;
        let Some(next) = gipa(vk::Instance::null(), c"vkCreateInstance".as_ptr()) else {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        };
        let next: vk::PFN_vkCreateInstance = std::mem::transmute(next);
        let app = (*info).p_application_info;
        let api_version = match app.is_null() {
            true => vk::API_VERSION_1_0,
            false => (*app).api_version.max(vk::API_VERSION_1_0),
        };

        let mut result = vk::Result::ERROR_INITIALIZATION_FAILED;
        if !*PASSIVE && api_version < vk::API_VERSION_1_1 {
            let names = with_extensions(
                (*info).pp_enabled_extension_names,
                (*info).enabled_extension_count,
                INSTANCE_EXTENSIONS_1_0,
            );
            let mut patched = *info;
            patched.enabled_extension_count = names.len() as u32;
            patched.pp_enabled_extension_names = names.as_ptr();
            result = next(&patched, alloc, out);
            if result != vk::Result::SUCCESS {
                debug("instance rejected the extra extensions, retrying without");
                (*link).layer_info = saved;
            }
        }
        if result != vk::Result::SUCCESS {
            result = next(info, alloc, out);
        }
        if result != vk::Result::SUCCESS {
            return result;
        }

        let handle = *out;
        let load =
            |name: &CStr| gipa(handle, name.as_ptr()).map_or(ptr::null(), |f| f as *const c_void);
        let mut inst = Instance {
            handle,
            gipa,
            fns: ash::Instance::load_with(load, handle),
            format_properties2: None,
            image_format_properties2: None,
            surface_capabilities: None,
        };
        inst.format_properties2 = inst.load(&[
            c"vkGetPhysicalDeviceFormatProperties2",
            c"vkGetPhysicalDeviceFormatProperties2KHR",
        ]);
        inst.image_format_properties2 = inst.load(&[
            c"vkGetPhysicalDeviceImageFormatProperties2",
            c"vkGetPhysicalDeviceImageFormatProperties2KHR",
        ]);
        inst.surface_capabilities = inst.load(&[c"vkGetPhysicalDeviceSurfaceCapabilitiesKHR"]);
        INSTANCES.locked().push((key(handle), Arc::new(inst)));
        vk::Result::SUCCESS
    })
}

unsafe extern "system" fn destroy_instance(
    instance: vk::Instance,
    alloc: *const vk::AllocationCallbacks<'_>,
) {
    if instance == vk::Instance::null() {
        return;
    }
    let inst = guard(None, || {
        let k = key(instance);
        let mut list = INSTANCES.locked();
        let i = list.iter().position(|(i, _)| *i == k);
        i.map(|i| list.swap_remove(i).1)
    });
    // Outside the guard: the next layer's destroy must run exactly once.
    if let Some(inst) = inst {
        unsafe { (inst.fns.fp_v1_0().destroy_instance)(instance, alloc) };
    }
}

// --- Devices ---

/// Device extensions enabled when supported, for exporting images and sync
/// files. Promoted ones are listed too, for Vulkan 1.0 apps.
const DEVICE_EXTENSIONS: &[&CStr] = &[
    c"VK_KHR_external_memory",
    c"VK_KHR_external_memory_fd",
    c"VK_EXT_external_memory_dma_buf",
    c"VK_KHR_external_semaphore",
    c"VK_KHR_external_semaphore_fd",
    c"VK_KHR_get_memory_requirements2",
    c"VK_KHR_dedicated_allocation",
    c"VK_KHR_bind_memory2",
    c"VK_KHR_sampler_ycbcr_conversion",
    c"VK_KHR_image_format_list",
    c"VK_EXT_image_drm_format_modifier",
    c"VK_EXT_queue_family_foreign",
];

unsafe extern "system" fn create_device(
    physical: vk::PhysicalDevice,
    info: *const vk::DeviceCreateInfo<'_>,
    alloc: *const vk::AllocationCallbacks<'_>,
    out: *mut vk::Device,
) -> vk::Result {
    guard(vk::Result::ERROR_INITIALIZATION_FAILED, || unsafe {
        let link: *mut LayerDeviceCreateInfo =
            find_link((*info).p_next, vk::StructureType::LOADER_DEVICE_CREATE_INFO);
        let Some(inst) = instance_for(key(physical)) else {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        };
        if link.is_null() || (*link).layer_info.is_null() {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        }
        let l = &*(*link).layer_info;
        let (gipa, gdpa) = (l.next_get_instance_proc_addr, l.next_get_device_proc_addr);
        (*link).layer_info = l.next;
        let saved = (*link).layer_info;
        let Some(next) = gipa(inst.handle, c"vkCreateDevice".as_ptr()) else {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        };
        let next: vk::PFN_vkCreateDevice = std::mem::transmute(next);

        let app_names = with_extensions(
            (*info).pp_enabled_extension_names,
            (*info).enabled_extension_count,
            &[],
        );
        let mut enabled: Vec<&CStr> = app_names.iter().map(|n| CStr::from_ptr(*n)).collect();
        let mut result = vk::Result::ERROR_INITIALIZATION_FAILED;
        if !*PASSIVE {
            let supported = inst
                .fns
                .enumerate_device_extension_properties(physical)
                .unwrap_or_default();
            let extra: Vec<&CStr> = DEVICE_EXTENSIONS
                .iter()
                .copied()
                .filter(|e| {
                    supported
                        .iter()
                        .any(|p| p.extension_name_as_c_str() == Ok(*e))
                })
                .collect();
            let names = with_extensions(app_names.as_ptr(), app_names.len() as u32, &extra);
            let mut patched = *info;
            patched.enabled_extension_count = names.len() as u32;
            patched.pp_enabled_extension_names = names.as_ptr();
            result = next(physical, &patched, alloc, out);
            if result == vk::Result::SUCCESS {
                for e in extra {
                    if !enabled.contains(&e) {
                        enabled.push(e);
                    }
                }
            } else {
                debug("device rejected the export extensions, retrying without");
                (*link).layer_info = saved;
            }
        }
        if result != vk::Result::SUCCESS {
            result = next(physical, info, alloc, out);
        }
        if result != vk::Result::SUCCESS {
            return result;
        }

        let handle = *out;
        let device = capture::Device::new(inst, physical, handle, gdpa, &enabled, *PASSIVE);
        DEVICES.locked().push((key(handle), Arc::new(device)));
        vk::Result::SUCCESS
    })
}

unsafe extern "system" fn destroy_device(
    device: vk::Device,
    alloc: *const vk::AllocationCallbacks<'_>,
) {
    if device == vk::Device::null() {
        return;
    }
    let dev = guard(None, || {
        let k = key(device);
        let mut list = DEVICES.locked();
        let i = list.iter().position(|(i, _)| *i == k);
        i.map(|i| list.swap_remove(i).1)
    });
    if let Some(dev) = dev {
        guard((), || dev.destroy_all());
        unsafe { (dev.fns.fp_v1_0().destroy_device)(device, alloc) };
    }
}

// --- Proc addr ---

unsafe extern "system" fn get_instance_proc_addr(
    instance: vk::Instance,
    name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    guard(None, || unsafe {
        let name = CStr::from_ptr(name);
        let own = match name.to_bytes() {
            b"vkGetInstanceProcAddr" => get_instance_proc_addr as *const (),
            b"vkCreateInstance" => create_instance as *const (),
            b"vkDestroyInstance" => destroy_instance as *const (),
            b"vkCreateDevice" => create_device as *const (),
            b"vkGetDeviceProcAddr" => get_device_proc_addr as *const (),
            _ => ptr::null(),
        };
        if !own.is_null() {
            return void_fn(own);
        }
        if instance == vk::Instance::null() {
            return None;
        }
        instance_for(key(instance)).and_then(|i| i.proc(name))
    })
}

unsafe extern "system" fn get_device_proc_addr(
    device: vk::Device,
    name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    guard(None, || unsafe {
        let name = CStr::from_ptr(name);
        let dev = device_for(key(device))?;
        let own = match name.to_bytes() {
            b"vkGetDeviceProcAddr" => get_device_proc_addr as *const (),
            b"vkDestroyDevice" => destroy_device as *const (),
            // Inactive devices get the next layer's functions: zero overhead.
            _ if !dev.active() => ptr::null(),
            b"vkGetDeviceQueue" => capture::get_device_queue as *const (),
            b"vkGetDeviceQueue2" => capture::get_device_queue2 as *const (),
            b"vkCreateSwapchainKHR" => capture::create_swapchain as *const (),
            b"vkDestroySwapchainKHR" => capture::destroy_swapchain as *const (),
            b"vkQueuePresentKHR" => capture::queue_present as *const (),
            _ => ptr::null(),
        };
        let next = dev.proc(name);
        // Hooks only for functions the next layer has (e.g. not vkGetDeviceQueue2 on 1.0).
        if !own.is_null() && next.is_some() {
            return void_fn(own);
        }
        next
    })
}

/// Entry point the loader calls first (layer interface version 2).
///
/// # Safety
///
/// Called by the Vulkan loader with a valid `NegotiateLayerInterface`.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn vkNegotiateLoaderLayerInterfaceVersion(
    v: *mut NegotiateLayerInterface,
) -> vk::Result {
    guard(vk::Result::ERROR_INITIALIZATION_FAILED, || unsafe {
        if v.is_null() || (*v).s_type != LAYER_NEGOTIATE_INTERFACE_STRUCT {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        }
        let v = &mut *v;
        if v.loader_layer_interface_version < 2 {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        }
        v.loader_layer_interface_version = 2;
        v.get_instance_proc_addr = Some(get_instance_proc_addr);
        v.get_device_proc_addr = Some(get_device_proc_addr);
        v.get_physical_device_proc_addr = ptr::null();
        vk::Result::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocklist_ignores_case() {
        assert!(is_blocked("kwin_wayland"));
        assert!(is_blocked("Xwayland"));
        assert!(is_blocked("Discord"));
        assert!(!is_blocked("vkcube"));
        assert!(!is_blocked("eldenring.exe"));
    }

    #[test]
    fn appends_missing_extensions_once() {
        let app = [
            c"VK_KHR_swapchain".as_ptr(),
            c"VK_KHR_external_memory".as_ptr(),
        ];
        let out = unsafe {
            with_extensions(
                app.as_ptr(),
                2,
                &[c"VK_KHR_external_memory", c"VK_KHR_external_memory_fd"],
            )
        };
        let names: Vec<_> = out.iter().map(|p| unsafe { CStr::from_ptr(*p) }).collect();
        assert_eq!(
            names,
            [
                c"VK_KHR_swapchain",
                c"VK_KHR_external_memory",
                c"VK_KHR_external_memory_fd"
            ]
        );
        assert!(unsafe { with_extensions(ptr::null(), 0, &[]) }.is_empty());
    }

    #[test]
    fn finds_link_info_in_chain() {
        let mut link = LayerDeviceCreateInfo {
            s_type: vk::StructureType::LOADER_DEVICE_CREATE_INFO,
            p_next: ptr::null(),
            function: LAYER_LINK_INFO,
            layer_info: ptr::null_mut(),
        };
        // A loader callback entry with the same sType must be skipped.
        let callback = LayerDeviceCreateInfo {
            s_type: vk::StructureType::LOADER_DEVICE_CREATE_INFO,
            p_next: (&raw mut link).cast(),
            function: 1,
            layer_info: ptr::null_mut(),
        };
        let features = vk::PhysicalDeviceFeatures2 {
            p_next: (&raw const callback) as *mut c_void,
            ..Default::default()
        };
        let found: *mut LayerDeviceCreateInfo = unsafe {
            find_link(
                (&raw const features).cast(),
                vk::StructureType::LOADER_DEVICE_CREATE_INFO,
            )
        };
        assert_eq!(found, &raw mut link);
        let none: *mut LayerDeviceCreateInfo = unsafe {
            find_link(
                (&raw const features).cast(),
                vk::StructureType::LOADER_INSTANCE_CREATE_INFO,
            )
        };
        assert!(none.is_null());
    }
}
