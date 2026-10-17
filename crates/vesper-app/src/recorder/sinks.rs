//! Where frames and packets go: the capture mailbox and the recording file.

use super::*;

/// Mailbox between the capture thread (PipeWire or game) and the pipeline
/// thread.
#[derive(Debug, Default)]
pub(super) struct Mailbox {
    pub(super) slot: Mutex<Slot>,
    /// Signalled on a frame, the capture's end, or a request.
    wake: Condvar,
}

#[derive(Debug, Default)]
pub(super) struct Slot {
    frame: Option<VideoFrame>,
    ended: Option<String>,
    /// A request came in: [`Mailbox::wait`] returns.
    pub(super) poked: bool,
}

impl Mailbox {
    /// Parks the newest frame; the one it replaces is released outside the lock.
    pub(super) fn park(&self, event: Result<VideoFrame, String>) {
        let mut s = lock(&self.slot);
        let old = match event {
            Ok(frame) => s.frame.replace(frame),
            Err(e) => {
                s.ended = Some(e);
                s.frame.take()
            }
        };
        drop(s);
        self.wake.notify_one();
        drop(old);
    }

    /// The parked frame and the capture's end, if any.
    pub(super) fn take(&self) -> (Option<VideoFrame>, Option<String>) {
        let mut s = lock(&self.slot);
        (s.frame.take(), s.ended.take())
    }

    /// Wakes [`Mailbox::wait`]: a request needs the pipeline thread.
    pub(super) fn poke(&self) {
        lock(&self.slot).poked = true;
        self.wake.notify_one();
    }

    /// Sleeps until a frame, the capture's end, a request, or `timeout`.
    pub(super) fn wait(&self, timeout: Option<Duration>) {
        let idle = |s: &mut Slot| s.frame.is_none() && s.ended.is_none() && !s.poked;
        let s = lock(&self.slot);
        let mut s = match timeout {
            None => self
                .wake
                .wait_while(s, idle)
                .unwrap_or_else(PoisonError::into_inner),
            Some(t) => {
                let waited = self.wake.wait_timeout_while(s, t, idle);
                waited.unwrap_or_else(PoisonError::into_inner).0
            }
        };
        s.poked = false;
    }
}

/// An open recording.
pub(super) struct Recording {
    pub(super) mux: Muxer,
    pub(super) path: PathBuf,
    /// Pts of its first (forced key) frame.
    pub(super) start: Duration,
    /// Set by a stop request: packets from this pts on end the file.
    pub(super) stop_at: Option<Duration>,
}

impl Recording {
    pub(super) fn finish(self, error: Option<String>) -> Result<PathBuf, String> {
        let finished = self.mux.finish();
        if finished.as_ref().is_err_and(vesper_mux::Error::is_deleted) {
            return Err(tr!("The recording file was deleted while recording").into());
        }
        match error.or(finished.err().map(|e| e.to_string())) {
            None => Ok(self.path),
            // Kept in English: matched by `friendly()`.
            Some(e) => Err(format!("{e} (saved up to there: {})", self.path.display())),
        }
    }
}

/// Consumers of the encoded packets, shared with the audio threads.
#[derive(Default)]
pub(super) struct Sinks {
    pub(super) rec: Option<Recording>,
    pub(super) ring: Option<Ring>,
}

pub(super) type SharedSinks = Arc<Mutex<Sinks>>;

/// A recording that ended, with its error if any, to finish outside the lock.
pub(super) type Ended = (Recording, Option<String>);

impl Sinks {
    pub(super) fn write_video(&mut self, packets: Vec<EncodedPacket>) -> Option<Ended> {
        let mut ended = None;
        for p in packets {
            if let Some(r) = &mut self.rec {
                let result = if r.stop_at.is_some_and(|t| p.pts >= t) {
                    Err(None)
                } else {
                    r.mux.write_video(&p).map_err(|e| Some(e.to_string()))
                };
                if let Err(e) = result {
                    ended = self.rec.take().map(|r| (r, e));
                }
            }
            if let Some(ring) = &mut self.ring {
                ring.push_video(p);
            }
        }
        ended
    }
}

/// Creates `<folder>/<stem>.mkv` (never overwriting) with its muxer.
pub(super) fn create_file(
    folder: &Path,
    stem: &str,
    start: Duration,
    video: &VideoTrack,
    audio: &[AudioTrack],
) -> Result<(Muxer, PathBuf), String> {
    std::fs::create_dir_all(folder)
        // Kept in English: matched by `friendly()`.
        .map_err(|e| format!("Could not create {}: {e}", folder.display()))?;
    let path = reserve_path(folder, &format!("{stem} {}", local_timestamp()))
        // Kept in English: matched by `friendly()`.
        .map_err(|e| format!("Could not create a file in {}: {e}", folder.display()))?;
    let mux = Muxer::create(&path, start, video.clone(), audio).map_err(|e| {
        let _ = std::fs::remove_file(&path);
        // Kept in English: matched by `friendly()`.
        format!("Could not create {}: {e}", path.display())
    })?;
    Ok((mux, path))
}

/// Creates `<folder>/<stem>.mkv`, or `<stem> (2).mkv`, `(3)`... if taken, so
/// two recordings in the same second (or another instance) never overwrite
/// each other. The muxer then reopens the reserved, empty file.
pub(super) fn reserve_path(folder: &Path, stem: &str) -> std::io::Result<PathBuf> {
    for n in 1..1000 {
        let name = match n {
            1 => format!("{stem}.mkv"),
            n => format!("{stem} ({n}).mkv"),
        };
        let path = folder.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(ErrorKind::AlreadyExists.into())
}
