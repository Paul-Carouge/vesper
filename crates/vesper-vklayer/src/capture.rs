//! Per-device state, swapchain hooks and the export ring.
//!
//! On each present of the captured swapchain, one command buffer waits on the
//! app's semaphores, copies (or blits, for format conversion) the swapchain
//! image into a free ring image, and signals two semaphores: one the present
//! waits on instead of the app's, one exported as a sync_file for Vesper.
//! No ring image free (Vesper is slow): the frame is skipped, never waited.

use std::ffi::{CStr, c_void};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ash::vk::{self, Handle};
use vesper_gamecapture::protocol::{self, Msg, Plane};

use crate::ipc::{self, IPC};
use crate::{Instance, LockExt, debug, device_for, guard, key, warn};

/// Images in the export ring. Three skipped frames at 160 fps with an
/// encoder in the loop; four did not.
const RING: usize = protocol::RING as usize;
/// At most one ring (re)creation per swapchain in this time.
const RING_RETRY: Duration = Duration::from_secs(1);
/// Smallest swapchain worth capturing (skips launchers' splash windows,
/// tooltips and the like).
const MIN_SIZE: (u32, u32) = (640, 360);
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// Swapchain the process currently offers to Vesper: (device key, raw
/// handle); (0, 0) = none. Handles are only unique per device.
static OWNER: Mutex<(usize, u64)> = Mutex::new((0, 0));

fn owner_is(dev: usize, sc: vk::SwapchainKHR) -> bool {
    *OWNER.locked() == (dev, sc.as_raw())
}

/// Clears the owner if it is this swapchain; true if it was.
fn release_owner(dev: usize, sc: vk::SwapchainKHR) -> bool {
    let mut o = OWNER.locked();
    let was = *o == (dev, sc.as_raw());
    if was {
        *o = (0, 0);
    }
    was
}
/// Ring generation, unique in the process.
static GENERATION: AtomicU32 = AtomicU32::new(0);

/// Ring format for a swapchain format: (format, DRM fourcc, needs a blit).
/// 8-bit formats are copied bit for bit (an sRGB swapchain's bytes are
/// already what the encoder expects); others are blitted to 8-bit BGRA.
fn ring_format(format: vk::Format) -> Option<(vk::Format, u32, bool)> {
    use vk::Format as F;
    let xr24 = protocol::XR24;
    Some(match format {
        F::B8G8R8A8_UNORM | F::B8G8R8A8_SRGB => (F::B8G8R8A8_UNORM, xr24, false),
        F::R8G8B8A8_UNORM
        | F::R8G8B8A8_SRGB
        | F::A8B8G8R8_UNORM_PACK32
        | F::A8B8G8R8_SRGB_PACK32 => (F::R8G8B8A8_UNORM, protocol::XB24, false),
        F::A2B10G10R10_UNORM_PACK32 | F::A2R10G10B10_UNORM_PACK32 => {
            (F::B8G8R8A8_UNORM, xr24, true)
        }
        // Under SRGB_NONLINEAR the values are already sRGB-encoded.
        F::R16G16B16A16_SFLOAT => (F::B8G8R8A8_UNORM, xr24, true),
        _ => return None,
    })
}

fn now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn check<T>(r: ash::prelude::VkResult<T>, what: &str) -> Result<T, String> {
    r.map_err(|e| format!("{what}: {e}"))
}

struct Export {
    memory_fd: ash::khr::external_memory_fd::DeviceFn,
    semaphore_fd: ash::khr::external_semaphore_fd::DeviceFn,
    drm: Option<ash::ext::image_drm_format_modifier::DeviceFn>,
    dedicated: bool,
    foreign: bool,
    memory: vk::PhysicalDeviceMemoryProperties,
    families: Vec<vk::QueueFamilyProperties>,
}

struct Slot {
    image: vk::Image,
    memory: vk::DeviceMemory,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    /// Exported as a sync_file after each submit (which resets it).
    sync: vk::Semaphore,
    /// Not held by Vesper.
    free: bool,
}

struct Ring {
    /// `Start` it serves: the server forgets every buffer on a new `Start`.
    start_id: u64,
    generation: u32,
    family: u32,
    pool: vk::CommandPool,
    blit: bool,
    slots: Vec<Slot>,
}

struct Swapchain {
    handle: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    format: vk::Format,
    extent: vk::Extent2D,
    capturable: bool,
    /// Connection this swapchain was announced on (0: never).
    announced: u64,
    /// One per swapchain image: signaled by the copy, waited by the present.
    /// Reused when the image is presented again, as apps do.
    present_sems: Vec<vk::Semaphore>,
    ring: Option<Ring>,
    /// Earliest next ring (re)creation (rate limit, also after failures).
    next_ring: Option<Instant>,
}

pub(crate) struct Device {
    handle: vk::Device,
    /// Dispatch key, for `OWNER`.
    key: usize,
    /// Some swapchain may hold a ring: checked on the dormant path so a
    /// ring never outlives the connection in VRAM.
    pub has_ring: AtomicBool,
    gdpa: vk::PFN_vkGetDeviceProcAddr,
    pub fns: ash::Device,
    inst: Arc<Instance>,
    physical: vk::PhysicalDevice,
    swapchain: ash::khr::swapchain::DeviceFn,
    export: Option<Export>,
    queues: Mutex<Vec<(vk::Queue, u32)>>,
    swapchains: Mutex<Vec<Swapchain>>,
    /// Present semaphores of destroyed swapchains. An old present may still
    /// wait on them, so they are reused, and destroyed only with the device.
    spare_sems: Mutex<Vec<vk::Semaphore>>,
}

// SAFETY: handles and function pointers; mutable state is behind mutexes.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    pub fn new(
        inst: Arc<Instance>,
        physical: vk::PhysicalDevice,
        handle: vk::Device,
        gdpa: vk::PFN_vkGetDeviceProcAddr,
        enabled: &[&CStr],
        passive: bool,
    ) -> Self {
        let load = |name: &CStr| unsafe {
            gdpa(handle, name.as_ptr()).map_or(std::ptr::null(), |f| f as *const c_void)
        };
        let has = |e: &CStr| enabled.contains(&e);
        let export = (!passive
            && has(c"VK_KHR_external_memory_fd")
            && has(c"VK_EXT_external_memory_dma_buf")
            && has(c"VK_KHR_external_semaphore_fd"))
        .then(|| Export {
            memory_fd: ash::khr::external_memory_fd::DeviceFn::load(load),
            semaphore_fd: ash::khr::external_semaphore_fd::DeviceFn::load(load),
            drm: (has(c"VK_EXT_image_drm_format_modifier") && inst.format_properties2.is_some())
                .then(|| ash::ext::image_drm_format_modifier::DeviceFn::load(load)),
            dedicated: has(c"VK_KHR_dedicated_allocation"),
            foreign: has(c"VK_EXT_queue_family_foreign"),
            memory: unsafe { inst.fns.get_physical_device_memory_properties(physical) },
            families: unsafe {
                inst.fns
                    .get_physical_device_queue_family_properties(physical)
            },
        });
        if export.is_none() && !passive {
            debug("device cannot export images: capture off");
        }
        Self {
            handle,
            key: key(handle),
            has_ring: AtomicBool::new(false),
            gdpa,
            fns: unsafe { ash::Device::load_with(load, handle) },
            swapchain: ash::khr::swapchain::DeviceFn::load(load),
            inst,
            physical,
            export,
            queues: Mutex::new(Vec::new()),
            swapchains: Mutex::new(Vec::new()),
            spare_sems: Mutex::new(Vec::new()),
        }
    }

    /// Whether this device can capture; otherwise its hooks are not installed.
    pub fn active(&self) -> bool {
        self.export.is_some()
    }

    pub fn proc(&self, name: &CStr) -> vk::PFN_vkVoidFunction {
        unsafe { (self.gdpa)(self.handle, name.as_ptr()) }
    }

    /// Frees every ring and semaphore (the device is being destroyed).
    pub fn destroy_all(&self) {
        let swapchains = std::mem::take(&mut *self.swapchains.locked());
        for mut sc in swapchains {
            self.forget(&mut sc);
        }
        for s in self.spare_sems.locked().drain(..) {
            unsafe { self.fns.destroy_semaphore(s, None) };
        }
    }

    /// Frees every ring (Vesper went away). Present semaphores stay.
    pub fn drop_rings(&self) {
        self.has_ring.store(false, Ordering::SeqCst);
        for sc in self.swapchains.locked().iter_mut() {
            if let Some(ring) = sc.ring.take() {
                self.destroy_ring(ring);
            }
        }
    }

    fn forget(&self, sc: &mut Swapchain) {
        if let Some(ring) = sc.ring.take() {
            self.destroy_ring(ring);
        }
        self.spare_sems.locked().extend(sc.present_sems.drain(..));
        if release_owner(self.key, sc.handle) && sc.announced == IPC.conn_id.load(Ordering::SeqCst)
        {
            ipc::send(
                &Msg::Swapchain {
                    width: 0,
                    height: 0,
                },
                None,
            );
        }
    }

    /// Whether a swapchain with this create info can be captured (and so
    /// gets `TRANSFER_SRC` added).
    fn wants(&self, ci: &vk::SwapchainCreateInfoKHR<'_>) -> bool {
        let e = ci.image_extent;
        if e.width < MIN_SIZE.0
            || e.height < MIN_SIZE.1
            || ci.image_array_layers != 1
            || ci.flags.contains(vk::SwapchainCreateFlagsKHR::PROTECTED)
            || ci.image_color_space != vk::ColorSpaceKHR::SRGB_NONLINEAR
            || ring_format(ci.image_format).is_none()
            || matches!(
                ci.present_mode,
                vk::PresentModeKHR::SHARED_DEMAND_REFRESH
                    | vk::PresentModeKHR::SHARED_CONTINUOUS_REFRESH
            )
        {
            return false;
        }
        let Some(caps_fn) = self.inst.surface_capabilities else {
            return false;
        };
        let mut caps = vk::SurfaceCapabilitiesKHR::default();
        let r = unsafe { caps_fn(self.physical, ci.surface, &mut caps) };
        r == vk::Result::SUCCESS
            && caps
                .supported_usage_flags
                .contains(vk::ImageUsageFlags::TRANSFER_SRC)
    }

    /// Runs before the real present. Returns the semaphore the present must
    /// wait on instead of the app's when a copy was submitted.
    fn before_present(
        &self,
        queue: vk::Queue,
        info: &vk::PresentInfoKHR<'_>,
    ) -> Option<vk::Semaphore> {
        let conn = IPC.conn_id.load(Ordering::SeqCst);
        let capturing = IPC.capturing.load(Ordering::SeqCst);
        let n = info.swapchain_count as usize;
        if n == 0 || info.p_swapchains.is_null() || info.p_image_indices.is_null() {
            return None;
        }
        let handles = unsafe { std::slice::from_raw_parts(info.p_swapchains, n) };
        let indices = unsafe { std::slice::from_raw_parts(info.p_image_indices, n) };
        let mut swapchains = self.swapchains.locked();
        for (h, &image) in handles.iter().zip(indices) {
            let Some(sc) = swapchains.iter_mut().find(|s| s.handle == *h) else {
                continue;
            };
            if !sc.capturable {
                continue;
            }
            // Also when the swapchain that held the slot went away.
            if sc.announced != conn || *OWNER.locked() == (0, 0) {
                let msg = Msg::Swapchain {
                    width: sc.extent.width,
                    height: sc.extent.height,
                };
                if !ipc::send(&msg, None) {
                    continue;
                }
                sc.announced = conn;
                *OWNER.locked() = (self.key, h.as_raw());
            }
            if !owner_is(self.key, *h) {
                // Another swapchain took over: free this one's ring.
                if let Some(ring) = sc.ring.take() {
                    self.destroy_ring(ring);
                }
                continue;
            }
            if !capturing {
                if let Some(ring) = sc.ring.take() {
                    self.destroy_ring(ring);
                }
                return None;
            }
            let start = IPC.start_id.load(Ordering::SeqCst);
            return self.capture(sc, queue, info, image, start);
        }
        None
    }

    /// Swaps a present semaphore that may still be signaled for a new one
    /// (the old one leaks: it may be signaled or awaited).
    fn replace_present_sem(&self, old: vk::Semaphore) {
        let mut swapchains = self.swapchains.locked();
        for sc in swapchains.iter_mut() {
            if let Some(slot) = sc.present_sems.iter_mut().find(|s| **s == old) {
                match unsafe {
                    self.fns
                        .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
                } {
                    Ok(s) => *slot = s,
                    Err(_) => self.disable(sc, "present failed"),
                }
                return;
            }
        }
    }

    fn disable(&self, sc: &mut Swapchain, why: &str) {
        warn(&format!("capture off for this swapchain: {why}"));
        sc.capturable = false;
        if let Some(ring) = sc.ring.take() {
            self.destroy_ring(ring);
        }
        if release_owner(self.key, sc.handle) {
            ipc::send(
                &Msg::Swapchain {
                    width: 0,
                    height: 0,
                },
                None,
            );
        }
    }

    fn capture(
        &self,
        sc: &mut Swapchain,
        queue: vk::Queue,
        info: &vk::PresentInfoKHR<'_>,
        image: u32,
        start: u64,
    ) -> Option<vk::Semaphore> {
        let family = self
            .queues
            .locked()
            .iter()
            .find(|(q, _)| *q == queue)
            .map(|(_, f)| *f)?;
        let src = *sc.images.get(image as usize)?;
        if sc
            .ring
            .as_ref()
            .is_some_and(|r| r.start_id != start || r.family != family)
        {
            // Restarted by the server, or the app moved presents to another queue family.
            let ring = sc.ring.take().expect("checked above");
            self.destroy_ring(ring);
        }
        if sc.ring.is_none() {
            let now = Instant::now();
            if sc.next_ring.is_some_and(|t| now < t) {
                return None;
            }
            sc.next_ring = Some(now + RING_RETRY);
            match self.create_ring(sc, family, start) {
                Ok(Some(r)) => {
                    sc.ring = Some(r);
                    self.has_ring.store(true, Ordering::SeqCst);
                }
                // Socket full: try again later.
                Ok(None) => return None,
                Err(e) => {
                    self.disable(sc, &e);
                    return None;
                }
            }
        }
        if sc.present_sems.is_empty() {
            for _ in 0..sc.images.len() {
                if let Some(s) = self.spare_sems.locked().pop() {
                    sc.present_sems.push(s);
                    continue;
                }
                match unsafe {
                    self.fns
                        .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
                } {
                    Ok(s) => sc.present_sems.push(s),
                    Err(e) => {
                        self.disable(sc, &format!("create semaphore: {e}"));
                        return None;
                    }
                }
            }
        }
        let ring = sc.ring.as_mut().expect("created above");
        for (g, i) in ipc::take_releases() {
            if g == ring.generation
                && let Some(s) = ring.slots.get_mut(i as usize)
            {
                s.free = true;
            }
        }
        let Some(slot) = ring.slots.iter().position(|s| {
            s.free
                && s.fence != vk::Fence::null()
                && unsafe { self.fns.get_fence_status(s.fence) } == Ok(true)
        }) else {
            debug("no free ring image: frame skipped");
            return None;
        };
        let present_sem = sc.present_sems[image as usize];
        if let Err(e) = self.submit_copy(ring, slot, queue, info, src, sc.extent, present_sem) {
            self.disable(sc, &e);
            return None;
        }
        // The copy is submitted and holds the app's semaphores: from here on
        // the present must wait on `present_sem` whatever happens.
        let generation = ring.generation;
        let s = &mut ring.slots[slot];
        let sent = guard(Err("panic".to_string()), || {
            let get_fd = vk::SemaphoreGetFdInfoKHR::default()
                .semaphore(s.sync)
                .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
            let mut fd = -1;
            let ex = self.export.as_ref().expect("active device");
            let r =
                unsafe { (ex.semaphore_fd.get_semaphore_fd_khr)(self.handle, &get_fd, &mut fd) };
            if r != vk::Result::SUCCESS {
                return Err(format!("export sync_file: {r}"));
            }
            // -1 means already signaled: nothing to wait for.
            let fd = (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) });
            let msg = Msg::Frame {
                generation,
                index: slot as u32,
                pts_ns: now_ns(),
            };
            Ok(ipc::send(&msg, fd.as_ref().map(|f| f.as_fd())))
        });
        match sent {
            Ok(true) => s.free = false,
            Ok(false) => {}
            Err(e) => self.disable(sc, &e),
        }
        Some(present_sem)
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_copy(
        &self,
        ring: &mut Ring,
        slot: usize,
        queue: vk::Queue,
        info: &vk::PresentInfoKHR<'_>,
        src: vk::Image,
        extent: vk::Extent2D,
        present_sem: vk::Semaphore,
    ) -> Result<(), String> {
        let d = &self.fns;
        let family = ring.family;
        let blit = ring.blit;
        let s = &mut ring.slots[slot];
        let ex = self.export.as_ref().expect("active device");
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let layers = vk::ImageSubresourceLayers::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .layer_count(1);
        let barrier = |image, old, new, src_access, dst_access| {
            vk::ImageMemoryBarrier::default()
                .image(image)
                .old_layout(old)
                .new_layout(new)
                .src_access_mask(src_access)
                .dst_access_mask(dst_access)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .subresource_range(range)
        };
        use vk::{AccessFlags as A, ImageLayout as L, PipelineStageFlags as P};
        // Chains on the semaphore wait (all stages) that covers the app's
        // writes to `src`.
        let before = [
            barrier(
                src,
                L::PRESENT_SRC_KHR,
                L::TRANSFER_SRC_OPTIMAL,
                A::MEMORY_WRITE,
                A::TRANSFER_READ,
            ),
            barrier(
                s.image,
                L::UNDEFINED,
                L::TRANSFER_DST_OPTIMAL,
                A::empty(),
                A::TRANSFER_WRITE,
            ),
        ];
        let foreign = if ex.foreign {
            vk::QUEUE_FAMILY_FOREIGN_EXT
        } else {
            vk::QUEUE_FAMILY_EXTERNAL
        };
        let after = [
            barrier(
                src,
                L::TRANSFER_SRC_OPTIMAL,
                L::PRESENT_SRC_KHR,
                A::TRANSFER_READ,
                A::empty(),
            ),
            // Hand the image to Vesper (another driver: VAAPI).
            barrier(
                s.image,
                L::TRANSFER_DST_OPTIMAL,
                L::GENERAL,
                A::TRANSFER_WRITE,
                A::empty(),
            )
            .src_queue_family_index(family)
            .dst_queue_family_index(foreign),
        ];
        let size = vk::Offset3D {
            x: extent.width as i32,
            y: extent.height as i32,
            z: 1,
        };
        unsafe {
            check(
                d.reset_command_buffer(s.cmd, vk::CommandBufferResetFlags::empty()),
                "reset command buffer",
            )?;
            check(
                d.begin_command_buffer(
                    s.cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                ),
                "begin command buffer",
            )?;
            d.cmd_pipeline_barrier(
                s.cmd,
                P::ALL_COMMANDS,
                P::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &before,
            );
            if blit {
                let region = vk::ImageBlit::default()
                    .src_subresource(layers)
                    .src_offsets([vk::Offset3D::default(), size])
                    .dst_subresource(layers)
                    .dst_offsets([vk::Offset3D::default(), size]);
                d.cmd_blit_image(
                    s.cmd,
                    src,
                    L::TRANSFER_SRC_OPTIMAL,
                    s.image,
                    L::TRANSFER_DST_OPTIMAL,
                    &[region],
                    vk::Filter::NEAREST,
                );
            } else {
                let region = vk::ImageCopy::default()
                    .src_subresource(layers)
                    .dst_subresource(layers)
                    .extent(vk::Extent3D {
                        width: extent.width,
                        height: extent.height,
                        depth: 1,
                    });
                d.cmd_copy_image(
                    s.cmd,
                    src,
                    L::TRANSFER_SRC_OPTIMAL,
                    s.image,
                    L::TRANSFER_DST_OPTIMAL,
                    &[region],
                );
            }
            d.cmd_pipeline_barrier(
                s.cmd,
                P::TRANSFER,
                P::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &after,
            );
            check(d.end_command_buffer(s.cmd), "end command buffer")?;

            let waits = if info.p_wait_semaphores.is_null() {
                &[][..]
            } else {
                std::slice::from_raw_parts(
                    info.p_wait_semaphores,
                    info.wait_semaphore_count as usize,
                )
            };
            let stages = vec![P::ALL_COMMANDS; waits.len()];
            let signals = [present_sem, s.sync];
            let cmds = [s.cmd];
            let submit = vk::SubmitInfo::default()
                .wait_semaphores(waits)
                .wait_dst_stage_mask(&stages)
                .command_buffers(&cmds)
                .signal_semaphores(&signals);
            check(d.reset_fences(&[s.fence]), "reset fence")?;
            if let Err(e) = d.queue_submit(queue, &[submit], s.fence) {
                // The reset fence would never signal: replace it with a signaled one.
                d.destroy_fence(s.fence, None);
                s.fence = d
                    .create_fence(
                        &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                        None,
                    )
                    .unwrap_or(vk::Fence::null());
                return Err(format!("submit copy: {e}"));
            }
        }
        Ok(())
    }

    /// DRM modifiers usable for the ring, with their plane counts, in the
    /// server's preference order. Empty: use plain linear tiling.
    fn modifiers(&self, format: vk::Format, blit: bool, extent: vk::Extent2D) -> Vec<(u64, u32)> {
        let ex = self.export.as_ref().expect("active device");
        let (Some(_), Some(props2)) = (&ex.drm, self.inst.format_properties2) else {
            return Vec::new();
        };
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        unsafe {
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            props2(self.physical, format, &mut props);
        }
        let mut all = vec![
            vk::DrmFormatModifierPropertiesEXT::default();
            list.drm_format_modifier_count as usize
        ];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut all);
        unsafe {
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            props2(self.physical, format, &mut props);
        }
        let count = list.drm_format_modifier_count as usize;
        let mut need = vk::FormatFeatureFlags::TRANSFER_DST;
        if blit {
            need |= vk::FormatFeatureFlags::BLIT_DST;
        }
        let mut out: Vec<(u64, u32)> = all[..count.min(all.len())]
            .iter()
            .filter(|m| m.drm_format_modifier_tiling_features.contains(need))
            .filter(|m| self.exportable(format, m.drm_format_modifier, extent))
            .map(|m| (m.drm_format_modifier, m.drm_format_modifier_plane_count))
            .collect();
        let wanted = ipc::modifiers();
        if !wanted.is_empty() {
            out.retain(|(m, _)| wanted.contains(m));
            out.sort_by_key(|(m, _)| wanted.iter().position(|w| w == m));
        }
        out
    }

    fn exportable(&self, format: vk::Format, modifier: u64, extent: vk::Extent2D) -> bool {
        let Some(f) = self.inst.image_format_properties2 else {
            return true;
        };
        let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
            .drm_format_modifier(modifier)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let mut ext_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let fi = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(format)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_DST)
            .push_next(&mut drm)
            .push_next(&mut ext_info);
        let mut ext_props = vk::ExternalImageFormatProperties::default();
        let mut props = vk::ImageFormatProperties2::default().push_next(&mut ext_props);
        let r = unsafe { f(self.physical, &fi, &mut props) };
        let max = props.image_format_properties.max_extent;
        r == vk::Result::SUCCESS
            && max.width >= extent.width
            && max.height >= extent.height
            && ext_props
                .external_memory_properties
                .external_memory_features
                .contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE)
    }

    /// `Ok(None)` when Vesper could not take the buffers right now.
    fn create_ring(&self, sc: &Swapchain, family: u32, start: u64) -> Result<Option<Ring>, String> {
        let ex = self.export.as_ref().expect("active device");
        let (format, fourcc, blit) = ring_format(sc.format).ok_or("unsupported format")?;
        let flags = ex
            .families
            .get(family as usize)
            .ok_or("unknown queue family")?
            .queue_flags;
        if !flags.intersects(
            vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE | vk::QueueFlags::TRANSFER,
        ) {
            return Err("present queue cannot copy images".into());
        }
        if blit {
            if !flags.contains(vk::QueueFlags::GRAPHICS) {
                return Err("format conversion needs a graphics queue".into());
            }
            let props = unsafe {
                self.inst
                    .fns
                    .get_physical_device_format_properties(self.physical, sc.format)
            };
            if !props
                .optimal_tiling_features
                .contains(vk::FormatFeatureFlags::BLIT_SRC)
            {
                return Err("swapchain format cannot be blitted".into());
            }
        }
        let modifiers = self.modifiers(format, blit, sc.extent);
        if modifiers.is_empty() {
            let props = unsafe {
                self.inst
                    .fns
                    .get_physical_device_format_properties(self.physical, format)
            };
            let need = if blit {
                vk::FormatFeatureFlags::BLIT_DST
            } else {
                vk::FormatFeatureFlags::TRANSFER_DST
            };
            if !props.linear_tiling_features.contains(need) {
                return Err("no linear layout usable for the ring".into());
            }
        }
        let pool = unsafe {
            self.fns.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )
        };
        let mut ring = Ring {
            start_id: start,
            generation: GENERATION.fetch_add(1, Ordering::SeqCst).wrapping_add(1),
            family,
            pool: check(pool, "create command pool")?,
            blit,
            slots: Vec::new(),
        };
        for index in 0..RING {
            let (fd, mut msg) = match self.create_slot(&mut ring, format, sc.extent, &modifiers) {
                Ok(r) => r,
                Err(e) => {
                    self.destroy_ring(ring);
                    return Err(e);
                }
            };
            if let Msg::Buffer {
                index: i,
                fourcc: f,
                ..
            } = &mut msg
            {
                (*i, *f) = (index as u32, fourcc);
            }
            if !ipc::send(&msg, Some(fd.as_fd())) {
                self.destroy_ring(ring);
                return Ok(None);
            }
        }
        let (w, h) = (sc.extent.width, sc.extent.height);
        debug(&format!(
            "ring of {RING} {w}x{h} images, VkFormat {} from {} (blit: {blit}, modifiers offered: {})",
            format.as_raw(),
            sc.format.as_raw(),
            modifiers.len()
        ));
        Ok(Some(ring))
    }

    /// Creates one exported ring image. Returns its dma-buf and its `Buffer`
    /// message (index and fourcc filled in by the caller).
    fn create_slot(
        &self,
        ring: &mut Ring,
        format: vk::Format,
        extent: vk::Extent2D,
        modifiers: &[(u64, u32)],
    ) -> Result<(OwnedFd, Msg), String> {
        let d = &self.fns;
        let ex = self.export.as_ref().expect("active device");
        let mods: Vec<u64> = modifiers.iter().map(|(m, _)| *m).collect();
        let mut ext = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut list =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&mods);
        let mut ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::LINEAR)
            .usage(vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut ext);
        if !mods.is_empty() {
            ci = ci
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .push_next(&mut list);
        }
        let image = check(unsafe { d.create_image(&ci, None) }, "create ring image")?;
        // Pushed first so `destroy_ring` frees whatever exists on error.
        ring.slots.push(Slot {
            image,
            memory: vk::DeviceMemory::null(),
            cmd: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            sync: vk::Semaphore::null(),
            free: true,
        });
        let slot = ring.slots.last_mut().expect("pushed above");

        let reqs = unsafe { d.get_image_memory_requirements(image) };
        let types = &ex.memory.memory_types[..ex.memory.memory_type_count as usize];
        let usable = |i: usize| reqs.memory_type_bits & (1 << i) != 0;
        let type_index = (0..types.len())
            .find(|&i| {
                usable(i)
                    && types[i]
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            })
            .or_else(|| (0..types.len()).find(|&i| usable(i)))
            .ok_or("no memory type for the ring image")?;
        let mut export = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut ai = vk::MemoryAllocateInfo::default()
            .allocation_size(reqs.size)
            .memory_type_index(type_index as u32)
            .push_next(&mut export);
        if ex.dedicated {
            ai = ai.push_next(&mut dedicated);
        }
        slot.memory = check(
            unsafe { d.allocate_memory(&ai, None) },
            "allocate ring memory",
        )?;
        check(
            unsafe { d.bind_image_memory(image, slot.memory, 0) },
            "bind ring memory",
        )?;

        let alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(ring.pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        slot.cmd = check(
            unsafe { d.allocate_command_buffers(&alloc) },
            "allocate command buffer",
        )?[0];
        slot.fence = check(
            unsafe {
                d.create_fence(
                    &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                    None,
                )
            },
            "create fence",
        )?;
        let mut sem_export = vk::ExportSemaphoreCreateInfo::default()
            .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        slot.sync = check(
            unsafe {
                d.create_semaphore(
                    &vk::SemaphoreCreateInfo::default().push_next(&mut sem_export),
                    None,
                )
            },
            "create exportable semaphore",
        )?;

        let get_fd = vk::MemoryGetFdInfoKHR::default()
            .memory(slot.memory)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut fd = -1;
        let r = unsafe { (ex.memory_fd.get_memory_fd_khr)(self.handle, &get_fd, &mut fd) };
        if r != vk::Result::SUCCESS || fd < 0 {
            return Err(format!("export dma-buf: {r}"));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        let (modifier, aspects) = match &ex.drm {
            Some(drm) if !mods.is_empty() => {
                let mut p = vk::ImageDrmFormatModifierPropertiesEXT::default();
                let r = unsafe {
                    (drm.get_image_drm_format_modifier_properties_ext)(self.handle, image, &mut p)
                };
                if r != vk::Result::SUCCESS {
                    return Err(format!("query image modifier: {r}"));
                }
                let planes = modifiers
                    .iter()
                    .find(|(m, _)| *m == p.drm_format_modifier)
                    .map_or(1, |(_, n)| *n);
                let aspects = [
                    vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
                    vk::ImageAspectFlags::MEMORY_PLANE_1_EXT,
                    vk::ImageAspectFlags::MEMORY_PLANE_2_EXT,
                    vk::ImageAspectFlags::MEMORY_PLANE_3_EXT,
                ];
                (
                    p.drm_format_modifier,
                    aspects[..(planes as usize).clamp(1, 4)].to_vec(),
                )
            }
            _ => (DRM_FORMAT_MOD_LINEAR, vec![vk::ImageAspectFlags::COLOR]),
        };
        let mut planes = Vec::new();
        for aspect in aspects {
            let l = unsafe {
                d.get_image_subresource_layout(
                    image,
                    vk::ImageSubresource::default().aspect_mask(aspect),
                )
            };
            let (Ok(offset), Ok(stride)) = (u32::try_from(l.offset), u32::try_from(l.row_pitch))
            else {
                return Err("ring image layout does not fit 32 bits".into());
            };
            planes.push(Plane { offset, stride });
        }
        let msg = Msg::Buffer {
            generation: ring.generation,
            index: 0,
            width: extent.width,
            height: extent.height,
            fourcc: 0,
            modifier,
            planes,
        };
        Ok((fd, msg))
    }

    fn destroy_ring(&self, ring: Ring) {
        let d = &self.fns;
        unsafe {
            let mut leaked = false;
            for s in ring.slots {
                if s.fence != vk::Fence::null() {
                    // Bounded: a lost device must not hang the game.
                    let done = d.wait_for_fences(
                        &[s.fence],
                        true,
                        Duration::from_secs(1).as_nanos() as u64,
                    );
                    if done.is_err() {
                        // The copy may still run: leak rather than free under the GPU.
                        warn("ring image still in use, leaking it");
                        leaked = true;
                        continue;
                    }
                    d.destroy_fence(s.fence, None);
                }
                d.destroy_semaphore(s.sync, None);
                d.destroy_image(s.image, None);
                d.free_memory(s.memory, None);
            }
            if !leaked {
                d.destroy_command_pool(ring.pool, None);
            }
        }
    }
}

// --- Hooks (installed only on active devices) ---

pub(crate) unsafe extern "system" fn get_device_queue(
    device: vk::Device,
    family: u32,
    index: u32,
    out: *mut vk::Queue,
) {
    let Some(dev) = device_for(key(device)) else {
        unsafe { *out = vk::Queue::null() };
        return;
    };
    unsafe { (dev.fns.fp_v1_0().get_device_queue)(device, family, index, out) };
    guard((), || unsafe { remember_queue(&dev, *out, family) });
}

pub(crate) unsafe extern "system" fn get_device_queue2(
    device: vk::Device,
    info: *const vk::DeviceQueueInfo2<'_>,
    out: *mut vk::Queue,
) {
    let Some(dev) = device_for(key(device)) else {
        unsafe { *out = vk::Queue::null() };
        return;
    };
    unsafe { (dev.fns.fp_v1_1().get_device_queue2)(device, info, out) };
    guard((), || unsafe {
        remember_queue(&dev, *out, (*info).queue_family_index)
    });
}

fn remember_queue(dev: &Device, queue: vk::Queue, family: u32) {
    if queue == vk::Queue::null() {
        return;
    }
    let mut queues = dev.queues.locked();
    if !queues.iter().any(|(q, _)| *q == queue) {
        queues.push((queue, family));
    }
}

pub(crate) unsafe extern "system" fn create_swapchain(
    device: vk::Device,
    ci: *const vk::SwapchainCreateInfoKHR<'_>,
    alloc: *const vk::AllocationCallbacks<'_>,
    out: *mut vk::SwapchainKHR,
) -> vk::Result {
    let Some(dev) = device_for(key(device)) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let next = dev.swapchain.create_swapchain_khr;
    let mut wants = guard(false, || dev.wants(unsafe { &*ci }));
    let mut result = vk::Result::ERROR_INITIALIZATION_FAILED;
    if wants {
        let mut patched = unsafe { *ci };
        patched.image_usage |= vk::ImageUsageFlags::TRANSFER_SRC;
        result = unsafe { next(device, &patched, alloc, out) };
        // A failed create still retires `oldSwapchain`, so a retry would be
        // invalid; window-state errors are not about our usage flag anyway.
        let retry = patched.old_swapchain == vk::SwapchainKHR::null()
            && !matches!(
                result,
                vk::Result::SUCCESS
                    | vk::Result::ERROR_OUT_OF_DATE_KHR
                    | vk::Result::ERROR_SURFACE_LOST_KHR
                    | vk::Result::ERROR_NATIVE_WINDOW_IN_USE_KHR
                    | vk::Result::ERROR_DEVICE_LOST
            );
        if result != vk::Result::SUCCESS && !retry {
            return result;
        }
        if retry {
            debug("swapchain rejected TRANSFER_SRC, retrying without");
            wants = false;
        }
    }
    if !wants {
        result = unsafe { next(device, ci, alloc, out) };
    }
    if result != vk::Result::SUCCESS {
        return result;
    }
    guard((), || unsafe {
        let handle = *out;
        let mut images = Vec::new();
        let get = dev.swapchain.get_swapchain_images_khr;
        let mut n = 0;
        let mut ok = get(device, handle, &mut n, std::ptr::null_mut()) == vk::Result::SUCCESS;
        if ok {
            images = vec![vk::Image::null(); n as usize];
            ok = get(device, handle, &mut n, images.as_mut_ptr()) == vk::Result::SUCCESS;
            images.truncate(n as usize);
        }
        dev.swapchains.locked().push(Swapchain {
            handle,
            images,
            format: (*ci).image_format,
            extent: (*ci).image_extent,
            capturable: wants && ok,
            announced: 0,
            present_sems: Vec::new(),
            ring: None,
            next_ring: None,
        });
        if wants && ok {
            ipc::ensure_thread();
        }
    });
    result
}

pub(crate) unsafe extern "system" fn destroy_swapchain(
    device: vk::Device,
    swapchain: vk::SwapchainKHR,
    alloc: *const vk::AllocationCallbacks<'_>,
) {
    let Some(dev) = device_for(key(device)) else {
        return;
    };
    let sc = guard(None, || {
        let mut list = dev.swapchains.locked();
        let i = list.iter().position(|s| s.handle == swapchain)?;
        let mut sc = list.swap_remove(i);
        if let Some(ring) = sc.ring.take() {
            dev.destroy_ring(ring);
        }
        Some(sc)
    });
    unsafe { (dev.swapchain.destroy_swapchain_khr)(device, swapchain, alloc) };
    if let Some(mut sc) = sc {
        guard((), || dev.forget(&mut sc));
    }
}

pub(crate) unsafe extern "system" fn queue_present(
    queue: vk::Queue,
    info: *const vk::PresentInfoKHR<'_>,
) -> vk::Result {
    let Some(dev) = device_for(key(queue)) else {
        return vk::Result::ERROR_DEVICE_LOST;
    };
    let next = dev.swapchain.queue_present_khr;
    // Dormant: Vesper is not running.
    if !IPC.connected.load(Ordering::Relaxed) {
        if dev.has_ring.load(Ordering::Relaxed) {
            guard((), || dev.drop_rings());
        }
        return unsafe { next(queue, info) };
    }
    let wait = guard(None, || dev.before_present(queue, unsafe { &*info }));
    let Some(sem) = wait else {
        return unsafe { next(queue, info) };
    };
    let mut patched = unsafe { *info };
    patched.wait_semaphore_count = 1;
    patched.p_wait_semaphores = &sem;
    let result = unsafe { next(queue, &patched) };
    if matches!(
        result,
        vk::Result::ERROR_OUT_OF_HOST_MEMORY | vk::Result::ERROR_OUT_OF_DEVICE_MEMORY
    ) {
        // The wait may not have run: never signal that semaphore again.
        guard((), || dev.replace_present_sem(sem));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_formats() {
        // `vk::Format` has no Debug impl without ash's "debug" feature.
        let raw = |f: vk::Format| {
            let (r, cc, blit) = ring_format(f).unwrap();
            (r.as_raw(), cc, blit)
        };
        let xr24 = (vk::Format::B8G8R8A8_UNORM.as_raw(), protocol::XR24, false);
        assert_eq!(raw(vk::Format::B8G8R8A8_SRGB), xr24);
        assert_eq!(raw(vk::Format::B8G8R8A8_UNORM), xr24);
        assert_eq!(
            raw(vk::Format::A8B8G8R8_UNORM_PACK32),
            (vk::Format::R8G8B8A8_UNORM.as_raw(), protocol::XB24, false)
        );
        assert_eq!(
            raw(vk::Format::A2B10G10R10_UNORM_PACK32),
            (vk::Format::B8G8R8A8_UNORM.as_raw(), protocol::XR24, true)
        );
        assert_eq!(
            raw(vk::Format::R16G16B16A16_SFLOAT),
            (vk::Format::B8G8R8A8_UNORM.as_raw(), protocol::XR24, true)
        );
        assert!(ring_format(vk::Format::R5G6B5_UNORM_PACK16).is_none());
    }
}
