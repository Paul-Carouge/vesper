//! Repair of recordings cut short by a kill, a crash or a power loss.
//!
//! While a [`Muxer`](crate::Muxer) writes a file, a marker naming it sits in
//! the folder given to [`set_marker_dir`]. A clean `finish` removes it, so a
//! marker left behind names a file without cues or duration: playable up to
//! its last cluster, but slow or impossible to seek, and editors may refuse it.
//! [`recover_stale`] remuxes such files losslessly (stream copy) into new
//! files with cues and duration. A copy that holds every packet of the
//! original replaces it; any doubt keeps both files.
//!
//! The same per-file thread writes the marker (with fsync, off the caller's
//! thread) and asks the kernel to start writing the file back every
//! [`SYNC_PERIOD`] (`sync_file_range`, no wait), so a power cut loses seconds
//! of a recording rather than the ~30 s of dirty pages the kernel keeps.

use std::ffi::CString;
use std::fs::File;
use std::io::{ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use ffmpeg_next::Rescale;
use ffmpeg_next::ffi as ff;

use crate::Error;

static MARKER_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Muxers writing a file right now (see [`writing`]).
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// How often the file's dirty pages are handed to the disk.
pub const SYNC_PERIOD: Duration = Duration::from_secs(2);

/// Sets the folder of the in-progress markers (once, at startup). Without
/// it, muxers write no marker, and files they leave unfinished are not
/// recovered.
pub fn set_marker_dir(dir: PathBuf) {
    let _ = MARKER_DIR.set(dir);
}

/// Whether a muxer of this process is writing a file: a recovery waits
/// meanwhile, not to compete with it for the disk.
pub fn writing() -> bool {
    LIVE.load(Ordering::Relaxed) > 0
}

/// A recording's helper thread: marker, then periodic writeback.
pub(crate) struct Guard {
    done: Sender<bool>,
}

impl Guard {
    /// Starts the helper for `path`, open as `file`. Never fails the
    /// recording: without a thread there is no marker and no writeback.
    pub(crate) fn start(path: &Path, file: &File) -> Option<Self> {
        Self::start_in(MARKER_DIR.get().cloned(), path, file)
    }

    fn start_in(dir: Option<PathBuf>, path: &Path, file: &File) -> Option<Self> {
        let file = file.try_clone().ok()?;
        let path = path.to_owned();
        let (done, rx) = mpsc::channel::<bool>();
        let spawned = std::thread::Builder::new()
            .name("vesper-mux-sync".into())
            .spawn(move || {
                let marker = dir.and_then(|dir| match write_marker(&dir, &path) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        eprintln!("[vesper] could not write the recording marker: {e}");
                        None
                    }
                });
                // `true`: finished cleanly, the marker goes.
                let finished = loop {
                    match rx.recv_timeout(SYNC_PERIOD) {
                        Ok(finished) => break finished,
                        Err(RecvTimeoutError::Disconnected) => break false,
                        Err(RecvTimeoutError::Timeout) => {
                            // Starts writeback of the dirty pages, does not wait for it.
                            unsafe {
                                libc::sync_file_range(
                                    file.as_raw_fd(),
                                    0,
                                    0,
                                    libc::SYNC_FILE_RANGE_WRITE,
                                )
                            };
                        }
                    }
                };
                // The finished file reaches the disk before its marker goes:
                // a power cut right after must still find one or the other.
                if finished
                    && let Some(m) = marker
                    && file.sync_data().is_ok()
                {
                    let _ = std::fs::remove_file(m);
                }
            });
        match spawned {
            Ok(_) => {
                LIVE.fetch_add(1, Ordering::Relaxed);
                Some(Self { done })
            }
            Err(e) => {
                eprintln!("[vesper] could not start the recording sync thread: {e}");
                None
            }
        }
    }

    /// Ends the helper; `clean`: the file is complete, remove its marker.
    /// Does not wait for the thread (it may be in a writeback call).
    pub(crate) fn end(self, clean: bool) {
        let _ = self.done.send(clean);
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// This boot's id: pids and start times only mean something within a boot.
fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

/// The start time of process `pid` (field 22 of its `stat`, in clock ticks
/// since boot): with the boot id, tells a pid reused by another process.
fn start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    // The command name, in parentheses, may hold spaces: fields count from
    // the last ')', which is followed by field 3.
    let after = stat.iter().rposition(|&b| b == b')')? + 1;
    let fields = std::str::from_utf8(&stat[after..]).ok()?;
    fields.split_whitespace().nth(22 - 3)?.parse().ok()
}

/// The writer of a marker: its pid, start time and boot.
#[derive(Debug, PartialEq)]
struct Writer {
    pid: u32,
    start: u64,
    boot: String,
}

impl Writer {
    fn me() -> Self {
        let pid = std::process::id();
        Self {
            pid,
            start: start_time(pid).unwrap_or_default(),
            boot: boot_id(),
        }
    }

    /// Whether this process still runs (same boot, same start time).
    fn running(&self) -> bool {
        self.boot == boot_id() && start_time(self.pid) == Some(self.start)
    }
}

/// Writes `<dir>/<pid>-<n>`: "<pid> <start time> <boot id>", a newline,
/// then `mkv`'s path bytes. Durable: the marker must outlive a power cut
/// that the file survives. The folder is private (0700).
fn write_marker(dir: &Path, mkv: &Path) -> std::io::Result<PathBuf> {
    static N: AtomicU64 = AtomicU64::new(0);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let me = Writer::me();
    let name = format!("{}-{}", me.pid, N.fetch_add(1, Ordering::Relaxed));
    let path = dir.join(&name);
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut bytes = format!("{} {} {}\n", me.pid, me.start, me.boot).into_bytes();
    bytes.extend_from_slice(mkv.as_os_str().as_bytes());
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(&bytes)?;
            f.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp, &path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    File::open(dir)?.sync_all()?;
    Ok(path)
}

/// Parses a marker: its writer and the file it names.
fn read_marker(bytes: Vec<u8>) -> Option<(Writer, PathBuf)> {
    let nl = bytes.iter().position(|&b| b == b'\n')?;
    let head = std::str::from_utf8(&bytes[..nl]).ok()?;
    let mut fields = head.split(' ');
    let writer = Writer {
        pid: fields.next()?.parse().ok()?,
        start: fields.next()?.parse().ok()?,
        boot: fields.next()?.to_owned(),
    };
    let path = PathBuf::from(std::ffi::OsString::from_vec(bytes[nl + 1..].to_vec()));
    (fields.next().is_none() && path.is_absolute()).then_some((writer, path))
}

/// What became of a file named by a stale marker.
#[derive(Debug)]
pub enum Outcome {
    /// Remuxed: the file at its original path now has cues and duration,
    /// and every packet it had.
    Recovered,
    /// Remuxed, but the copy may miss part of the original (damaged or
    /// refused packets): the copy is kept beside the untouched original.
    Partial { copy: PathBuf, reason: String },
    /// The file was complete after all (the process died after `finish`).
    Complete,
    /// The file is gone (its folder is there): nothing to do.
    Missing,
    /// Not enough free space beside the file for its copy: tried again at
    /// the next launch.
    NoSpace,
    /// The file cannot be reached now (unmounted disk, I/O error, full
    /// disk): tried again at the next launch.
    Postponed(Error),
    /// The file cannot be read as a recording; it is left as it was.
    Failed(Error),
}

impl Outcome {
    /// Whether the marker stays for the next launch.
    fn retry(&self) -> bool {
        matches!(self, Self::NoSpace | Self::Postponed(_))
    }
}

/// Recovers the files of the markers in `dir` left by processes that are
/// gone, and removes those markers (unless the file is to be tried again,
/// see [`Outcome`]). `suffix` (e.g. "recovered", translated) names a copy
/// kept beside its original. While `busy()` (e.g. [`writing`]), the copy
/// waits. Slow (it copies whole files): call it off the UI thread, at a low
/// I/O priority.
pub fn recover_stale(dir: &Path, suffix: &str, busy: &dyn Fn() -> bool) -> Vec<(PathBuf, Outcome)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let marker = entry.path();
        let name = entry.file_name();
        if name.as_bytes().starts_with(b".") {
            // A marker being written, or left half-written by a crash.
            if name.as_bytes().ends_with(b".tmp") && !running_tmp(name.as_bytes()) {
                let _ = std::fs::remove_file(&marker);
            }
            continue;
        }
        let Some((writer, mkv)) = std::fs::read(&marker).ok().and_then(read_marker) else {
            eprintln!("[vesper] unreadable recording marker {}", marker.display());
            let _ = std::fs::remove_file(&marker);
            continue;
        };
        if writer.running() {
            continue;
        }
        let outcome = recover(&mkv, suffix, busy);
        if !outcome.retry() {
            let _ = std::fs::remove_file(&marker);
        }
        out.push((mkv, outcome));
    }
    out
}

/// Whether a `.<pid>-<n>.tmp` may belong to a running process.
fn running_tmp(name: &[u8]) -> bool {
    let pid = name[1..].split(|&b| b == b'-').next().unwrap_or_default();
    std::str::from_utf8(pid)
        .ok()
        .and_then(|p| p.parse::<u32>().ok())
        .is_some_and(|pid| std::fs::metadata(format!("/proc/{pid}")).is_ok())
}

/// Copies `mkv` (see [`recover_stale`]). The original is only replaced by
/// a copy that holds all its packets, read back with cues and a duration,
/// and synced to the disk; it is never removed or changed otherwise.
fn recover(mkv: &Path, suffix: &str, busy: &dyn Fn() -> bool) -> Outcome {
    let postponed =
        |what: &str, e: std::io::Error| Outcome::Postponed(Error(format!("{what}: {e}")));
    match std::fs::symlink_metadata(mkv) {
        Ok(_) => {}
        // Gone, from a folder that is there: deleted, not unmounted.
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return match mkv.parent().map(std::fs::symlink_metadata) {
                Some(Ok(m)) if m.is_dir() => Outcome::Missing,
                _ => postponed("the folder is not there", e),
            };
        }
        Err(e) => return postponed("read the recording", e),
    }
    match finalized(mkv) {
        Ok(true) => return Outcome::Complete,
        Ok(false) => {}
        Err(Fault::Io(e)) => return Outcome::Postponed(e),
        Err(Fault::Bad(e)) => return Outcome::Failed(e),
    }
    let size = match std::fs::metadata(mkv) {
        Ok(m) => m.len(),
        Err(e) => return postponed("read the recording", e),
    };
    // The copy must leave room for a new recording.
    if free_space(mkv).is_some_and(|free| free < size + (size / 10).max(1 << 30)) {
        return Outcome::NoSpace;
    }
    let Some(stem) = mkv.file_stem() else {
        return Outcome::Failed(Error("the file has no name".into()));
    };
    let tmp = mkv.with_file_name(format!(".{}.recovering.mkv", stem.to_string_lossy()));
    // Left by an interrupted earlier attempt: ours, and incomplete.
    let _ = std::fs::remove_file(&tmp);
    let outcome = match remux(mkv, &tmp, busy) {
        Ok(doubt) => {
            let doubt = doubt.or_else(|| match finalized(&tmp) {
                Ok(true) => None,
                Ok(false) => Some("the copy has no cues or duration".into()),
                Err(Fault::Io(e) | Fault::Bad(e)) => Some(e.to_string()),
            });
            match doubt {
                None => replace(&tmp, mkv).map(|()| Outcome::Recovered),
                Some(reason) => {
                    keep_beside(&tmp, mkv, suffix).map(|copy| Outcome::Partial { copy, reason })
                }
            }
            .unwrap_or_else(|e| postponed("save the copy", e))
        }
        Err(Fault::Io(e)) => Outcome::Postponed(e),
        Err(Fault::Bad(e)) => Outcome::Failed(e),
    };
    if !matches!(outcome, Outcome::Recovered | Outcome::Partial { .. }) {
        // Only our own copy goes: the original stays as it was.
        let _ = std::fs::remove_file(&tmp);
    }
    outcome
}

/// Free bytes for this user on `path`'s file system.
fn free_space(path: &Path) -> Option<u64> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0).then(|| s.f_bavail * s.f_frsize)
}

/// Syncs `copy`, renames it over `mkv`, syncs the folder.
fn replace(copy: &Path, mkv: &Path) -> std::io::Result<()> {
    File::open(copy)?.sync_all()?;
    std::fs::rename(copy, mkv)?;
    sync_parent(mkv)
}

/// Syncs `copy` and gives it a free `<stem> (<suffix>).mkv` name.
fn keep_beside(copy: &Path, mkv: &Path, suffix: &str) -> std::io::Result<PathBuf> {
    File::open(copy)?.sync_all()?;
    let name = free_name(mkv, suffix).ok_or(ErrorKind::AlreadyExists)?;
    // No overwrite: a file taking the name meanwhile fails the link.
    std::fs::hard_link(copy, &name)?;
    std::fs::remove_file(copy)?;
    sync_parent(mkv)?;
    Ok(name)
}

fn sync_parent(path: &Path) -> std::io::Result<()> {
    match path.parent() {
        Some(dir) => File::open(dir)?.sync_all(),
        None => Ok(()),
    }
}

/// `<stem> (<suffix>).mkv` next to `mkv`, or with a number if taken.
fn free_name(mkv: &Path, suffix: &str) -> Option<PathBuf> {
    let stem = mkv.file_stem()?.to_string_lossy();
    (1..100)
        .map(|n| match n {
            1 => format!("{stem} ({suffix}).mkv"),
            n => format!("{stem} ({suffix} {n}).mkv"),
        })
        .map(|name| mkv.with_file_name(name))
        .find(|p| std::fs::symlink_metadata(p).is_err())
}

/// A failure: `Io` may pass (the disk, the mount), `Bad` is the file's own.
#[derive(Debug)]
enum Fault {
    Io(Error),
    Bad(Error),
}

impl Fault {
    /// Sorts an FFmpeg error: system errors (EIO, ENOSPC, EACCES...) may
    /// pass, the rest is in the file.
    fn ff(what: &str, e: ffmpeg_next::Error) -> Self {
        let error = Error(format!("{what}: {e}"));
        match e {
            ffmpeg_next::Error::Other { .. } => Self::Io(error),
            _ => Self::Bad(error),
        }
    }
}

/// Whether `path` has a duration and cues, as a finished file has.
fn finalized(path: &Path) -> Result<bool, Fault> {
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Fault::Bad(Error("path contains a NUL byte".into())))?;
    // Header only: no packets read, so the duration is the file's own, not
    // an estimate. (libavformat defers the cues until a seek, and a seek
    // indexes a file without cues too: they are looked up below.)
    let duration = unsafe {
        let mut ctx = ptr::null_mut();
        let ret = ff::avformat_open_input(&mut ctx, cpath.as_ptr(), ptr::null(), ptr::null_mut());
        if ret < 0 {
            return Err(Fault::ff(
                "open the recording",
                ffmpeg_next::Error::from(ret),
            ));
        }
        let duration = (*ctx).duration;
        ff::avformat_close_input(&mut ctx);
        duration
    };
    let io = |e: std::io::Error| Fault::Io(Error(format!("read the recording: {e}")));
    Ok(duration > 0 && duration != ff::AV_NOPTS_VALUE && has_cues(path).map_err(io)?)
}

const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const CUES: u32 = 0x1C53_BB6B;
const CLUSTER: u32 = 0x1F43_B675;

/// Whether the Matroska file has a Cues element. libavformat lists it in
/// the SeekHead at the start, rewritten by the trailer (or writes the cues
/// before the clusters); the listed position is checked too.
fn has_cues(path: &Path) -> std::io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = File::open(path)?;
    let mut head = Vec::new();
    (&mut file).take(64 * 1024).read_to_end(&mut head)?;
    let Some(cues) = cues_position(&head) else {
        return Ok(false);
    };
    let mut id = [0; 4];
    file.seek(SeekFrom::Start(cues))?;
    Ok(file.read_exact(&mut id).is_ok() && u32::from_be_bytes(id) == CUES)
}

/// The EBML variable-size integer at `b[*at..]`: (its value, its raw bytes
/// with the length marker, as IDs keep it). `None` past the end.
fn vint(b: &[u8], at: &mut usize) -> Option<(u64, u64)> {
    let first = *b.get(*at)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 {
        return None;
    }
    let bytes = b.get(*at..*at + len)?;
    *at += len;
    let raw = bytes.iter().fold(0u64, |v, &x| (v << 8) | u64::from(x));
    let value = raw & (u64::MAX >> (64 - 7 * len));
    Some((value, raw))
}

/// An element header at `b[*at..]`: (ID, data size, `None` if unknown).
fn element(b: &[u8], at: &mut usize) -> Option<(u32, Option<u64>)> {
    let (_, id) = vint(b, at)?;
    let before = *at;
    let (size, _) = vint(b, at)?;
    let unknown = size == u64::MAX >> (64 - 7 * (*at - before));
    Some((u32::try_from(id).ok()?, (!unknown).then_some(size)))
}

/// The file offset of the Cues element, from the head of a Matroska file.
fn cues_position(b: &[u8]) -> Option<u64> {
    let mut at = 0;
    let (_, ebml) = element(b, &mut at)?;
    at = at.checked_add(usize::try_from(ebml?).ok()?)?;
    let (id, _) = element(b, &mut at)?;
    if id != SEGMENT {
        return None;
    }
    let segment = at;
    loop {
        let el = at;
        let (id, size) = element(b, &mut at)?;
        match id {
            CUES => return Some(el as u64),
            CLUSTER => return None,
            SEEK_HEAD => {
                let end = at.checked_add(usize::try_from(size?).ok()?)?;
                let mut seek = at;
                while seek < end {
                    let (id, size) = element(b, &mut seek)?;
                    let data = b.get(seek..seek.checked_add(usize::try_from(size?).ok()?)?)?;
                    if id == SEEK
                        && let Some(pos) = seek_entry(data)
                    {
                        return (segment as u64).checked_add(pos);
                    }
                    seek += data.len();
                }
            }
            _ => {}
        }
        at = at.checked_add(usize::try_from(size?).ok()?)?;
    }
}

/// The SeekPosition of a Seek entry, if it is the Cues'.
fn seek_entry(b: &[u8]) -> Option<u64> {
    let (mut at, mut cues, mut pos) = (0, false, None);
    while at < b.len() {
        let (id, size) = element(b, &mut at)?;
        let data = b.get(at..at.checked_add(usize::try_from(size?).ok()?)?)?;
        match id {
            SEEK_ID => cues = data == CUES.to_be_bytes(),
            SEEK_POSITION if data.len() <= 8 => {
                pos = Some(data.iter().fold(0u64, |v, &x| (v << 8) | u64::from(x)));
            }
            _ => {}
        }
        at += data.len();
    }
    pos.filter(|_| cues)
}

/// Matroska timestamps, in which last packets are compared.
const MS: ffmpeg_next::Rational = ffmpeg_next::Rational(1, 1000);

/// Copies every packet of every track of `src` into a new MKV `dst`, with
/// the tracks' parameters (Colour included), names and dispositions. Waits
/// while `busy()`. `Ok(Some(reason))`: the copy may miss part of `src`
/// (damaged, unreadable or refused packets, a track ending earlier).
fn remux(src: &Path, dst: &Path, busy: &dyn Fn() -> bool) -> Result<Option<String>, Fault> {
    use ffmpeg_next::{codec, encoder, format};
    let io =
        |what: &'static str| move |e: ffmpeg_next::Error| Fault::Io(Error(format!("{what}: {e}")));
    // Before the open: it reads (and buffers) the first packets.
    let damage_before = damage_logged();
    let mut input = format::input(src).map_err(|e| Fault::ff("open the recording", e))?;
    let mut output = format::output_as(dst, "matroska").map_err(io("create the copy"))?;
    let mut time_bases = Vec::new();
    for ist in input.streams() {
        let mut ost = output
            .add_stream(encoder::find(codec::Id::None))
            .map_err(io("add a track"))?;
        ost.set_parameters(ist.parameters());
        ost.set_metadata(ist.metadata().to_owned());
        ost.set_avg_frame_rate(ist.avg_frame_rate());
        unsafe {
            (*ost.parameters().as_mut_ptr()).codec_tag = 0;
            (*ost.as_mut_ptr()).disposition = (*ist.as_ptr()).disposition;
        }
        time_bases.push(ist.time_base());
    }
    output.set_metadata(input.metadata().to_owned());
    output.write_header().map_err(io("write the header"))?;
    let mut packet = ffmpeg_next::Packet::empty();
    // Packets the copy lacks: unreadable spots, damaged or refused packets.
    let (mut read_errors, mut skipped, mut refused, mut in_a_row) = (0u64, 0u64, 0u64, 0u32);
    let mut last = vec![i64::MIN; time_bases.len()];
    for n in 0u64.. {
        if n % 256 == 0 {
            while busy() {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        match packet.read(&mut input) {
            Ok(()) => in_a_row = 0,
            Err(ffmpeg_next::Error::Eof) => break,
            Err(e @ ffmpeg_next::Error::Other { .. }) => {
                return Err(Fault::Io(Error(format!("read the recording: {e}"))));
            }
            // The demuxer resyncs to the next cluster after damage.
            Err(_) => {
                read_errors += 1;
                in_a_row += 1;
                if in_a_row > 100 {
                    break;
                }
                continue;
            }
        }
        let index = packet.stream();
        let Some(pts) = packet.pts().filter(|_| index < time_bases.len()) else {
            skipped += 1;
            continue;
        };
        let pts_ms = pts.rescale(time_bases[index], MS);
        last[index] = last[index].max(pts_ms);
        if packet.is_corrupt() {
            skipped += 1;
            continue;
        }
        if packet.dts().is_none() {
            packet.set_dts(packet.pts()); // no B-frames (vesper-core contract)
        }
        let tb = output.stream(index).map(|s| s.time_base());
        packet.rescale_ts(time_bases[index], tb.unwrap_or(time_bases[index]));
        packet.set_position(-1);
        match packet.write_interleaved(&mut output) {
            Ok(()) => {}
            // The disk (full, failing): the copy is not worth keeping.
            Err(e @ ffmpeg_next::Error::Other { .. }) => {
                return Err(Fault::Io(Error(format!("write the copy: {e}"))));
            }
            // E.g. a timestamp going back.
            Err(_) => refused += 1,
        }
    }
    if last.iter().all(|&l| l == i64::MIN) {
        return Err(Fault::Bad(Error("no packet could be read".into())));
    }
    output
        .write_trailer()
        .map_err(io("write cues and duration"))?;
    drop(output);
    // The demuxer skips a damaged spot (e.g. zeroed pages after a power
    // cut) without an error: it only logs it.
    let damaged = damage_logged() - damage_before;
    if read_errors + skipped + refused + damaged > 0 {
        return Ok(Some(format!(
            "{read_errors} read errors, {damaged} damaged spots, {skipped} damaged and {refused} refused packets"
        )));
    }
    let copied = last_pts(dst).map_err(|e| Fault::Io(Error(format!("read the copy: {e}"))))?;
    let short = last
        .iter()
        .zip(copied.iter().chain(std::iter::repeat(&i64::MIN)))
        .position(|(original, copy)| copy < original);
    Ok(short.map(|track| format!("track {track} of the copy ends earlier")))
}

/// The last pts of each track of `path`, in milliseconds (`i64::MIN`: none),
/// read from its last seconds.
fn last_pts(path: &Path) -> Result<Vec<i64>, ffmpeg_next::Error> {
    let mut input = ffmpeg_next::format::input(path)?;
    let tail = input.duration() - 10 * i64::from(ff::AV_TIME_BASE);
    if tail > 0 {
        input.seek(tail, ..tail)?;
    }
    let bases: Vec<_> = input.streams().map(|s| s.time_base()).collect();
    let mut last = vec![i64::MIN; bases.len()];
    for (stream, packet) in input.packets() {
        if let Some(pts) = packet.pts() {
            let i = stream.index();
            last[i] = last[i].max(pts.rescale(bases[i], MS));
        }
    }
    Ok(last)
}

/// Errors FFmpeg logged on this thread so far (see `count_damage`).
fn damage_logged() -> u64 {
    install_damage_counter();
    DAMAGE.with(std::cell::Cell::get)
}

thread_local! {
    static DAMAGE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn install_damage_counter() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe { ff::av_log_set_callback(Some(count_damage)) });
}

/// FFmpeg's log callback: counts the errors of the calling thread, then
/// logs as FFmpeg would.
unsafe extern "C" fn count_damage(
    avcl: *mut libc::c_void,
    level: libc::c_int,
    fmt: *const libc::c_char,
    vl: *mut ff::__va_list_tag,
) {
    // Only the demuxer's (the decoders probing the streams complain too).
    if level <= ff::AV_LOG_ERROR as libc::c_int && unsafe { from_format(avcl) } {
        let _ = DAMAGE.try_with(|d| d.set(d.get() + 1));
    }
    unsafe { ff::av_log_default_callback(avcl, level, fmt, vl) };
}

/// Whether a log context is a format (demuxer) context: its first field
/// is its `AVClass`, named "AVFormatContext".
unsafe fn from_format(avcl: *mut libc::c_void) -> bool {
    if avcl.is_null() {
        return false;
    }
    unsafe {
        let class = *(avcl as *const *const ff::AVClass);
        !class.is_null()
            && !(*class).class_name.is_null()
            && std::ffi::CStr::from_ptr((*class).class_name) == c"AVFormatContext"
    }
}

#[cfg(test)]
mod tests;
