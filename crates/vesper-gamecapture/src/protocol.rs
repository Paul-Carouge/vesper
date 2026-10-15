//! Wire protocol between the Vulkan layer (client, one per game process) and
//! Vesper (server), over a `SOCK_SEQPACKET` Unix socket.
//!
//! One message per packet: a `u32` tag, then little-endian fields. Strings and
//! lists are a `u32` length followed by the items. File descriptors travel as
//! `SCM_RIGHTS` ancillary data: one dma-buf with `Buffer`, an optional
//! sync_file with `Frame`.
//!
//! Flow: the layer connects and sends `Hello`, then `Swapchain` whenever its
//! capturable swapchain changes (0x0 when it has none). The server answers
//! `Start` to the game it wants; that layer then sends `Buffer` for each
//! image of its export ring and a `Frame` per present. The server sends
//! `Release` when it is done with a buffer, and `Stop` to make the layer
//! dormant again.

use std::ffi::{CString, OsStr};
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Bumped on any incompatible change. Peers with another version are ignored.
pub const VERSION: u32 = 1;

/// Largest encoded message. Every message is far smaller.
pub const MAX_MSG: usize = 1024;

/// DRM fourccs the layer sends: BGRA (`XR24`) and RGBA (`XB24`) byte order.
pub const XR24: u32 = u32::from_le_bytes(*b"XR24");
pub const XB24: u32 = u32::from_le_bytes(*b"XB24");

/// Images in the layer's export ring; buffer indices are below this.
pub const RING: u32 = 4;

const MAX_PLANES: usize = 4;
const MAX_MODIFIERS: usize = 64;
const MAX_EXE: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plane {
    pub offset: u32,
    pub stride: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// Layer to server, first message.
    Hello { version: u32, pid: u32, exe: String },
    /// Layer to server: size of the swapchain it would capture, 0x0 if none.
    Swapchain { width: u32, height: u32 },
    /// Server to layer: start capturing. `modifiers` lists the DRM modifiers
    /// the server can import, in preference order; empty means any.
    Start { modifiers: Vec<u64> },
    /// Server to layer: stop capturing and free the export ring.
    Stop,
    /// Layer to server, with the image's dma-buf fd: one image of the ring.
    /// A new `generation` replaces the whole previous ring.
    Buffer {
        generation: u32,
        index: u32,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: u64,
        planes: Vec<Plane>,
    },
    /// Layer to server, with an optional sync_file fd that signals when the
    /// copy into the buffer is done. `pts_ns` is `CLOCK_MONOTONIC`.
    Frame {
        generation: u32,
        index: u32,
        pts_ns: u64,
    },
    /// Server to layer: the buffer may be overwritten.
    Release { generation: u32, index: u32 },
}

const HELLO: u32 = 1;
const SWAPCHAIN: u32 = 2;
const START: u32 = 3;
const STOP: u32 = 4;
const BUFFER: u32 = 5;
const FRAME: u32 = 6;
const RELEASE: u32 = 7;

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let (head, rest) = self
            .0
            .split_first_chunk::<N>()
            .ok_or_else(|| invalid("truncated message"))?;
        self.0 = rest;
        Ok(*head)
    }
    fn u32(&mut self) -> io::Result<u32> {
        self.take().map(u32::from_le_bytes)
    }
    fn u64(&mut self) -> io::Result<u64> {
        self.take().map(u64::from_le_bytes)
    }
    fn len(&mut self, max: usize) -> io::Result<usize> {
        let n = self.u32()? as usize;
        if n > max {
            return Err(invalid("list too long"));
        }
        Ok(n)
    }
}

impl Msg {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(64);
        let u32 = |b: &mut Vec<u8>, v: u32| b.extend_from_slice(&v.to_le_bytes());
        match self {
            Msg::Hello { version, pid, exe } => {
                let exe = &exe.as_bytes()[..exe.len().min(MAX_EXE)];
                u32(&mut b, HELLO);
                u32(&mut b, *version);
                u32(&mut b, *pid);
                u32(&mut b, exe.len() as u32);
                b.extend_from_slice(exe);
            }
            Msg::Swapchain { width, height } => {
                u32(&mut b, SWAPCHAIN);
                u32(&mut b, *width);
                u32(&mut b, *height);
            }
            Msg::Start { modifiers } => {
                let modifiers = &modifiers[..modifiers.len().min(MAX_MODIFIERS)];
                u32(&mut b, START);
                u32(&mut b, modifiers.len() as u32);
                for m in modifiers {
                    b.extend_from_slice(&m.to_le_bytes());
                }
            }
            Msg::Stop => u32(&mut b, STOP),
            Msg::Buffer {
                generation,
                index,
                width,
                height,
                fourcc,
                modifier,
                planes,
            } => {
                let planes = &planes[..planes.len().min(MAX_PLANES)];
                u32(&mut b, BUFFER);
                for v in [*generation, *index, *width, *height, *fourcc] {
                    u32(&mut b, v);
                }
                b.extend_from_slice(&modifier.to_le_bytes());
                u32(&mut b, planes.len() as u32);
                for p in planes {
                    u32(&mut b, p.offset);
                    u32(&mut b, p.stride);
                }
            }
            Msg::Frame {
                generation,
                index,
                pts_ns,
            } => {
                u32(&mut b, FRAME);
                u32(&mut b, *generation);
                u32(&mut b, *index);
                b.extend_from_slice(&pts_ns.to_le_bytes());
            }
            Msg::Release { generation, index } => {
                u32(&mut b, RELEASE);
                u32(&mut b, *generation);
                u32(&mut b, *index);
            }
        }
        b
    }

    pub fn decode(bytes: &[u8]) -> io::Result<Msg> {
        let mut r = Reader(bytes);
        let msg = match r.u32()? {
            HELLO => {
                let (version, pid) = (r.u32()?, r.u32()?);
                let n = r.len(MAX_EXE)?;
                let exe = r.0.get(..n).ok_or_else(|| invalid("truncated message"))?;
                let exe = String::from_utf8_lossy(exe).into_owned();
                r.0 = &r.0[n..];
                Msg::Hello { version, pid, exe }
            }
            SWAPCHAIN => Msg::Swapchain {
                width: r.u32()?,
                height: r.u32()?,
            },
            START => {
                let n = r.len(MAX_MODIFIERS)?;
                let modifiers = (0..n).map(|_| r.u64()).collect::<io::Result<_>>()?;
                Msg::Start { modifiers }
            }
            STOP => Msg::Stop,
            BUFFER => {
                let (generation, index, width, height, fourcc) =
                    (r.u32()?, r.u32()?, r.u32()?, r.u32()?, r.u32()?);
                let modifier = r.u64()?;
                let n = r.len(MAX_PLANES)?;
                if n == 0 {
                    return Err(invalid("buffer without planes"));
                }
                let planes = (0..n)
                    .map(|_| {
                        Ok(Plane {
                            offset: r.u32()?,
                            stride: r.u32()?,
                        })
                    })
                    .collect::<io::Result<_>>()?;
                Msg::Buffer {
                    generation,
                    index,
                    width,
                    height,
                    fourcc,
                    modifier,
                    planes,
                }
            }
            FRAME => Msg::Frame {
                generation: r.u32()?,
                index: r.u32()?,
                pts_ns: r.u64()?,
            },
            RELEASE => Msg::Release {
                generation: r.u32()?,
                index: r.u32()?,
            },
            _ => return Err(invalid("unknown message")),
        };
        if !r.0.is_empty() {
            return Err(invalid("trailing bytes"));
        }
        Ok(msg)
    }
}

/// `$XDG_RUNTIME_DIR/vesper/capture.sock`.
pub fn socket_path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty())?;
    Some(Path::new(&dir).join("vesper").join("capture.sock"))
}

fn cvt(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

fn sockaddr(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let mut addr: libc::sockaddr_un = unsafe { zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path too long",
        ));
    }
    for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    let len = size_of::<libc::sa_family_t>() + bytes.len() + 1;
    Ok((addr, len as libc::socklen_t))
}

fn seqpacket() -> io::Result<OwnedFd> {
    let fd =
        cvt(unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Connects to the server (blocking socket; use `MSG_DONTWAIT` per call).
pub fn connect(path: &Path) -> io::Result<OwnedFd> {
    let (addr, len) = sockaddr(path)?;
    let fd = seqpacket()?;
    cvt(unsafe { libc::connect(fd.as_raw_fd(), (&raw const addr).cast(), len) })?;
    Ok(fd)
}

/// Binds and listens on `path`. The caller removes stale sockets first.
pub fn listen(path: &Path) -> io::Result<OwnedFd> {
    let (addr, len) = sockaddr(path)?;
    let fd = seqpacket()?;
    cvt(unsafe { libc::bind(fd.as_raw_fd(), (&raw const addr).cast(), len) })?;
    cvt(unsafe { libc::listen(fd.as_raw_fd(), 8) })?;
    Ok(fd)
}

/// Accepts one connection (close-on-exec, blocking).
pub fn accept(listener: BorrowedFd) -> io::Result<OwnedFd> {
    let fd = cvt(unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Sends one message, with at most one fd. Never raises `SIGPIPE`. With
/// `nonblock`, fails with `WouldBlock` instead of waiting for buffer space.
pub fn send(sock: BorrowedFd, msg: &Msg, fd: Option<BorrowedFd>, nonblock: bool) -> io::Result<()> {
    let bytes = msg.encode();
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut _,
        iov_len: bytes.len(),
    };
    // u64 storage keeps the control buffer aligned for cmsghdr.
    let mut cbuf = [0u64; 4];
    let mut hdr: libc::msghdr = unsafe { zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    if let Some(fd) = fd {
        let raw: RawFd = fd.as_raw_fd();
        unsafe {
            hdr.msg_control = cbuf.as_mut_ptr().cast();
            hdr.msg_controllen = libc::CMSG_SPACE(size_of::<RawFd>() as u32) as _;
            let c = libc::CMSG_FIRSTHDR(&hdr);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
            libc::CMSG_DATA(c).cast::<RawFd>().write_unaligned(raw);
        }
    }
    let flags = libc::MSG_NOSIGNAL | if nonblock { libc::MSG_DONTWAIT } else { 0 };
    let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &hdr, flags) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Receives one message and the first fd attached to it (extra fds are
/// closed). `Ok(None)` when the peer hung up. With `nonblock`, fails with
/// `WouldBlock` when nothing is queued. Malformed messages give `InvalidData`.
pub fn recv(sock: BorrowedFd, nonblock: bool) -> io::Result<Option<(Msg, Option<OwnedFd>)>> {
    let mut buf = [0u8; MAX_MSG];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut cbuf = [0u64; 8];
    let mut hdr: libc::msghdr = unsafe { zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    hdr.msg_control = cbuf.as_mut_ptr().cast();
    hdr.msg_controllen = size_of_val(&cbuf) as _;
    let flags = libc::MSG_CMSG_CLOEXEC | if nonblock { libc::MSG_DONTWAIT } else { 0 };
    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut hdr, flags) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    // Take ownership of every received fd first so none leaks on error.
    let mut fds = Vec::new();
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&hdr);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c).cast::<RawFd>();
                let count =
                    ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / size_of::<RawFd>();
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                }
            }
            c = libc::CMSG_NXTHDR(&hdr, c);
        }
    }
    if n == 0 {
        return Ok(None);
    }
    if hdr.msg_flags & libc::MSG_TRUNC != 0 {
        return Err(invalid("message too long"));
    }
    let msg = Msg::decode(&buf[..n as usize])?;
    Ok(Some((msg, fds.into_iter().next())))
}

/// Credentials of the peer of a connected Unix socket.
pub fn peer_cred(sock: BorrowedFd) -> io::Result<libc::ucred> {
    let mut cred: libc::ucred = unsafe { zeroed() };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    cvt(unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    })?;
    Ok(cred)
}

/// Whether the peer runs as the same user as this process.
pub fn peer_is_same_user(sock: BorrowedFd) -> bool {
    peer_cred(sock).is_ok_and(|c| c.uid == unsafe { libc::getuid() })
}

/// Process name from `/proc/<pid>/comm` (the Windows exe name under Wine).
pub fn process_name_of(pid: &str) -> String {
    std::fs::read(Path::new("/proc").join(pid).join("comm"))
        .map(|b| {
            OsStr::from_bytes(b.trim_ascii_end())
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_default()
}

/// Process name of the calling process.
pub fn process_name() -> String {
    process_name_of("self")
}

/// Whether `environ` (NUL-separated `KEY=VALUE`) marks a Steam game:
/// `SteamAppId` set to something other than empty or 0. `SteamGameId` alone
/// also marks non-Steam shortcuts, so it does not count.
pub fn is_steam_game(environ: &[u8]) -> bool {
    environ.split(|b| *b == 0).any(|kv| {
        let Some(eq) = kv.iter().position(|b| *b == b'=') else {
            return false;
        };
        let (k, v) = (&kv[..eq], &kv[eq + 1..]);
        k == b"SteamAppId" && !v.is_empty() && v != b"0"
    })
}

/// Whether `fd` is a dma-buf (lives on the dma-buf pseudo filesystem).
pub fn is_dmabuf(fd: BorrowedFd) -> bool {
    const DMA_BUF_MAGIC: libc::c_long = 0x444d_4142;
    let mut st: libc::statfs = unsafe { zeroed() };
    unsafe {
        libc::fstatfs(fd.as_raw_fd(), &mut st) == 0 && st.f_type as libc::c_long == DMA_BUF_MAGIC
    }
}

/// Whether `fd` is a sync_file (an anonymous inode named `sync_file`).
pub fn is_sync_file(fd: BorrowedFd) -> bool {
    std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
        .is_ok_and(|p| p.as_os_str().as_bytes() == b"anon_inode:sync_file")
}

/// Creates `dir` with mode 0700 if missing, then checks it is a real
/// directory owned by this user with mode 0700 (not a symlink).
pub(crate) fn create_private_dir(dir: &Path) -> io::Result<()> {
    let c = CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "nul in path"))?;
    if unsafe { libc::mkdir(c.as_ptr(), 0o700) } < 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::AlreadyExists {
            return Err(e);
        }
    }
    let mut st: libc::stat = unsafe { zeroed() };
    cvt(unsafe { libc::lstat(c.as_ptr(), &mut st) })?;
    if st.st_mode & libc::S_IFMT != libc::S_IFDIR
        || st.st_uid != unsafe { libc::getuid() }
        || st.st_mode & 0o777 != 0o700
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a private directory of this user", dir.display()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, Write};
    use std::os::fd::AsFd;

    fn all() -> Vec<Msg> {
        vec![
            Msg::Hello {
                version: VERSION,
                pid: 42,
                exe: "Game.exe".into(),
            },
            Msg::Swapchain {
                width: 2560,
                height: 1440,
            },
            Msg::Start {
                modifiers: vec![0, 0x0200_0000_0000_1234],
            },
            Msg::Stop,
            Msg::Buffer {
                generation: 3,
                index: 2,
                width: 1920,
                height: 1080,
                fourcc: u32::from_le_bytes(*b"XR24"),
                modifier: u64::MAX - 1,
                planes: vec![
                    Plane {
                        offset: 0,
                        stride: 7680,
                    },
                    Plane {
                        offset: 9_000_000,
                        stride: 64,
                    },
                ],
            },
            Msg::Frame {
                generation: 3,
                index: 1,
                pts_ns: 123_456_789_012,
            },
            Msg::Release {
                generation: 3,
                index: 1,
            },
        ]
    }

    #[test]
    fn detects_steam_games() {
        assert!(is_steam_game(b"HOME=/h\0SteamAppId=1245620\0"));
        assert!(is_steam_game(b"SteamAppId=570"));
        // Non-Steam shortcuts only get SteamGameId.
        assert!(!is_steam_game(b"SteamGameId=12345678901234\0"));
        assert!(!is_steam_game(b"SteamAppId=0\0SteamAppId=\0"));
        assert!(!is_steam_game(b"XSteamAppId=5\0SteamAppIdX=5\0"));
        assert!(!is_steam_game(b""));
        assert!(!is_dmabuf(
            std::fs::File::open("/dev/null").unwrap().as_fd()
        ));
    }

    #[test]
    fn roundtrip() {
        for m in all() {
            let b = m.encode();
            assert!(b.len() <= MAX_MSG);
            assert_eq!(Msg::decode(&b).unwrap(), m);
        }
    }

    #[test]
    fn rejects_malformed() {
        for m in all() {
            let b = m.encode();
            for n in 0..b.len() {
                assert!(Msg::decode(&b[..n]).is_err(), "{m:?} truncated to {n}");
            }
            let mut long = b.clone();
            long.push(0);
            assert!(Msg::decode(&long).is_err());
        }
        assert!(Msg::decode(&99u32.to_le_bytes()).is_err());
        let mut many = START.to_le_bytes().to_vec();
        many.extend_from_slice(&1000u32.to_le_bytes());
        assert!(Msg::decode(&many).is_err());
        let mut no_planes = Msg::Buffer {
            generation: 0,
            index: 0,
            width: 1,
            height: 1,
            fourcc: 0,
            modifier: 0,
            planes: vec![Plane {
                offset: 0,
                stride: 4,
            }],
        }
        .encode();
        no_planes.truncate(no_planes.len() - 12);
        no_planes.extend_from_slice(&0u32.to_le_bytes());
        assert!(Msg::decode(&no_planes).is_err());
    }

    #[test]
    fn passes_fds_over_a_socket() {
        let mut fds = [0; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    fds.as_mut_ptr(),
                )
            },
            0
        );
        let (a, b) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };

        assert_eq!(
            recv(b.as_fd(), true).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );

        let path = std::env::temp_dir().join(format!("vesper-proto-{}", std::process::id()));
        let mut file = std::fs::File::options()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        file.write_all(b"dmabuf").unwrap();

        let msg = Msg::Frame {
            generation: 1,
            index: 0,
            pts_ns: 5,
        };
        send(a.as_fd(), &msg, Some(file.as_fd()), true).unwrap();
        send(a.as_fd(), &Msg::Stop, None, true).unwrap();

        let (got, fd) = recv(b.as_fd(), true).unwrap().unwrap();
        assert_eq!(got, msg);
        let mut received = std::fs::File::from(fd.unwrap());
        received.rewind().unwrap();
        let mut s = String::new();
        received.read_to_string(&mut s).unwrap();
        assert_eq!(s, "dmabuf");

        let (got, fd) = recv(b.as_fd(), true).unwrap().unwrap();
        assert_eq!(got, Msg::Stop);
        assert!(fd.is_none());

        drop(a);
        assert!(recv(b.as_fd(), true).unwrap().is_none());
    }
}
