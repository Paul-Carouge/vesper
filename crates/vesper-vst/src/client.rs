//! Vesper's side: one host process per plugin instance, kept alive by a
//! supervisor thread, and the [`Vst3Filter`] that runs in a track's chain.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use tracing::{info, warn};
use vesper_dsp::Filter;

use crate::host::{SHM_FD, SOCKET_FD};
use crate::protocol::{self, Load, Msg, Param};
use crate::scan::{self, PluginInfo};
use crate::shm::{Shm, futex_wait, futex_wake, load};
use crate::sidecar::{self, Sidecar};
use crate::{CHANNELS, MAX_FRAMES, SAMPLE_RATE};

/// Name of the host binary.
pub const HOST_BIN: &str = "vesper-vst-host";

/// Longest the plugins of a chain may take on one block, together: when
/// it is spent, the remaining plugins' blocks go out dry.
pub const DEADLINE: Duration = Duration::from_millis(8);
/// Most parameters taken from a plugin, and longest names (chars).
pub const MAX_PARAMS: usize = 4096;
pub const MAX_NAME: usize = 256;
/// Blocks in a row the host may leave unanswered before it is restarted.
const STUCK_BLOCKS: u32 = 3;
/// Longest latency followed, frames (1 s); longer goes out dry.
pub const MAX_LATENCY: usize = SAMPLE_RATE as usize;
/// The supervisor's check period (host exit, stuck host).
const TICK: Duration = Duration::from_millis(50);
const LOAD_TIMEOUT: Duration = Duration::from_secs(10);
/// Plugins with big states (samples, impulse responses) can be slow.
const STATE_TIMEOUT: Duration = Duration::from_secs(10);
/// Restart delays: doubling from the first, capped at the last.
const BACKOFF_FIRST: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A host that ran this long resets the restart delay.
const HEALTHY: Duration = Duration::from_secs(30);

/// How to start a host.
#[derive(Debug, Clone)]
pub struct HostCommand {
    pub program: PathBuf,
    /// Added to the host's environment.
    pub env: Vec<(OsString, OsString)>,
}

impl HostCommand {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            env: Vec::new(),
        }
    }

    /// `vesper-vst-host` next to the running binary (a build tree), else in
    /// `../lib/vesper/` (a package). Never from `PATH`: when it is in
    /// neither, plugins fail with "not installed".
    pub fn installed() -> Self {
        let dir = std::env::current_exe()
            .ok()
            .and_then(|exe| Some(exe.parent()?.to_owned()))
            .unwrap_or_default();
        let next = dir.join(HOST_BIN);
        let lib = dir.join("../lib/vesper").join(HOST_BIN);
        Self::new(if !next.is_file() && lib.is_file() {
            lib
        } else {
            next
        })
    }

    /// Whether the host binary exists.
    pub fn exists(&self) -> bool {
        self.program.is_file()
    }

    /// The host command, started so it never outlives Vesper (killed when
    /// the spawning thread exits) and is the first process the kernel kills
    /// when out of memory.
    pub(crate) fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null());
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                // Vesper died before the prctl: nobody would kill us.
                if libc::getppid() != parent {
                    libc::_exit(1);
                }
                let fd = libc::open(c"/proc/self/oom_score_adj".as_ptr(), libc::O_WRONLY);
                if fd >= 0 {
                    libc::write(fd, b"1000".as_ptr().cast(), 4);
                    libc::close(fd);
                }
                Ok(())
            });
        }
        cmd
    }

    /// Starts a host serving `socket` and `shm` (as fds 3 and 4). It is
    /// killed if the calling thread exits first.
    fn serve(&self, socket: &UnixStream, shm: &Shm) -> io::Result<Child> {
        // Above 4, so the dup2s below cannot clobber each other.
        let (s, m) = (dup_high(socket.as_fd())?, dup_high(shm.as_fd())?);
        let (s, m) = (s.as_raw_fd(), m.as_raw_fd());
        let mut cmd = self.command();
        cmd.arg("--serve").stdout(Stdio::null());
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(s, SOCKET_FD) < 0 || libc::dup2(m, SHM_FD) < 0 {
                    return Err(io::Error::last_os_error());
                }
                close_others(SHM_FD as u32 + 1);
                Ok(())
            });
        }
        cmd.spawn()
    }
}

/// Marks every fd from `first` close-on-exec: the host inherits nothing
/// else of Vesper. Async-signal-safe. (Close-on-exec, not closed: std's
/// exec-error pipe must stay open until the exec.)
pub(crate) fn close_others(first: u32) {
    // SAFETY: a plain syscall (CLOSE_RANGE_CLOEXEC, Linux 5.11).
    unsafe {
        libc::syscall(
            libc::SYS_close_range,
            first,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
}

fn dup_high(fd: BorrowedFd) -> io::Result<OwnedFd> {
    // SAFETY: duplicates a valid fd; the new one is ours.
    let new = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    if new < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just created.
    Ok(unsafe { OwnedFd::from_raw_fd(new) })
}

/// Where a plugin instance is at.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// Its host is starting: the audio goes through dry.
    Starting,
    Running,
    /// Its host crashed or stalled: dry until it is back.
    Restarting(String),
    /// It cannot load (bad file, unsupported layout): dry for good.
    Failed(String),
}

/// What the UI shows of an instance.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub name: String,
    pub status: Status,
    pub params: Vec<Param>,
}

/// State shared by an instance's supervisor, its handles and its filters.
struct Inner {
    shm: Shm,
    /// The host is loaded and answering blocks.
    ready: AtomicBool,
    /// Blocks in a row the host left unanswered.
    late: AtomicU32,
    /// Taken by a filter during a block (two chains never run at once, but
    /// a duplicated id must not interleave blocks).
    in_use: AtomicBool,
    /// Set by [`Registry::remove`]: its state is no longer saved.
    removed: AtomicBool,
    pid: AtomicU32,
    view: Mutex<View>,
    /// Bumped when the view changes other than by `set_param`.
    generation: AtomicU64,
}

impl Inner {
    fn view(&self) -> MutexGuard<'_, View> {
        self.view.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_status(&self, status: Status) {
        self.view().status = status;
        self.generation.fetch_add(1, Ordering::Release);
    }
}

enum Cmd {
    SetParam(u32, f64),
    Persist,
}

/// A running plugin instance. Clones share it; its host stops once the
/// last clone (filters included) is gone.
#[derive(Clone)]
pub struct Instance {
    inner: Arc<Inner>,
    cmd: Sender<Cmd>,
}

impl Instance {
    /// Starts instance `id` of `dir`: its host comes up in the background.
    fn start(dir: &Path, id: u32, host: &HostCommand) -> io::Result<Self> {
        let sidecar = Sidecar::load(dir, id);
        let name = sidecar
            .as_ref()
            .map_or_else(|_| "VST3 plugin".to_owned(), |s| s.name.clone());
        let inner = Arc::new(Inner {
            shm: Shm::create(CHANNELS, MAX_FRAMES)?,
            ready: AtomicBool::new(false),
            late: AtomicU32::new(0),
            in_use: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            pid: AtomicU32::new(0),
            view: Mutex::new(View {
                name,
                status: Status::Starting,
                params: Vec::new(),
            }),
            generation: AtomicU64::new(0),
        });
        let (tx, rx) = mpsc::channel();
        let supervisor = Supervisor {
            inner: inner.clone(),
            rx,
            host: host.clone(),
            dir: dir.to_owned(),
            id,
            pending: Vec::new(),
        };
        std::thread::Builder::new()
            .name(format!("vst3-{id}"))
            .spawn(move || match sidecar {
                Ok(sidecar) => supervisor.run(sidecar),
                Err(e) => {
                    supervisor
                        .inner
                        .set_status(Status::Failed(format!("plugin settings: {e}")));
                    // Wait for the handles to go.
                    while supervisor.rx.recv().is_ok() {}
                }
            })?;
        Ok(Self { inner, cmd: tx })
    }

    pub fn view(&self) -> View {
        self.inner.view().clone()
    }

    /// Changes when the status or the parameter list does.
    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::Acquire)
    }

    /// Sets parameter `index` of [`View::params`] (normalized, clamped).
    pub fn set_param(&self, index: usize, value: f64) {
        // `clamp` lets NaN through.
        if !value.is_finite() {
            return;
        }
        let value = value.clamp(0.0, 1.0);
        let id = {
            let mut view = self.inner.view();
            let Some(p) = view.params.get_mut(index) else {
                return;
            };
            p.value = value;
            p.id
        };
        let _ = self.cmd.send(Cmd::SetParam(id, value));
    }

    /// Saves the plugin's state to its settings file, in the background.
    pub fn persist(&self) {
        let _ = self.cmd.send(Cmd::Persist);
    }

    /// The host's pid while one runs.
    pub fn pid(&self) -> Option<u32> {
        Some(self.inner.pid.load(Ordering::Acquire)).filter(|&p| p != 0)
    }

    /// A filter running this instance (any number may exist; one runs at a
    /// time). Allocates its buffers here, not on the audio thread.
    pub fn filter(&self) -> Vst3Filter {
        Vst3Filter {
            instance: self.clone(),
            ring: vec![0.0; (MAX_LATENCY + 1) * CHANNELS],
            pos: 0,
            // The last latency seen, also across restarts: a chain rebuilt
            // while the host is down keeps the timeline where it was.
            latency: (load(&self.inner.shm.header().latency) as usize).min(MAX_LATENCY),
            dry: vec![0.0; MAX_FRAMES * CHANNELS],
        }
    }
}

/// Owns an instance's host: starts it, restarts it after a crash or a
/// stall (with a growing delay), and carries the control messages.
struct Supervisor {
    inner: Arc<Inner>,
    rx: Receiver<Cmd>,
    host: HostCommand,
    dir: PathBuf,
    id: u32,
    /// Parameters set since the last saved state: replayed after a restart.
    pending: Vec<(u32, f64)>,
}

/// A running host.
struct Host {
    child: Child,
    socket: UnixStream,
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

enum StartError {
    /// The plugin refused to load: no retry.
    Plugin(String),
    /// The host failed: retried.
    Host(String),
}

impl Supervisor {
    fn run(mut self, mut sidecar: Sidecar) {
        let mut delay = BACKOFF_FIRST;
        loop {
            let started = Instant::now();
            let reason = match self.start(&sidecar) {
                Ok(host) => {
                    let reason = self.serve(host, &mut sidecar);
                    self.inner.ready.store(false, Ordering::Release);
                    self.inner.pid.store(0, Ordering::Release);
                    match reason {
                        Some(reason) => reason,
                        None => return,
                    }
                }
                Err(StartError::Plugin(e)) => {
                    warn!("VST3 plugin {:?} cannot load: {e}", sidecar.name);
                    self.inner.set_status(Status::Failed(e));
                    // Keep the parameters' place until the handles go.
                    while self.rx.recv().is_ok() {}
                    return;
                }
                Err(StartError::Host(e)) => e,
            };
            if started.elapsed() >= HEALTHY {
                delay = BACKOFF_FIRST;
            }
            warn!(
                "VST3 plugin {:?} bypassed: {reason}; restarting in {delay:?}",
                sidecar.name
            );
            self.inner.set_status(Status::Restarting(reason));
            if !self.wait(delay) {
                return;
            }
            delay = (delay * 2).min(BACKOFF_MAX);
        }
    }

    /// Waits `delay`, keeping parameter changes. False once the handles
    /// are gone.
    fn wait(&mut self, delay: Duration) -> bool {
        let until = Instant::now() + delay;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match self.rx.recv_timeout(left) {
                Ok(Cmd::SetParam(id, value)) => self.remember(id, value),
                Ok(Cmd::Persist) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }
        true
    }

    /// Keeps the latest value of each parameter set since the last state.
    fn remember(&mut self, id: u32, value: f64) {
        match self.pending.iter_mut().find(|(i, _)| *i == id) {
            Some(p) => p.1 = value,
            None => self.pending.push((id, value)),
        }
    }

    fn start(&mut self, sidecar: &Sidecar) -> Result<Host, StartError> {
        if !self.host.exists() {
            return Err(StartError::Plugin(format!("{HOST_BIN} not installed")));
        }
        let host_err = |what: &str, e: io::Error| StartError::Host(format!("{what}: {e}"));
        let (ours, theirs) = UnixStream::pair().map_err(|e| host_err("socket", e))?;
        let child = self
            .host
            .serve(&theirs, &self.inner.shm)
            .map_err(|e| host_err(&format!("cannot start {:?}", self.host.program), e))?;
        drop(theirs);
        // A host stuck in the plugin must not block the supervisor on a full
        // socket: it would never see the host stall nor restart it.
        ours.set_write_timeout(Some(STATE_TIMEOUT))
            .map_err(|e| host_err("socket", e))?;
        let mut host = Host {
            child,
            socket: ours,
        };
        let load = Msg::Load(Load {
            path: sidecar.path.as_os_str().as_bytes().to_vec(),
            cid: sidecar.cid,
            channels: CHANNELS as u32,
            max_frames: MAX_FRAMES as u32,
            sample_rate: SAMPLE_RATE,
            component: sidecar.component.clone(),
            controller: sidecar.controller.clone(),
        });
        let reply = request(&mut host, &load, LOAD_TIMEOUT).map_err(|e| {
            if e.kind() == io::ErrorKind::InvalidInput {
                // Would fail the same way every time.
                return StartError::Plugin("saved state too big".into());
            }
            StartError::Host(match host.child.try_wait() {
                Ok(Some(status)) => format!("host died while loading ({status})"),
                _ => format!("loading: {e}"),
            })
        })?;
        let (name, params) = match reply {
            Msg::Loaded { name, params, .. } => sanitize(name, params),
            Msg::Error(e) => return Err(StartError::Plugin(e)),
            other => return Err(StartError::Host(format!("unexpected answer {other:?}"))),
        };
        for &(id, value) in &self.pending {
            protocol::write(&mut &host.socket, &Msg::SetParam { id, value })
                .map_err(|e| host_err("host", e))?;
        }
        {
            let mut view = self.inner.view();
            view.name = name;
            view.params = params;
            for &(id, value) in &self.pending {
                if let Some(p) = view.params.iter_mut().find(|p| p.id == id) {
                    p.value = value;
                }
            }
        }
        self.inner.late.store(0, Ordering::Relaxed);
        self.inner.pid.store(host.child.id(), Ordering::Release);
        self.inner.ready.store(true, Ordering::Release);
        self.inner.set_status(Status::Running);
        info!("VST3 plugin {:?} running", sidecar.name);
        Ok(host)
    }

    /// Serves a running host until it fails (Some(why)) or the handles are
    /// gone (None).
    fn serve(&mut self, mut host: Host, sidecar: &mut Sidecar) -> Option<String> {
        loop {
            match self.rx.recv_timeout(TICK) {
                Ok(Cmd::SetParam(id, value)) => {
                    self.remember(id, value);
                    if let Err(e) = protocol::write(&mut &host.socket, &Msg::SetParam { id, value })
                    {
                        return Some(format!("host gone: {e}"));
                    }
                }
                Ok(Cmd::Persist) => match request(&mut host, &Msg::GetState, STATE_TIMEOUT) {
                    Ok(Msg::State {
                        component,
                        controller,
                    }) => {
                        (sidecar.component, sidecar.controller) = (component, controller);
                        self.pending.clear();
                        // A removed instance stays removed (its id may be reused).
                        let removed = self.inner.removed.load(Ordering::Acquire);
                        if !removed && let Err(e) = sidecar.save(&self.dir, self.id) {
                            warn!(
                                "cannot save the state of VST3 plugin {:?}: {e}",
                                sidecar.name
                            );
                        }
                    }
                    Ok(other) => warn!("VST3 plugin {:?} gave no state: {other:?}", sidecar.name),
                    Err(e) => return Some(format!("host not answering: {e}")),
                },
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return None,
            }
            if let Ok(Some(status)) = host.child.try_wait() {
                return Some(format!("host exited ({status})"));
            }
            if self.inner.late.load(Ordering::Relaxed) >= STUCK_BLOCKS {
                return Some("host too slow".into());
            }
        }
    }
}

/// What a host says of its plugin, bounded: it is not trusted.
fn sanitize(name: String, mut params: Vec<Param>) -> (String, Vec<Param>) {
    let cut = |s: String| s.chars().take(MAX_NAME).collect::<String>();
    let unit = |v: f64| {
        if v.is_finite() {
            v.clamp(0.0, 1.0)
        } else {
            0.0
        }
    };
    params.truncate(MAX_PARAMS);
    let params = params
        .into_iter()
        .map(|p| Param {
            title: cut(p.title),
            units: cut(p.units),
            default: unit(p.default),
            value: unit(p.value),
            ..p
        })
        .collect();
    (cut(name), params)
}

/// Sends `msg` and reads the answer, all within `timeout`.
fn request(host: &mut Host, msg: &Msg, timeout: Duration) -> io::Result<Msg> {
    protocol::write(&mut &host.socket, msg)?;
    protocol::read(&mut Deadline {
        socket: &host.socket,
        until: Instant::now() + timeout,
    })
}

/// Reads with one deadline for all the reads of a message.
struct Deadline<'a> {
    socket: &'a UnixStream,
    until: Instant,
}

impl io::Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self
            .until
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(io::ErrorKind::TimedOut)?;
        self.socket.set_read_timeout(Some(left))?;
        (&mut &*self.socket).read(buf)
    }
}

/// A plugin instance in a track's chain. Never waits for its host longer
/// than [`DEADLINE`] per block: when the host is late, starting, or gone,
/// the block goes out dry, delayed by the plugin's last latency so the
/// timeline holds.
pub struct Vst3Filter {
    instance: Instance,
    /// The dry signal's delay line.
    ring: Vec<f32>,
    /// Next frame written in `ring`.
    pos: usize,
    latency: usize,
    /// The block's dry signal, delayed.
    dry: Vec<f32>,
}

impl Vst3Filter {
    fn block(&mut self, buf: &mut [f32], deadline: Instant) {
        let n = buf.len();
        let frames = self.ring.len() / CHANNELS;
        for (src, dst) in buf.chunks(CHANNELS).zip(self.dry.chunks_mut(CHANNELS)) {
            let w = self.pos * CHANNELS;
            self.ring[w..w + CHANNELS].copy_from_slice(src);
            let r = (self.pos + frames - self.latency) % frames * CHANNELS;
            dst.copy_from_slice(&self.ring[r..r + CHANNELS]);
            self.pos = (self.pos + 1) % frames;
        }
        let inner = &*self.instance.inner;
        let wet =
            inner.ready.load(Ordering::Acquire) && !inner.in_use.swap(true, Ordering::Acquire) && {
                let wet = exchange(inner, buf, deadline);
                inner.in_use.store(false, Ordering::Release);
                wet
            };
        let latency = load(&inner.shm.header().latency) as usize;
        if wet && latency <= MAX_LATENCY {
            self.latency = latency;
        } else {
            buf.copy_from_slice(&self.dry[..n]);
        }
    }
}

/// Runs one block through the host by `deadline`: true with the answer
/// in `buf`.
fn exchange(inner: &Inner, buf: &mut [f32], deadline: Instant) -> bool {
    let shm = &inner.shm;
    let h = shm.header();
    let req = h.req.load(Ordering::Relaxed);
    if load(&h.done) != req {
        // Still on an earlier block.
        inner.late.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    if Instant::now() >= deadline {
        // Plugins earlier in the chain spent the block's time: not this
        // host's fault.
        return false;
    }
    shm.write_input(buf);
    h.frames
        .store((buf.len() / CHANNELS) as u32, Ordering::Relaxed);
    let req = req.wrapping_add(1);
    h.req.store(req, Ordering::Release);
    futex_wake(&h.req);
    loop {
        let done = load(&h.done);
        if done == req {
            shm.read_output(buf);
            inner.late.store(0, Ordering::Relaxed);
            if h.failed.load(Ordering::Relaxed) != 0 {
                return false;
            }
            // A diverging plugin's NaN would stick in every filter state
            // downstream, the mix's included: that block goes out dry.
            return buf.iter().all(|s| s.is_finite());
        }
        let now = Instant::now();
        if now >= deadline {
            inner.late.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        futex_wait(&h.done, done, deadline - now);
    }
}

impl Filter for Vst3Filter {
    fn process(&mut self, buf: &mut [f32]) {
        // One budget for every plugin of the chain's block.
        let deadline = vesper_dsp::block_start().unwrap_or_else(Instant::now) + DEADLINE;
        for block in buf.chunks_mut(MAX_FRAMES * CHANNELS) {
            self.block(block, deadline);
        }
    }

    fn reset(&mut self) {
        self.ring.fill(0.0);
    }

    fn latency(&self) -> usize {
        self.latency
    }

    fn set_param(&mut self, _: usize, _: f32) {}
}

/// The plugin instances of a settings directory.
pub struct Registry {
    dir: PathBuf,
    host: HostCommand,
    open: Mutex<HashMap<u32, Instance>>,
}

impl Registry {
    /// Instances keep their settings in `dir` (see [`crate::sidecar`]).
    pub fn new(dir: impl Into<PathBuf>, host: HostCommand) -> Self {
        let dir = dir.into();
        sidecar::clean_temporary(&dir);
        Self {
            dir,
            host,
            open: Mutex::default(),
        }
    }

    fn open(&self) -> MutexGuard<'_, HashMap<u32, Instance>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Instance `id`, started if it was not running.
    pub fn instance(&self, id: u32) -> io::Result<Instance> {
        let mut open = self.open();
        if let Some(i) = open.get(&id) {
            return Ok(i.clone());
        }
        let i = Instance::start(&self.dir, id, &self.host)?;
        open.insert(id, i.clone());
        Ok(i)
    }

    /// A new instance of `plugin`: writes its settings, returns its id.
    pub fn add(&self, plugin: &PluginInfo) -> io::Result<u32> {
        let _open = self.open();
        let id = sidecar::next_id(&self.dir)?;
        Sidecar {
            path: plugin.path.clone(),
            cid: plugin.cid,
            name: plugin.name.clone(),
            ..Sidecar::default()
        }
        .save(&self.dir, id)?;
        Ok(id)
    }

    /// Forgets instance `id`: its host stops once no chain runs it, its
    /// settings file is deleted.
    pub fn remove(&self, id: u32) {
        if let Some(i) = self.open().remove(&id) {
            i.inner.removed.store(true, Ordering::Release);
        }
        let _ = std::fs::remove_file(sidecar::file(&self.dir, id));
    }

    /// Saves every running instance's state (see [`Instance::persist`]).
    pub fn persist_all(&self) {
        self.open().values().for_each(Instance::persist);
    }

    /// The installed effect plugins, from the cache when unchanged. Slow the
    /// first time (one host per bundle): call it off the UI thread.
    pub fn scan(&self) -> Vec<PluginInfo> {
        scan::scan(
            &self.host,
            &scan::default_dirs(),
            &self.dir.join("scan-cache"),
        )
    }
}

static GLOBAL: OnceLock<Registry> = OnceLock::new();

/// Makes `registry` the one [`vesper_dsp::FilterSpec::Vst3`] filters are
/// built from. None if one was installed already.
pub fn install(registry: Registry) -> Option<&'static Registry> {
    GLOBAL.set(registry).ok()?;
    vesper_dsp::set_external(build);
    GLOBAL.get()
}

/// The installed registry.
pub fn global() -> Option<&'static Registry> {
    GLOBAL.get()
}

fn build(id: u32, channels: usize) -> Option<Box<dyn Filter + Send>> {
    if channels != CHANNELS {
        return None;
    }
    match GLOBAL.get()?.instance(id) {
        Ok(i) => Some(Box::new(i.filter())),
        Err(e) => {
            warn!("VST3 plugin {id} cannot start: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn host_answers_are_bounded() {
        let p = Param {
            id: 1,
            title: "t".repeat(10_000),
            units: "u".repeat(300),
            steps: 0,
            default: f64::NAN,
            value: f64::INFINITY,
        };
        let many = vec![p; MAX_PARAMS + 10];
        let (name, params) = sanitize("n".repeat(1_000), many);
        assert_eq!(name.chars().count(), MAX_NAME);
        assert_eq!(params.len(), MAX_PARAMS);
        assert_eq!(params[0].title.chars().count(), MAX_NAME);
        assert_eq!(params[0].units.chars().count(), MAX_NAME);
        assert_eq!((params[0].default, params[0].value), (0.0, 0.0));
        let odd = Param {
            value: 2.0,
            default: -1.0,
            ..params[0].clone()
        };
        let (_, odd) = sanitize(String::new(), vec![odd]);
        assert_eq!((odd[0].default, odd[0].value), (0.0, 1.0));
    }

    #[test]
    fn requests_have_one_deadline() {
        let (a, b) = UnixStream::pair().unwrap();
        // A peer that trickles a length, then nothing.
        let t = std::thread::spawn(move || {
            for byte in [8u8, 0, 0] {
                (&b).write_all(&[byte]).unwrap();
                std::thread::sleep(Duration::from_millis(60));
            }
            std::thread::sleep(Duration::from_millis(300));
        });
        let start = Instant::now();
        let mut r = Deadline {
            socket: &a,
            until: start + Duration::from_millis(100),
        };
        let e = protocol::read(&mut r).unwrap_err();
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "{:?}",
            start.elapsed()
        );
        assert!(matches!(
            e.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
        t.join().unwrap();
    }
}
