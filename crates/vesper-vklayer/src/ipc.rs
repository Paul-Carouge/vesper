//! Connection to Vesper, one per process.
//!
//! A background thread connects when Vesper's socket appears (inotify on its
//! directory; once per second while that directory does not exist), so the
//! present path only reads atomics while Vesper is not running. Once
//! connected, the thread blocks on the socket and records
//! `Start`/`Stop`/`Release` from the server. The render thread sends its own
//! messages with non-blocking writes.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use vesper_gamecapture::protocol::{self, Msg};

use crate::LockExt;

pub struct Ipc {
    /// A server is connected. The only check on the present path when not.
    pub connected: AtomicBool,
    /// The server asked this process to capture.
    pub capturing: AtomicBool,
    /// Bumped on every new connection, so swapchains announce themselves again.
    pub conn_id: AtomicU64,
    /// Bumped on every `Start`, so the export ring is announced again.
    pub start_id: AtomicU64,
    /// Recording rate of the last `Start`: copies are paced to it.
    pub fps: AtomicU32,
    /// Process that started the thread: a forked child has no thread, and
    /// its copies of the mutexes may be held by threads that do not exist.
    pid: AtomicU32,
    sock: Mutex<Option<Arc<OwnedFd>>>,
    modifiers: Mutex<Vec<u64>>,
    releases: Mutex<Vec<(u32, u32)>>,
}

pub static IPC: Ipc = Ipc {
    connected: AtomicBool::new(false),
    capturing: AtomicBool::new(false),
    conn_id: AtomicU64::new(0),
    start_id: AtomicU64::new(0),
    fps: AtomicU32::new(0),
    pid: AtomicU32::new(0),
    sock: Mutex::new(None),
    modifiers: Mutex::new(Vec::new()),
    releases: Mutex::new(Vec::new()),
};

/// More releases than this pending means a misbehaving server.
const MAX_RELEASES: usize = 64;

static THREAD: Once = Once::new();

/// Runs in a forked child: atomics only, the mutexes may be held.
extern "C" fn after_fork_child() {
    IPC.connected.store(false, Ordering::SeqCst);
    IPC.capturing.store(false, Ordering::SeqCst);
}

fn same_process() -> bool {
    IPC.pid.load(Ordering::SeqCst) == std::process::id()
}

/// Starts the connection thread once per process.
pub fn ensure_thread() {
    THREAD.call_once(|| {
        // The loader unloads layers on vkDestroyInstance; the thread runs our
        // code forever, so keep the library mapped.
        if !pin_library() {
            crate::warn("cannot pin the layer library; capture disabled");
            return;
        }
        IPC.pid.store(std::process::id(), Ordering::SeqCst);
        unsafe { libc::pthread_atfork(None, None, Some(after_fork_child)) };
        // The thread inherits the signal mask: block everything so the game's
        // signals (and handlers) never run on our thread.
        let spawned = unsafe {
            let mut all: libc::sigset_t = std::mem::zeroed();
            let mut old: libc::sigset_t = std::mem::zeroed();
            libc::sigfillset(&mut all);
            libc::pthread_sigmask(libc::SIG_BLOCK, &all, &mut old);
            let r = std::thread::Builder::new()
                .name("vesper-layer".into())
                .spawn(run);
            libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
            r
        };
        if let Err(e) = spawned {
            crate::warn(&format!("cannot start the IPC thread: {e}"));
        }
    });
}

fn pin_library() -> bool {
    unsafe {
        let mut info: libc::Dl_info = std::mem::zeroed();
        if libc::dladdr(ensure_thread as *const libc::c_void, &mut info) == 0
            || info.dli_fname.is_null()
        {
            return false;
        }
        !libc::dlopen(
            info.dli_fname,
            libc::RTLD_NOW | libc::RTLD_NOLOAD | libc::RTLD_NODELETE,
        )
        .is_null()
    }
}

/// Quick retries after the socket appears: the server binds, then listens.
const FAST_RETRIES: u32 = 5;
const FAST_RETRY_MS: i32 = 20;

fn run() {
    let path = protocol::socket_path();
    let dir = path.parent().expect("socket path has a parent");
    let hello = Msg::Hello {
        version: protocol::VERSION,
        pid: std::process::id(),
        exe: protocol::process_name(),
    };
    let mut watch = None;
    let mut fast = 0;
    loop {
        // Watch before connecting, so a socket created in between wakes us.
        if watch.is_none() {
            watch = watch_dir(dir);
        }
        // After a connection ends: once per second, as the server may have
        // dropped us while staying up.
        let mut timeout = 1000;
        match protocol::connect(&path) {
            Ok(sock) => {
                // Only talk to a server of our own user.
                if protocol::peer_is_same_user(sock.as_fd())
                    && protocol::send(sock.as_fd(), &hello, None, false).is_ok()
                {
                    let sock = Arc::new(sock);
                    *IPC.sock.locked() = Some(sock.clone());
                    IPC.conn_id.fetch_add(1, Ordering::SeqCst);
                    IPC.connected.store(true, Ordering::SeqCst);
                    serve(sock.as_fd());
                    IPC.connected.store(false, Ordering::SeqCst);
                    IPC.capturing.store(false, Ordering::SeqCst);
                    *IPC.sock.locked() = None;
                    IPC.releases.locked().clear();
                }
                fast = 0;
            }
            // No server: sleep until its socket appears.
            Err(e)
                if watch.is_some()
                    && matches!(
                        e.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) =>
            {
                timeout = -1
            }
            Err(_) => {}
        }
        if fast > 0 {
            fast -= 1;
            timeout = FAST_RETRY_MS;
        }
        match wait_dir(watch.as_ref(), timeout) {
            Wake::Created => fast = FAST_RETRIES,
            Wake::Timeout => {}
            Wake::Lost => watch = None,
        }
    }
}

enum Wake {
    /// Something was created in the directory.
    Created,
    Timeout,
    /// The watch is gone (directory removed): watch again.
    Lost,
}

/// An inotify fd watching `dir` for new entries. None if `dir` does not exist
/// (Vesper never ran since boot) or inotify is unavailable.
///
/// Costs one inotify instance per process with a capturable swapchain (the
/// IPC thread starts with the first one), counted against
/// `fs.inotify.max_user_instances`
/// (often 128 or 1024). Past that limit `inotify_init1` fails with `EMFILE`
/// and that process falls back to trying to connect once per second, as
/// before inotify; other apps' watches are not affected beyond sharing the
/// limit.
fn watch_dir(dir: &Path) -> Option<OwnedFd> {
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let c = CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mask = libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF;
    (unsafe { libc::inotify_add_watch(fd.as_raw_fd(), c.as_ptr(), mask) } >= 0).then_some(fd)
}

/// Waits up to `timeout_ms` (-1: forever) for an entry to appear in the
/// watched directory; sleeps the timeout (or a second) without a watch.
fn wait_dir(watch: Option<&OwnedFd>, timeout_ms: i32) -> Wake {
    let Some(fd) = watch else {
        let ms = if timeout_ms < 0 { 1000 } else { timeout_ms };
        std::thread::sleep(Duration::from_millis(ms as u64));
        return Wake::Timeout;
    };
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if n < 0 {
        return if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            Wake::Timeout
        } else {
            Wake::Lost
        };
    }
    if n == 0 {
        return Wake::Timeout;
    }
    // u64 storage keeps the buffer aligned for inotify_event.
    let mut buf = [0u64; 512];
    let len = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), size_of_val(&buf)) };
    if len <= 0 {
        return Wake::Lost;
    }
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), len as usize) };
    let (mut created, mut lost, mut at) = (false, false, 0);
    while at + size_of::<libc::inotify_event>() <= bytes.len() {
        let ev = unsafe {
            bytes
                .as_ptr()
                .add(at)
                .cast::<libc::inotify_event>()
                .read_unaligned()
        };
        if ev.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 {
            created = true;
        }
        if ev.mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_IGNORED) != 0 {
            lost = true;
        }
        at += size_of::<libc::inotify_event>() + ev.len as usize;
    }
    match (lost, created) {
        (true, _) => Wake::Lost,
        (false, true) => Wake::Created,
        (false, false) => Wake::Timeout,
    }
}

fn serve(sock: BorrowedFd) {
    loop {
        match protocol::recv(sock, false) {
            Ok(Some((Msg::Start { modifiers, fps }, _))) => {
                *IPC.modifiers.locked() = modifiers;
                IPC.fps.store(fps, Ordering::SeqCst);
                IPC.releases.locked().clear();
                IPC.start_id.fetch_add(1, Ordering::SeqCst);
                IPC.capturing.store(true, Ordering::SeqCst);
            }
            Ok(Some((Msg::Stop, _))) => IPC.capturing.store(false, Ordering::SeqCst),
            Ok(Some((Msg::Release { generation, index }, _))) => {
                let mut releases = IPC.releases.locked();
                if IPC.capturing.load(Ordering::SeqCst)
                    && index < protocol::RING
                    && releases.len() < MAX_RELEASES
                {
                    releases.push((generation, index));
                }
            }
            Ok(Some(_)) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Ok(None) | Err(_) => return,
        }
    }
}

/// Sends without blocking. False if the message was not sent.
pub fn send(msg: &Msg, fd: Option<BorrowedFd>) -> bool {
    if !same_process() {
        return false;
    }
    let sock = IPC.sock.locked().clone();
    sock.is_some_and(|s| protocol::send(s.as_fd(), msg, fd, true).is_ok())
}

pub fn fps() -> u32 {
    IPC.fps.load(Ordering::SeqCst)
}

pub fn modifiers() -> Vec<u64> {
    IPC.modifiers.locked().clone()
}

pub fn take_releases() -> Vec<(u32, u32)> {
    std::mem::take(&mut *IPC.releases.locked())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wakes_when_the_socket_appears() {
        let dir = std::env::temp_dir().join(format!("vesper-watch-{}", std::process::id()));
        assert!(watch_dir(&dir).is_none());
        std::fs::create_dir(&dir).unwrap();
        let watch = watch_dir(&dir).unwrap();
        assert!(matches!(wait_dir(Some(&watch), 0), Wake::Timeout));
        std::fs::write(dir.join("capture.sock"), b"").unwrap();
        assert!(matches!(wait_dir(Some(&watch), 1000), Wake::Created));
        std::fs::remove_file(dir.join("capture.sock")).unwrap();
        std::fs::remove_dir(&dir).unwrap();
        assert!(matches!(wait_dir(Some(&watch), 1000), Wake::Lost));
    }
}
