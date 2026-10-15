//! The PipeWire thread: format negotiation and frame delivery.

use std::cell::{Cell, OnceCell, RefCell};
use std::os::fd::{BorrowedFd, OwnedFd};
use std::rc::Rc;
use std::time::Duration;

use pipewire as pw;
use pw::spa::param::ParamType;
use pw::spa::param::video::VideoFormat;
use pw::spa::pod::Pod;
use pw::spa::sys as spa_sys;
use pw::sys::pw_buffer;
use pw::{core::CoreRc, main_loop::MainLoopRc, registry::RegistryRc, stream::StreamRc};
use tracing::{error, info, warn};
use vesper_core::{DmaBufPlane, ReleaseGuard, VideoFrame};

use crate::Error;
use crate::format;

/// A `pw_buffer` pointer sent back to the PipeWire thread.
pub struct BufferPtr(pub *mut pw_buffer);
// SAFETY: the pointer is only dereferenced on the PipeWire thread, which owns it.
unsafe impl Send for BufferPtr {}

/// Messages to the PipeWire thread.
pub enum Msg {
    /// A frame was dropped: requeue its buffer if its pool still exists.
    Release(BufferPtr, u32),
    Stop,
}

/// Handles one message. A release from an older pool generation is ignored:
/// that buffer was freed by the renegotiation.
pub fn handle_msg(
    msg: Msg,
    generation: u32,
    mut queue: impl FnMut(*mut pw_buffer),
    quit: impl FnOnce(),
) {
    match msg {
        Msg::Release(buffer, gen_of_frame) if gen_of_frame == generation => queue(buffer.0),
        Msg::Release(..) => {}
        Msg::Stop => quit(),
    }
}

/// How long one setup roundtrip may take before `Capture::start` gives up.
const ROUNDTRIP_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs `mainloop` until `finished` holds, which callbacks signal by quitting
/// the loop. Returns `Ok(false)` if `timeout` passes first.
fn run_until(
    mainloop: &MainLoopRc,
    finished: impl Fn() -> bool,
    timeout: Duration,
) -> Result<bool, Error> {
    let timed_out = Rc::new(Cell::new(false));
    let timer = mainloop.loop_().add_timer({
        let (timed_out, mainloop) = (timed_out.clone(), mainloop.clone());
        move |_| {
            timed_out.set(true);
            mainloop.quit();
        }
    });
    timer
        .update_timer(Some(timeout), None)
        .into_result()
        .map_err(|e| format!("arming the roundtrip timer failed: {e}"))?;
    while !finished() && !timed_out.get() {
        mainloop.run();
    }
    Ok(finished())
}

/// Runs a full roundtrip with the server.
fn roundtrip(mainloop: &MainLoopRc, core: &CoreRc) -> Result<(), Error> {
    let pending = core.sync(0)?.seq();
    let done = Rc::new(Cell::new(false));
    let failed = Rc::new(Cell::new(false));
    let _listener = core
        .add_listener_local()
        .done({
            let (done, mainloop) = (done.clone(), mainloop.clone());
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq.seq() == pending {
                    done.set(true);
                    mainloop.quit();
                }
            }
        })
        .error({
            let (failed, mainloop) = (failed.clone(), mainloop.clone());
            move |id, _, res, msg| {
                error!("pipewire core error on {id}: {msg} ({res})");
                failed.set(true);
                mainloop.quit();
            }
        })
        .register();
    if !run_until(mainloop, || done.get() || failed.get(), ROUNDTRIP_TIMEOUT)? {
        return Err("pipewire server did not answer in time".into());
    }
    if failed.get() {
        return Err("pipewire connection failed".into());
    }
    Ok(())
}

/// Formats the node can produce, each with its DMA-BUF modifiers (empty: shm only).
type Advertised = Vec<(VideoFormat, Vec<u64>)>;

/// Asks the screencast node which formats and modifiers it can produce.
fn advertised_formats(
    mainloop: &MainLoopRc,
    core: &CoreRc,
    registry: &RegistryRc,
    node_id: u32,
) -> Result<Advertised, Error> {
    let found: Rc<RefCell<Advertised>> = Rc::default();
    // Listener first: tuple fields drop in order, and a listener must go before
    // its proxy (dropping it unhooks from the proxy's list).
    let node: Rc<RefCell<Option<(pw::node::NodeListener, pw::node::Node)>>> = Rc::default();
    let _listener = registry
        .add_listener_local()
        .global({
            let (registry, found, node) = (registry.clone(), found.clone(), node.clone());
            move |global| {
                if global.id != node_id || global.type_ != pw::types::ObjectType::Node {
                    return;
                }
                let Ok(proxy) = registry.bind::<pw::node::Node, _>(global) else {
                    return;
                };
                let found = found.clone();
                let listener = proxy
                    .add_listener_local()
                    .param(move |_, _, _, _, pod| {
                        let Some(info) = pod.and_then(|p| format::parse_format(p.as_bytes()))
                        else {
                            return;
                        };
                        let mut found = found.borrow_mut();
                        for f in info.formats {
                            match found.iter_mut().find(|(g, _)| *g == f) {
                                Some((_, mods)) => {
                                    for m in &info.modifiers {
                                        if !mods.contains(m) {
                                            mods.push(*m);
                                        }
                                    }
                                }
                                None => found.push((f, info.modifiers.clone())),
                            }
                        }
                    })
                    .register();
                proxy.enum_params(0, Some(ParamType::EnumFormat), 0, u32::MAX);
                *node.borrow_mut() = Some((listener, proxy));
            }
        })
        .register();
    // First roundtrip: globals announced and enum_params sent. Second: replies in.
    roundtrip(mainloop, core)?;
    roundtrip(mainloop, core)?;
    if node.borrow().is_none() {
        return Err(format!("pipewire node {node_id} not found").into());
    }
    Ok(found.take())
}

/// The format both sides agreed on.
struct Negotiated {
    width: u32,
    height: u32,
    fourcc: u32,
    modifier: u64,
}

struct State<F> {
    on_frame: F,
    /// Our `EnumFormat` offer, reused when fixating a modifier.
    offer: Vec<Vec<u8>>,
    max_fps: u32,
    format: Option<Negotiated>,
    generation: Rc<Cell<u32>>,
    next_buffer_id: u32,
    tx: pw::channel::Sender<Msg>,
    mainloop: MainLoopRc,
    ended: bool,
}

impl<F: FnMut(Result<VideoFrame, Error>)> State<F> {
    /// Tells the caller the capture ended (once) and stops the loop.
    fn end(&mut self, reason: String) {
        if !self.ended {
            self.ended = true;
            error!("capture ended: {reason}");
            (self.on_frame)(Err(reason.into()));
            self.mainloop.quit();
        }
    }
}

fn update_params(stream: &pw::stream::Stream, pods: &[Vec<u8>]) {
    let mut pods: Vec<&Pod> = pods.iter().filter_map(|b| Pod::from_bytes(b)).collect();
    if let Err(e) = stream.update_params(&mut pods) {
        error!("pw_stream_update_params failed: {e}");
    }
}

fn on_format<F: FnMut(Result<VideoFrame, Error>)>(
    stream: &pw::stream::Stream,
    state: &mut State<F>,
    pod: &Pod,
) {
    // Until this format is accepted, no frame may go out with the old one.
    state.format = None;
    let Some(info) = format::parse_format(pod.as_bytes()) else {
        warn!("ignoring a non raw-video format");
        return;
    };
    let Some(&spa_format) = info.formats.first() else {
        warn!("ignoring a format without video format: {info:?}");
        return;
    };
    let Some(fourcc) = format::spa_to_drm(spa_format) else {
        error!("server picked unsupported format {spa_format:?}");
        return;
    };
    if info.dont_fixate && !info.modifiers.is_empty() {
        // The server lists modifiers by preference: take the first and renegotiate.
        info!(
            "fixating modifier {:#x} out of {} offered",
            info.modifiers[0],
            info.modifiers.len()
        );
        let mut pods = vec![format::enum_format(
            spa_format,
            &info.modifiers[..1],
            state.max_fps,
        )];
        pods.extend(state.offer.iter().cloned());
        update_params(stream, &pods);
        return;
    }
    let Some(size) = info.size else {
        warn!("ignoring a fixated format without size: {info:?}");
        return;
    };
    let Some(&modifier) = info.modifiers.first() else {
        return state.end(format!(
            "DMA-BUF negotiation failed: the server picked {spa_format:?} without a \
             modifier, and the encoder only imports DMA-BUF"
        ));
    };
    info!(
        "negotiated DMA-BUF {}x{} {spa_format:?} modifier {modifier:#x}",
        size.width, size.height
    );
    state.format = Some(Negotiated {
        width: size.width,
        height: size.height,
        fourcc,
        modifier,
    });
    update_params(
        stream,
        &[format::buffers_param(), format::header_meta_param()],
    );
}

/// Builds a frame from a dequeued buffer, or `None` if it carries no image.
///
/// # Safety
/// `buffer` must be a buffer dequeued from the stream and not yet requeued.
unsafe fn build_frame<F>(buffer: *mut pw_buffer, state: &State<F>) -> Option<VideoFrame> {
    let format = state.format.as_ref()?;
    // SAFETY: per the contract, `buffer` and its spa_buffer are valid.
    let spa_buf = unsafe { (*buffer).buffer };
    if spa_buf.is_null() || unsafe { (*spa_buf).n_datas == 0 } {
        return None;
    }
    let datas =
        unsafe { std::slice::from_raw_parts((*spa_buf).datas, (*spa_buf).n_datas as usize) };
    let header = unsafe {
        spa_sys::spa_buffer_find_meta_data(
            spa_buf,
            spa_sys::SPA_META_Header,
            size_of::<spa_sys::spa_meta_header>(),
        ) as *const spa_sys::spa_meta_header
    };
    let header = unsafe { header.as_ref() };
    if header.is_some_and(|h| h.flags & spa_sys::SPA_META_HEADER_FLAG_CORRUPTED != 0) {
        return None;
    }
    let mut planes = Vec::with_capacity(datas.len());
    for (i, data) in datas.iter().enumerate() {
        let chunk = unsafe { data.chunk.as_ref() }?;
        // A zero-sized first chunk is a cursor-only update with no new image.
        if (i == 0 && chunk.size == 0)
            || chunk.flags & spa_sys::SPA_CHUNK_FLAG_CORRUPTED as i32 != 0
        {
            return None;
        }
        if data.type_ != spa_sys::SPA_DATA_DmaBuf {
            warn!("skipping a non DMA-BUF buffer (data type {})", data.type_);
            return None;
        }
        // Planes may share one BO: dup an fd per plane anyway.
        // SAFETY: the fd is open while the buffer is dequeued.
        let fd: OwnedFd = unsafe { BorrowedFd::borrow_raw(data.fd as i32) }
            .try_clone_to_owned()
            .inspect_err(|e| error!("dup of buffer fd failed: {e}"))
            .ok()?;
        planes.push(DmaBufPlane {
            fd,
            offset: chunk.offset,
            stride: chunk.stride as u32,
        });
    }
    let generation = state.generation.get();
    let msg = Msg::Release(BufferPtr(buffer), generation);
    let tx = state.tx.clone();
    Some(VideoFrame {
        width: format.width,
        height: format.height,
        fourcc: format.fourcc,
        modifier: format.modifier,
        planes,
        buffer_id: unsafe { (*buffer).user_data } as usize as u32,
        pool_generation: generation,
        pts: format::frame_pts(header.map(|h| h.pts)),
        release: ReleaseGuard::new(move || {
            // Fails only after the capture stopped; the pool is gone then anyway.
            let _ = tx.send(msg);
        }),
    })
}

/// Connects to the portal's PipeWire remote and streams `node_id`, at most
/// `max_fps` frames per second, until a `Msg::Stop` arrives. Reports setup
/// success on `ready` before streaming.
pub fn run<F>(
    fd: OwnedFd,
    node_id: u32,
    max_fps: u32,
    on_frame: F,
    tx: pw::channel::Sender<Msg>,
    rx: pw::channel::Receiver<Msg>,
    ready: std::sync::mpsc::Sender<Result<(), Error>>,
) where
    F: FnMut(Result<VideoFrame, Error>) + 'static,
{
    pw::init();
    let mainloop = match MainLoopRc::new(None) {
        Ok(m) => m,
        Err(e) => return drop(ready.send(Err(e.into()))),
    };
    let setup = (|| -> Result<_, Error> {
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_fd_rc(fd, None)?;
        let generation = Rc::new(Cell::new(0u32));
        // An AUTOCONNECT stream only pauses when its source node goes away
        // (sharing stopped, window closed): watch the node itself from now on.
        // Disconnecting moves the stream to Unconnected, which ends the capture.
        let registry = core.get_registry_rc()?;
        let removed = Rc::new(Cell::new(false));
        let stream_slot: Rc<OnceCell<StreamRc>> = Rc::default();
        let node_listener = registry
            .add_listener_local()
            .global_remove({
                let (removed, stream_slot, generation) =
                    (removed.clone(), stream_slot.clone(), generation.clone());
                move |id| {
                    if id != node_id {
                        return;
                    }
                    removed.set(true);
                    // Releases still queued in this loop iteration must not
                    // requeue buffers that the disconnect frees.
                    generation.set(generation.get().wrapping_add(1));
                    if let Some(stream) = stream_slot.get()
                        && let Err(e) = stream.disconnect()
                    {
                        error!("pw_stream_disconnect failed: {e}");
                    }
                }
            })
            .register();
        let offer: Vec<Vec<u8>> = {
            let advertised = advertised_formats(&mainloop, &core, &registry, node_id)?;
            let supported: Vec<_> = advertised
                .into_iter()
                .filter(|(f, _)| format::spa_to_drm(*f).is_some())
                .collect();
            let offer: Vec<_> = supported
                .iter()
                .filter(|(_, mods)| !mods.is_empty())
                .map(|(f, mods)| format::enum_format(*f, mods, max_fps))
                .collect();
            if offer.is_empty() {
                return Err(
                    "screencast node offers no RGB DMA-BUF format with modifiers \
                            (the encoder only imports DMA-BUF)"
                        .into(),
                );
            }
            offer
        };
        let stream = StreamRc::new(
            core.clone(),
            "vesper-capture",
            pw::properties::properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Screen",
            },
        )?;
        let state = State {
            on_frame,
            offer: offer.clone(),
            max_fps,
            format: None,
            generation: generation.clone(),
            next_buffer_id: 0,
            tx,
            mainloop: mainloop.clone(),
            ended: false,
        };
        let listener = stream
            .add_local_listener_with_user_data(state)
            .state_changed(|_, state, old, new| {
                info!("stream state {old:?} -> {new:?}");
                match new {
                    pw::stream::StreamState::Error(e) => state.end(format!("stream error: {e}")),
                    pw::stream::StreamState::Unconnected => state.end("source closed".into()),
                    _ => {}
                }
            })
            .param_changed(|stream, state, id, pod| {
                if id != ParamType::Format.as_raw() {
                    return;
                }
                match pod {
                    Some(pod) => on_format(stream, state, pod),
                    None => state.format = None,
                }
            })
            .add_buffer(|_, state, buffer| {
                // SAFETY: PipeWire hands us a valid buffer; user_data is ours.
                unsafe { (*buffer).user_data = state.next_buffer_id as usize as *mut _ };
                state.next_buffer_id += 1;
            })
            .remove_buffer(|_, state, _| {
                // The pool is being replaced: frames still out must not requeue.
                state.generation.set(state.generation.get().wrapping_add(1));
                state.next_buffer_id = 0;
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
                if newest.is_null() {
                    return;
                }
                // SAFETY: `newest` is dequeued and not requeued.
                match unsafe { build_frame(newest, state) } {
                    // The buffer goes back from the frame's ReleaseGuard.
                    Some(frame) => (state.on_frame)(Ok(frame)),
                    None => unsafe { stream.queue_raw_buffer(newest) },
                }
            })
            .register()?;
        let mut pods: Vec<&Pod> = offer.iter().filter_map(|b| Pod::from_bytes(b)).collect();
        stream.connect(
            pw::spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT,
            &mut pods,
        )?;
        let _ = stream_slot.set(stream.clone());
        if removed.get() {
            return Err("source closed".into());
        }
        Ok((
            context,
            core,
            stream,
            listener,
            generation,
            registry,
            node_listener,
        ))
    })();
    let (_context, _core, stream, listener, generation, _registry, node_listener) = match setup {
        Ok(s) => s,
        Err(e) => return drop(ready.send(Err(e))),
    };
    let _rx = rx.attach(mainloop.loop_(), {
        let (stream, mainloop) = (stream.clone(), mainloop.clone());
        move |msg| {
            handle_msg(
                msg,
                generation.get(),
                // SAFETY: same generation, so the buffer is still in the pool.
                |buffer| unsafe { stream.queue_raw_buffer(buffer) },
                || mainloop.quit(),
            )
        }
    });
    let _ = ready.send(Ok(()));
    mainloop.run();
    // A normal stop is not an end of capture to report.
    drop(node_listener);
    drop(listener);
    if let Err(e) = stream.disconnect() {
        warn!("pw_stream_disconnect failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_from_another_thread_requeues_current_pool_only() {
        pw::init();
        let mainloop = MainLoopRc::new(None).unwrap();
        let (tx, rx) = pw::channel::channel::<Msg>();
        let queued: Rc<RefCell<Vec<usize>>> = Rc::default();
        let _rx = rx.attach(mainloop.loop_(), {
            let (queued, mainloop) = (queued.clone(), mainloop.clone());
            move |msg| {
                handle_msg(
                    msg,
                    1,
                    |b| queued.borrow_mut().push(b as usize),
                    || mainloop.quit(),
                )
            }
        });
        let guard = |ptr: usize, generation: u32| {
            let tx = tx.clone();
            let msg = Msg::Release(BufferPtr(ptr as *mut pw_buffer), generation);
            ReleaseGuard::new(move || drop(tx.send(msg)))
        };
        let (stale, current) = (guard(0x10, 0), guard(0x20, 1));
        let stop = tx.clone();
        std::thread::spawn(move || {
            drop(stale);
            drop(current);
            let _ = stop.send(Msg::Stop);
        })
        .join()
        .unwrap();
        mainloop.run();
        assert_eq!(*queued.borrow(), vec![0x20]);
    }

    #[test]
    fn run_until_gives_up_after_timeout() {
        pw::init();
        let mainloop = MainLoopRc::new(None).unwrap();
        let start = std::time::Instant::now();
        assert!(!run_until(&mainloop, || false, Duration::from_millis(50)).unwrap());
        assert!(start.elapsed() >= Duration::from_millis(50));
        assert!(run_until(&mainloop, || true, Duration::from_secs(60)).unwrap());
    }
}
