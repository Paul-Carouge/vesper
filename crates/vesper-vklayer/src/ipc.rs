//! Connection to Vesper, one per process.
//!
//! A background thread tries to connect once per second, so the present path
//! only reads atomics while Vesper is not running. Once connected, the thread
//! blocks on the socket and records `Start`/`Stop`/`Release` from the server.
//! The render thread sends its own messages with non-blocking writes.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use vesper_gamecapture::protocol::{self, Msg};

pub struct Ipc {
    /// A server is connected. The only check on the present path when not.
    pub connected: AtomicBool,
    /// The server asked this process to capture.
    pub capturing: AtomicBool,
    /// Bumped on every new connection, so swapchains announce themselves again.
    pub conn_id: AtomicU64,
    /// Bumped on every `Start`, so the export ring is announced again.
    pub start_id: AtomicU64,
    sock: Mutex<Option<Arc<OwnedFd>>>,
    modifiers: Mutex<Vec<u64>>,
    releases: Mutex<Vec<(u32, u32)>>,
}

pub static IPC: Ipc = Ipc {
    connected: AtomicBool::new(false),
    capturing: AtomicBool::new(false),
    conn_id: AtomicU64::new(0),
    start_id: AtomicU64::new(0),
    sock: Mutex::new(None),
    modifiers: Mutex::new(Vec::new()),
    releases: Mutex::new(Vec::new()),
};

static THREAD: Once = Once::new();

/// Starts the connection thread once per process.
pub fn ensure_thread() {
    THREAD.call_once(|| {
        // The loader unloads layers on vkDestroyInstance; the thread runs our
        // code forever, so keep the library mapped.
        if !pin_library() {
            crate::warn("cannot pin the layer library; capture disabled");
            return;
        }
        if let Err(e) = std::thread::Builder::new()
            .name("vesper-layer".into())
            .spawn(run)
        {
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

fn run() {
    let Some(path) = protocol::socket_path() else {
        return;
    };
    let hello = Msg::Hello {
        version: protocol::VERSION,
        pid: std::process::id(),
        exe: protocol::process_name(),
    };
    loop {
        if let Ok(sock) = protocol::connect(&path)
            && protocol::send(sock.as_fd(), &hello, None, false).is_ok()
        {
            let sock = Arc::new(sock);
            *IPC.sock.lock().unwrap() = Some(sock.clone());
            IPC.conn_id.fetch_add(1, Ordering::SeqCst);
            IPC.connected.store(true, Ordering::SeqCst);
            serve(sock.as_fd());
            IPC.connected.store(false, Ordering::SeqCst);
            IPC.capturing.store(false, Ordering::SeqCst);
            *IPC.sock.lock().unwrap() = None;
            IPC.releases.lock().unwrap().clear();
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn serve(sock: BorrowedFd) {
    loop {
        match protocol::recv(sock, false) {
            Ok(Some((Msg::Start { modifiers }, _))) => {
                *IPC.modifiers.lock().unwrap() = modifiers;
                IPC.start_id.fetch_add(1, Ordering::SeqCst);
                IPC.capturing.store(true, Ordering::SeqCst);
            }
            Ok(Some((Msg::Stop, _))) => IPC.capturing.store(false, Ordering::SeqCst),
            Ok(Some((Msg::Release { generation, index }, _))) => {
                IPC.releases.lock().unwrap().push((generation, index));
            }
            Ok(Some(_)) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Ok(None) | Err(_) => return,
        }
    }
}

/// Sends without blocking. False if the message was not sent.
pub fn send(msg: &Msg, fd: Option<BorrowedFd>) -> bool {
    let sock = IPC.sock.lock().unwrap().clone();
    sock.is_some_and(|s| protocol::send(s.as_fd(), msg, fd, true).is_ok())
}

pub fn modifiers() -> Vec<u64> {
    IPC.modifiers.lock().unwrap().clone()
}

pub fn take_releases() -> Vec<(u32, u32)> {
    std::mem::take(&mut *IPC.releases.lock().unwrap())
}
