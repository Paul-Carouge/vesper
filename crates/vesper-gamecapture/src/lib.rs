//! Game capture through Vesper's own Vulkan layer (`vesper-vklayer`).
//!
//! [`GameCapture::start`] listens on `/tmp/vesper-<uid>/capture.sock`
//! ([`protocol::socket_path`]), reachable from Steam Runtime containers.
//! Every Vulkan process with the layer installed connects while the server
//! runs and reports its swapchain size. The server captures one game at a
//! time and only with consent: Steam games (`SteamAppId` in their
//! environment) are allowed, others wait for [`GameCapture::allow`]. Among
//! allowed games it keeps the captured one while it sends frames (a game
//! silent for 3 s gives way to a newer one), picks the newest otherwise, or
//! follows [`GameCapture::select`]. Other games stay dormant (pure
//! pass-through).
//!
//! Peers are identified by the kernel (`SO_PEERCRED`), never by what they
//! claim: same user only, process name and environment read from `/proc`.
//!
//! The selected game copies presented images, paced to the recording rate
//! ([`protocol::Pacer`]), into a small ring of exported DMA-BUFs. Frames arrive as [`VideoFrame`]s once their GPU copy
//! is done; dropping a frame tells the game the buffer is free again. See
//! [`protocol`] for the wire format.

pub mod protocol;

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Seek, SeekFrom};
use std::mem::ManuallyDrop;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use protocol::{Msg, Plane};
use tracing::{debug, info, warn};
use vesper_core::{DmaBufPlane, ReleaseGuard, VideoFrame};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

/// DRM modifiers the server asks for. Empty lets the layer pick any
/// single-allocation layout the GPU can export; same-GPU VAAPI imports them.
const MODIFIERS: &[u64] = &[];

/// Bounds on what untrusted clients may make the server hold.
const MAX_CLIENTS: usize = 32;
const MAX_PENDING: usize = 16;
const MAX_SIZE: u32 = 16384;
/// A client must say `Hello` within this time after connecting.
const HELLO_TIMEOUT: Duration = Duration::from_secs(1);
/// The captured game loses its priority after this long without a frame
/// (frozen, stopped, or done presenting).
const STALE_AFTER: Duration = Duration::from_secs(3);
/// The captured game's process is checked for liveness only after this long
/// without a frame (its socket may live on in a child).
const LIVENESS_AFTER: Duration = Duration::from_millis(500);
/// Messages read from one client per poll round, so none can starve others.
const MAX_MESSAGES_PER_ROUND: usize = 64;

/// Whether a game may be captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    /// A Steam game, or allowed with [`GameCapture::allow`].
    Allowed,
    /// Needs [`GameCapture::allow`] first (the app should ask the user).
    Pending,
}

/// A game connected through the layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Game {
    /// From the kernel (`SO_PEERCRED`).
    pub pid: u32,
    /// Process name (`/proc/<pid>/comm`, at most 15 bytes; the `.exe` name
    /// under Proton).
    pub exe: String,
    /// Size of its capturable swapchain, 0x0 when it has none.
    pub width: u32,
    pub height: u32,
    pub consent: Consent,
    /// Whether this game is the one being captured.
    pub captured: bool,
    /// Order of its first swapchain: the newest game (highest) is captured
    /// by default.
    pub seq: u64,
}

struct Shared {
    games: Mutex<Vec<Game>>,
    select: Mutex<Option<u32>>,
    allowed: Mutex<Vec<String>>,
    /// Recording rate frames are wanted at, 0 for none
    /// ([`GameCapture::set_active`]).
    fps: AtomicU32,
    stop: AtomicBool,
}

/// A running game capture server. Dropping it stops the capture.
pub struct GameCapture {
    shared: Arc<Shared>,
    wake: OwnedFd,
    thread: Option<JoinHandle<()>>,
    path: PathBuf,
}

impl GameCapture {
    /// Starts listening for games. `on_frame` runs on the server thread:
    /// keep it short, e.g. send the frame to a channel. It gets one final
    /// `Err` if the server fails; no frame follows. A game closing is not an
    /// error: the server waits for the next one. It starts inactive: no
    /// game copies frames until [`GameCapture::set_active`].
    pub fn start(on_frame: impl FnMut(Result<VideoFrame>) + Send + 'static) -> Result<Self> {
        Self::start_at(protocol::socket_path(), on_frame)
    }

    /// [`GameCapture::start`] on another socket path (tests).
    fn start_at(
        path: PathBuf,
        on_frame: impl FnMut(Result<VideoFrame>) + Send + 'static,
    ) -> Result<Self> {
        protocol::create_private_dir(path.parent().expect("socket path has a parent"))?;
        if path.exists() {
            match protocol::connect(&path) {
                // A full backlog is a live server too.
                Err(e) if e.kind() != io::ErrorKind::WouldBlock => std::fs::remove_file(&path)?,
                _ => return Err("another Vesper instance is already capturing games".into()),
            }
        }
        let listener = protocol::listen(&path)?;
        let mut pipe = [0; 2];
        if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
            let _ = std::fs::remove_file(&path);
            return Err(io::Error::last_os_error().into());
        }
        let (wake_r, wake) =
            unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
        let shared = Arc::new(Shared {
            games: Mutex::new(Vec::new()),
            select: Mutex::new(None),
            allowed: Mutex::new(Vec::new()),
            fps: AtomicU32::new(0),
            stop: AtomicBool::new(false),
        });
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name("vesper-gamecapture".into())
            .spawn(move || Server::new(s, listener, wake_r, Box::new(on_frame)).run());
        let thread = match thread {
            Ok(t) => t,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err(e.into());
            }
        };
        info!("game capture listening on {}", path.display());
        Ok(Self {
            shared,
            wake,
            thread: Some(thread),
            path,
        })
    }

    /// Games currently connected, with their consent state.
    pub fn games(&self) -> Vec<Game> {
        self.shared.games.lock().unwrap().clone()
    }

    /// The game being captured, if any.
    pub fn captured(&self) -> Option<Game> {
        self.games().into_iter().find(|g| g.captured)
    }

    /// Captures the game with this pid (if allowed), or picks automatically
    /// with `None`.
    pub fn select(&self, pid: Option<u32>) {
        *self.shared.select.lock().unwrap() = pid;
        self.wake();
    }

    /// Allows capturing games with this process name ([`Game::exe`], the
    /// kernel's 15-byte `comm`). The app persists its allow list and calls
    /// this at startup. Never allow generic names (`java`, `python3`,
    /// `game.exe`, `launcher.exe`): any process could carry them.
    // shortcut: keyed on the 15-byte comm; key on the executable path (/proc/<pid>/exe) once the app has an allow-list UI.
    pub fn allow(&self, exe: &str) {
        let mut allowed = self.shared.allowed.lock().unwrap();
        if !allowed.iter().any(|e| e == exe) {
            allowed.push(exe.to_owned());
        }
        drop(allowed);
        self.wake();
    }

    /// `Some(fps)`: the selected game copies frames, paced to a recording
    /// at `fps`. `None` (the default): no game is started, games stay a
    /// pass-through and [`Game::captured`] is false, but games and consent
    /// are still listed.
    pub fn set_active(&self, fps: Option<u32>) {
        let fps = fps.map_or(0, |f| f.max(1));
        self.shared.fps.store(fps, Ordering::SeqCst);
        self.wake();
    }

    /// Stops the capture. Same as dropping it.
    pub fn stop(self) {}

    fn wake(&self) {
        let _ = unsafe { libc::write(self.wake.as_raw_fd(), [0u8].as_ptr().cast(), 1) };
    }
}

impl Drop for GameCapture {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.wake();
        // Dropped from `on_frame`: the loop exits once the callback returns.
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != std::thread::current().id()
            && thread.join().is_err()
        {
            warn!("game capture thread panicked");
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

struct Buffer {
    fd: OwnedFd,
    width: u32,
    height: u32,
    fourcc: u32,
    modifier: u64,
    planes: Vec<Plane>,
}

struct Ring {
    generation: u32,
    buffers: HashMap<u32, Buffer>,
}

struct Client {
    sock: Arc<OwnedFd>,
    pid: u32,
    exe: String,
    /// Steam/Proton game: allowed without asking.
    steam: bool,
    hello: bool,
    since: Instant,
    /// Last frame, or the `Start` that asked for frames.
    last_frame: Instant,
    width: u32,
    height: u32,
    /// Order of its first non-empty `Swapchain` message, for auto-selection.
    announced: u64,
    /// Ring announced by the layer while started.
    ring: Option<Ring>,
}

/// A frame waiting for its sync_file to signal.
struct Pending {
    sock: Arc<OwnedFd>,
    generation: u32,
    index: u32,
    pts_ns: u64,
    sync: OwnedFd,
}

type OnFrame = Box<dyn FnMut(Result<VideoFrame>) + Send>;

struct Server {
    shared: Arc<Shared>,
    listener: OwnedFd,
    wake: OwnedFd,
    on_frame: OnFrame,
    clients: Vec<Client>,
    pending: Vec<Pending>,
    /// Socket of the client told to `Start`, and the rate it was given.
    started: Option<Arc<OwnedFd>>,
    started_fps: u32,
    /// Reused every poll round.
    candidates: Vec<Candidate>,
    announce_seq: u64,
    pool_generation: u32,
    /// `protocol::is_sync_file` / `protocol::is_dmabuf`; replaced in tests
    /// (no GPU to make them).
    is_sync_file: fn(BorrowedFd) -> bool,
    is_dmabuf: fn(BorrowedFd) -> bool,
    /// `alive`; replaced in tests, whose clients have made-up pids.
    alive: fn(u32) -> bool,
    /// Uids already warned about.
    refused_uids: Vec<u32>,
    /// Pids of games with another protocol version, already warned about.
    refused_pids: Vec<u32>,
}

/// What selection needs to know about a client.
struct Candidate {
    pid: u32,
    allowed: bool,
    /// Said hello and has a swapchain.
    ready: bool,
    seq: u64,
    /// Captured but sent no frame for `STALE_AFTER`, or its process is gone.
    stale: bool,
}

/// Picks the client to capture. Only allowed clients qualify. The current
/// one is kept while it sends frames (games recreate their swapchain on
/// resize or focus changes); a stale one is kept only if it is still the
/// newest ready game (long loading screens must not flip). Otherwise the
/// selected pid, or the newest ready client.
fn choose(clients: &[Candidate], select: Option<u32>, current: Option<usize>) -> Option<usize> {
    let ok = |i: usize| {
        let c = &clients[i];
        c.allowed && (c.ready || (Some(i) == current && !c.stale))
    };
    match select {
        Some(pid) => (0..clients.len()).find(|&i| clients[i].pid == pid && ok(i)),
        None => current.filter(|&i| ok(i) && !clients[i].stale).or_else(|| {
            (0..clients.len())
                .filter(|&i| ok(i) && clients[i].ready)
                .max_by_key(|&i| clients[i].seq)
        }),
    }
}

/// Whether a process still exists.
fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    unsafe { libc::kill(pid, 0) == 0 || *libc::__errno_location() != libc::ESRCH }
}

/// Size of a dma-buf (or any seekable fd), without taking ownership.
fn fd_size(fd: BorrowedFd) -> io::Result<u64> {
    let mut f = ManuallyDrop::new(unsafe { File::from_raw_fd(fd.as_raw_fd()) });
    f.seek(SeekFrom::End(0))
}

/// Checks a `Buffer` against the client's announced swapchain and the
/// dma-buf's real size.
fn validate_buffer(
    c: &Client,
    index: u32,
    width: u32,
    height: u32,
    fourcc: u32,
    planes: &[Plane],
    fd: BorrowedFd,
) -> std::result::Result<(), String> {
    if index >= protocol::RING {
        return Err(format!("buffer index {index}"));
    }
    if !(1..=MAX_SIZE).contains(&width) || !(1..=MAX_SIZE).contains(&height) {
        return Err(format!("buffer size {width}x{height}"));
    }
    if (width, height) != (c.width, c.height) {
        return Err(format!(
            "buffer {width}x{height} does not match swapchain {}x{}",
            c.width, c.height
        ));
    }
    if fourcc != protocol::XR24 && fourcc != protocol::XB24 {
        return Err(format!("fourcc {fourcc:#x}"));
    }
    let size = fd_size(fd).map_err(|e| format!("dma-buf size: {e}"))?;
    let first = planes.first().ok_or("no planes")?;
    let rows_end = first.offset as u64 + first.stride as u64 * height as u64;
    if (first.stride as u64) < width as u64 * 4 || rows_end > size {
        return Err("plane 0 does not fit the dma-buf".into());
    }
    if planes.iter().any(|p| p.offset as u64 >= size) {
        return Err("plane offset beyond the dma-buf".into());
    }
    Ok(())
}

impl Server {
    fn new(shared: Arc<Shared>, listener: OwnedFd, wake: OwnedFd, on_frame: OnFrame) -> Self {
        Self {
            shared,
            listener,
            wake,
            on_frame,
            clients: Vec::new(),
            pending: Vec::new(),
            started: None,
            started_fps: 0,
            candidates: Vec::new(),
            announce_seq: 0,
            pool_generation: 0,
            is_sync_file: protocol::is_sync_file,
            is_dmabuf: protocol::is_dmabuf,
            alive,
            refused_uids: Vec::new(),
            refused_pids: Vec::new(),
        }
    }

    fn run(mut self) {
        let mut fds = Vec::new();
        loop {
            fds.clear();
            fds.extend([pollfd(self.wake.as_fd()), pollfd(self.listener.as_fd())]);
            let nc = self.clients.len();
            fds.extend(self.clients.iter().map(|c| pollfd(c.sock.as_fd())));
            fds.extend(self.pending.iter().map(|p| pollfd(p.sync.as_fd())));
            // Wake up for the first client whose hello is overdue.
            let timeout = self
                .clients
                .iter()
                .filter(|c| !c.hello)
                .map(|c| (c.since + HELLO_TIMEOUT).saturating_duration_since(Instant::now()))
                // While capturing, re-check staleness every second.
                .chain(self.started.as_ref().map(|_| Duration::from_secs(1)))
                .min()
                .map_or(-1, |d| d.as_millis() as i32 + 1);
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) } < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                (self.on_frame)(Err(format!("game capture poll failed: {e}").into()));
                return;
            }
            if self.shared.stop.load(Ordering::SeqCst) {
                return;
            }
            if fds[0].revents != 0 {
                let mut buf = [0u8; 64];
                while unsafe {
                    libc::read(self.wake.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len())
                } > 0
                {}
            }
            // Pending frames first, oldest first: their indices match `fds`
            // until clients change.
            let mut removed = 0;
            for (k, f) in fds[2 + nc..].iter().enumerate() {
                if f.revents != 0 {
                    let p = self.pending.remove(k - removed);
                    removed += 1;
                    self.deliver(p.sock, p.generation, p.index, p.pts_ns);
                }
            }
            // Back to front so removals keep earlier indices valid.
            for i in (0..nc).rev() {
                if fds[2 + i].revents != 0 && !self.read_client(i) {
                    self.remove_client(i);
                }
            }
            for i in (0..self.clients.len()).rev() {
                let c = &self.clients[i];
                if !c.hello && c.since.elapsed() >= HELLO_TIMEOUT {
                    warn!("{} ({}) sent no hello, dropping it", c.exe, c.pid);
                    self.remove_client(i);
                }
            }
            if fds[1].revents & libc::POLLIN != 0 {
                self.accept();
            }
            self.reselect();
            self.publish_games();
        }
    }

    fn accept(&mut self) {
        let sock = match protocol::accept(self.listener.as_fd()) {
            Ok(s) => s,
            Err(e) => return warn!("accepting a game connection failed: {e}"),
        };
        if self.clients.len() >= MAX_CLIENTS {
            return warn!("{MAX_CLIENTS} games connected already, refusing one more");
        }
        let cred = match protocol::peer_cred(sock.as_fd()) {
            Ok(c) if c.uid == unsafe { libc::getuid() } => c,
            other => {
                // Once per uid: a peer must not flood the log.
                let uid = other.map_or(u32::MAX, |c| c.uid);
                if !self.refused_uids.contains(&uid) {
                    self.refused_uids.push(uid);
                    warn!("refusing connections from another user (uid {uid})");
                }
                return;
            }
        };
        let pid = cred.pid.to_string();
        let exe = protocol::process_name_of(&pid);
        let steam = std::fs::read(format!("/proc/{pid}/environ"))
            .is_ok_and(|env| protocol::is_steam_game(&env));
        self.add_client(sock, cred.pid as u32, exe, steam);
    }

    fn add_client(&mut self, sock: OwnedFd, pid: u32, exe: String, steam: bool) {
        self.clients.push(Client {
            sock: Arc::new(sock),
            pid,
            exe,
            steam,
            hello: false,
            since: Instant::now(),
            last_frame: Instant::now(),
            width: 0,
            height: 0,
            announced: 0,
            ring: None,
        });
    }

    /// Reads all queued messages. False when the client is gone or broken.
    fn read_client(&mut self, i: usize) -> bool {
        for _ in 0..MAX_MESSAGES_PER_ROUND {
            let sock = self.clients[i].sock.clone();
            match protocol::recv(sock.as_fd(), true) {
                Ok(Some((msg, fd))) => {
                    if !self.handle(i, msg, fd) {
                        return false;
                    }
                }
                Ok(None) => return false,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    warn!("dropping game connection: {e}");
                    return false;
                }
            }
        }
        // More is queued: poll reports it again next round.
        true
    }

    /// Handles one message. False drops the client (protocol violation).
    fn handle(&mut self, i: usize, msg: Msg, fd: Option<OwnedFd>) -> bool {
        let started = self
            .started
            .as_ref()
            .is_some_and(|s| Arc::ptr_eq(s, &self.clients[i].sock));
        let c = &mut self.clients[i];
        if !c.hello && !matches!(msg, Msg::Hello { .. }) {
            return false;
        }
        match msg {
            // Identity comes from SO_PEERCRED, not from what the peer claims.
            Msg::Hello { version, .. } => {
                if c.hello {
                    warn!("{} ({}): second hello", c.exe, c.pid);
                    return false;
                }
                if version != protocol::VERSION {
                    // Once per process: an old layer reconnects every second.
                    if !self.refused_pids.contains(&c.pid) {
                        if self.refused_pids.len() >= MAX_CLIENTS {
                            self.refused_pids.remove(0);
                        }
                        self.refused_pids.push(c.pid);
                        warn!(
                            "{} ({}): its Vesper Vulkan layer speaks protocol {version}, this Vesper \
                             needs {}; not capturing it. Update the layer in Vesper's settings \
                             and restart the game",
                            c.exe,
                            c.pid,
                            protocol::VERSION
                        );
                    }
                    return false;
                }
                c.hello = true;
                debug!("game connected: {} ({})", c.exe, c.pid);
            }
            Msg::Swapchain { width, height } => {
                debug!("{} ({}) swapchain {width}x{height}", c.exe, c.pid);
                (c.width, c.height) = (width, height);
                // Ranked by its first swapchain: recreating one is not a new game.
                if width > 0 && height > 0 && c.announced == 0 {
                    self.announce_seq += 1;
                    c.announced = self.announce_seq;
                }
            }
            Msg::Buffer {
                generation,
                index,
                width,
                height,
                fourcc,
                modifier,
                planes,
            } => {
                let Some(fd) = fd else {
                    return false;
                };
                if !started {
                    // Our Stop got lost: say it again.
                    let _ = protocol::send(c.sock.as_fd(), &Msg::Stop, None, true);
                    return true;
                }
                if !(self.is_dmabuf)(fd.as_fd()) {
                    warn!("{} ({}): buffer fd is not a dma-buf", c.exe, c.pid);
                    return false;
                }
                // Size from lseek only once the fd is known to be a dma-buf.
                if let Err(e) =
                    validate_buffer(c, index, width, height, fourcc, &planes, fd.as_fd())
                {
                    warn!("{} ({}): invalid buffer: {e}", c.exe, c.pid);
                    return false;
                }
                if c.ring.as_ref().is_none_or(|r| r.generation != generation) {
                    c.ring = Some(Ring {
                        generation,
                        buffers: HashMap::new(),
                    });
                    self.pool_generation = self.pool_generation.wrapping_add(1);
                    info!("capturing {} ({}): {width}x{height}", c.exe, c.pid);
                }
                let ring = c.ring.as_mut().expect("ring set above");
                ring.buffers.insert(
                    index,
                    Buffer {
                        fd,
                        width,
                        height,
                        fourcc,
                        modifier,
                        planes,
                    },
                );
            }
            Msg::Frame {
                generation,
                index,
                pts_ns,
            } => {
                let known = c
                    .ring
                    .as_ref()
                    .is_some_and(|r| r.generation == generation && r.buffers.contains_key(&index));
                // A frame we cannot keep: hand the buffer straight back.
                let release = |c: &Client| {
                    let msg = Msg::Release { generation, index };
                    let _ = protocol::send(c.sock.as_fd(), &msg, None, true);
                };
                if !started {
                    release(c);
                    let _ = protocol::send(c.sock.as_fd(), &Msg::Stop, None, true);
                    return true;
                }
                c.last_frame = Instant::now();
                if !known {
                    release(c);
                    return true;
                }
                if fd.as_ref().is_some_and(|f| !(self.is_sync_file)(f.as_fd())) {
                    warn!("{} ({}): frame fence is not a sync_file", c.exe, c.pid);
                    release(c);
                    return true;
                }
                let waiting = self
                    .pending
                    .iter()
                    .filter(|p| Arc::ptr_eq(&p.sock, &c.sock))
                    .count();
                if waiting >= MAX_PENDING {
                    warn!("{} ({}): frames never complete, dropping it", c.exe, c.pid);
                    return false;
                }
                let sock = c.sock.clone();
                match fd {
                    Some(sync) => self.pending.push(Pending {
                        sock,
                        generation,
                        index,
                        pts_ns,
                        sync,
                    }),
                    None => self.deliver(sock, generation, index, pts_ns),
                }
            }
            Msg::Start { .. } | Msg::Stop | Msg::Release { .. } => return false,
        }
        true
    }

    fn deliver(&mut self, sock: Arc<OwnedFd>, generation: u32, index: u32, pts_ns: u64) {
        let buffer = self
            .clients
            .iter()
            .find(|c| Arc::ptr_eq(&c.sock, &sock))
            .and_then(|c| c.ring.as_ref())
            .filter(|r| r.generation == generation)
            .and_then(|r| r.buffers.get(&index));
        let release = move || {
            let msg = Msg::Release { generation, index };
            // shortcut: a lost Release costs the game one ring image until the next Start; retry from the server thread if this ever logs.
            if let Err(e) = protocol::send(sock.as_fd(), &msg, None, true)
                && e.kind() != io::ErrorKind::BrokenPipe
            {
                warn!("releasing buffer {index} failed: {e}");
            }
        };
        let Some(b) = buffer else {
            return release();
        };
        let planes: io::Result<Vec<DmaBufPlane>> = b
            .planes
            .iter()
            .map(|p| {
                Ok(DmaBufPlane {
                    fd: b.fd.try_clone()?,
                    offset: p.offset,
                    stride: p.stride,
                })
            })
            .collect();
        let planes = match planes {
            Ok(p) => p,
            Err(e) => {
                warn!("dup dma-buf fd: {e}");
                return release();
            }
        };
        (self.on_frame)(Ok(VideoFrame {
            width: b.width,
            height: b.height,
            fourcc: b.fourcc,
            modifier: b.modifier,
            planes,
            buffer_id: index,
            pool_generation: self.pool_generation,
            pts: Duration::from_nanos(pts_ns),
            release: ReleaseGuard::new(release),
        }));
    }

    fn remove_client(&mut self, i: usize) {
        let c = self.clients.remove(i);
        debug!("game disconnected: {} ({})", c.exe, c.pid);
        let mut select = self.shared.select.lock().unwrap();
        if *select == Some(c.pid) {
            *select = None;
        }
        drop(select);
        self.pending.retain(|p| !Arc::ptr_eq(&p.sock, &c.sock));
        if self
            .started
            .as_ref()
            .is_some_and(|s| Arc::ptr_eq(s, &c.sock))
        {
            self.started = None;
        }
    }

    fn allowed(&self, c: &Client) -> bool {
        c.steam || self.shared.allowed.lock().unwrap().contains(&c.exe)
    }

    /// Starts the chosen game and stops the previous one.
    fn reselect(&mut self) {
        let select = *self.shared.select.lock().unwrap();
        let current = self
            .started
            .as_ref()
            .and_then(|s| self.clients.iter().position(|c| Arc::ptr_eq(&c.sock, s)));
        let mut candidates = std::mem::take(&mut self.candidates);
        candidates.clear();
        candidates.extend(self.clients.iter().enumerate().map(|(i, c)| {
            let silent = c.last_frame.elapsed();
            Candidate {
                pid: c.pid,
                allowed: c.hello && self.allowed(c),
                ready: c.hello && c.width > 0 && c.height > 0,
                seq: c.announced,
                // A game sending frames is alive: no syscall per frame.
                stale: Some(i) == current
                    && (silent > STALE_AFTER || (silent > LIVENESS_AFTER && !(self.alive)(c.pid))),
            }
        }));
        let fps = self.shared.fps.load(Ordering::SeqCst);
        let want = if fps > 0 {
            choose(&candidates, select, current)
        } else {
            None
        };
        self.candidates = candidates;
        // Same game at another rate: start it again with the new one.
        if want == current && (want.is_none() || fps == self.started_fps) {
            return;
        }
        if let Some(i) = current {
            let c = &mut self.clients[i];
            let _ = protocol::send(c.sock.as_fd(), &Msg::Stop, None, true);
            c.ring = None;
            let sock = c.sock.clone();
            self.pending.retain(|p| !Arc::ptr_eq(&p.sock, &sock));
        }
        self.started = None;
        if let Some(i) = want {
            let c = &mut self.clients[i];
            let start = Msg::Start {
                modifiers: MODIFIERS.to_vec(),
                fps,
            };
            match protocol::send(c.sock.as_fd(), &start, None, true) {
                Ok(()) => {
                    c.ring = None;
                    c.last_frame = Instant::now();
                    self.started = Some(c.sock.clone());
                    self.started_fps = fps;
                }
                Err(e) => warn!("starting game capture failed: {e}"),
            }
        }
    }

    /// Updates the list for the app, only when it changed: this runs on
    /// every wakeup, so for every game frame.
    fn publish_games(&self) {
        let view = |c: &Client| {
            let consent = if self.allowed(c) {
                Consent::Allowed
            } else {
                Consent::Pending
            };
            let captured = self
                .started
                .as_ref()
                .is_some_and(|s| Arc::ptr_eq(s, &c.sock));
            (c.pid, c.width, c.height, consent, captured, c.announced)
        };
        let clients = self.clients.iter().filter(|c| c.hello);
        let mut games = self.shared.games.lock().unwrap();
        if games.len() == clients.clone().count()
            && games.iter().zip(clients.clone()).all(|(g, c)| {
                g.exe == c.exe
                    && (g.pid, g.width, g.height, g.consent, g.captured, g.seq) == view(c)
            })
        {
            return;
        }
        *games = clients
            .map(|c| {
                let (pid, width, height, consent, captured, seq) = view(c);
                Game {
                    pid,
                    exe: c.exe.clone(),
                    width,
                    height,
                    consent,
                    captured,
                    seq,
                }
            })
            .collect();
    }
}

fn pollfd(fd: BorrowedFd) -> libc::pollfd {
    libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }
}

#[cfg(test)]
mod fuzz;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn cand(pid: u32, allowed: bool, ready: bool, seq: u64) -> Candidate {
        Candidate {
            pid,
            allowed,
            ready,
            seq,
            stale: false,
        }
    }

    #[test]
    fn selection_is_sticky_and_needs_consent() {
        let view = [
            cand(1, true, true, 5),
            cand(2, true, true, 7),
            cand(3, true, false, 9),
            cand(4, false, true, 10),
        ];
        // Newest allowed game with a swapchain; never one without consent.
        assert_eq!(choose(&view, None, None), Some(1));
        assert_eq!(choose(&view, Some(1), None), Some(0));
        assert_eq!(choose(&view, Some(4), None), None);
        // Explicit pid without a swapchain, or unknown: nothing.
        assert_eq!(choose(&view, Some(3), None), None);
        assert_eq!(choose(&view, Some(9), None), None);
        assert_eq!(choose(&[], None, None), None);
        // The captured game stays, even while it has no swapchain and a
        // newer game is ready.
        assert_eq!(choose(&view, None, Some(0)), Some(0));
        assert_eq!(choose(&view, None, Some(2)), Some(2));
        // ...but an explicit selection moves it.
        assert_eq!(choose(&view, Some(2), Some(0)), Some(1));
    }

    #[test]
    fn stale_capture_gives_way() {
        let stale = |mut c: Candidate| {
            c.stale = true;
            c
        };
        // A frozen older game yields to a newer ready one...
        let view = [stale(cand(1, true, true, 5)), cand(2, true, true, 7)];
        assert_eq!(choose(&view, None, Some(0)), Some(1));
        // ...but stays when it is still the newest (long loading screen).
        let view = [cand(1, true, true, 5), stale(cand(2, true, true, 7))];
        assert_eq!(choose(&view, None, Some(1)), Some(1));
        // A stale game without a swapchain is dropped.
        let view = [stale(cand(1, true, false, 5))];
        assert_eq!(choose(&view, None, Some(0)), None);
        // Never to a game without consent.
        let view = [stale(cand(1, true, true, 5)), cand(2, false, true, 7)];
        assert_eq!(choose(&view, None, Some(0)), Some(0));
    }

    #[test]
    fn checks_process_liveness() {
        assert!(alive(std::process::id()));
        assert!(!alive(u32::MAX));
    }

    #[test]
    fn games_connect_over_the_socket() {
        let dir = std::env::temp_dir().join(format!("vesper-test-{}", std::process::id()));
        let name = dir.join("capture.sock");
        // A stale socket from a crashed instance is replaced.
        protocol::create_private_dir(&dir).unwrap();
        drop(protocol::listen(&name).unwrap());
        let capture = GameCapture::start_at(name.clone(), |_| {}).unwrap();
        let err = GameCapture::start_at(name.clone(), |_| {}).err().unwrap();
        assert!(err.to_string().contains("another Vesper instance"), "{err}");

        let sock = protocol::connect(&name).unwrap();
        assert!(protocol::peer_is_same_user(sock.as_fd()));
        let hello = Msg::Hello {
            version: protocol::VERSION,
            pid: 1,
            exe: "lies".into(),
        };
        protocol::send(sock.as_fd(), &hello, None, false).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let games = loop {
            let games = capture.games();
            if !games.is_empty() || Instant::now() > deadline {
                break games;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        // Identity comes from SO_PEERCRED, not from the hello.
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].pid, std::process::id());

        drop(capture);
        assert!(!name.exists());
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn forgets_selection_when_the_game_leaves() {
        let (mut s, _rx) = server();
        let (i, _layer) = connect(&mut s, "game.exe", true);
        let pid = s.clients[i].pid;
        *s.shared.select.lock().unwrap() = Some(pid);
        s.remove_client(i);
        assert_eq!(*s.shared.select.lock().unwrap(), None);
    }

    fn pair() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        let r = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        assert_eq!(r, 0);
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    /// A memfd standing in for a dma-buf of `size` bytes.
    fn memfd(size: u64) -> OwnedFd {
        let fd = unsafe { libc::memfd_create(c"vesper-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        File::from(fd.try_clone().unwrap()).set_len(size).unwrap();
        fd
    }

    fn server() -> (Server, mpsc::Receiver<VideoFrame>) {
        let (listener, _) = pair();
        let (wake, _) = pair();
        let shared = Arc::new(Shared {
            games: Mutex::new(Vec::new()),
            select: Mutex::new(None),
            allowed: Mutex::new(Vec::new()),
            fps: AtomicU32::new(60),
            stop: AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::channel();
        let mut s = Server::new(
            shared,
            listener,
            wake,
            Box::new(move |f| drop(tx.send(f.unwrap()))),
        );
        s.is_dmabuf = |_| true;
        s.alive = |_| true;
        (s, rx)
    }

    /// Connects a game and returns the layer's end of its socket.
    fn connect(s: &mut Server, exe: &str, steam: bool) -> (usize, OwnedFd) {
        let (ours, theirs) = pair();
        s.add_client(ours, 100 + s.clients.len() as u32, exe.into(), steam);
        let i = s.clients.len() - 1;
        let hello = Msg::Hello {
            version: protocol::VERSION,
            pid: 1,
            exe: "lies".into(),
        };
        assert!(s.handle(i, hello, None));
        assert!(s.handle(
            i,
            Msg::Swapchain {
                width: 64,
                height: 32
            },
            None
        ));
        (i, theirs)
    }

    fn next(sock: &OwnedFd) -> Option<Msg> {
        match protocol::recv(sock.as_fd(), true) {
            Ok(Some((m, _))) => Some(m),
            _ => None,
        }
    }

    fn buffer(generation: u32, index: u32, width: u32) -> Msg {
        Msg::Buffer {
            generation,
            index,
            width,
            height: 32,
            fourcc: protocol::XR24,
            modifier: 0,
            planes: vec![Plane {
                offset: 0,
                stride: 256,
            }],
        }
    }

    #[test]
    fn needs_hello_first_and_ignores_claimed_identity() {
        let (mut s, _rx) = server();
        let (ours, _theirs) = pair();
        s.add_client(ours, 7, "real".into(), true);
        assert!(!s.handle(
            0,
            Msg::Swapchain {
                width: 1,
                height: 1
            },
            None
        ));
        let (_, _t) = connect(&mut s, "real", true);
        s.publish_games();
        let games = s.shared.games.lock().unwrap().clone();
        assert_eq!(games[0].exe, "real");
        assert_ne!(games[0].pid, 1);
    }

    #[test]
    fn starts_only_allowed_games() {
        let (mut s, _rx) = server();
        let (_, layer) = connect(&mut s, "native", false);
        s.reselect();
        assert!(s.started.is_none());
        assert_eq!(next(&layer), None);
        s.publish_games();
        assert_eq!(s.shared.games.lock().unwrap()[0].consent, Consent::Pending);

        s.shared.allowed.lock().unwrap().push("native".into());
        s.reselect();
        assert!(s.started.is_some());
        assert!(matches!(next(&layer), Some(Msg::Start { .. })));

        // A Steam game connecting later does not steal the capture.
        let (_, steam) = connect(&mut s, "game.exe", true);
        s.reselect();
        assert_eq!(next(&steam), None);
    }

    #[test]
    fn inactive_starts_no_game() {
        let (mut s, _rx) = server();
        s.shared.fps.store(0, Ordering::SeqCst);
        let (_, layer) = connect(&mut s, "game.exe", true);
        s.reselect();
        assert!(s.started.is_none());
        assert_eq!(next(&layer), None);

        s.shared.fps.store(60, Ordering::SeqCst);
        s.reselect();
        assert!(matches!(next(&layer), Some(Msg::Start { fps: 60, .. })));
        s.reselect();
        assert_eq!(next(&layer), None);

        // Another recording rate: started again with it.
        s.shared.fps.store(144, Ordering::SeqCst);
        s.reselect();
        assert_eq!(next(&layer), Some(Msg::Stop));
        assert!(matches!(next(&layer), Some(Msg::Start { fps: 144, .. })));

        s.shared.fps.store(0, Ordering::SeqCst);
        s.reselect();
        assert!(s.started.is_none());
        assert_eq!(next(&layer), Some(Msg::Stop));
    }

    #[test]
    fn refuses_other_protocol_versions() {
        let (mut s, _rx) = server();
        for _ in 0..3 {
            let (ours, _theirs) = pair();
            s.add_client(ours, 7, "old".into(), true);
            let hello = Msg::Hello {
                version: protocol::VERSION - 1,
                pid: 7,
                exe: "old".into(),
            };
            let i = s.clients.len() - 1;
            assert!(!s.handle(i, hello, None));
            s.remove_client(i);
        }
        // Warned once for that process, however often it reconnects.
        assert_eq!(s.refused_pids, [7]);
        // A second hello is a protocol violation too.
        let (i, _layer) = connect(&mut s, "game.exe", true);
        let hello = Msg::Hello {
            version: protocol::VERSION,
            pid: 1,
            exe: "x".into(),
        };
        assert!(!s.handle(i, hello, None));
    }

    #[test]
    fn publishes_games_only_on_change() {
        let (mut s, _rx) = server();
        let (_, _layer) = connect(&mut s, "game.exe", true);
        s.publish_games();
        let before = s.shared.games.lock().unwrap().as_ptr();
        s.publish_games();
        assert_eq!(s.shared.games.lock().unwrap().as_ptr(), before);
        s.reselect();
        s.publish_games();
        let games = s.shared.games.lock().unwrap().clone();
        assert!(games[0].captured);
        assert_eq!(games[0].exe, "game.exe");
    }

    #[test]
    fn validates_buffers_and_delivers_frames() {
        let (mut s, rx) = server();
        let (i, layer) = connect(&mut s, "game.exe", true);
        s.reselect();
        assert!(matches!(next(&layer), Some(Msg::Start { .. })));

        // Size not matching the swapchain, index out of the ring, short dma-buf.
        assert!(!s.handle(i, buffer(1, 0, 128), Some(memfd(256 * 32))));
        let (i, layer) = connect(&mut s, "game.exe", true);
        s.started = Some(s.clients[i].sock.clone());
        assert!(!s.handle(i, buffer(1, protocol::RING, 64), Some(memfd(256 * 32))));
        let (i, layer2) = connect(&mut s, "game.exe", true);
        s.started = Some(s.clients[i].sock.clone());
        assert!(!s.handle(i, buffer(1, 0, 64), Some(memfd(100))));
        drop((layer, layer2));

        let (i, layer) = connect(&mut s, "game.exe", true);
        s.started = Some(s.clients[i].sock.clone());
        assert!(s.handle(i, buffer(1, 0, 64), Some(memfd(256 * 32))));
        let gen1 = s.pool_generation;
        let frame = |index| Msg::Frame {
            generation: 1,
            index,
            pts_ns: 42,
        };
        assert!(s.handle(i, frame(0), None));
        let f = rx.try_recv().unwrap();
        assert_eq!((f.buffer_id, f.pool_generation, f.width), (0, gen1, 64));
        assert_eq!(f.pts, Duration::from_nanos(42));
        drop(f);
        assert_eq!(
            next(&layer),
            Some(Msg::Release {
                generation: 1,
                index: 0
            })
        );

        // Unknown buffer: released at once, nothing delivered.
        assert!(s.handle(i, frame(2), None));
        assert!(rx.try_recv().is_err());
        assert_eq!(
            next(&layer),
            Some(Msg::Release {
                generation: 1,
                index: 2
            })
        );

        // New ring generation: new pool generation.
        assert!(s.handle(i, buffer(2, 0, 64), Some(memfd(256 * 32))));
        assert_ne!(s.pool_generation, gen1);
    }

    #[test]
    fn frames_from_a_stopped_game_are_returned() {
        let (mut s, rx) = server();
        let (i, layer) = connect(&mut s, "native", false);
        let frame = Msg::Frame {
            generation: 3,
            index: 1,
            pts_ns: 0,
        };
        assert!(s.handle(i, frame, None));
        assert!(rx.try_recv().is_err());
        assert_eq!(
            next(&layer),
            Some(Msg::Release {
                generation: 3,
                index: 1
            })
        );
        assert_eq!(next(&layer), Some(Msg::Stop));
    }

    #[test]
    fn caps_pending_frames() {
        let (mut s, _rx) = server();
        s.is_sync_file = |_| true;
        let (i, _layer) = connect(&mut s, "game.exe", true);
        s.reselect();
        assert!(s.handle(i, buffer(1, 0, 64), Some(memfd(256 * 32))));
        let frame = Msg::Frame {
            generation: 1,
            index: 0,
            pts_ns: 0,
        };
        for _ in 0..MAX_PENDING {
            assert!(s.handle(i, frame.clone(), Some(memfd(1))));
        }
        assert!(!s.handle(i, frame.clone(), Some(memfd(1))));

        // Without the test override, a non-sync_file fence is refused.
        s.is_sync_file = protocol::is_sync_file;
        s.pending.clear();
        assert!(s.handle(i, frame, Some(memfd(1))));
        assert!(s.pending.is_empty());
    }
}
