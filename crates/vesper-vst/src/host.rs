//! The host process's side: serves the control socket (fd 3) and the audio
//! region (fd 4) for one plugin. `vesper-vst-host` plugs VST3 in through
//! [`Plugin`]; tests plug fakes in.

use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use crate::protocol::{self, Load, Msg, Param};
use crate::shm::{Shm, futex_wait, futex_wake, load};

/// File descriptors a host is started with.
pub const SOCKET_FD: i32 = 3;
pub const SHM_FD: i32 = 4;

/// Most parameter changes handed to one block; more wait for the next.
const MAX_CHANGES: usize = 256;
/// Longest a state request waits for parameter changes to reach the processor.
const FLUSH_TIMEOUT: Duration = Duration::from_millis(500);

/// What [`Plugin::load`] gives: the plugin's name, its parameters and its
/// audio side.
pub type Loaded = (String, Vec<Param>, Box<dyn Processor>);

/// A plugin as the host's control thread sees it.
pub trait Plugin {
    /// Loads the plugin for `load`, restoring its states. Returns its name,
    /// parameters and audio side. An error is final (another try would fail
    /// the same way): bad file, unsupported bus layout...
    fn load(&mut self, load: &Load) -> Result<Loaded, String>;
    /// Sets a parameter on the controller side (the processor gets it with
    /// its next block too).
    fn set_param(&mut self, id: u32, value: f64);
    /// Component and controller states.
    fn state(&mut self) -> Result<(Vec<u8>, Vec<u8>), String>;
}

/// A plugin's audio side, run on the host's audio thread.
pub trait Processor: Send {
    /// Processes one block of interleaved samples (the channel count given
    /// to [`Plugin::load`]), with the parameter changes since the last one.
    /// An empty block only hands over parameter changes.
    fn process(&mut self, input: &[f32], output: &mut [f32], changes: &[(u32, f64)]);
    /// Current latency, frames.
    fn latency(&self) -> u32;
}

/// Serves Vesper on fds 3 and 4 until it closes the socket.
pub fn serve(plugin: impl Plugin) -> io::Result<()> {
    // SAFETY: the fds Vesper starts every host with (see `client::spawn`).
    let (socket, shm) = unsafe {
        (
            UnixStream::from_raw_fd(SOCKET_FD),
            OwnedFd::from_raw_fd(SHM_FD),
        )
    };
    // Not inherited by what the plugin may start (a license helper...).
    for fd in [SOCKET_FD, SHM_FD] {
        // SAFETY: sets a flag on an fd this process owns.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    serve_on(plugin, socket, shm)
}

pub fn serve_on(mut plugin: impl Plugin, socket: UnixStream, shm: OwnedFd) -> io::Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let (changes_tx, changes_rx) = mpsc::channel();
    let mut changes_rx = Some(changes_rx);
    let mut shm = Some(shm);
    let mut audio = None;
    // Shared with the audio thread once loaded.
    let mut region: Option<Arc<Shm>> = None;
    // Parameter changes sent to the audio thread, and applied by it.
    let mut sent = 0;
    let applied = Arc::new(AtomicU64::new(0));
    let result = loop {
        let msg = match protocol::read(&mut &socket) {
            Ok(msg) => msg,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break Ok(()),
            Err(e) => break Err(e),
        };
        let reply = match msg {
            Msg::Load(l) if audio.is_none() => match plugin.load(&l) {
                Ok((name, params, processor)) => {
                    let shared = Arc::new(Shm::map(
                        shm.take().expect("loaded once"),
                        l.channels as usize,
                        l.max_frames as usize,
                    )?);
                    let latency = processor.latency();
                    shared.header().latency.store(latency, Ordering::Release);
                    let audio_side = Audio {
                        shm: shared.clone(),
                        channels: l.channels as usize,
                        processor,
                        changes_rx: changes_rx.take().expect("loaded once"),
                        applied: applied.clone(),
                        stop: stop.clone(),
                    };
                    region = Some(shared);
                    audio = Some(
                        std::thread::Builder::new()
                            .name("vst-audio".into())
                            .spawn(move || audio_side.run())?,
                    );
                    Some(Msg::Loaded {
                        name,
                        latency,
                        params,
                    })
                }
                Err(e) => Some(Msg::Error(e)),
            },
            Msg::SetParam { id, value } => {
                plugin.set_param(id, value);
                if let Some(region) = &region
                    && changes_tx.send((id, value)).is_ok()
                {
                    sent += 1;
                    // An idle audio thread hands it to the processor now.
                    futex_wake(&region.header().req);
                }
                None
            }
            Msg::GetState => {
                // The processor's state only holds the parameters it got.
                let until = Instant::now() + FLUSH_TIMEOUT;
                while applied.load(Ordering::Acquire) < sent && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Some(match plugin.state() {
                    Ok((component, controller)) => Msg::State {
                        component,
                        controller,
                    },
                    Err(e) => Msg::Error(e),
                })
            }
            other => Some(Msg::Error(format!("unexpected message {other:?}"))),
        };
        if let Some(reply) = reply {
            protocol::write(&mut &socket, &reply)?;
        }
    };
    stop.store(true, Ordering::Relaxed);
    if let Some(audio) = audio {
        let _ = audio.join();
    }
    result
}

/// The host's audio thread.
struct Audio {
    shm: Arc<Shm>,
    channels: usize,
    processor: Box<dyn Processor>,
    changes_rx: Receiver<(u32, f64)>,
    applied: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
}

impl Audio {
    /// Parameter changes waiting, at most [`MAX_CHANGES`].
    fn changes(&self, out: &mut Vec<(u32, f64)>) {
        out.clear();
        while out.len() < MAX_CHANGES {
            match self.changes_rx.try_recv() {
                Ok(c) => out.push(c),
                Err(_) => break,
            }
        }
    }

    /// Answers blocks until `stop`. While no block comes, parameter changes
    /// still reach the processor (a VST3 "flush": a block of 0 frames), so
    /// its state follows the sliders.
    fn run(mut self) {
        let shm = self.shm.clone();
        let h = shm.header();
        let mut input = vec![0.0; shm.samples()];
        let mut output = vec![0.0; shm.samples()];
        let mut changes = Vec::with_capacity(MAX_CHANGES);
        loop {
            let (req, done) = (load(&h.req), load(&h.done));
            if req == done {
                if self.stop.load(Ordering::Relaxed) {
                    return;
                }
                self.changes(&mut changes);
                if changes.is_empty() {
                    futex_wait(&h.req, done, Duration::from_millis(100));
                } else {
                    self.processor.process(&[], &mut [], &changes);
                    self.applied
                        .fetch_add(changes.len() as u64, Ordering::Release);
                }
                continue;
            }
            // A request left by a host that died is answered too: Vesper
            // waits for its answer before sending the next.
            let samples = (load(&h.frames) as usize)
                .saturating_mul(self.channels)
                .min(shm.samples());
            shm.read_input(&mut input[..samples]);
            self.changes(&mut changes);
            self.processor
                .process(&input[..samples], &mut output[..samples], &changes);
            shm.write_output(&output[..samples]);
            h.latency.store(self.processor.latency(), Ordering::Relaxed);
            h.done.store(req, Ordering::Release);
            futex_wake(&h.done);
            self.applied
                .fetch_add(changes.len() as u64, Ordering::Release);
        }
    }
}
