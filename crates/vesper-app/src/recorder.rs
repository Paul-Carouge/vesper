//! The capture pipeline: screen capture -> constant-frame-rate VAAPI encode ->
//! a recording (MKV) and/or the replay buffer (encoded packets in RAM).
//!
//! One [`Pipeline`] runs while a recording or the replay buffer needs it, and
//! ends on its own once neither does. Both share one capture and one encode.
//!
//! Threads:
//! - PipeWire thread (`vesper-capture`): `on_frame` only parks the newest
//!   frame in a one-slot mailbox. A frame it replaces is released at once:
//!   the compositor may send more frames than the recording's `fps`.
//! - Pipeline thread (here): opens the capture (blocks on the portal picker),
//!   then ticks on an absolute `CLOCK_MONOTONIC` grid `start + k / fps`. Each
//!   tick encodes the parked frame, or repeats the last picture when the
//!   compositor sent none (Wayland sends frames on damage only), so the file
//!   has exactly `fps` frames per second. Packets go straight to the sinks.
//! - Audio threads (`vesper-audio`, two per source): Opus packets of the
//!   desktop and microphone tracks go to the same sinks, behind a mutex.
//! - Replay threads: each saved replay is muxed on its own thread from a
//!   snapshot of the ring (shared packet data, no copy).

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use vesper_audio::{AudioCapture, CHANNELS, SAMPLE_RATE, Source};
use vesper_capture::Capture;
use vesper_core::{Codec, EncodedPacket, VideoFrame};
use vesper_encode::{Encoder, RateControl};
use vesper_mux::{AudioTrack, Muxer, VideoTrack};

/// How often `Event::Stats` is sent.
const STATS_PERIOD: Duration = Duration::from_millis(500);

/// Audio kept before the replay's first video packet, so the muxer can pick
/// the packet whose pre-skip covers the start.
const AUDIO_MARGIN: Duration = Duration::from_millis(100);

/// Encoder settings, fixed for the life of a pipeline.
#[derive(Debug, Clone)]
pub struct Settings {
    pub codec: Codec,
    pub fps: u32,
    /// Target bitrate per pixel per frame; scaled by the captured size and `fps`.
    pub bits_per_pixel: f64,
    pub desktop_muted: bool,
    pub mic_muted: bool,
}

#[derive(Debug)]
pub enum Event {
    /// A recording started: its file exists.
    Started,
    /// Sent while recording.
    Stats {
        width: u32,
        height: u32,
        /// Recorded media time.
        elapsed: Duration,
        /// Frames encoded per wall-clock second over the last period.
        fps: f32,
        /// Ticks skipped because encoding fell more than a frame behind.
        dropped: u64,
    },
    /// The recording ended. On a mid-recording error the file is still
    /// finalized up to that point.
    Finished(Result<PathBuf, String>),
    ReplaySaved(Result<PathBuf, String>),
    /// The pipeline ended, with the error that ended it if any. A recording
    /// still running then is lost only if the pipeline crashed.
    Stopped(Option<String>),
}

/// Audio captures and mute state, shared with the UI thread. One lock, so a
/// mute toggled while the picker is open applies once the captures start.
#[derive(Default)]
struct Audio {
    running: Vec<(Source, AudioCapture)>,
    muted: Vec<Source>,
}

type AudioSources = Arc<Mutex<Audio>>;

/// Locks, ignoring poisoning: a panicked holder must not take the UI down.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Requests from the UI, read by the pipeline thread on every tick.
#[derive(Default)]
struct Control {
    /// Folder of a recording to start.
    record: Option<PathBuf>,
    stop_recording: bool,
    /// Length of the replay buffer; `None` drops it.
    replay: Option<Duration>,
    /// Folder to save the replay buffer to.
    save: Option<PathBuf>,
    quit: bool,
    /// Set once the pipeline thread takes no more requests.
    ended: bool,
}

type OnEvent = Arc<dyn Fn(Event) + Send + Sync>;

pub struct Pipeline {
    control: Arc<Mutex<Control>>,
    /// Set once the capture runs: a recording may exist from then on.
    started: Arc<AtomicBool>,
    audio: AudioSources,
    thread: JoinHandle<()>,
}

impl Pipeline {
    /// Starts a pipeline on a new thread, with the replay buffer on if
    /// `replay` is set and a recording into `record` if set. `on_event` runs
    /// on the pipeline thread and on replay threads.
    pub fn start(
        settings: Settings,
        replay: Option<Duration>,
        record: Option<PathBuf>,
        on_event: impl Fn(Event) + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let control = Arc::new(Mutex::new(Control {
            record,
            replay,
            ..Control::default()
        }));
        let audio = AudioSources::default();
        for (source, muted) in [
            (Source::Desktop, settings.desktop_muted),
            (Source::Microphone, settings.mic_muted),
        ] {
            if muted {
                lock(&audio).muted.push(source);
            }
        }
        let started = Arc::new(AtomicBool::new(false));
        let on_event: OnEvent = Arc::new(on_event);
        let (ctl, started_flag, sources) = (control.clone(), started.clone(), audio.clone());
        let thread = std::thread::Builder::new()
            .name("vesper-recorder".into())
            .spawn(move || {
                // A panic must still end in `Stopped`, or the UI stays stuck.
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    run(&settings, &ctl, &started_flag, &sources, &on_event)
                }))
                .unwrap_or_else(|_| Err("Internal error: the recorder crashed".into()));
                lock(&ctl).ended = true;
                // After a panic, the audio captures (mic included) must not keep running.
                // Taken under the lock, dropped outside it (their flush writes to the sinks).
                let running = std::mem::take(&mut lock(&sources).running);
                drop(running);
                on_event(Event::Stopped(result.err()));
            })?;
        Ok(Self {
            control,
            started,
            audio,
            thread,
        })
    }

    /// Applies a request; false if the pipeline already ended (start a new one).
    fn request(&self, f: impl FnOnce(&mut Control)) -> bool {
        let mut c = lock(&self.control);
        if !c.ended {
            f(&mut c);
        }
        !c.ended
    }

    /// Whether the capture started (the portal picker is closed).
    pub fn started(&self) -> bool {
        self.started.load(Ordering::Relaxed)
    }

    /// Starts a recording into `folder` at the next frame, on a keyframe.
    pub fn record(&self, folder: PathBuf) -> bool {
        self.request(|c| c.record = Some(folder))
    }

    /// Asks the recording to end; `Event::Finished` follows within a few
    /// frame intervals.
    pub fn stop_recording(&self) {
        self.request(|c| c.stop_recording = true);
    }

    /// Sets the replay buffer length; `None` drops the buffer (and ends the
    /// pipeline if nothing records).
    pub fn set_replay(&self, span: Option<Duration>) -> bool {
        self.request(|c| c.replay = span)
    }

    /// Saves the replay buffer into `folder`; `Event::ReplaySaved` follows.
    pub fn save_replay(&self, folder: PathBuf) -> bool {
        self.request(|c| c.save = Some(folder))
    }

    /// Mutes a track (it records silence).
    pub fn set_muted(&self, source: Source, muted: bool) {
        let mut audio = lock(&self.audio);
        audio.muted.retain(|s| *s != source);
        if muted {
            audio.muted.push(source);
        }
        for (s, capture) in &audio.running {
            if *s == source {
                capture.set_muted(muted);
            }
        }
    }

    /// Recent peak of a track, linear 0..=1; 0 when it does not run.
    pub fn peak(&self, source: Source) -> f32 {
        let audio = lock(&self.audio);
        audio
            .running
            .iter()
            .find(|(s, _)| *s == source)
            .map_or(0.0, |(_, capture)| capture.peak())
    }

    /// Stops and waits for the recording, if any, to be finalized.
    pub fn join(self) {
        self.request(|c| c.quit = true);
        let _ = self.thread.join();
    }
}

/// Mailbox between the PipeWire thread and the pipeline thread.
#[derive(Default)]
struct Slot {
    frame: Option<VideoFrame>,
    ended: Option<String>,
}

/// An open recording.
struct Recording {
    mux: Muxer,
    path: PathBuf,
    /// Pts of its first (forced key) frame.
    start: Duration,
    /// Set by a stop request: packets from this pts on end the file.
    stop_at: Option<Duration>,
}

impl Recording {
    fn finish(self, error: Option<String>) -> Result<PathBuf, String> {
        let finished = self.mux.finish().map_err(|e| e.to_string());
        match error.or(finished.err()) {
            None => Ok(self.path),
            Some(e) => Err(format!("{e} (saved up to there: {})", self.path.display())),
        }
    }
}

/// Consumers of the encoded packets, shared with the audio threads.
#[derive(Default)]
struct Sinks {
    rec: Option<Recording>,
    ring: Option<Ring>,
}

type SharedSinks = Arc<Mutex<Sinks>>;

/// A recording that ended, with its error if any, to finish outside the lock.
type Ended = (Recording, Option<String>);

impl Sinks {
    fn write_video(&mut self, packets: Vec<EncodedPacket>) -> Option<Ended> {
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

/// The replay buffer: the last `span` of encoded packets, starting at a video
/// keyframe. Memory is about (`span` + one GOP) times the bitrate.
struct Ring {
    span: Duration,
    video: VecDeque<EncodedPacket>,
    audio: Vec<VecDeque<EncodedPacket>>,
}

impl Ring {
    fn new(span: Duration, tracks: usize) -> Self {
        Self {
            span,
            video: VecDeque::new(),
            audio: vec![VecDeque::new(); tracks],
        }
    }

    fn push_video(&mut self, p: EncodedPacket) {
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

    fn push_audio(&mut self, track: usize, p: EncodedPacket) {
        if !self.video.is_empty()
            && let Some(t) = self.audio.get_mut(track)
        {
            t.push_back(p);
        }
    }

    /// All packets as `(stream, packet)`, video being stream 0, in pts order.
    fn snapshot(&self) -> Vec<(usize, EncodedPacket)> {
        let mut all: Vec<_> = self.video.iter().map(|p| (0, p.clone())).collect();
        for (i, track) in self.audio.iter().enumerate() {
            all.extend(track.iter().map(|p| (1 + i, p.clone())));
        }
        all.sort_by_key(|(_, p)| p.pts);
        all
    }
}

/// Starts the desktop and microphone captures. A source that fails to start
/// is left out of the files (e.g. no microphone).
fn start_audio(sources: &AudioSources, sinks: &SharedSinks) -> Vec<AudioTrack> {
    let mut tracks = Vec::new();
    for source in [Source::Desktop, Source::Microphone] {
        let (index, sinks) = (tracks.len(), sinks.clone());
        let capture = AudioCapture::start(source, move |packet| {
            let mut s = lock(&sinks);
            if let Some(r) = s.rec.as_mut()
                && let Err(e) = r.mux.write_audio(index, &packet)
            {
                log!("{source:?} audio: {e}");
            }
            if let Some(ring) = s.ring.as_mut() {
                ring.push_audio(index, packet);
            }
        });
        match capture {
            Ok(capture) => {
                // Started outside the lock (it blocks until PipeWire is
                // ready); mute applied under it, so no toggle is missed.
                let mut audio = lock(sources);
                capture.set_muted(audio.muted.contains(&source));
                tracks.push(AudioTrack {
                    sample_rate: SAMPLE_RATE,
                    channels: u32::from(CHANNELS),
                    extradata: capture.extradata().to_vec(),
                });
                audio.running.push((source, capture));
            }
            Err(e) => log!("running without {source:?} audio: {e}"),
        }
    }
    tracks
}

fn run(
    settings: &Settings,
    control: &Mutex<Control>,
    started: &AtomicBool,
    sources: &AudioSources,
    on_event: &OnEvent,
) -> Result<(), String> {
    let slot = Arc::new(Mutex::new(Slot::default()));
    let s = slot.clone();
    let capture = Capture::start(load_restore_token().as_deref(), move |event| {
        let mut slot = lock(&s);
        let old = match event {
            Ok(frame) => slot.frame.replace(frame),
            Err(e) => {
                slot.ended = Some(e.to_string());
                slot.frame.take()
            }
        };
        drop(slot);
        // Released outside the lock.
        drop(old);
    })
    .map_err(|e| format!("Could not start the capture: {e}"))?;
    if let Some(token) = capture.restore_token()
        && let Err(e) = save_restore_token(token)
    {
        log!("could not save the restore token: {e}");
    }
    let sinks = SharedSinks::default();
    let audio = start_audio(sources, &sinks);
    started.store(true, Ordering::Relaxed);
    let result = pump(settings, &slot, control, &sinks, &audio, on_event);
    drop(capture);
    // Stopping the audio flushes its last packets into the sinks, so do it
    // before closing, without holding the sinks lock the audio threads need.
    let running = std::mem::take(&mut lock(sources).running);
    drop(running);
    let rec = lock(&sinks).rec.take();
    if let Some(r) = rec {
        on_event(Event::Finished(r.finish(result.clone().err())));
    }
    result
}

/// The encoder, opened on the first frame.
struct Video {
    enc: Encoder,
    track: VideoTrack,
    /// Pts of the first frame: the tick grid starts there.
    start: Duration,
}

impl Video {
    fn open(settings: &Settings, first: &VideoFrame) -> Result<Self, String> {
        let (width, height, fps) = (first.width, first.height, settings.fps);
        let bitrate =
            (f64::from(width) * f64::from(height) * f64::from(fps) * settings.bits_per_pixel)
                as u64;
        let enc = Encoder::new(
            settings.codec,
            width,
            height,
            fps,
            RateControl::Bitrate(bitrate),
        )
        .map_err(|e| format!("Could not open the encoder: {e}"))?;
        let track = VideoTrack {
            codec: settings.codec,
            width,
            height,
            fps,
            extradata: enc.extradata().to_vec(),
        };
        log!("encoding {width}x{height} at {bitrate} bit/s");
        // The grid starts now, not at the frame's own pts: opening the encoder
        // takes a while, and a producer clock off by seconds must not stall
        // or burst the ticks. The first frame gets this pts.
        Ok(Self {
            enc,
            track,
            start: monotonic_now(),
        })
    }
}

/// Creates `<folder>/<stem>.mkv` (never overwriting) with its muxer.
fn create_file(
    folder: &Path,
    stem: &str,
    start: Duration,
    video: &VideoTrack,
    audio: &[AudioTrack],
) -> Result<(Muxer, PathBuf), String> {
    std::fs::create_dir_all(folder)
        .map_err(|e| format!("Could not create {}: {e}", folder.display()))?;
    let path = reserve_path(folder, &format!("{stem} {}", local_timestamp()))
        .map_err(|e| format!("Could not create a file in {}: {e}", folder.display()))?;
    let mux = Muxer::create(&path, start, video.clone(), audio).map_err(|e| {
        let _ = std::fs::remove_file(&path);
        format!("Could not create {}: {e}", path.display())
    })?;
    Ok((mux, path))
}

/// Writes a ring snapshot to a new file. A failed file is removed.
fn write_replay(
    folder: &Path,
    video: &VideoTrack,
    audio: &[AudioTrack],
    packets: &[(usize, EncodedPacket)],
) -> Result<PathBuf, String> {
    let start = packets
        .iter()
        .find(|(s, _)| *s == 0)
        .ok_or("The replay buffer is empty")?
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
            Err(format!("Could not save the replay: {e}"))
        }
    }
}

/// Saves the ring on a new thread; the pipeline keeps running.
fn save_replay(
    folder: PathBuf,
    video: Option<&Video>,
    sinks: &SharedSinks,
    audio: &[AudioTrack],
    on_event: &OnEvent,
) {
    let snapshot = lock(sinks).ring.as_ref().map(Ring::snapshot);
    let (Some(video), Some(packets)) = (video, snapshot) else {
        on_event(Event::ReplaySaved(Err("The replay buffer is empty".into())));
        return;
    };
    let (track, audio, event) = (video.track.clone(), audio.to_vec(), on_event.clone());
    let spawned = std::thread::Builder::new()
        .name("vesper-replay".into())
        .spawn(move || {
            event(Event::ReplaySaved(write_replay(
                &folder, &track, &audio, &packets,
            )))
        });
    if let Err(e) = spawned {
        on_event(Event::ReplaySaved(Err(format!(
            "Could not save the replay: {e}"
        ))));
    }
}

/// Ticks until neither a recording nor the replay buffer needs the pipeline.
fn pump(
    settings: &Settings,
    slot: &Mutex<Slot>,
    control: &Mutex<Control>,
    sinks: &SharedSinks,
    audio: &[AudioTrack],
    on_event: &OnEvent,
) -> Result<(), String> {
    let fps = settings.fps;
    let mut video: Option<Video> = None;
    let mut k = 0u64;
    let mut skipped = 0;
    // `skipped` when the recording started, and whether Stats were sent since.
    let mut rec_skipped = 0;
    let mut stats_sent = false;
    // Last capture size the encoder could not take, logged once per change.
    let mut bad_size = None;
    let mut deadline = monotonic_now();
    let mut window = (monotonic_now(), 0u32);
    let mut error = None;

    loop {
        sleep_until(deadline);
        let (frame, ended) = {
            let mut s = lock(slot);
            (s.frame.take(), s.ended.take())
        };
        if let Some(e) = ended {
            error = Some(format!("Capture ended: {e}"));
            break;
        }
        let first = video.is_none() && frame.is_some();
        if let (None, Some(f)) = (&video, &frame) {
            match Video::open(settings, f) {
                Ok(v) => video = Some(v),
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }
        // Requests are read under the control lock, so none lands on an
        // ended pipeline unseen.
        let (record, stop, replay, save) = {
            let mut c = lock(control);
            let recording = lock(sinks).rec.is_some();
            if c.quit || !(recording || c.record.is_some() || c.replay.is_some()) {
                c.ended = true;
                break;
            }
            let stop = std::mem::take(&mut c.stop_recording);
            if stop {
                c.record = None;
            }
            // A recording waits for the first frame, and for the previous
            // one to be finished.
            let record = if video.is_some() && !recording {
                c.record.take()
            } else {
                None
            };
            (record, stop, c.replay, c.save.take())
        };
        {
            let mut s = lock(sinks);
            match replay {
                None => s.ring = None,
                Some(span) => {
                    s.ring
                        .get_or_insert_with(|| Ring::new(span, audio.len()))
                        .span = span
                }
            }
        }
        if let Some(folder) = save {
            save_replay(folder, video.as_ref(), sinks, audio, on_event);
        }
        let Some(v) = &mut video else {
            // Waiting for the first frame: poll once per interval.
            deadline += frame_offset(1, fps);
            continue;
        };
        if !first {
            k += 1;
        }
        let pts = v.start + frame_offset(k, fps);
        if let Some(folder) = record {
            match create_file(&folder, "Vesper", pts, &v.track, audio) {
                Ok((mut mux, path)) => {
                    log!("recording to {}", path.display());
                    v.enc.force_keyframe();
                    let mut s = lock(sinks);
                    // Desktop audio is stamped ahead of real time (playback
                    // delay): its first packets already wait in the ring.
                    for (i, track) in s.ring.iter().flat_map(|r| r.audio.iter().enumerate()) {
                        for p in track.iter().filter(|p| p.pts + AUDIO_MARGIN >= pts) {
                            if let Err(e) = mux.write_audio(i, p) {
                                log!("audio: {e}");
                            }
                        }
                    }
                    s.rec = Some(Recording {
                        mux,
                        path,
                        start: pts,
                        stop_at: None,
                    });
                    drop(s);
                    (rec_skipped, stats_sent) = (skipped, false);
                    on_event(Event::Started);
                }
                Err(e) => on_event(Event::Finished(Err(e))),
            }
        }
        if stop && let Some(r) = lock(sinks).rec.as_mut() {
            r.stop_at.get_or_insert(pts);
        }

        // shortcut: a resized source (e.g. a resized window) keeps the
        // last picture; scale or restart the encoder to follow it.
        let frame = frame.filter(|f| {
            let size = (f.width, f.height);
            let fits = size == (v.track.width, v.track.height);
            if !fits && bad_size != Some(size) {
                log!(
                    "source is now {}x{}: holding the last picture",
                    size.0,
                    size.1
                );
            }
            bad_size = (!fits).then_some(size);
            fits
        });
        let packets = match frame {
            Some(mut frame) => {
                frame.pts = pts;
                v.enc.encode(frame)
            }
            None => v.enc.repeat(pts),
        };
        let packets = match packets {
            Ok(p) => p,
            Err(e) => {
                error = Some(format!("Encoding failed: {e}"));
                break;
            }
        };
        let ended = lock(sinks).write_video(packets);
        if let Some((r, e)) = ended {
            on_event(Event::Finished(r.finish(e)));
        }

        deadline = v.start + frame_offset(k + 1, fps);
        let now = monotonic_now();
        if now >= deadline + frame_offset(1, fps) {
            // More than a frame behind (GPU too busy): skip the missed ticks,
            // so video keeps its place against the audio, and count them.
            let behind = (now - v.start).as_nanos() * u128::from(fps) / 1_000_000_000;
            let behind = (behind as u64).saturating_sub(k + 1);
            k += behind;
            skipped += behind;
            deadline = v.start + frame_offset(k + 1, fps);
        }

        window.1 += 1;
        let span = now.saturating_sub(window.0);
        let rec_start = lock(sinks).rec.as_ref().map(|r| r.start);
        if let Some(start) = rec_start
            && (!stats_sent || span >= STATS_PERIOD)
        {
            on_event(Event::Stats {
                width: v.track.width,
                height: v.track.height,
                elapsed: pts.saturating_sub(start),
                fps: if stats_sent {
                    window.1 as f32 / span.as_secs_f32()
                } else {
                    fps as f32
                },
                dropped: skipped - rec_skipped,
            });
            stats_sent = true;
        }
        if span >= STATS_PERIOD {
            window = (now, 0);
        }
    }

    // Error breaks too: later requests must start a new pipeline.
    lock(control).ended = true;
    if let Some(v) = &mut video {
        match v.enc.flush() {
            Ok(packets) => {
                let ended = lock(sinks).write_video(packets);
                if let Some((r, e)) = ended {
                    on_event(Event::Finished(r.finish(e)));
                }
            }
            Err(e) => {
                error.get_or_insert(e.to_string());
            }
        }
    }
    match error {
        Some(e) => Err(e),
        None if video.is_none() && lock(control).record.is_some() => {
            Err("No frame was captured".into())
        }
        None => Ok(()),
    }
}

/// Creates `<folder>/<stem>.mkv`, or `<stem> (2).mkv`, `(3)`... if taken, so
/// two recordings in the same second (or another instance) never overwrite
/// each other. The muxer then reopens the reserved, empty file.
fn reserve_path(folder: &Path, stem: &str) -> std::io::Result<PathBuf> {
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

/// Offset of frame `k` from the start: exact multiples of `1 / fps`.
fn frame_offset(k: u64, fps: u32) -> Duration {
    Duration::from_nanos(k * 1_000_000_000 / u64::from(fps))
}

fn monotonic_now() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Sleeps until the `CLOCK_MONOTONIC` instant `t` (returns at once if past).
fn sleep_until(t: Duration) {
    let ts = libc::timespec {
        tv_sec: t.as_secs() as libc::time_t,
        tv_nsec: t.subsec_nanos() as libc::c_long,
    };
    while unsafe {
        libc::clock_nanosleep(
            libc::CLOCK_MONOTONIC,
            libc::TIMER_ABSTIME,
            &ts,
            std::ptr::null_mut(),
        )
    } == libc::EINTR
    {}
}

/// Local time as `2026-10-15 14-03-22` (no `:`, file-name safe everywhere).
fn local_timestamp() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!(
            "{:04}-{:02}-{:02} {:02}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

/// `$XDG_CONFIG_HOME/vesper/restore-token`: the portal token of the last
/// picked source, as plain text.
fn token_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("vesper").join("restore-token"))
}

/// The saved restore token; `VESPER_RESTORE_TOKEN` overrides it (testing).
pub fn load_restore_token() -> Option<String> {
    std::env::var("VESPER_RESTORE_TOKEN")
        .ok()
        .or_else(|| std::fs::read_to_string(token_path()?).ok())
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
}

fn save_restore_token(token: &str) -> std::io::Result<()> {
    let path = token_path().ok_or(std::io::ErrorKind::NotFound)?;
    std::fs::create_dir_all(path.parent().unwrap_or(Path::new("/")))?;
    // Private: the token lets anyone running as this user record the source.
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // `mode` only applies on creation; fix older files too.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    std::io::Write::write_all(&mut file, token.as_bytes())
}

/// Forgets the source so the next recording opens the portal picker.
pub fn clear_restore_token() {
    if let Some(path) = token_path()
        && let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log!("could not clear the restore token: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_offsets_are_exact_multiples_of_the_interval() {
        assert_eq!(frame_offset(0, 60), Duration::ZERO);
        assert_eq!(frame_offset(60, 60), Duration::from_secs(1));
        assert_eq!(frame_offset(3600 * 144, 144), Duration::from_secs(3600));
        // No drift from rounding a 16.67 ms interval.
        assert_eq!(frame_offset(1, 60).as_nanos(), 16_666_666);
        assert_eq!(frame_offset(2, 60).as_nanos(), 33_333_333);
    }

    #[test]
    fn sleep_until_waits_on_the_monotonic_clock() {
        let t = monotonic_now() + Duration::from_millis(20);
        sleep_until(t);
        assert!(monotonic_now() >= t);
        sleep_until(Duration::ZERO); // past: returns at once
    }

    #[test]
    fn reserve_path_never_reuses_a_file() {
        let dir = std::env::temp_dir().join(format!("vesper-reserve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = reserve_path(&dir, "Vesper t").unwrap();
        let b = reserve_path(&dir, "Vesper t").unwrap();
        let c = reserve_path(&dir, "Vesper t").unwrap();
        assert_eq!(a, dir.join("Vesper t.mkv"));
        assert_eq!(b, dir.join("Vesper t (2).mkv"));
        assert_eq!(c, dir.join("Vesper t (3).mkv"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn packet(ms: u64, keyframe: bool) -> EncodedPacket {
        EncodedPacket {
            data: Arc::from(&[0u8][..]),
            pts: Duration::from_millis(ms),
            keyframe,
        }
    }

    /// Feeds 100 ms video frames with a keyframe every second, and an audio
    /// packet per frame, up to `end_ms`.
    fn filled(span_s: u64, end_ms: u64) -> Ring {
        let mut ring = Ring::new(Duration::from_secs(span_s), 1);
        for ms in (500..=end_ms).step_by(100) {
            ring.push_audio(0, packet(ms, false));
            ring.push_video(packet(ms, ms % 1000 == 0));
        }
        ring
    }

    #[test]
    fn ring_starts_at_a_keyframe_even_before_the_first_one() {
        let ring = filled(10, 1500);
        // 500..900 came before any keyframe, and audio with them.
        assert_eq!(ring.video[0].pts, Duration::from_millis(1000));
        assert!(ring.video[0].keyframe);
        assert_eq!(ring.audio[0][0].pts, Duration::from_millis(1100));
    }

    #[test]
    fn ring_keeps_the_last_keyframe_before_the_span() {
        // Newest 10_300, span 3 s: cutoff 7_300, last keyframe by then 7_000.
        let ring = filled(3, 10_300);
        assert_eq!(ring.video[0].pts, Duration::from_millis(7000));
        assert!(ring.video[0].keyframe);
        assert_eq!(ring.video.len(), 34);
        // Audio from AUDIO_MARGIN before the video on.
        assert_eq!(ring.audio[0][0].pts, Duration::from_millis(6900));
    }

    #[test]
    fn ring_shrinks_when_the_span_does() {
        let mut ring = filled(10, 10_300);
        assert_eq!(ring.video[0].pts, Duration::from_millis(1000));
        ring.span = Duration::from_secs(2);
        ring.push_video(packet(10_400, false));
        assert_eq!(ring.video[0].pts, Duration::from_millis(8000));
    }

    #[test]
    fn ring_snapshot_is_in_pts_order() {
        let ring = filled(3, 4000);
        let snap = ring.snapshot();
        assert!(snap.windows(2).all(|w| w[0].1.pts <= w[1].1.pts));
        assert_eq!(
            snap.iter().filter(|(s, _)| *s == 0).count(),
            ring.video.len()
        );
        assert_eq!(
            snap.iter().filter(|(s, _)| *s == 1).count(),
            ring.audio[0].len()
        );
    }

    #[test]
    fn timestamp_is_file_name_safe() {
        let ts = local_timestamp();
        assert_eq!(ts.len(), 19, "{ts}");
        assert!(!ts.contains(':') && !ts.contains('/'), "{ts}");
    }
}
