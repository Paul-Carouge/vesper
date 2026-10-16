//! Webcams, straight from PipeWire (no portal, no raw V4L2): the camera
//! nodes (`media.class = Video/Source` from a device), and a [`Camera`]
//! streaming one of them by `node.name`.
//!
//! The mode is picked from what the node offers ([`modes::select`]): MJPEG
//! from 720p30 up, raw YUY2/NV12 below. MJPEG is decoded on the GPU (VCN
//! JPEG engine, VA-VPP to RGB) into DMA-BUF [`VideoFrame`]s, like every other
//! source; only when VAAPI cannot decode it does the CPU take over
//! ([`Camera::software_decode`], for a notice). Raw pictures are copied out
//! of PipeWire's shared memory as they are ([`CpuFrame`]): the compositor
//! uploads them and converts YUV to RGB in its shader.
//!
//! Frames are latest-wins: [`Camera::take`] returns the newest one, older
//! ones are dropped (and their buffers released) as new ones come.

mod decode;
pub mod modes;

use std::cell::{Cell, RefCell};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use pipewire as pw;
use pw::spa::param::ParamType;
use pw::spa::pod::Pod;
use pw::spa::sys as spa_sys;
use pw::sys::pw_buffer;
use pw::types::ObjectType;
use pw::{core::CoreRc, main_loop::MainLoopRc, registry::RegistryRc, stream::StreamRc};
use tracing::{info, warn};
use vesper_core::VideoFrame;

pub use modes::{Encoding, Matrix, Mode, Range};

use crate::pw::roundtrip;
use crate::{Result, format};

/// The biggest mode asked for, and the frame rate wanted.
const MAX_SIZE: (u32, u32) = (1920, 1080);
const FPS: u32 = 30;

/// Consecutive JPEG pictures the GPU may fail before its first success,
/// before the CPU takes over.
const GPU_TRIES: u32 = 3;
/// Consecutive failed pictures (about two seconds at 30 fps) that end the
/// stream.
const MAX_FAILURES: u32 = 60;
/// How long the camera may take to agree on a format once linked.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// A camera on the PipeWire graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraInfo {
    /// `node.name`: stable across replugs, what to save and open by.
    pub node_name: String,
    /// Human-readable name, e.g. "HD Pro Webcam C920".
    pub description: String,
}

/// The camera a node with these properties is, if any. Only nodes of a
/// device (`device.id`: v4l2, libcamera): screencasts and other streams are
/// `Video/Source` too. (`device.api` would say more, but the registry does
/// not announce it.)
fn camera_info(
    class: Option<&str>,
    device: Option<&str>,
    name: Option<&str>,
    description: Option<&str>,
    nick: Option<&str>,
) -> Option<CameraInfo> {
    if class != Some("Video/Source") || device.is_none_or(str::is_empty) {
        return None;
    }
    let name = name.filter(|n| !n.is_empty())?;
    let description = [description, nick]
        .into_iter()
        .flatten()
        .find(|d| !d.is_empty())
        .unwrap_or(name);
    Some(CameraInfo {
        node_name: name.to_owned(),
        description: description.to_owned(),
    })
}

fn info_of(props: &pw::spa::utils::dict::DictRef) -> Option<CameraInfo> {
    camera_info(
        props.get(*pw::keys::MEDIA_CLASS),
        props.get(*pw::keys::DEVICE_ID),
        props.get(*pw::keys::NODE_NAME),
        props.get(*pw::keys::NODE_DESCRIPTION),
        props.get(*pw::keys::NODE_NICK),
    )
}

/// The cameras on the PipeWire graph now, sorted by description. Blocks up
/// to a few seconds when PipeWire does not answer: call it off the UI thread.
pub fn cameras() -> Result<Vec<CameraInfo>> {
    pw::init();
    let mainloop = MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;
    let found: Rc<RefCell<Vec<CameraInfo>>> = Rc::default();
    let _listener = registry
        .add_listener_local()
        .global({
            let found = found.clone();
            move |global| {
                if global.type_ == ObjectType::Node
                    && let Some(info) = global.props.and_then(info_of)
                {
                    found.borrow_mut().push(info);
                }
            }
        })
        .register();
    roundtrip(&mainloop, &core)?;
    let mut list = found.take();
    list.sort_by(|a, b| (&a.description, &a.node_name).cmp(&(&b.description, &b.node_name)));
    list.dedup_by(|a, b| a.node_name == b.node_name);
    Ok(list)
}

/// Layout of a [`CpuFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuLayout {
    /// One plane, `Y0 U Y1 V` per two pixels.
    Yuy2,
    /// Two planes: Y, then interleaved UV at half width and height.
    Nv12,
}

/// One plane of a [`CpuFrame`] in its `data`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuPlane {
    pub offset: usize,
    pub stride: usize,
}

/// A YUV camera picture in system memory, as the camera sent it (raw
/// modes) or as the software JPEG decoder made it (the fallback). Not
/// converted: the consumer uploads `data` and converts to RGB on the GPU
/// with `matrix` and `range`. Every plane fits in `data` (checked).
#[derive(Debug)]
pub struct CpuFrame {
    pub width: u32,
    pub height: u32,
    pub layout: CpuLayout,
    /// One plane for YUY2, two for NV12.
    pub planes: Vec<CpuPlane>,
    pub data: Vec<u8>,
    pub matrix: Matrix,
    pub range: Range,
    /// `CLOCK_MONOTONIC` presentation time.
    pub pts: Duration,
}

/// A camera picture.
#[derive(Debug)]
pub enum CameraFrame {
    /// RGB (`XR24`-like) DMA-BUF, from the GPU JPEG decoder.
    Gpu(VideoFrame),
    /// YUV in system memory: raw modes and the CPU JPEG fallback.
    Cpu(CpuFrame),
}

impl CameraFrame {
    pub fn pts(&self) -> Duration {
        match self {
            Self::Gpu(f) => f.pts,
            Self::Cpu(f) => f.pts,
        }
    }
}

/// What the camera threads hand to the [`Camera`].
#[derive(Default)]
struct Slot {
    frame: Option<CameraFrame>,
    /// Why the stream ended; no frame follows.
    ended: Option<String>,
    mode: Option<Mode>,
    software: bool,
    /// The newest JPEG for the decode thread, and whether it must stop.
    jpeg: Option<(Vec<u8>, Duration)>,
    stop: bool,
}

struct Shared {
    slot: Mutex<Slot>,
    /// Wakes the decode thread.
    jpeg_ready: Condvar,
    /// Wakes the PipeWire loop, which then checks `stop`.
    wake: pw::channel::Sender<()>,
}

impl Shared {
    fn new(wake: pw::channel::Sender<()>) -> Self {
        Self {
            slot: Mutex::default(),
            jpeg_ready: Condvar::new(),
            wake,
        }
    }

    /// Stops both threads; `stop` is set before they wake to check it.
    fn stop(&self) -> Option<CameraFrame> {
        let old = {
            let mut slot = self.lock();
            slot.stop = true;
            slot.jpeg = None;
            slot.frame.take()
        };
        self.jpeg_ready.notify_all();
        // Fails only once the PipeWire thread is gone.
        let _ = self.wake.send(());
        old
    }

    fn lock(&self) -> MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publish(&self, frame: CameraFrame) {
        let old = {
            let mut slot = self.lock();
            if slot.ended.is_some() || slot.stop {
                return;
            }
            slot.frame.replace(frame)
        };
        // Released outside the lock: a GPU frame's release unrefs a surface.
        drop(old);
    }

    /// Ends the stream once with `reason`, and stops both threads: the
    /// camera is released at once, not when the caller drops the [`Camera`].
    fn end(&self, reason: String) {
        {
            let mut slot = self.lock();
            if slot.ended.is_none() {
                warn!("camera stream ended: {reason}");
                slot.ended = Some(reason);
            }
        }
        drop(self.stop());
    }
}

/// A streaming camera. Dropping it stops the stream without waiting.
pub struct Camera {
    shared: Arc<Shared>,
}

impl Camera {
    /// Starts streaming the camera named `node_name` on a thread of its own:
    /// returns at once. Finding the node, negotiating and decoding happen on
    /// that thread; a failure shows in [`Camera::take`]. Errs only if the
    /// thread cannot start.
    pub fn open(node_name: &str) -> Result<Self> {
        let (wake, stop_rx) = pw::channel::channel();
        let shared = Arc::new(Shared::new(wake));
        let name = node_name.to_owned();
        let s = shared.clone();
        std::thread::Builder::new()
            .name("vesper-camera".into())
            .spawn(move || {
                match catch_unwind(AssertUnwindSafe(|| run(&name, &s, stop_rx))) {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => s.end(e.to_string()),
                    // Ends the decode thread too, and tells the caller.
                    Err(_) => s.end("the camera thread crashed".into()),
                }
            })?;
        Ok(Self { shared })
    }

    /// The newest picture since the last call, if any. Errs once the stream
    /// ended (camera unplugged, PipeWire gone, no usable format...), with
    /// the reason; reopening is the caller's call.
    pub fn take(&self) -> Result<Option<CameraFrame>> {
        let mut slot = self.shared.lock();
        match &slot.ended {
            Some(reason) => Err(reason.clone().into()),
            None => Ok(slot.frame.take()),
        }
    }

    /// The mode the camera streams in, once negotiated.
    pub fn mode(&self) -> Option<Mode> {
        self.shared.lock().mode
    }

    /// Whether MJPEG is decoded on the CPU because the GPU could not: worth
    /// a notice, it costs a core at 1080p.
    pub fn software_decode(&self) -> bool {
        self.shared.lock().software
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        // The threads exit on their own: never wait on PipeWire or the GPU here.
        drop(self.shared.stop());
    }
}

/// The decode thread: decodes the newest JPEG, drops the ones it had no
/// time for.
fn decode_loop(shared: &Shared) {
    let mut decoder = match decode::Decoder::gpu() {
        Ok(d) => Some(d),
        Err(e) => {
            warn!("no GPU JPEG decoding ({e}): decoding on the CPU");
            shared.lock().software = true;
            None
        }
    };
    let (mut gpu_worked, mut failures) = (false, 0u32);
    loop {
        let (jpeg, pts) = {
            let mut slot = shared.lock();
            loop {
                if slot.stop {
                    return;
                }
                if let Some(j) = slot.jpeg.take() {
                    break j;
                }
                slot = shared
                    .jpeg_ready
                    .wait(slot)
                    .unwrap_or_else(|e| e.into_inner());
            }
        };
        if decoder.is_none() {
            match decode::Decoder::cpu() {
                Ok(d) => decoder = Some(d),
                Err(e) => return shared.end(format!("cannot decode the camera's MJPEG: {e}")),
            }
        }
        let d = decoder.as_mut().expect("set above");
        match d.decode(&jpeg, pts) {
            Ok(frame) => {
                gpu_worked |= d.is_gpu();
                failures = 0;
                shared.publish(frame);
            }
            Err(e) => {
                failures += 1;
                // Webcams send a torn picture now and then: skip it.
                warn!("camera picture dropped: {e}");
                if e.is::<decode::Unsupported>() {
                    return shared.end(e.to_string());
                }
                if d.is_gpu() && !gpu_worked && failures >= GPU_TRIES {
                    warn!("the GPU cannot decode this camera's MJPEG: decoding on the CPU");
                    shared.lock().software = true;
                    decoder = None;
                    failures = 0;
                } else if failures >= MAX_FAILURES {
                    return shared.end(format!("the camera sends no decodable picture: {e}"));
                }
            }
        }
    }
}

/// What the camera's stream callbacks share.
struct State {
    shared: Arc<Shared>,
    mainloop: MainLoopRc,
    format: Option<(Mode, Matrix, Range)>,
    /// Started on the first MJPEG format.
    decoding: bool,
}

impl State {
    fn end(&mut self, reason: String) {
        self.shared.end(reason);
        self.mainloop.quit();
    }

    fn on_format(&mut self, stream: &pw::stream::Stream, pod: Option<&Pod>) {
        self.format = pod.and_then(|p| modes::negotiated(p.as_bytes()));
        let Some((mode, ..)) = self.format else {
            if let Some(pod) = pod {
                let value = pw::spa::pod::deserialize::PodDeserializer::deserialize_any_from(
                    pod.as_bytes(),
                )
                .map(|(_, v)| v);
                warn!("ignoring an unusable camera format: {value:?}");
            }
            return;
        };
        info!("camera negotiated {mode:?}");
        self.shared.lock().mode = Some(mode);
        if mode.encoding == Encoding::Mjpeg && !self.decoding {
            let shared = self.shared.clone();
            let spawned = std::thread::Builder::new()
                .name("vesper-camera-decode".into())
                .spawn(move || {
                    if catch_unwind(AssertUnwindSafe(|| decode_loop(&shared))).is_err() {
                        shared.end("the camera decode thread crashed".into());
                    }
                });
            if let Err(e) = spawned {
                return self.end(format!("cannot start the decode thread: {e}"));
            }
            self.decoding = true;
        }
        let params = [modes::buffers_param(), format::header_meta_param()];
        let mut pods: Vec<&Pod> = params.iter().filter_map(|b| Pod::from_bytes(b)).collect();
        if let Err(e) = stream.update_params(&mut pods) {
            warn!("pw_stream_update_params failed: {e}");
        }
    }

    /// Hands a picture on: the JPEG to the decode thread, a raw one copied.
    ///
    /// # Safety
    /// `buffer` must be dequeued from the stream and not yet requeued.
    unsafe fn on_buffer(&mut self, buffer: *mut pw_buffer) {
        let Some((mode, matrix, range)) = self.format else {
            return;
        };
        if self.shared.lock().stop {
            return;
        }
        // SAFETY: per the contract.
        let Some((datas, pts)) = (unsafe { buffer_data(buffer) }) else {
            return;
        };
        if mode.encoding == Encoding::Mjpeg {
            let [(jpeg, _)] = datas[..] else {
                return;
            };
            let mut slot = self.shared.lock();
            // Ended or closing: nothing to copy (the stream stops right after).
            if slot.stop {
                return;
            }
            // The decoder had no time for the previous one: it is dropped.
            slot.jpeg = Some((jpeg.to_vec(), pts));
            drop(slot);
            self.shared.jpeg_ready.notify_one();
            return;
        }
        match raw_frame(mode, &datas) {
            Some((layout, planes, data)) => self.shared.publish(CameraFrame::Cpu(CpuFrame {
                width: mode.width,
                height: mode.height,
                layout,
                planes,
                data,
                matrix,
                range,
                pts,
            })),
            None => warn!("camera buffer too small for {mode:?}: dropped"),
        }
    }
}

/// The mapped bytes of one buffer data, and its stride (0: unknown).
type Data<'a> = (&'a [u8], usize);

/// The mapped bytes and stride of each data of `buffer`, and its pts.
///
/// # Safety
/// `buffer` must be dequeued from a `MAP_BUFFERS` stream and not requeued;
/// the slices live until it is.
unsafe fn buffer_data<'a>(buffer: *mut pw_buffer) -> Option<(Vec<Data<'a>>, Duration)> {
    unsafe {
        let spa_buf = (*buffer).buffer;
        if spa_buf.is_null() || (*spa_buf).n_datas == 0 {
            return None;
        }
        let header = spa_sys::spa_buffer_find_meta_data(
            spa_buf,
            spa_sys::SPA_META_Header,
            size_of::<spa_sys::spa_meta_header>(),
        ) as *const spa_sys::spa_meta_header;
        let header = header.as_ref();
        if header.is_some_and(|h| h.flags & spa_sys::SPA_META_HEADER_FLAG_CORRUPTED != 0) {
            return None;
        }
        let datas = std::slice::from_raw_parts((*spa_buf).datas, (*spa_buf).n_datas as usize);
        let mut out = Vec::with_capacity(datas.len());
        for d in datas {
            let chunk = d.chunk.as_ref()?;
            if d.data.is_null() || chunk.flags & spa_sys::SPA_CHUNK_FLAG_CORRUPTED as i32 != 0 {
                return None;
            }
            // The producer's numbers are not trusted to stay in the mapping.
            let offset = (chunk.offset % d.maxsize.max(1)) as usize;
            let size = (chunk.size as usize).min(d.maxsize as usize - offset);
            if size == 0 {
                return None;
            }
            let bytes = std::slice::from_raw_parts((d.data as *const u8).add(offset), size);
            out.push((bytes, chunk.stride.max(0) as usize));
        }
        Some((out, format::frame_pts(header.map(|h| h.pts))))
    }
}

/// Copies a raw picture out of its buffer datas (one data, or one per
/// plane), checking every plane fits.
fn raw_frame(mode: Mode, datas: &[Data]) -> Option<(CpuLayout, Vec<CpuPlane>, Vec<u8>)> {
    let (w, h) = (mode.width as usize, mode.height as usize);
    // (layout, bytes per row and rows of each plane)
    let (layout, shape): (_, &[(usize, usize)]) = match mode.encoding {
        Encoding::Yuy2 => (CpuLayout::Yuy2, &[(w.div_ceil(2) * 4, h)]),
        Encoding::Nv12 => (
            CpuLayout::Nv12,
            &[(w, h), (w.div_ceil(2) * 2, h.div_ceil(2))],
        ),
        Encoding::Mjpeg => return None,
    };
    let fits = |stride: usize, (row, _): (usize, usize)| stride >= row;
    let need = |stride: usize, (row, rows): (usize, usize)| stride * (rows - 1) + row;
    let mut planes = Vec::with_capacity(shape.len());
    let data = match datas {
        // All planes in one data, each right after the previous one.
        [(bytes, stride)] => {
            let stride = if *stride == 0 { shape[0].0 } else { *stride };
            let mut offset = 0;
            for &s in shape {
                if !fits(stride, s) || offset + need(stride, s) > bytes.len() {
                    return None;
                }
                planes.push(CpuPlane { offset, stride });
                offset += stride * s.1;
            }
            bytes.to_vec()
        }
        datas if datas.len() == shape.len() => {
            let mut data = Vec::new();
            for (&(bytes, stride), &s) in datas.iter().zip(shape) {
                let stride = if stride == 0 { s.0 } else { stride };
                if !fits(stride, s) || need(stride, s) > bytes.len() {
                    return None;
                }
                planes.push(CpuPlane {
                    offset: data.len(),
                    stride,
                });
                data.extend_from_slice(&bytes[..need(stride, s)]);
            }
            data
        }
        _ => return None,
    };
    Some((layout, planes, data))
}

/// Finds the node named `name` and what it offers.
fn find_camera(
    mainloop: &MainLoopRc,
    core: &CoreRc,
    registry: &RegistryRc,
    name: &str,
) -> Result<(u32, Vec<Mode>)> {
    let found: Rc<Cell<Option<u32>>> = Rc::default();
    let modes: Rc<RefCell<Vec<Mode>>> = Rc::default();
    // Listener first: it must drop before its proxy.
    let node: Rc<RefCell<Option<(pw::node::NodeListener, pw::node::Node)>>> = Rc::default();
    let _listener = registry
        .add_listener_local()
        .global({
            let (found, modes, node) = (found.clone(), modes.clone(), node.clone());
            let registry = registry.downgrade();
            let name = name.to_owned();
            move |global| {
                if global.type_ != ObjectType::Node
                    || found.get().is_some()
                    || global
                        .props
                        .and_then(info_of)
                        .is_none_or(|i| i.node_name != name)
                {
                    return;
                }
                let Some(proxy) = registry
                    .upgrade()
                    .and_then(|r| r.bind::<pw::node::Node, _>(global).ok())
                else {
                    return;
                };
                found.set(Some(global.id));
                let modes = modes.clone();
                let listener = proxy
                    .add_listener_local()
                    .param(move |_, _, _, _, pod| {
                        if let Some(pod) = pod {
                            modes.borrow_mut().extend(modes::enum_modes(pod.as_bytes()));
                        }
                    })
                    .register();
                proxy.enum_params(0, Some(ParamType::EnumFormat), 0, u32::MAX);
                *node.borrow_mut() = Some((listener, proxy));
            }
        })
        .register();
    // First roundtrip: globals in and enum_params sent. Second: its replies.
    roundtrip(mainloop, core)?;
    roundtrip(mainloop, core)?;
    let id = found
        .get()
        .ok_or_else(|| format!("camera {name} is not connected"))?;
    Ok((id, modes.take()))
}

/// The camera's PipeWire thread, until stopped or the stream ends.
fn run(name: &str, shared: &Arc<Shared>, stop: pw::channel::Receiver<()>) -> Result<()> {
    pw::init();
    let mainloop = MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;
    // Stopped while connecting: nothing to set up.
    let _stop = stop.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    // Unplugged: the node goes away (the stream would only pause). Watched
    // before the node is looked up, so a removal right after is not missed.
    let target: Rc<Cell<Option<u32>>> = Rc::default();
    let _registry_listener = registry
        .add_listener_local()
        .global_remove({
            let (shared, mainloop, target) = (shared.clone(), mainloop.clone(), target.clone());
            move |id| {
                if target.get() == Some(id) {
                    shared.end("camera unplugged".into());
                    mainloop.quit();
                }
            }
        })
        .register();
    let (node_id, offered) = find_camera(&mainloop, &core, &registry, name)?;
    target.set(Some(node_id));
    if shared.lock().stop {
        return Ok(());
    }
    let mode = modes::select(&offered, MAX_SIZE, FPS)
        .ok_or("the camera offers no supported format (MJPEG, YUY2 or NV12)")?;
    info!("camera {name}: asking for {mode:?}");
    let stream = StreamRc::new(
        core.clone(),
        "vesper-camera",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Camera",
        },
    )?;
    let state = State {
        shared: shared.clone(),
        mainloop: mainloop.clone(),
        format: None,
        decoding: false,
    };
    let stream_listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(|_, state, old, new| {
            info!("camera stream {old:?} -> {new:?}");
            match new {
                pw::stream::StreamState::Error(e) => state.end(format!("camera stream error: {e}")),
                pw::stream::StreamState::Unconnected => state.end("camera disconnected".into()),
                _ => {}
            }
        })
        .param_changed(|stream, state, id, pod| {
            if id == ParamType::Format.as_raw() {
                state.on_format(stream, pod);
            }
        })
        .process(|stream, state| {
            // Keep only the newest buffer; requeue older ones at once.
            let mut newest: *mut pw_buffer = std::ptr::null_mut();
            loop {
                // SAFETY: called on the stream's own thread.
                let buffer = unsafe { stream.dequeue_raw_buffer() };
                if buffer.is_null() {
                    break;
                }
                if !newest.is_null() {
                    unsafe { stream.queue_raw_buffer(newest) };
                }
                newest = buffer;
            }
            if !newest.is_null() {
                // SAFETY: dequeued above; the data is copied before the requeue.
                unsafe {
                    state.on_buffer(newest);
                    stream.queue_raw_buffer(newest);
                }
            }
        })
        .register()?;
    let _core_listener = core
        .add_listener_local()
        .error({
            let (shared, mainloop) = (shared.clone(), mainloop.clone());
            move |id, _, res, msg| {
                if id == pw::core::PW_ID_CORE {
                    shared.end(format!("PipeWire connection lost: {msg} ({res})"));
                    mainloop.quit();
                }
            }
        })
        .register();
    let pod = modes::format_pod(&mode);
    let mut pods: Vec<&Pod> = Pod::from_bytes(&pod).into_iter().collect();
    stream.connect(
        pw::spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut pods,
    )?;
    // Never linked (refused by the session manager, gone meanwhile): ends
    // instead of waiting forever.
    let timer = mainloop.loop_().add_timer({
        let (shared, mainloop) = (shared.clone(), mainloop.clone());
        move |_| {
            if shared.lock().mode.is_none() {
                shared.end("the camera did not start".into());
                mainloop.quit();
            }
        }
    });
    timer
        .update_timer(Some(START_TIMEOUT), None)
        .into_result()
        .map_err(|e| format!("arming the start timer failed: {e}"))?;
    let stopped = || {
        let slot = shared.lock();
        slot.stop || slot.ended.is_some()
    };
    while !stopped() {
        mainloop.run();
    }
    // Closing is not an end of stream to report.
    drop(stream_listener);
    if let Err(e) = stream.disconnect() {
        warn!("pw_stream_disconnect failed: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
