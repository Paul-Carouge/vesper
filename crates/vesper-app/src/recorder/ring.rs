//! The replay buffer and saving it.

use super::*;

/// The replay buffer: the last `span` of encoded packets, starting at a video
/// keyframe. Memory is about (`span` + one GOP) times the bitrate.
pub(super) struct Ring {
    pub(super) span: Duration,
    pub(super) video: VecDeque<EncodedPacket>,
    pub(super) audio: Vec<VecDeque<EncodedPacket>>,
}

impl Ring {
    pub(super) fn new(span: Duration, tracks: usize) -> Self {
        Self {
            span,
            video: VecDeque::new(),
            audio: vec![VecDeque::new(); tracks],
        }
    }

    pub(super) fn push_video(&mut self, p: EncodedPacket) {
        // Back after a gap longer than the span (the game was gone): a replay
        // must not join the old packets to the new ones across that hole.
        if self
            .video
            .back()
            .is_some_and(|last| p.pts.saturating_sub(last.pts) > self.span)
        {
            *self = Ring::new(self.span, self.audio.len());
        }
        // A replay must start decodable.
        if self.video.is_empty() && !p.keyframe {
            return;
        }
        let cutoff = p.pts.saturating_sub(self.span);
        self.video.push_back(p);
        // Drop the oldest GOP while the next one still starts by the cutoff.
        while let Some(next) = self.video.iter().skip(1).position(|p| p.keyframe)
            && self.video[next + 1].pts <= cutoff
        {
            self.video.drain(..=next);
        }
        let start = self.video[0].pts;
        for track in &mut self.audio {
            while track.front().is_some_and(|p| p.pts + AUDIO_MARGIN < start) {
                track.pop_front();
            }
        }
    }

    pub(super) fn push_audio(&mut self, track: usize, p: EncodedPacket) {
        // Bounded while no video comes (the game is gone): only the
        // audio that goes with the video kept.
        let latest = self.video.back().map(|v| v.pts + AUDIO_LEAD);
        if latest.is_some_and(|t| p.pts <= t)
            && let Some(t) = self.audio.get_mut(track)
        {
            t.push_back(p);
        }
    }

    /// A copy of the packets (shared data): cheap enough under the lock.
    pub(super) fn snapshot(&self) -> Snapshot {
        (self.video.clone(), self.audio.clone())
    }
}

pub(super) type Snapshot = (VecDeque<EncodedPacket>, Vec<VecDeque<EncodedPacket>>);

/// All packets as `(stream, packet)`, video being stream 0, in pts order.
pub(super) fn interleave((video, audio): Snapshot) -> Vec<(usize, EncodedPacket)> {
    let mut all: Vec<_> = video.into_iter().map(|p| (0, p)).collect();
    for (i, track) in audio.into_iter().enumerate() {
        all.extend(track.into_iter().map(|p| (1 + i, p)));
    }
    all.sort_by_key(|(_, p)| p.pts);
    all
}

/// Writes a ring snapshot to a new file. A failed file is removed.
pub(super) fn write_replay(
    folder: &Path,
    video: &VideoTrack,
    audio: &[AudioTrack],
    snapshot: Snapshot,
) -> Result<PathBuf, String> {
    let packets = interleave(snapshot);
    let start = packets
        .iter()
        .find(|(s, _)| *s == 0)
        .ok_or(tr!("The replay buffer is empty"))?
        .1
        .pts;
    let (mut mux, path) = create_file(folder, "Vesper Replay", start, video, audio)?;
    let written = packets.iter().try_for_each(|(s, p)| match s {
        0 => mux.write_video(p),
        n => mux.write_audio(n - 1, p),
    });
    match written.and_then(|()| mux.finish()) {
        Ok(()) => Ok(path),
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            Err(tr!("Could not save the replay: {}", e))
        }
    }
}

/// Saves the ring on a new thread; the pipeline keeps running.
pub(super) fn save_replay(
    folder: PathBuf,
    video: Option<&Video>,
    sinks: &SharedSinks,
    control: &Mutex<Control>,
    sources: &AudioSources,
    on_event: &OnEvent,
) {
    let audio = lock(sources).file_tracks();
    let snapshot = lock(sinks).ring.as_ref().map(Ring::snapshot);
    let (Some(video), Some(snapshot)) = (video, snapshot) else {
        on_event(Event::ReplaySaved(Err(
            tr!("The replay buffer is empty").into()
        )));
        return;
    };
    let (track, event) = (video.track.clone(), on_event.clone());
    let spawned = std::thread::Builder::new()
        .name("vesper-replay".into())
        .spawn(move || {
            event(Event::ReplaySaved(write_replay(
                &folder, &track, &audio, snapshot,
            )))
        });
    match spawned {
        Ok(save) => {
            let mut c = lock(control);
            c.saves.retain(|s| !s.is_finished());
            c.saves.push(save);
        }
        Err(e) => on_event(Event::ReplaySaved(Err(tr!(
            "Could not save the replay: {}",
            e
        )))),
    }
}
