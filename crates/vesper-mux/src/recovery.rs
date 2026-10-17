//! Repair of recordings cut short by a kill, a crash or a power loss.
//!
//! While a [`Muxer`](crate::Muxer) writes a file, a marker naming it sits in
//! the folder given to [`set_marker_dir`]. A clean `finish` removes it, so a
//! marker left behind names a file without cues or duration: playable up to
//! its last cluster, but slow or impossible to seek, and editors may refuse it.
//! [`recover_stale`] remuxes such files losslessly (stream copy) into new
//! files with cues and duration, then renames them over the originals.
//!
//! The same per-file thread writes the marker (with fsync, off the caller's
//! thread) and asks the kernel to start writing the file back every
//! [`SYNC_PERIOD`] (`sync_file_range`, no wait), so a power cut loses seconds
//! of a recording rather than the ~30 s of dirty pages the kernel keeps.

use std::ffi::CString;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use ffmpeg_next::ffi as ff;

use crate::Error;

static MARKER_DIR: OnceLock<PathBuf> = OnceLock::new();

/// How often the file's dirty pages are handed to the disk.
pub const SYNC_PERIOD: Duration = Duration::from_secs(2);

/// Sets the folder of the in-progress markers (once, at startup). Without
/// it, muxers write no marker, and files they leave unfinished are not
/// recovered.
pub fn set_marker_dir(dir: PathBuf) {
    let _ = MARKER_DIR.set(dir);
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
                if finished && let Some(m) = marker {
                    let _ = std::fs::remove_file(m);
                }
            });
        match spawned {
            Ok(_) => Some(Self { done }),
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

/// Writes `<dir>/<pid>-<n>`: the pid, a newline, then `mkv`'s path bytes.
/// Durable: the marker must outlive a power cut that the file survives.
fn write_marker(dir: &Path, mkv: &Path) -> std::io::Result<PathBuf> {
    static N: AtomicU64 = AtomicU64::new(0);
    std::fs::create_dir_all(dir)?;
    let name = format!(
        "{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    );
    let path = dir.join(&name);
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut bytes = format!("{}\n", std::process::id()).into_bytes();
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

/// Parses a marker: the writer's pid and the file it names.
fn read_marker(bytes: Vec<u8>) -> Option<(u32, PathBuf)> {
    let nl = bytes.iter().position(|&b| b == b'\n')?;
    let pid = std::str::from_utf8(&bytes[..nl]).ok()?.parse().ok()?;
    let path = PathBuf::from(std::ffi::OsString::from_vec(bytes[nl + 1..].to_vec()));
    path.is_absolute().then_some((pid, path))
}

/// Whether `pid` is another running Vesper (same executable name), e.g. a
/// recording just started by this process, or a dev run beside the app.
fn running(pid: u32) -> bool {
    let comm = |p: &str| std::fs::read(format!("/proc/{p}/comm")).ok();
    comm(&pid.to_string()).is_some_and(|c| Some(c) == comm("self"))
}

/// What became of a file named by a stale marker.
#[derive(Debug)]
pub enum Outcome {
    /// Remuxed: the file at its original path now has cues and duration.
    Recovered,
    /// The file was complete after all (the process died after `finish`).
    Complete,
    /// The file is gone: nothing to do.
    Missing,
    /// Not recovered; the original file is untouched.
    Failed(Error),
}

/// Recovers the files of the markers in `dir` left by processes that are
/// gone, and removes those markers. `suffix` (e.g. "recovered", translated)
/// names the repaired copy while it is written and checked. Slow (it copies
/// whole files): call it off the UI thread.
pub fn recover_stale(dir: &Path, suffix: &str) -> Vec<(PathBuf, Outcome)> {
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
        let Some((pid, mkv)) = std::fs::read(&marker).ok().and_then(read_marker) else {
            eprintln!("[vesper] unreadable recording marker {}", marker.display());
            let _ = std::fs::remove_file(&marker);
            continue;
        };
        if running(pid) {
            continue;
        }
        let outcome = recover(&mkv, suffix);
        let _ = std::fs::remove_file(&marker);
        out.push((mkv, outcome));
    }
    out
}

/// Whether a `.<pid>-<n>.tmp` belongs to a running Vesper.
fn running_tmp(name: &[u8]) -> bool {
    let pid = name[1..].split(|&b| b == b'-').next().unwrap_or_default();
    std::str::from_utf8(pid)
        .ok()
        .and_then(|p| p.parse().ok())
        .is_some_and(running)
}

/// Recovers one file. The original is only replaced by a copy that has
/// been read back with cues and a duration, and synced to the disk.
pub fn recover(mkv: &Path, suffix: &str) -> Outcome {
    if !mkv.exists() {
        return Outcome::Missing;
    }
    if finalized(mkv).unwrap_or(false) {
        return Outcome::Complete;
    }
    let Some(copy) = free_name(mkv, suffix) else {
        return Outcome::Failed(Error("no free name for the recovered copy".into()));
    };
    let result = remux(mkv, &copy)
        .and_then(|()| match finalized(&copy) {
            Ok(true) => Ok(()),
            Ok(false) => Err(Error("the recovered copy has no cues or duration".into())),
            Err(e) => Err(e),
        })
        .and_then(|()| {
            let io = |e: std::io::Error| Error(format!("replace the original: {e}"));
            File::open(&copy).and_then(|f| f.sync_all()).map_err(io)?;
            std::fs::rename(&copy, mkv).map_err(io)?;
            if let Some(dir) = mkv.parent() {
                File::open(dir).and_then(|d| d.sync_all()).map_err(io)?;
            }
            Ok(())
        });
    match result {
        Ok(()) => Outcome::Recovered,
        Err(e) => {
            // Only our own copy goes: the original stays as it was.
            let _ = std::fs::remove_file(&copy);
            Outcome::Failed(e)
        }
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
        .find(|p| !p.exists())
}

/// Whether `path` has a duration and cues, as a finished file has.
pub fn finalized(path: &Path) -> Result<bool, Error> {
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error("path contains a NUL byte".into()))?;
    // Header only: no packets read, so the duration is the file's own, not
    // an estimate. (libavformat defers the cues until a seek, and a seek
    // indexes a file without cues too: they are looked up below.)
    let duration = unsafe {
        let mut ctx = ptr::null_mut();
        crate::check(
            ff::avformat_open_input(&mut ctx, cpath.as_ptr(), ptr::null(), ptr::null_mut()),
            "open the recording",
        )?;
        let duration = (*ctx).duration;
        ff::avformat_close_input(&mut ctx);
        duration
    };
    let io = |e: std::io::Error| Error(format!("read the recording: {e}"));
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
                        return Some(segment as u64 + pos);
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

/// Copies every packet of every track of `src` into a new MKV `dst`, with
/// the tracks' parameters (Colour included), names and dispositions. A
/// truncated or damaged tail ends the copy; damaged packets are left out.
fn remux(src: &Path, dst: &Path) -> Result<(), Error> {
    use ffmpeg_next::{codec, encoder, format};
    let err = |what: &'static str| move |e: ffmpeg_next::Error| Error(format!("{what}: {e}"));
    let mut input = format::input(src).map_err(err("open the recording"))?;
    let mut output = format::output_as(dst, "matroska").map_err(err("create the copy"))?;
    let mut time_bases = Vec::new();
    for ist in input.streams() {
        let mut ost = output
            .add_stream(encoder::find(codec::Id::None))
            .map_err(err("add a track"))?;
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
    output.write_header().map_err(err("write the header"))?;
    let mut packet = ffmpeg_next::Packet::empty();
    let (mut written, mut errors) = (0u64, 0u32);
    loop {
        match packet.read(&mut input) {
            Ok(()) => errors = 0,
            Err(ffmpeg_next::Error::Eof) => break,
            // The demuxer resyncs to the next cluster after damage (e.g.
            // zeroed pages after a power cut); a cut-off tail ends here.
            Err(_) => {
                errors += 1;
                if errors > 100 {
                    break;
                }
                continue;
            }
        }
        let index = packet.stream();
        if packet.is_corrupt() || index >= time_bases.len() || packet.pts().is_none() {
            continue;
        }
        if packet.dts().is_none() {
            packet.set_dts(packet.pts()); // no B-frames (vesper-core contract)
        }
        let tb = output.stream(index).map(|s| s.time_base());
        packet.rescale_ts(time_bases[index], tb.unwrap_or(time_bases[index]));
        packet.set_position(-1);
        // A packet the muxer refuses (e.g. timestamp going back) is left out.
        if packet.write_interleaved(&mut output).is_ok() {
            written += 1;
        }
    }
    if written == 0 {
        return Err(Error("no packet could be read".into()));
    }
    output
        .write_trailer()
        .map_err(err("write cues and duration"))
}

#[cfg(test)]
mod tests;
