//! Random-input tests for the game capture protocol and server: a layer client
//! in a game is untrusted input (same user, but any process of the account can
//! speak the protocol). No cargo-fuzz needed: a seeded xorshift generator keeps
//! failures reproducible (`FUZZ_SEED=<n>` reruns one seed, `FUZZ_ITERS=<n>`
//! scales the work).

use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, mpsc};

use super::protocol::{self, Msg, Plane};
use super::*;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    /// Small values often, extremes sometimes: the interesting edges.
    fn edge32(&mut self) -> u32 {
        match self.below(8) {
            0 => 0,
            1 => 1,
            2 => u32::MAX,
            3 => u32::MAX - 1,
            4 => 1 << 31,
            5 => self.below(5) as u32,
            6 => self.below(70_000) as u32,
            _ => self.next() as u32,
        }
    }
    fn bytes_upto(&mut self, max: u64) -> Vec<u8> {
        let n = self.below(max) as usize;
        self.bytes(n)
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

fn iters(default: u64) -> u64 {
    std::env::var("FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn seeds() -> Vec<u64> {
    match std::env::var("FUZZ_SEED").ok().and_then(|v| v.parse().ok()) {
        Some(seed) => vec![seed],
        None => (1..=8).collect(),
    }
}

fn random_msg(r: &mut Rng) -> Msg {
    match r.below(7) {
        0 => Msg::Hello {
            version: if r.below(4) == 0 {
                r.edge32()
            } else {
                protocol::VERSION
            },
            pid: r.edge32(),
            exe: String::from_utf8_lossy(&r.bytes_upto(300)).into_owned(),
        },
        1 => Msg::Swapchain {
            width: r.edge32(),
            height: r.edge32(),
        },
        2 => Msg::Start {
            modifiers: (0..r.below(80)).map(|_| r.next()).collect(),
        },
        3 => Msg::Stop,
        4 => Msg::Buffer {
            generation: r.edge32(),
            index: r.below(6) as u32,
            width: r.edge32(),
            height: r.edge32(),
            fourcc: if r.below(2) == 0 {
                protocol::XR24
            } else {
                r.edge32()
            },
            modifier: r.next(),
            planes: (0..r.below(6))
                .map(|_| Plane {
                    offset: r.edge32(),
                    stride: r.edge32(),
                })
                .collect(),
        },
        5 => Msg::Frame {
            generation: r.edge32(),
            index: r.below(6) as u32,
            pts_ns: r.next(),
        },
        _ => Msg::Release {
            generation: r.edge32(),
            index: r.edge32(),
        },
    }
}

#[test]
fn decode_never_panics_on_random_bytes() {
    for seed in seeds() {
        let mut r = Rng::new(seed);
        for _ in 0..iters(100_000) {
            // Purely random, and random after a plausible tag.
            let mut b = r.bytes_upto(80);
            if b.len() >= 4 && r.below(2) == 0 {
                b[..4].copy_from_slice(&(1 + r.below(8) as u32).to_le_bytes());
            }
            let _ = Msg::decode(&b);
        }
    }
}

#[test]
fn decode_survives_mutated_valid_messages() {
    for seed in seeds() {
        let mut r = Rng::new(seed);
        for _ in 0..iters(100_000) {
            let msg = random_msg(&mut r);
            let mut b = msg.encode();
            assert!(
                b.len() <= protocol::MAX_MSG,
                "{msg:?} encodes to {}",
                b.len()
            );
            // Valid input round-trips (a lossy `exe` aside).
            let no_planes = matches!(&msg, Msg::Buffer { planes, .. } if planes.is_empty());
            if !matches!(msg, Msg::Hello { .. }) && !no_planes {
                let back = Msg::decode(&b);
                assert!(back.is_ok(), "{msg:?}: {back:?}");
            }
            for _ in 0..1 + r.below(3) {
                match r.below(4) {
                    0 if !b.is_empty() => {
                        let i = r.below(b.len() as u64) as usize;
                        b[i] ^= 1 << r.below(8);
                    }
                    1 if !b.is_empty() => b.truncate(r.below(b.len() as u64) as usize),
                    2 => b.extend(r.bytes_upto(8)),
                    _ if b.len() >= 8 => {
                        let i = 4 + 4 * r.below((b.len() as u64 - 4) / 4) as usize;
                        if i + 4 <= b.len() {
                            b[i..i + 4].copy_from_slice(&r.edge32().to_le_bytes());
                        }
                    }
                    _ => {}
                }
            }
            if let Ok(m) = Msg::decode(&b) {
                // Whatever decodes stays within the documented bounds.
                match m {
                    Msg::Start { modifiers } => assert!(modifiers.len() <= 64),
                    Msg::Buffer { planes, .. } => assert!((1..=4).contains(&planes.len())),
                    Msg::Hello { exe, .. } => assert!(exe.len() <= 256 * 3),
                    _ => {}
                }
            }
        }
    }
}

/// Open descriptors that are this file's memfds. Only those: other tests run
/// in parallel and open sockets and files of their own (and a child process
/// would hold copies of their sockets for a moment, breaking their hang-up
/// checks).
fn open_fuzz_fds() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .filter(|target| target.to_string_lossy().contains("memfd:fuzz"))
        .count()
}

/// The hammer tests count descriptors: one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

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

/// An fd of a random kind: nothing but a memfd of random size, a pipe end,
/// /dev/null, a socket or a directory.
fn random_fd(r: &mut Rng) -> Option<OwnedFd> {
    match r.below(6) {
        0 => None,
        1 => {
            let fd = unsafe { libc::memfd_create(c"fuzz".as_ptr(), libc::MFD_CLOEXEC) };
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let size = [0, 1, 4096, 256 * 32, 1 << 20][r.below(5) as usize];
            File::from(fd.try_clone().unwrap()).set_len(size).unwrap();
            Some(fd)
        }
        2 => {
            let mut p = [0; 2];
            unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) };
            unsafe { libc::close(p[1]) };
            Some(unsafe { OwnedFd::from_raw_fd(p[0]) })
        }
        3 => Some(File::open("/dev/null").unwrap().into()),
        4 => Some(pair().0),
        _ => Some(File::open("/").unwrap().into()),
    }
}

/// Sends raw bytes with up to `fds` attached, like a hostile layer would.
fn send_raw(sock: &OwnedFd, bytes: &[u8], fds: &[OwnedFd]) {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut _,
        iov_len: bytes.len(),
    };
    let raw: Vec<RawFd> = fds.iter().map(|f| f.as_raw_fd()).collect();
    let mut cbuf = vec![0u64; 64];
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    if !raw.is_empty() {
        unsafe {
            hdr.msg_control = cbuf.as_mut_ptr().cast();
            hdr.msg_controllen = libc::CMSG_SPACE((raw.len() * 4) as u32) as _;
            let c = libc::CMSG_FIRSTHDR(&hdr);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN((raw.len() * 4) as u32) as _;
            std::ptr::copy_nonoverlapping(
                raw.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(c),
                raw.len() * 4,
            );
        }
    }
    // Full queue (the server is not reading): drop the message, never block.
    unsafe {
        libc::sendmsg(
            sock.as_raw_fd(),
            &hdr,
            libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
        )
    };
}

fn server(real_checks: bool) -> (Server, mpsc::Receiver<VideoFrame>) {
    let (listener, _) = pair();
    let (wake, _) = pair();
    let shared = Arc::new(Shared {
        games: Mutex::new(Vec::new()),
        select: Mutex::new(None),
        allowed: Mutex::new(Vec::new()),
        active: AtomicBool::new(true),
        stop: AtomicBool::new(false),
    });
    let (tx, rx) = mpsc::channel();
    let mut s = Server::new(
        shared,
        listener,
        wake,
        Box::new(move |f| drop(tx.send(f.unwrap()))),
    );
    s.alive = |_| true;
    if !real_checks {
        s.is_dmabuf = |_| true;
        s.is_sync_file = |_| true;
    }
    (s, rx)
}

/// Hostile clients (valid-looking and garbage messages, any fd) against the
/// server loop's own entry points: it must not panic, hang, leak descriptors
/// or hold more than its documented bounds.
fn hammer(seed: u64, real_checks: bool) {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let before = open_fuzz_fds();
    let mut r = Rng::new(seed);
    {
        let (mut s, rx) = server(real_checks);
        s.shared.allowed.lock().unwrap().push("fuzz".into());
        let mut peers: Vec<OwnedFd> = Vec::new();
        for step in 0..iters(4_000) {
            // Keep a couple of clients connected; replace dropped ones.
            while s.clients.len() < 3 {
                let (ours, theirs) = pair();
                s.add_client(ours, 100 + step as u32, "fuzz".into(), r.below(2) == 0);
                peers.push(theirs);
                let i = s.clients.len() - 1;
                // Most connect properly first, so later messages reach the handlers.
                if r.below(4) != 0 {
                    let hello = Msg::Hello {
                        version: protocol::VERSION,
                        pid: 1,
                        exe: "x".into(),
                    };
                    send_raw(peers.last().unwrap(), &hello.encode(), &[]);
                    let swap = Msg::Swapchain {
                        width: 64,
                        height: 32,
                    };
                    send_raw(peers.last().unwrap(), &swap.encode(), &[]);
                    assert!(s.read_client(i));
                }
            }
            let i = r.below(s.clients.len() as u64) as usize;
            let peer = &peers[i];
            let count = match r.below(10) {
                0 => 3,
                1..=5 => 1,
                _ => 0,
            };
            let fds: Vec<OwnedFd> = (0..count).filter_map(|_| random_fd(&mut r)).collect();
            match r.below(5) {
                0 => {
                    let b = r.bytes_upto(120);
                    send_raw(peer, &b, &fds);
                }
                _ => {
                    let m = if r.below(3) == 0 {
                        Msg::Buffer {
                            generation: r.below(3) as u32,
                            index: r.below(4) as u32,
                            width: 64,
                            height: 32,
                            fourcc: protocol::XR24,
                            modifier: 0,
                            planes: vec![Plane {
                                offset: r.below(4) as u32 * 100,
                                stride: 256,
                            }],
                        }
                    } else {
                        random_msg(&mut r)
                    };
                    send_raw(peer, &m.encode(), &fds);
                }
            }
            if !s.read_client(i) {
                s.remove_client(i);
                peers.remove(i);
            }
            if step % 8 == 0 {
                s.reselect();
                s.publish_games();
            }
            // Bounds the server promises regardless of the input.
            assert!(
                s.pending.len() <= MAX_PENDING * 3 + 3,
                "pending {}",
                s.pending.len()
            );
            assert!(s.clients.len() <= MAX_CLIENTS);
            // Frames handed out are released when dropped, not held forever.
            while let Ok(f) = rx.try_recv() {
                drop(f);
            }
        }
    }
    let after = open_fuzz_fds();
    assert_eq!(after, before, "seed {seed}: leaked fds {before} -> {after}");
}

#[test]
fn server_survives_hostile_clients() {
    for seed in seeds() {
        hammer(seed, false);
    }
}

#[test]
fn server_survives_hostile_clients_with_real_fd_checks() {
    for seed in seeds() {
        hammer(seed, true);
    }
}

#[test]
fn a_flood_is_read_in_bounded_rounds() {
    // One read round handles at most MAX_MESSAGES_PER_ROUND messages, so a
    // chatty client cannot starve the others sharing the server thread.
    let (mut s, _rx) = server(false);
    let (ours, theirs) = pair();
    s.add_client(ours, 100, "fuzz".into(), true);
    let hello = Msg::Hello {
        version: protocol::VERSION,
        pid: 1,
        exe: "x".into(),
    };
    send_raw(&theirs, &hello.encode(), &[]);
    let swap = Msg::Swapchain {
        width: 64,
        height: 32,
    }
    .encode();
    let queued = 150;
    for _ in 0..queued {
        send_raw(&theirs, &swap, &[]);
    }
    let t = Instant::now();
    let mut rounds = 0;
    // Each round returns true; the queue is empty when a round reads nothing.
    loop {
        assert!(s.read_client(0));
        rounds += 1;
        let mut probe = [0u8; 1];
        let more = unsafe {
            libc::recv(
                s.clients[0].sock.as_raw_fd(),
                probe.as_mut_ptr().cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        } > 0;
        if !more {
            break;
        }
    }
    assert!(rounds >= 2, "{queued} messages read in {rounds} round(s)");
    assert!(t.elapsed() < Duration::from_secs(1));
}
