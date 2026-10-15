//! The audio region shared with a host: a memfd holding a [`Header`], then
//! one input and one output block of interleaved `f32`. Vesper creates it
//! (it outlives host restarts) and hands the fd to each host.
//!
//! One block at a time: Vesper writes the input and `frames`, then bumps
//! `req` and wakes the host; the host processes, writes the output and
//! `latency`, then sets `done` to `req` and wakes Vesper. Each side waits
//! on the other's counter with a futex (shared, not private: two processes).

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

#[repr(C)]
pub struct Header {
    /// Requests sent by Vesper (wrapping).
    pub req: AtomicU32,
    /// The last request the host answered.
    pub done: AtomicU32,
    /// Frames in the current request.
    pub frames: AtomicU32,
    /// The plugin's latency at its last block, frames.
    pub latency: AtomicU32,
    /// Non-zero: the plugin failed the last block, its output is not valid.
    pub failed: AtomicU32,
}

/// Where the buffers start: the header, padded to a cache line.
const BUFFERS: usize = 64;
const _: () = assert!(size_of::<Header>() <= BUFFERS);

pub struct Shm {
    ptr: *mut u8,
    len: usize,
    fd: OwnedFd,
    /// Samples in one buffer.
    samples: usize,
}

// SAFETY: the mapping is plain shared memory; the header is atomics and
// the buffers are only touched through the request/answer handshake.
unsafe impl Send for Shm {}
unsafe impl Sync for Shm {}

fn size(samples: usize) -> usize {
    BUFFERS + 2 * samples * size_of::<f32>()
}

impl Shm {
    /// A new region for blocks of up to `max_frames` frames of `channels`.
    pub fn create(channels: usize, max_frames: usize) -> io::Result<Self> {
        // SAFETY: a valid C string; the fd is ours.
        let fd = unsafe {
            libc::memfd_create(
                c"vesper-vst".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: just created, owned by nothing else.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let samples = channels * max_frames;
        // SAFETY: a valid fd. A new memfd reads as zeros.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), size(samples) as libc::off_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // A host shrinking the region would make Vesper's next access to
        // the mapping a SIGBUS: the size is sealed.
        let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
        // SAFETY: a valid fd.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Self::map(fd, channels, max_frames)
    }

    /// Maps a region made by [`Shm::create`] with the same sizes.
    pub fn map(fd: OwnedFd, channels: usize, max_frames: usize) -> io::Result<Self> {
        let samples = channels
            .checked_mul(max_frames)
            .filter(|&s| s > 0 && s <= 1 << 24)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad block size"))?;
        let len = size(samples);
        // SAFETY: fstat into a zeroed struct.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if st.st_size as usize != len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "audio region of the wrong size",
            ));
        }
        // SAFETY: maps `len` bytes of the fd, which has exactly that size.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            ptr: ptr.cast(),
            len,
            fd,
            samples,
        })
    }

    pub fn header(&self) -> &Header {
        // SAFETY: page-aligned, zero-initialized atomics, mapped while self lives.
        unsafe { &*self.ptr.cast::<Header>() }
    }

    /// Samples in each buffer.
    pub fn samples(&self) -> usize {
        self.samples
    }

    fn buffer(&self, i: usize) -> *mut f32 {
        // SAFETY: inside the mapping (see `size`), f32-aligned.
        unsafe { self.ptr.add(BUFFERS + i * self.samples * size_of::<f32>()) }.cast()
    }

    /// Copies `src` to the input buffer. Only by the side whose turn it is.
    pub fn write_input(&self, src: &[f32]) {
        self.write(0, src);
    }

    pub fn read_input(&self, dst: &mut [f32]) {
        self.read(0, dst);
    }

    pub fn write_output(&self, src: &[f32]) {
        self.write(1, src);
    }

    pub fn read_output(&self, dst: &mut [f32]) {
        self.read(1, dst);
    }

    fn write(&self, i: usize, src: &[f32]) {
        let n = src.len().min(self.samples);
        // SAFETY: `n` samples fit the buffer; the handshake gives this side
        // sole access to it now (a peer breaking it only garbles audio).
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), self.buffer(i), n) };
    }

    fn read(&self, i: usize, dst: &mut [f32]) {
        let n = dst.len().min(self.samples);
        // SAFETY: as in `write`.
        unsafe { std::ptr::copy_nonoverlapping(self.buffer(i), dst.as_mut_ptr(), n) };
    }
}

impl AsFd for Shm {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        // SAFETY: our mapping, unmapped once.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// Sleeps while `word` holds `expected`, at most `timeout` (spurious
/// wake-ups happen: callers check again). No allocation.
pub fn futex_wait(word: &AtomicU32, expected: u32, timeout: Duration) {
    let ts = libc::timespec {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_nsec: timeout.subsec_nanos() as libc::c_long,
    };
    // SAFETY: a valid futex word and timespec. Not FUTEX_PRIVATE: shared.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAIT,
            expected,
            &ts as *const libc::timespec,
        )
    };
}

/// Wakes the waiters on `word`.
pub fn futex_wake(word: &AtomicU32) {
    // SAFETY: a valid futex word.
    unsafe { libc::syscall(libc::SYS_futex, word.as_ptr(), libc::FUTEX_WAKE, i32::MAX) };
}

/// Loads with Acquire: what the other side wrote before is visible.
pub fn load(word: &AtomicU32) -> u32 {
    word.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_mappings_share_the_region() {
        let a = Shm::create(2, 16).unwrap();
        let fd = a.as_fd().try_clone_to_owned().unwrap();
        let b = Shm::map(fd, 2, 16).unwrap();
        a.write_input(&[0.5; 32]);
        a.header().frames.store(16, Ordering::Release);
        let mut got = [0.0; 32];
        b.read_input(&mut got);
        assert_eq!(got, [0.5; 32]);
        assert_eq!(b.header().frames.load(Ordering::Acquire), 16);
        let fd = a.as_fd().try_clone_to_owned().unwrap();
        assert!(Shm::map(fd, 2, 32).is_err(), "size mismatch");
    }

    #[test]
    fn region_size_is_sealed() {
        let a = Shm::create(2, 16).unwrap();
        let fd = a.as_fd().as_raw_fd();
        for len in [0, 1 << 20] {
            // SAFETY: a valid fd.
            assert_ne!(unsafe { libc::ftruncate(fd, len) }, 0, "{len}");
        }
        a.write_input(&[1.0; 32]);
    }

    #[test]
    fn futex_wait_times_out_and_wakes() {
        let word = AtomicU32::new(1);
        let t = std::time::Instant::now();
        futex_wait(&word, 1, Duration::from_millis(20));
        assert!(t.elapsed() >= Duration::from_millis(15));
        // Not waiting at all when the value already differs.
        let t = std::time::Instant::now();
        futex_wait(&word, 2, Duration::from_secs(5));
        assert!(t.elapsed() < Duration::from_secs(1));
    }
}
