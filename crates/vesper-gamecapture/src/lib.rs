//! Game capture through Vesper's own Vulkan layer (`vesper-vklayer`).
//!
//! [`GameCapture::start`] listens on `$XDG_RUNTIME_DIR/vesper/capture.sock`.
//! Every Vulkan process with the layer installed connects while the server
//! runs and reports its swapchain size. The server captures one game at a
//! time: by default the one that most recently started presenting a
//! swapchain, or the one picked with [`GameCapture::select`]. Other games
//! stay dormant (pure pass-through).
//!
//! The selected game copies each presented image into a small ring of
//! exported DMA-BUFs. Frames arrive as [`VideoFrame`]s once their GPU copy
//! is done; dropping a frame tells the game the buffer is free again. See
//! [`protocol`] for the wire format.

pub mod protocol;

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use protocol::{Msg, Plane};
use tracing::{debug, info, warn};
use vesper_core::{DmaBufPlane, ReleaseGuard, VideoFrame};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

/// DRM modifiers the server asks for. Empty lets the layer pick any
/// single-allocation layout the GPU can export; same-GPU VAAPI imports them.
const MODIFIERS: &[u64] = &[];

/// Bounds on what one (untrusted) client may make the server hold: ring
/// images, and frames waiting for their sync_file.
const MAX_BUFFERS: u32 = 16;
const MAX_PENDING: usize = 16;

/// A game connected through the layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Game {
    pub pid: u32,
    /// Process name (`/proc/<pid>/comm`, at most 15 bytes; the `.exe` name
    /// under Proton).
    pub exe: String,
    /// Size of its capturable swapchain, 0x0 when it has none.
    pub width: u32,
    pub height: u32,
    /// Whether this game is the one being captured.
    pub captured: bool,
}

struct Shared {
    games: Mutex<Vec<Game>>,
    select: Mutex<Option<u32>>,
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
    /// error: the server waits for the next one.
    pub fn start(on_frame: impl FnMut(Result<VideoFrame>) + Send + 'static) -> Result<Self> {
        let path = protocol::socket_path().ok_or("XDG_RUNTIME_DIR is not set")?;
        protocol::create_private_dir(path.parent().expect("socket path has a parent"))?;
        if path.exists() {
            if protocol::connect(&path).is_ok() {
                return Err("another Vesper instance is already capturing games".into());
            }
            std::fs::remove_file(&path)?;
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

    /// Games currently connected.
    pub fn games(&self) -> Vec<Game> {
        self.shared.games.lock().unwrap().clone()
    }

    /// Captures the game with this pid, or picks automatically with `None`
    /// (the game that most recently started presenting).
    pub fn select(&self, pid: Option<u32>) {
        *self.shared.select.lock().unwrap() = pid;
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

struct Client {
    sock: Arc<OwnedFd>,
    game: Option<Game>,
    /// Order of its last non-empty `Swapchain` message, for auto-selection.
    announced: u64,
    /// Ring announced by the layer while started: generation and buffers.
    ring: Option<(u32, HashMap<u32, Buffer>)>,
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
    /// Socket of the client told to `Start`.
    started: Option<Arc<OwnedFd>>,
    announce_seq: u64,
    pool_generation: u32,
}

/// Picks the client to capture: the selected pid, else the most recent
/// non-empty swapchain announcement.
/// The current game stays eligible while it has no swapchain: games
/// recreate theirs on resize or focus changes.
fn choose(
    clients: &[(Option<&Game>, u64)],
    select: Option<u32>,
    current: Option<usize>,
) -> Option<usize> {
    let eligible = |i: usize, g: &Game| (g.width > 0 && g.height > 0) || Some(i) == current;
    let mut games = clients
        .iter()
        .enumerate()
        .filter_map(|(i, (g, seq))| Some((i, (*g)?, *seq)))
        .filter(|(i, g, _)| eligible(*i, g));
    match select {
        Some(pid) => games.find(|(_, g, _)| g.pid == pid).map(|(i, _, _)| i),
        None => games.max_by_key(|(_, _, seq)| *seq).map(|(i, _, _)| i),
    }
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
            announce_seq: 0,
            pool_generation: 0,
        }
    }

    fn run(mut self) {
        loop {
            let mut fds = vec![pollfd(self.wake.as_fd()), pollfd(self.listener.as_fd())];
            let nc = self.clients.len();
            fds.extend(self.clients.iter().map(|c| pollfd(c.sock.as_fd())));
            fds.extend(self.pending.iter().map(|p| pollfd(p.sync.as_fd())));
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) } < 0 {
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
            // Pending frames first: their indices match `fds` until clients change.
            let mut ready = fds[2 + nc..].iter().map(|f| f.revents != 0);
            let (done, waiting) = std::mem::take(&mut self.pending)
                .into_iter()
                .partition(|_| ready.next().unwrap_or(false));
            self.pending = waiting;
            for p in done {
                self.deliver(p.sock, p.generation, p.index, p.pts_ns);
            }
            // Back to front so removals keep earlier indices valid.
            for i in (0..nc).rev() {
                if fds[2 + i].revents != 0 && !self.read_client(i) {
                    self.remove_client(i);
                }
            }
            if fds[1].revents & libc::POLLIN != 0 {
                match protocol::accept(self.listener.as_fd()) {
                    Ok(sock) => self.clients.push(Client {
                        sock: Arc::new(sock),
                        game: None,
                        announced: 0,
                        ring: None,
                    }),
                    Err(e) => warn!("accepting a game connection failed: {e}"),
                }
            }
            self.reselect();
            self.publish_games();
        }
    }

    /// Reads all queued messages. False when the client is gone or broken.
    fn read_client(&mut self, i: usize) -> bool {
        loop {
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
    }

    fn handle(&mut self, i: usize, msg: Msg, fd: Option<OwnedFd>) -> bool {
        let started = self
            .started
            .as_ref()
            .is_some_and(|s| Arc::ptr_eq(s, &self.clients[i].sock));
        let c = &mut self.clients[i];
        match msg {
            Msg::Hello { version, pid, exe } => {
                if version != protocol::VERSION {
                    warn!(
                        "{exe} ({pid}) speaks protocol {version}, expected {}",
                        protocol::VERSION
                    );
                    return false;
                }
                debug!("game connected: {exe} ({pid})");
                c.game = Some(Game {
                    pid,
                    exe,
                    width: 0,
                    height: 0,
                    captured: false,
                });
            }
            Msg::Swapchain { width, height } => {
                let Some(game) = &mut c.game else {
                    return false;
                };
                debug!("{} ({}) swapchain {width}x{height}", game.exe, game.pid);
                (game.width, game.height) = (width, height);
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
                if index >= MAX_BUFFERS {
                    warn!("game sent buffer index {index}, dropping it");
                    return false;
                }
                if !started {
                    return true;
                }
                if c.ring.as_ref().is_none_or(|(g, _)| *g != generation) {
                    c.ring = Some((generation, HashMap::new()));
                    self.pool_generation = self.pool_generation.wrapping_add(1);
                    if let Some(g) = &c.game {
                        info!("capturing {} ({}): {width}x{height}", g.exe, g.pid);
                    }
                }
                let ring = &mut c.ring.as_mut().expect("ring set above").1;
                ring.insert(
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
                    .is_some_and(|(g, b)| *g == generation && b.contains_key(&index));
                if !started || !known {
                    // Not ours to keep: hand the buffer straight back.
                    let _ = protocol::send(
                        c.sock.as_fd(),
                        &Msg::Release { generation, index },
                        None,
                        true,
                    );
                    return true;
                }
                let waiting = self
                    .pending
                    .iter()
                    .filter(|p| Arc::ptr_eq(&p.sock, &c.sock))
                    .count();
                if waiting >= MAX_PENDING {
                    warn!("game has {waiting} frames that never complete, dropping it");
                    return false;
                }
                match fd {
                    Some(sync) => self.pending.push(Pending {
                        sock: c.sock.clone(),
                        generation,
                        index,
                        pts_ns,
                        sync,
                    }),
                    None => {
                        let sock = c.sock.clone();
                        self.deliver(sock, generation, index, pts_ns);
                    }
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
            .filter(|(g, _)| *g == generation)
            .and_then(|(_, b)| b.get(&index));
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
        if let Some(g) = &c.game {
            debug!("game disconnected: {} ({})", g.exe, g.pid);
        }
        self.pending.retain(|p| !Arc::ptr_eq(&p.sock, &c.sock));
        if self
            .started
            .as_ref()
            .is_some_and(|s| Arc::ptr_eq(s, &c.sock))
        {
            self.started = None;
        }
    }

    /// Starts the chosen game and stops the previous one.
    fn reselect(&mut self) {
        let select = *self.shared.select.lock().unwrap();
        let view: Vec<_> = self
            .clients
            .iter()
            .map(|c| (c.game.as_ref(), c.announced))
            .collect();
        let current = self
            .started
            .as_ref()
            .and_then(|s| self.clients.iter().position(|c| Arc::ptr_eq(&c.sock, s)));
        let want = choose(&view, select, current);
        if want == current {
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
            };
            match protocol::send(c.sock.as_fd(), &start, None, true) {
                Ok(()) => {
                    c.ring = None;
                    self.started = Some(c.sock.clone());
                }
                Err(e) => warn!("starting game capture failed: {e}"),
            }
        }
    }

    fn publish_games(&self) {
        let games = self
            .clients
            .iter()
            .filter_map(|c| {
                let mut g = c.game.clone()?;
                g.captured = self
                    .started
                    .as_ref()
                    .is_some_and(|s| Arc::ptr_eq(s, &c.sock));
                Some(g)
            })
            .collect();
        *self.shared.games.lock().unwrap() = games;
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
mod tests {
    use super::*;

    fn game(pid: u32, w: u32) -> Game {
        Game {
            pid,
            exe: format!("g{pid}"),
            width: w,
            height: w,
            captured: false,
        }
    }

    #[test]
    fn auto_selects_latest_announcement() {
        let (a, b, c) = (game(1, 800), game(2, 800), game(3, 0));
        let view = [(Some(&a), 5), (Some(&b), 7), (Some(&c), 9), (None, 10)];
        assert_eq!(choose(&view, None, None), Some(1));
        assert_eq!(choose(&view, Some(1), None), Some(0));
        // Selected game without a swapchain, or unknown pid: nothing.
        assert_eq!(choose(&view, Some(3), None), None);
        assert_eq!(choose(&view, Some(9), None), None);
        assert_eq!(choose(&[], None, None), None);
        // The captured game keeps its place while recreating its swapchain.
        assert_eq!(choose(&view, None, Some(2)), Some(2));
        assert_eq!(choose(&view, Some(3), Some(2)), Some(2));
        // A newer game still wins over an older one that lost its swapchain.
        let view = [(Some(&c), 1), (Some(&b), 2)];
        assert_eq!(choose(&view, None, Some(0)), Some(1));
    }
}
