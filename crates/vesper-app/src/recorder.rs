//! The capture pipeline: screen or game capture -> constant-frame-rate VAAPI encode ->
//! a recording (MKV) and/or the replay buffer (encoded packets in RAM).
//!
//! One [`Pipeline`] runs while a recording or the replay buffer needs it, and
//! ends on its own once neither does. Both share one capture and one encode.
//!
//! Threads:
//! - Capture thread (`vesper-capture` PipeWire, or `vesper-gamecapture`
//!   through [`GameFrames`]): `on_frame` only parks the newest
//!   frame in a one-slot mailbox. A frame it replaces is released at once:
//!   the compositor may send more frames than the recording's `fps`.
//! - Pipeline thread (here): opens the capture (blocks on the portal picker),
//!   sleeps until the first frame (or a request), starts the audio, then
//!   ticks on an absolute `CLOCK_MONOTONIC` grid `start + k / fps`. Each
//!   tick encodes the parked frame, or repeats the last picture when the
//!   compositor sent none (Wayland sends frames on damage only), so the file
//!   has exactly `fps` frames per second. Packets go straight to the sinks.
//! - Audio threads (`vesper-audio`, two per track): Opus packets of each
//!   audio track, and of the mix of them all, go to the same sinks, behind a
//!   mutex. The mix is encoded on whichever track thread completes a block.
//! - Replay threads: each saved replay is muxed on its own thread from a
//!   snapshot of the ring (shared packet data, no copy).
//! - Overlay thread (`overlay`), while the active scene is more than the
//!   capture alone (and 10 s after): creates the scene compositor and
//!   stages its images and text. The pipeline thread queues compositions
//!   under `try_lock`, only when the capture or an overlay changed (else the
//!   picture repeats), and waits at most half a frame for one.
//! - Camera thread (`overlay::Cameras`), while the active scene shows a
//!   webcam (and 10 s after): streams them, stages their YUV pictures, and
//!   hands the latest picture of each to the pipeline thread, which gives
//!   it to the compositor (copied, then released, by the next composition).
//! - Live thread (`stream`): sends a live stream's packets. Its video comes
//!   from the pipeline thread (the recording's packets, or a second encode of
//!   the same pictures scaled on the GPU), its AAC audio from the mix's tap.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use vesper_audio::{Aac, AudioCapture, CHANNELS, Mix, Options, SAMPLE_RATE, Source, TrackState};
use vesper_capture::Capture;
use vesper_compose::{Compositor, Draw, Source as DrawSource};
use vesper_core::{Codec, EncodedPacket, VideoFrame};
use vesper_dsp::{FilterSpec, LiveControl};
use vesper_encode::{Encoder, RateControl};
use vesper_gamecapture::{GameCapture, protocol};
use vesper_mux::{AudioTrack, Muxer, VideoTrack};

use crate::overlay::{Cameras, Feed, Key, Overlays};
use crate::scenes::{Canvas, LayerSource, Scene};
use crate::stream::{AudioTap, LiveSink, StreamRequest, StreamState, live_audio, stream_size};

/// How often `Event::Stats` is sent.
const STATS_PERIOD: Duration = Duration::from_millis(500);

/// Audio kept before the replay's first video packet, so the muxer can pick
/// the packet whose pre-skip covers the start.
const AUDIO_MARGIN: Duration = Duration::from_millis(100);

/// Audio kept in the replay buffer past its last video packet (desktop
/// audio is stamped ahead of real time by its playback delay).
const AUDIO_LEAD: Duration = Duration::from_secs(1);

/// A recording request waiting this long for a first frame fails (e.g. the
/// game quit), so the UI does not stay in `starting`.
const FIRST_FRAME_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    Duration::from_secs(5)
};

/// A new source size kept this long reopens the encoder at that size while
/// nothing records (a launcher, then the game in fullscreen).
const RESIZE_AFTER: Duration = Duration::from_secs(1);

/// First wait before the scene compositor is tried again after a failure;
/// doubled on each failure in a row, up to [`RETRY_MAX`].
const RETRY_FIRST: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(2)
};
const RETRY_MAX: Duration = Duration::from_secs(60);

/// Encoder settings, fixed for the life of a pipeline.
#[derive(Debug, Clone)]
pub struct Settings {
    pub codec: Codec,
    pub fps: u32,
    /// Constant-quality target, see [`RateControl::Quality`].
    pub quality: u32,
    /// Bitrate cap per pixel per frame; scaled by the captured size and `fps`.
    pub max_bits_per_pixel: f64,
    /// Audio tracks, disabled ones included (indices match the UI's).
    pub tracks: Vec<Track>,
    /// Put a mix of the tracks first, for players that play one track only.
    pub mix: bool,
    /// Record the game these frames come from instead of a portal source.
    pub game: Option<GameFrames>,
    /// The scene shown first; [`Pipeline::set_scene`] changes it.
    pub scene: Scene,
    /// The recorded size.
    pub canvas: Canvas,
}

/// Routes game capture frames to the running pipeline. The game capture
/// outlives pipelines (the UI lists games and asks for consent while none
/// runs); it only asks the game for frames while a pipeline is attached.
#[derive(Debug, Clone, Default)]
pub struct GameFrames(Arc<GameRoute>);

#[derive(Debug, Default)]
struct GameRoute {
    /// The attached pipeline's slot and recording rate.
    slot: Mutex<Option<(Arc<Mailbox>, u32)>>,
    /// Weak: the capture's frame callback holds this route.
    capture: OnceLock<Weak<GameCapture>>,
    /// The game's window (portal) feeds the route: no layer game needed.
    window: AtomicBool,
    /// The game window's picker is open: a recording waits for it.
    picking: AtomicBool,
    /// Tests: the game is gone, without a capture server.
    #[cfg(test)]
    gone: AtomicBool,
    /// Tests: watches and breaks the scene compositor.
    #[cfg(test)]
    probe: Arc<Probe>,
}

/// Tests: the scene compositor of the pipeline a route feeds.
#[cfg(test)]
#[derive(Debug, Default)]
struct Probe {
    /// Compositors created.
    compositors: AtomicUsize,
    /// The next composition fails.
    fail: AtomicBool,
    /// Keep the capture copy as for a damage-driven source.
    base: AtomicBool,
    /// The compositor looks busy (locked by the overlay thread).
    busy: AtomicBool,
    /// Never wait for a composition on the tick.
    no_wait: AtomicBool,
    /// Producer pts of the newest capture frame composed, and whether an
    /// older one was composed after it.
    last_main: std::sync::atomic::AtomicU64,
    went_back: AtomicBool,
    /// Compositions, and their GPU time (when measured), ns.
    composed: std::sync::atomic::AtomicU64,
    gpu_ns: std::sync::atomic::AtomicU64,
}

impl GameFrames {
    /// Ties the route to its capture, inactive until a pipeline attaches.
    pub fn bind(&self, capture: &Arc<GameCapture>) {
        let _ = self.0.capture.set(Arc::downgrade(capture));
        capture.set_active(self.fps());
    }

    /// Recording rate of the attached pipeline, `None` while none is.
    pub fn fps(&self) -> Option<u32> {
        lock(&self.0.slot).as_ref().map(|(_, fps)| *fps)
    }

    /// Hands a frame (or the capture's final error) to the pipeline.
    pub fn push(&self, event: Result<VideoFrame, String>) {
        let slot = lock(&self.0.slot).as_ref().map(|(s, _)| s.clone());
        match slot {
            Some(slot) => slot.park(event),
            None => drop(event),
        }
    }

    /// Whether no game is captured any more: it disconnected (quit or
    /// crashed), or sent no frame for 3 s without a swapchain (the server
    /// then drops it, though still connected), and none replaced it yet. A
    /// paused game that keeps its swapchain stays captured: not gone.
    fn gone(&self) -> bool {
        // A static window sends no frame: its picture repeats.
        if self.0.window.load(Ordering::Relaxed) {
            return false;
        }
        #[cfg(test)]
        if self.0.gone.load(Ordering::Relaxed) {
            return true;
        }
        let capture = self.0.capture.get().and_then(Weak::upgrade);
        capture.is_some_and(|c| c.captured().is_none())
    }

    /// The game's window feeds the route (see [`crate::game`]).
    pub fn set_window(&self, on: bool) {
        self.0.window.store(on, Ordering::Relaxed);
    }

    /// The game window's picker is open: no first-frame timeout meanwhile.
    pub fn set_picking(&self, on: bool) {
        self.0.picking.store(on, Ordering::Relaxed);
    }

    fn picking(&self) -> bool {
        self.0.picking.load(Ordering::Relaxed)
    }

    /// Never called under the slot lock: dropping the upgraded capture may
    /// join the server thread, which takes that lock in `push`.
    fn set_active(&self, fps: Option<u32>) {
        if let Some(capture) = self.0.capture.get().and_then(Weak::upgrade) {
            capture.set_active(fps);
        }
    }

    fn attach(&self, slot: &Arc<Mailbox>, fps: u32) {
        let old = lock(&self.0.slot).replace((slot.clone(), fps));
        drop(old);
        self.set_active(Some(fps));
    }

    /// Detaches `slot`, unless a newer pipeline replaced it already.
    fn detach(&self, slot: &Arc<Mailbox>) {
        let mut current = lock(&self.0.slot);
        if current.as_ref().is_some_and(|(s, _)| Arc::ptr_eq(s, slot)) {
            *current = None;
            drop(current);
            self.set_active(None);
            // A newer pipeline attached meanwhile: keep it fed.
            if let Some(fps) = self.fps() {
                self.set_active(Some(fps));
            }
        }
    }
}

#[derive(Debug)]
pub enum Event {
    /// The portal capture started on this source.
    Source(crate::outputs::SourceLabel),
    /// A recording started: its file exists.
    Started,
    /// The audio tracks started (on the first frame) or restarted: their
    /// peaks move from now on.
    AudioStarted,
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
    /// The audio track of this name lost its source, or has it back.
    AudioTrack(String, TrackState),
    /// Some audio tracks or the mix could not start: what is left out, for
    /// a notice.
    AudioMissing(String),
    /// Something to tell the user that changes nothing for the recording.
    Notice(String),
    /// A webcam is open (true, its light is on), or none is any more.
    Camera(bool),
    /// The live stream's state changed.
    Stream(StreamState),
    /// Sent every second while the stream is on air.
    StreamStats {
        /// Since the stream first went on air.
        elapsed: Duration,
        /// Bits per second sent over the last second.
        bitrate: u64,
        /// Video frames dropped because the network was too slow.
        dropped: u64,
    },
    /// The pipeline ended, with the error that ended it if any. A recording
    /// still running then is lost only if the pipeline crashed.
    Stopped(Option<String>),
    /// A small copy of the composed picture, while [`PREVIEW`] is set.
    Preview(slint::SharedPixelBuffer<slint::Rgba8Pixel>),
}

/// The scene editor shows the recorded scene: composed pictures come as
/// [`Event::Preview`], [`PREVIEW_PERIOD`] apart. Off the hot path: a
/// 480 x 270 downscale read back only while this is set.
pub static PREVIEW: AtomicBool = AtomicBool::new(false);
const PREVIEW_PERIOD: Duration = Duration::from_millis(100);

/// Most audio tracks a recording has, the mix aside.
pub const MAX_TRACKS: usize = 8;

/// Most filters a track has.
pub const MAX_FILTERS: usize = 16;

/// Name of the mix in the files.
const MIX_NAME: &str = "Mix";

/// One audio track as the user set it up.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub name: String,
    pub source: Source,
    /// Recorded at all.
    pub enabled: bool,
    pub muted: bool,
    /// See `vesper_audio::db_to_gain`.
    pub volume_db: f32,
    /// Processing, in order, before the volume.
    pub filters: Vec<TrackFilter>,
}

/// One filter of a track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackFilter {
    /// Off: left out of the chain, settings kept.
    pub enabled: bool,
    pub spec: FilterSpec,
}

/// The filters that run: the enabled ones, in order.
pub fn active_filters(filters: &[TrackFilter]) -> Vec<FilterSpec> {
    filters
        .iter()
        .filter(|f| f.enabled)
        .map(|f| f.spec)
        .collect()
}

impl Track {
    pub fn new(name: &str, source: Source) -> Self {
        Self {
            name: name.to_owned(),
            source,
            enabled: true,
            muted: false,
            volume_db: 0.0,
            filters: Vec::new(),
        }
    }
}

/// Audio tracks and their captures, shared with the UI thread. One lock, so
/// a mute toggled while the picker is open applies once the captures start.
#[derive(Default)]
struct Audio {
    /// The tracks, kept current by the UI (mute, volume, name).
    tracks: Vec<Track>,
    mix: bool,
    /// Set when `tracks` changed shape: the audio restarts once nothing records.
    changed: bool,
    /// Running captures with their index in `tracks`, in file order.
    running: Vec<(usize, AudioCapture)>,
    /// Filter controls of the running captures, with their index in `tracks`.
    filters: Vec<(usize, LiveControl)>,
    /// Runs whenever a track does: a live stream's audio. First in the
    /// files when `mix_in_files`.
    mixer: Option<Mix>,
    mix_in_files: bool,
    /// The running live stream's audio, fed by the mix.
    stream_tap: Option<Arc<AudioTap>>,
}

impl Audio {
    /// Number of audio streams in a file.
    fn streams(&self) -> usize {
        usize::from(self.mix_in_files) + self.running.len()
    }

    /// Feeds `tap` (or nothing) with the mix, now and after audio restarts.
    fn set_stream_tap(&mut self, tap: Option<Arc<AudioTap>>) {
        if let Some(m) = &self.mixer {
            m.set_tap(tap.as_ref().map(AudioTap::tap));
        }
        self.stream_tap = tap;
    }

    /// Audio stream descriptions for a new file, with the current names.
    fn file_tracks(&self) -> Vec<AudioTrack> {
        let track = |name: &str, extradata: &[u8]| AudioTrack {
            name: name.to_owned(),
            sample_rate: SAMPLE_RATE,
            channels: u32::from(CHANNELS),
            extradata: extradata.to_vec(),
        };
        let mix = self
            .mixer
            .iter()
            .filter(|_| self.mix_in_files)
            .map(|m| track(MIX_NAME, m.extradata()));
        let tracks = self.running.iter().map(|(i, capture)| {
            let name = self.tracks.get(*i).map_or("", |t| t.name.as_str());
            track(name, capture.extradata())
        });
        mix.chain(tracks).collect()
    }

    fn track(&mut self, index: usize) -> (Option<&mut Track>, Option<&AudioCapture>) {
        let capture = self.running.iter().find(|(i, _)| *i == index);
        (self.tracks.get_mut(index), capture.map(|(_, c)| c))
    }
}

type AudioSources = Arc<Mutex<Audio>>;

/// Locks, ignoring poisoning: a panicked holder must not take the UI down.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
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
    /// A live stream to start.
    stream: Option<StreamRequest>,
    stop_stream: bool,
    /// The scene to show from the next tick.
    scene: Option<Scene>,
    quit: bool,
    /// Set once the pipeline thread takes no more requests.
    ended: bool,
    /// Replays being written and live streams ending, joined by `Pipeline::join`.
    saves: Vec<JoinHandle<()>>,
}

pub(crate) type OnEvent = Arc<dyn Fn(Event) + Send + Sync>;

pub struct Pipeline {
    control: Arc<Mutex<Control>>,
    /// Woken on requests while the pipeline waits for its first frame.
    mailbox: Arc<Mailbox>,
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
        stream: Option<StreamRequest>,
        on_event: impl Fn(Event) + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let control = Arc::new(Mutex::new(Control {
            record,
            replay,
            stream,
            ..Control::default()
        }));
        let audio = Arc::new(Mutex::new(Audio {
            tracks: settings.tracks.clone(),
            mix: settings.mix,
            ..Audio::default()
        }));
        let started = Arc::new(AtomicBool::new(false));
        let mailbox = Arc::new(Mailbox::default());
        let on_event: OnEvent = Arc::new(on_event);
        let (ctl, started_flag, sources) = (control.clone(), started.clone(), audio.clone());
        let slot = mailbox.clone();
        let thread = std::thread::Builder::new()
            .name("vesper-recorder".into())
            .spawn(move || {
                // A panic must still end in `Stopped`, or the UI stays stuck.
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    run(&settings, &slot, &ctl, &started_flag, &sources, &on_event)
                }))
                .unwrap_or_else(|_| Err(tr!("Internal error: the recorder crashed").into()));
                lock(&ctl).ended = true;
                // After a panic, the audio captures (mic included) must not keep running.
                stop_audio(&sources);
                on_event(Event::Stopped(result.err()));
            })?;
        Ok(Self {
            control,
            mailbox,
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
        let running = !c.ended;
        drop(c);
        self.mailbox.poke();
        running
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

    /// Goes live at the next frame; `Event::Stream` follows.
    pub fn start_stream(&self, request: StreamRequest) -> bool {
        self.request(|c| {
            c.stream = Some(request);
            c.stop_stream = false;
        })
    }

    /// Ends the live stream; `Event::Stream(Ended)` follows. False if the
    /// pipeline already ended (and the stream with it).
    pub fn stop_stream(&self) -> bool {
        self.request(|c| {
            c.stream = None;
            c.stop_stream = true;
        })
    }

    /// Shows `scene` from the next tick, recording or not. The canvas
    /// stays: the encoder is not reopened.
    pub fn set_scene(&self, scene: Scene) -> bool {
        self.request(|c| c.scene = Some(scene))
    }

    /// Mutes track `index` (it records silence), live.
    pub fn set_muted(&self, index: usize, muted: bool) {
        let mut audio = lock(&self.audio);
        let (track, capture) = audio.track(index);
        if let Some(t) = track {
            t.muted = muted;
        }
        if let Some(c) = capture {
            c.set_muted(muted);
        }
    }

    /// Sets the volume of track `index`, live.
    pub fn set_volume(&self, index: usize, db: f32) {
        let mut audio = lock(&self.audio);
        let (track, capture) = audio.track(index);
        if let Some(t) = track {
            t.volume_db = db;
        }
        if let Some(c) = capture {
            c.set_volume_db(db);
        }
    }

    /// Sets the filters of track `index`, live: parameters glide, a new
    /// structure is handed to the audio thread at its next block.
    pub fn set_filters(&self, index: usize, filters: Vec<TrackFilter>) {
        let mut audio = lock(&self.audio);
        // Once the tracks changed, running indices are stale: the restart
        // applies the filters instead.
        let changed = audio.changed;
        if let Some((_, control)) = audio.filters.iter_mut().find(|(i, _)| *i == index)
            && !changed
        {
            control.set(&active_filters(&filters));
        }
        if let Some(t) = audio.tracks.get_mut(index) {
            t.filters = filters;
        }
    }

    /// Gain reduction of running filter `filter` (among the enabled ones) of
    /// track `index`, dB; 0 when it does not run.
    pub fn reduction_db(&self, index: usize, filter: usize) -> f32 {
        let audio = lock(&self.audio);
        audio
            .filters
            .iter()
            .find(|(i, _)| *i == index)
            .map_or(0.0, |(_, control)| control.reduction_db(filter))
    }

    /// Renames track `index`: files created from now on carry the new name.
    pub fn set_name(&self, index: usize, name: &str) {
        if let Some(t) = lock(&self.audio).tracks.get_mut(index) {
            t.name = name.to_owned();
        }
    }

    /// Replaces the tracks (added, removed, enabled) or the mix setting. The
    /// audio restarts on the next frame, or once the recording ends.
    pub fn set_tracks(&self, tracks: Vec<Track>, mix: bool) {
        let mut audio = lock(&self.audio);
        (audio.tracks, audio.mix, audio.changed) = (tracks, mix, true);
    }

    /// Whether audio tracks run, so their peaks may move.
    pub fn audio_running(&self) -> bool {
        !lock(&self.audio).running.is_empty()
    }

    /// Recent peak of track `index`, linear 0..=1; 0 when it does not run.
    pub fn peak(&self, index: usize) -> f32 {
        let audio = lock(&self.audio);
        // Ticked by the meters: frees a replaced filter chain soon after the
        // audio thread hands it back, not at the next change.
        if let Some((_, control)) = audio.filters.iter().find(|(i, _)| *i == index) {
            control.collect();
        }
        audio
            .running
            .iter()
            .find(|(i, _)| *i == index)
            .map_or(0.0, |(_, capture)| capture.peak())
    }

    /// Stops and waits for the recording, if any, to be finalized.
    /// Also waits for replays being saved, so none is cut short.
    pub fn join(self) {
        self.request(|c| c.quit = true);
        let _ = self.thread.join();
        let saves = std::mem::take(&mut lock(&self.control).saves);
        for save in saves {
            let _ = save.join();
        }
    }
}

/// Mailbox between the capture thread (PipeWire or game) and the pipeline
/// thread.
#[derive(Debug, Default)]
struct Mailbox {
    slot: Mutex<Slot>,
    /// Signalled on a frame, the capture's end, or a request.
    wake: Condvar,
}

#[derive(Debug, Default)]
struct Slot {
    frame: Option<VideoFrame>,
    ended: Option<String>,
    /// A request came in: [`Mailbox::wait`] returns.
    poked: bool,
}

impl Mailbox {
    /// Parks the newest frame; the one it replaces is released outside the lock.
    fn park(&self, event: Result<VideoFrame, String>) {
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
    fn take(&self) -> (Option<VideoFrame>, Option<String>) {
        let mut s = lock(&self.slot);
        (s.frame.take(), s.ended.take())
    }

    /// Wakes [`Mailbox::wait`]: a request needs the pipeline thread.
    fn poke(&self) {
        lock(&self.slot).poked = true;
        self.wake.notify_one();
    }

    /// Sleeps until a frame, the capture's end, a request, or `timeout`.
    fn wait(&self, timeout: Option<Duration>) {
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

    fn push_audio(&mut self, track: usize, p: EncodedPacket) {
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
    fn snapshot(&self) -> Snapshot {
        (self.video.clone(), self.audio.clone())
    }
}

type Snapshot = (VecDeque<EncodedPacket>, Vec<VecDeque<EncodedPacket>>);

/// All packets as `(stream, packet)`, video being stream 0, in pts order.
fn interleave((video, audio): Snapshot) -> Vec<(usize, EncodedPacket)> {
    let mut all: Vec<_> = video.into_iter().map(|p| (0, p)).collect();
    for (i, track) in audio.into_iter().enumerate() {
        all.extend(track.into_iter().map(|p| (1 + i, p)));
    }
    all.sort_by_key(|(_, p)| p.pts);
    all
}

/// Writes an audio packet of file stream `index` (the video aside) to the sinks.
fn write_audio(sinks: &SharedSinks, index: usize, packet: EncodedPacket) {
    let mut s = lock(sinks);
    if let Some(r) = s.rec.as_mut()
        && let Err(e) = r.mux.write_audio(index, &packet)
    {
        log!("audio track {index}: {e}");
    }
    if let Some(ring) = s.ring.as_mut() {
        ring.push_audio(index, packet);
    }
}

/// File stream of a track among the audio streams: after the mix, if any.
struct StreamIndex {
    first: Arc<AtomicUsize>,
    track: usize,
}

impl StreamIndex {
    fn new(first: &Arc<AtomicUsize>, track: usize) -> Self {
        Self {
            first: first.clone(),
            track,
        }
    }

    fn get(&self) -> usize {
        self.first.load(Ordering::Relaxed) + self.track
    }
}

/// Starts the enabled tracks and their mix. A track that fails to start is
/// left out of the files (e.g. no microphone); the mix needs two tracks.
fn start_audio(sources: &AudioSources, sinks: &SharedSinks, on_event: &OnEvent) {
    let (tracks, mix) = {
        let mut audio = lock(sources);
        audio.changed = false;
        (audio.tracks.clone(), audio.mix)
    };
    let enabled: Vec<usize> = (0..tracks.len()).filter(|&i| tracks[i].enabled).collect();
    // One clock for every track, so the mix lines their samples up.
    let anchor = monotonic_now();
    // The mix always runs with the tracks: a live stream sends it. Only
    // encoded to Opus when the files have it.
    let want_mix = mix && enabled.len() >= 2;
    let mut mixer = None;
    let mut mix_failed = false;
    if !enabled.is_empty() {
        let sinks = sinks.clone();
        match Mix::start(anchor, move |packet| write_audio(&sinks, 0, packet)) {
            Ok(m) => {
                m.set_encoding(want_mix);
                mixer = Some(m);
            }
            Err(e) => {
                log!("running without the mix: {e}");
                mix_failed = want_mix;
            }
        }
    }
    let mut in_files = want_mix && mixer.is_some();
    // Every input exists before any track runs.
    let mut inputs: Vec<_> = enabled
        .iter()
        .map(|_| mixer.as_ref().map(Mix::input))
        .collect();
    // Streams before the tracks: the mix, until it is dropped below.
    let first = Arc::new(AtomicUsize::new(usize::from(in_files)));
    let mut running = Vec::new();
    let mut controls = Vec::new();
    let mut failed = Vec::new();
    for (input, &i) in inputs.iter_mut().zip(&enabled) {
        let track = &tracks[i];
        let (index, sinks) = (StreamIndex::new(&first, running.len()), sinks.clone());
        let (mut chain, control) =
            vesper_dsp::live(&active_filters(&track.filters), usize::from(CHANNELS));
        let options = Options {
            anchor,
            muted: track.muted,
            volume_db: track.volume_db,
            mix: input.take(),
            // Always there, even empty: filters can be added while it runs.
            process: Some(Box::new(move |buf| {
                chain.process(buf);
                chain.latency()
            })),
            on_state: Some(Box::new({
                let (on_event, name) = (on_event.clone(), track.name.clone());
                move |state| on_event(Event::AudioTrack(name.clone(), state))
            })),
        };
        match AudioCapture::start(track.source.clone(), options, move |packet| {
            write_audio(&sinks, index.get(), packet)
        }) {
            Ok(capture) => {
                running.push((i, capture));
                controls.push((i, control));
            }
            Err(e) => {
                log!("running without the {:?} track: {e}", track.name);
                failed.push(track.name.as_str());
            }
        }
    }
    // A mix of one track would be a copy of it.
    let dropped = in_files && running.len() < 2;
    if dropped {
        if let Some(m) = &mixer {
            m.set_encoding(false);
        }
        in_files = false;
        // The tracks move up one stream: before any file or replay uses them.
        first.store(0, Ordering::Relaxed);
    }
    if let Some(text) = missing_audio(&failed, mix_failed || dropped) {
        on_event(Event::AudioMissing(text));
    }
    // Started outside the lock (it blocks until PipeWire is ready); the
    // latest mute and volume applied under it, so no change is missed.
    let mut audio = lock(sources);
    for ((i, capture), (_, control)) in running.iter().zip(&mut controls) {
        if let Some(t) = audio.tracks.get(*i) {
            capture.set_muted(t.muted);
            capture.set_volume_db(t.volume_db);
            control.set(&active_filters(&t.filters));
        }
    }
    (audio.running, audio.mixer, audio.filters) = (running, mixer, controls);
    audio.mix_in_files = in_files;
    // A stream running across the restart goes on with the new mix.
    let tap = audio.stream_tap.clone();
    audio.set_stream_tap(tap);
}

/// One notice for the tracks that could not start, and a lost mix.
fn missing_audio(failed: &[&str], no_mix: bool) -> Option<String> {
    match (failed, no_mix) {
        ([], false) => None,
        ([], true) => Some(tr!("Recording without the mix track").to_owned()),
        ([one], _) => Some(tr!("Recording without the audio track {}", one)),
        (many, _) => Some(tr!(
            "Recording without the audio tracks {}",
            many.join(", ")
        )),
    }
}

/// Empties the ring's audio and sizes it for the running streams.
fn reset_ring_audio(sources: &AudioSources, sinks: &SharedSinks) {
    let streams = lock(sources).streams();
    if let Some(ring) = lock(sinks).ring.as_mut() {
        ring.audio = vec![VecDeque::new(); streams];
    }
}

/// Stops the captures, then the mix: their last packets go to the sinks, so
/// never call it holding the sinks lock.
fn stop_audio(sources: &AudioSources) {
    let (running, mixer) = {
        let mut audio = lock(sources);
        audio.filters.clear();
        audio.mix_in_files = false;
        (std::mem::take(&mut audio.running), audio.mixer.take())
    };
    drop(running);
    drop(mixer);
}

fn run(
    settings: &Settings,
    slot: &Arc<Mailbox>,
    control: &Mutex<Control>,
    started: &AtomicBool,
    sources: &AudioSources,
    on_event: &OnEvent,
) -> Result<(), String> {
    let capture = match &settings.game {
        Some(game) => {
            game.attach(slot, settings.fps);
            None
        }
        None => {
            let s = slot.clone();
            // Twice the recording rate, not once: a compositor pacing frames
            // at exactly `fps` with a phase close to our ticks' would leave
            // some ticks without a new frame (a repeat, then a skipped one).
            let max_fps = settings.fps.saturating_mul(2);
            let capture = Capture::start(load_restore_token().as_deref(), max_fps, move |event| {
                s.park(event.map_err(|e| e.to_string()));
            })
            .map_err(|e| tr!("Could not start the capture: {}", e))?;
            if let Some(token) = capture.restore_token()
                && let Err(e) = save_restore_token(token)
            {
                log!("could not save the restore token: {e}");
            }
            on_event(Event::Source(crate::outputs::describe(capture.source())));
            Some(capture)
        }
    };
    let sinks = SharedSinks::default();
    started.store(true, Ordering::Relaxed);
    let result = pump(settings, slot, control, &sinks, sources, on_event);
    if let Some(game) = &settings.game {
        game.detach(slot);
    }
    // Stopping the audio flushes its last packets into the sinks, so do it
    // before closing, without holding the sinks lock the audio threads need.
    stop_audio(sources);
    let rec = lock(&sinks).rec.take();
    if let Some(r) = rec {
        on_event(Event::Finished(r.finish(result.clone().err())));
    }
    // Last: closing the portal session may hang, the file is safe by now.
    drop(capture);
    result
}

/// The encoder, opened on the first frame.
struct Video {
    enc: Encoder,
    track: VideoTrack,
    /// Bitrate cap of the encoder, bits per second.
    bitrate: u64,
    /// Pts of the first frame: the tick grid starts there.
    start: Duration,
}

impl Video {
    fn open(settings: &Settings, width: u32, height: u32) -> Result<Self, String> {
        let fps = settings.fps;
        let bitrate =
            (f64::from(width) * f64::from(height) * f64::from(fps) * settings.max_bits_per_pixel)
                as u64;
        let enc = Encoder::new(
            settings.codec,
            width,
            height,
            fps,
            RateControl::Quality {
                quality: settings.quality,
                max_bitrate: bitrate,
            },
        )
        // Kept in English: matched by `friendly()`.
        .map_err(|e| format!("Could not open the encoder: {e}"))?;
        let track = VideoTrack {
            codec: settings.codec,
            width,
            height,
            fps,
            extradata: enc.extradata().to_vec(),
        };
        log!(
            "encoding {width}x{height} at quality {}, max {bitrate} bit/s",
            settings.quality
        );
        // The grid starts now, not at the frame's own pts: opening the encoder
        // takes a while, and a producer clock off by seconds must not stall
        // or burst the ticks. The first frame gets this pts. Rounded up to
        // the tick grid the game layer paces its copies to.
        Ok(Self {
            enc,
            track,
            bitrate,
            start: next_tick(monotonic_now(), fps),
        })
    }
}

/// The video of a live stream.
enum StreamVideo {
    /// The recording's packets.
    Shared,
    /// The stream's own encoder, being opened on a helper thread (opening
    /// takes tens of milliseconds: never on the tick).
    Opening(std::sync::mpsc::Receiver<Result<Encoder, String>>),
    Own(Encoder),
}

/// A live stream, from the pipeline thread's side.
struct Streaming {
    /// `None` until the stream's encoder is open: its header is needed.
    sink: Option<LiveSink>,
    video: StreamVideo,
    url: String,
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    /// AAC when the mix runs.
    aac: Option<Aac>,
    on_event: OnEvent,
    /// Stream frame number of the last picture sent.
    last: Option<u64>,
    /// The recording encoder has a new picture since the stream's last one.
    fresh: bool,
    /// A keyframe was asked for and has not come out yet.
    key_pending: bool,
    /// Stream frames in a row whose encode took longer than a frame.
    slow: u32,
}

impl Streaming {
    /// Checks the request, starts opening the stream's encoder (unless it
    /// can share the recording's) and opens its audio.
    fn start(
        request: &StreamRequest,
        v: &Video,
        settings: &Settings,
        sources: &AudioSources,
        on_event: &OnEvent,
    ) -> Result<Self, String> {
        let url = request.url()?;
        let s = &request.settings;
        let codec = s.codec();
        let fps = s.fps.clamp(1, settings.fps);
        let (width, height) = stream_size(v.track.width, v.track.height, s.height);
        // Same codec, size and rate, and the recording's cap within the
        // stream's budget: one encode serves both.
        let shared = codec == settings.codec
            && (width, height) == (v.track.width, v.track.height)
            && fps == settings.fps
            && v.bitrate <= s.bitrate;
        let video = if shared {
            StreamVideo::Shared
        } else {
            let source = v
                .enc
                .scaled_source()
                .map_err(|e| tr!("Could not open the stream encoder: {}", e))?;
            let (tx, rx) = std::sync::mpsc::channel();
            let rate = RateControl::ConstantBitrate(s.bitrate);
            std::thread::Builder::new()
                .name("vesper-live-open".into())
                .spawn(move || {
                    let opened = source
                        .open(codec, width, height, fps, rate)
                        .map_err(|e| tr!("Could not open the stream encoder: {}", e));
                    let _ = tx.send(opened);
                })
                .map_err(|e| tr!("Could not open the stream encoder: {}", e))?;
            StreamVideo::Opening(rx)
        };
        // The mix runs whenever a track does: without it, a silent stream.
        let aac = if lock(sources).mixer.is_some() {
            Some(Aac::new().map_err(|e| tr!("Could not open the AAC encoder: {}", e))?)
        } else {
            on_event(Event::AudioMissing(
                tr!("Streaming without sound: no audio track runs").into(),
            ));
            None
        };
        log!(
            "live: {width}x{height} {fps} fps {codec:?}, {} kbit/s, {}",
            s.bitrate / 1000,
            if shared {
                "sharing the recording's encode"
            } else {
                "own encode"
            }
        );
        let mut streaming = Self {
            sink: None,
            video,
            url,
            codec,
            width,
            height,
            fps,
            aac,
            on_event: on_event.clone(),
            last: None,
            fresh: true,
            key_pending: false,
            slow: 0,
        };
        if shared {
            streaming.connect(v.track.clone(), sources)?;
        }
        Ok(streaming)
    }

    /// Starts the live thread and the audio, once the video header is known.
    fn connect(&mut self, track: VideoTrack, sources: &AudioSources) -> Result<(), String> {
        let audio = self.aac.as_ref().map(live_audio);
        let sink = LiveSink::start(
            std::mem::take(&mut self.url),
            track,
            audio,
            self.on_event.clone(),
        )
        .map_err(|e| tr!("Could not start the stream: {}", e))?;
        if let Some(aac) = self.aac.take() {
            lock(sources).set_stream_tap(Some(AudioTap::new(aac, sink.audio_in())));
        }
        self.sink = Some(sink);
        Ok(())
    }

    /// The live thread ended on its own (the server refused the stream).
    fn ended(&self) -> bool {
        self.sink.as_ref().is_some_and(LiveSink::ended)
    }

    /// Sends this tick's picture. `packets`: what the recording encoder
    /// made of it; `new_picture`: it encoded a new picture (not a repeat).
    fn tick(
        &mut self,
        v: &mut Video,
        k: u64,
        fps: u32,
        packets: &[EncodedPacket],
        new_picture: bool,
        sources: &AudioSources,
    ) -> Result<(), String> {
        self.fresh |= new_picture;
        if let StreamVideo::Opening(rx) = &self.video {
            let enc = match rx.try_recv() {
                Ok(opened) => opened?,
                Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(()),
                Err(_) => return Err(tr!("Could not open the stream encoder").into()),
            };
            let track = VideoTrack {
                codec: self.codec,
                width: self.width,
                height: self.height,
                fps: self.fps,
                extradata: enc.extradata().to_vec(),
            };
            self.video = StreamVideo::Own(enc);
            self.connect(track, sources)?;
        }
        let Some(sink) = &self.sink else {
            return Ok(());
        };
        let enc = match &mut self.video {
            StreamVideo::Own(enc) => enc,
            _ => {
                // A keyframe in the recording only for a new connection;
                // after a congestion, the next one comes within a GOP (2 s).
                if sink.starting() && !self.key_pending {
                    v.enc.force_keyframe();
                    self.key_pending = true;
                }
                for p in packets {
                    self.key_pending &= !p.keyframe;
                    sink.push_video(p.clone());
                }
                return Ok(());
            }
        };
        // Every tick at the recording's rate, but only one per stream frame.
        let n = k * u64::from(self.fps) / u64::from(fps);
        if self.last == Some(n) {
            return Ok(());
        }
        self.last = Some(n);
        if sink.wants_keyframe() && !self.key_pending {
            enc.force_keyframe();
            self.key_pending = true;
        }
        let pts = v.start + frame_offset(n, self.fps);
        let t = std::time::Instant::now();
        let out = if std::mem::take(&mut self.fresh) {
            enc.encode_scaled(&v.enc, pts)
        } else {
            enc.repeat(pts)
        };
        // The GPU cannot keep up with both encodes: the recording comes first.
        if t.elapsed() > frame_offset(1, fps) {
            self.slow += 1;
            if self.slow >= self.fps {
                return Err("The GPU encoder is overloaded: the stream ended so the \
                            recording stays smooth"
                    .into());
            }
        } else {
            self.slow = 0;
        }
        for p in out.map_err(|e| tr!("Stream encoding failed: {}", e))? {
            self.key_pending &= !p.keyframe;
            sink.push_video(p);
        }
        Ok(())
    }

    /// Ends the stream (with `error` if it failed here) and its audio;
    /// returns the live thread to join. `abort`: do not wait for a slow
    /// network (the pipeline is ending).
    fn stop(
        self,
        sources: &AudioSources,
        error: Option<String>,
        abort: bool,
    ) -> Option<JoinHandle<()>> {
        lock(sources).set_stream_tap(None);
        match (self.sink, error) {
            // Never connected: the UI still waits for the end.
            (None, error) => {
                (self.on_event)(Event::Stream(StreamState::Ended(error)));
                None
            }
            (Some(sink), _) if abort => sink.abort(),
            (Some(sink), Some(e)) => sink.fail(e),
            (Some(sink), None) => sink.stop(),
        }
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

/// Writes a ring snapshot to a new file. A failed file is removed.
fn write_replay(
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
fn save_replay(
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

/// Ticks until neither a recording nor the replay buffer needs the pipeline.
fn pump(
    settings: &Settings,
    slot: &Mailbox,
    control: &Mutex<Control>,
    sinks: &SharedSinks,
    sources: &AudioSources,
    on_event: &OnEvent,
) -> Result<(), String> {
    let fps = settings.fps;
    let mut video: Option<Video> = None;
    let mut k = 0u64;
    let mut skipped = 0;
    // `skipped` when the recording started, and whether Stats were sent since.
    let mut rec_skipped = 0;
    let mut stats_sent = false;
    // Last capture size, to log size changes once.
    let mut last_size = None;
    // A source size other than the encoder's, and since when.
    let mut new_size: Option<((u32, u32), Duration)> = None;
    // Since when a recording request waits for a first frame.
    let mut waiting_since: Option<Duration> = None;
    // Set by every encoded tick, read once `video` exists.
    let mut deadline = Duration::ZERO;
    // Reads the requests before the first wait: a recording request must
    // arm its first-frame timeout even if no frame ever comes.
    slot.poke();
    let mut window = (monotonic_now(), 0u32);
    let mut error = None;
    // Ticks were skipped because the game was gone: its return starts a GOP.
    let mut was_gone = false;
    let mut streaming: Option<Streaming> = None;
    let mut stopping: Option<Streaming> = None;
    let mut stage = Stage::new(settings, on_event);

    loop {
        match &video {
            Some(_) => sleep_until(deadline),
            // No frame yet (e.g. no game): sleep until one, or a request.
            None => slot.wait(
                waiting_since.map(|t| (t + FIRST_FRAME_TIMEOUT).saturating_sub(monotonic_now())),
            ),
        }
        let (frame, ended) = slot.take();
        if let Some(e) = ended {
            error = Some(tr!("Capture ended: {}", e));
            break;
        }
        // The canvas: a fixed one, else the first frame's size. A fixed
        // canvas with a scene that does not show the capture (e.g. "Be
        // right back") needs no frame at all.
        let size = match settings.canvas {
            Canvas::Fixed(w, h) => Some((w, h)),
            Canvas::Auto => frame.as_ref().map(|f| (f.width, f.height)),
        };
        let mut first = video.is_none() && (frame.is_some() || !stage.shows_main);
        if let (true, Some((width, height))) = (first, size) {
            // Audio starts with the first frame: before it, the ring and a
            // recording would drop its packets anyway. Before the encoder
            // opens, so the tracks cover the first video tick.
            start_audio(sources, sinks, on_event);
            reset_ring_audio(sources, sinks);
            on_event(Event::AudioStarted);
            match Video::open(settings, width, height) {
                Ok(v) => {
                    stage.set_canvas(width, height);
                    video = Some(v);
                }
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }
        first &= video.is_some();
        // Requests are read under the control lock, so none lands on an
        // ended pipeline unseen.
        // The live thread ended on its own (the server refused the stream).
        if streaming.as_ref().is_some_and(Streaming::ended) {
            let handle = streaming.take().and_then(|s| s.stop(sources, None, false));
            lock(control).saves.extend(handle);
        }
        let (record, stop, replay, save, go_live, scene) = {
            // Only this thread sets `rec`; never take `sinks` under
            // `control`: the UI would wait on muxer writes.
            let recording = lock(sinks).rec.is_some();
            let mut c = lock(control);
            let live = streaming.is_some() || c.stream.is_some();
            if c.quit || !(recording || c.record.is_some() || c.replay.is_some() || live) {
                c.ended = true;
                break;
            }
            // Stopped after the control lock: it takes the mix's lock,
            // which an audio thread may hold while it waits on a muxer write.
            if std::mem::take(&mut c.stop_stream) {
                match streaming.take() {
                    Some(s) => stopping = Some(s),
                    // Still waiting for a first frame: nothing to close.
                    None => on_event(Event::Stream(StreamState::Ended(None))),
                }
            }
            // A stream waits for the first frame; one at a time.
            let go_live = if video.is_some() && streaming.is_none() {
                c.stream.take()
            } else {
                if streaming.is_some() {
                    c.stream = None;
                }
                None
            };
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
            // No frame at all (the game quit or stopped presenting): give up
            // on the recording and the stream; the replay buffer keeps waiting.
            // The game window's picker restarts the wait: the user is picking.
            if settings.game.as_ref().is_some_and(GameFrames::picking) {
                waiting_since = None;
            }
            waiting_since = (video.is_none() && (c.record.is_some() || c.stream.is_some()))
                .then(|| waiting_since.unwrap_or_else(monotonic_now));
            if waiting_since.is_some_and(|t| monotonic_now() - t >= FIRST_FRAME_TIMEOUT) {
                waiting_since = None;
                let what = if settings.game.is_some() {
                    tr!("No frame from the game")
                } else {
                    tr!("No frame from the source")
                };
                if c.record.take().is_some() {
                    on_event(Event::Finished(Err(what.into())));
                }
                if c.stream.take().is_some() {
                    on_event(Event::Stream(StreamState::Ended(Some(what.into()))));
                }
                // Without a replay buffer the pipeline ends: check again at
                // once instead of waiting for a frame.
                slot.poke();
            }
            (
                record,
                stop,
                c.replay,
                c.save.take(),
                go_live,
                c.scene.take(),
            )
        };
        if let Some(scene) = scene {
            stage.set_scene(scene);
            // Without a frame yet, a scene without the capture may start now.
            if video.is_none() {
                slot.poke();
            }
        }
        if let Some(s) = stopping.take() {
            let handle = s.stop(sources, None, false);
            lock(control).saves.extend(handle);
        }
        // New tracks: restart the audio, and its part of the replay buffer
        // (the old tracks' packets do not match the new files' streams).
        // Before the first frame, the audio has not started: it reads the
        // new tracks when it does.
        let changed = lock(sources).changed;
        if changed && video.is_some() && lock(sinks).rec.is_none() {
            log!("audio tracks changed: restarting the audio");
            stop_audio(sources);
            start_audio(sources, sinks, on_event);
            reset_ring_audio(sources, sinks);
            on_event(Event::AudioStarted);
        }
        let streams = lock(sources).streams();
        {
            let mut s = lock(sinks);
            match replay {
                None => s.ring = None,
                Some(span) => s.ring.get_or_insert_with(|| Ring::new(span, streams)).span = span,
            }
        }
        if let Some(folder) = save {
            save_replay(folder, video.as_ref(), sinks, control, sources, on_event);
        }
        // While nothing records, follow a lasting size change with a new
        // encoder (and an empty replay buffer: its packets need the old one).
        // A fixed canvas never changes: the compositor or the encoder
        // scales the capture into it.
        let auto = settings.canvas == Canvas::Auto;
        if let (Some(v), Some(f), true) = (&video, &frame, auto) {
            let size = (f.width, f.height);
            if size == (v.track.width, v.track.height) {
                new_size = None;
            } else if new_size.is_none_or(|(s, _)| s != size) {
                new_size = Some((size, monotonic_now()));
            }
        }
        if let (Some((_, since)), Some(f)) = (new_size, &frame)
            && monotonic_now() - since >= RESIZE_AFTER
            && lock(sinks).rec.is_none()
            && streaming.is_none()
        {
            new_size = None;
            match Video::open(settings, f.width, f.height) {
                Ok(v) => {
                    stage.set_canvas(f.width, f.height);
                    video = Some(v);
                }
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
            (k, first, last_size) = (0, true, None);
            let streams = lock(sources).streams();
            if let Some(ring) = lock(sinks).ring.as_mut() {
                *ring = Ring::new(ring.span, streams);
            }
        }
        let Some(v) = &mut video else {
            continue;
        };
        if !first {
            k += 1;
        }
        let pts = v.start + frame_offset(k, fps);
        if let Some(folder) = record {
            let audio = lock(sources).file_tracks();
            match create_file(&folder, "Vesper", pts, &v.track, &audio) {
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
        // Only a video packet ends a recording: with no picture yet (a fixed
        // canvas waiting for a compositor that never came), end it here.
        if stop && !stage.pictured {
            let rec = lock(sinks).rec.take();
            if let Some(r) = rec {
                on_event(Event::Finished(
                    r.finish(Some(tr!("No picture was recorded").into())),
                ));
            }
        }
        if let Some(request) = go_live {
            match Streaming::start(&request, v, settings, sources, on_event) {
                Ok(s) => streaming = Some(s),
                Err(e) => {
                    log!("live: {e}");
                    on_event(Event::Stream(StreamState::Ended(Some(e))));
                }
            }
        }

        // A resized source (window, game) is scaled to the file's size.
        if let Some(f) = &frame
            && last_size != Some((f.width, f.height))
        {
            if last_size.is_some() {
                log!(
                    "source is now {}x{}: scaled to {}x{}",
                    f.width,
                    f.height,
                    v.track.width,
                    v.track.height
                );
            }
            last_size = Some((f.width, f.height));
        }
        // The game quit or crashed: no frozen picture, so the replay
        // buffer keeps the seconds before it. A recording or a stream keeps
        // its frame rate until the UI stops it (its stop needs a packet).
        let recording = lock(sinks).rec.is_some() || streaming.is_some();
        let gone =
            frame.is_none() && !recording && settings.game.as_ref().is_some_and(GameFrames::gone);
        let packets = if gone {
            was_gone = true;
            Ok(Vec::new())
        } else {
            if frame.is_some() && std::mem::take(&mut was_gone) {
                v.enc.force_keyframe();
            }
            stage.picture(&mut v.enc, frame, pts)
        };
        let packets = match packets {
            Ok(p) => p,
            Err(e) => {
                error = Some(tr!("Encoding failed: {}", e));
                break;
            }
        };
        let new_picture = stage.fresh();
        // Before the sinks take the packets. A failing stream ends alone.
        // A stream with its own encoder needs a picture to scale.
        if !gone
            && stage.pictured
            && let Some(s) = &mut streaming
            && let Err(e) = s.tick(v, k, fps, &packets, new_picture, sources)
        {
            log!("live: {e}");
            let handle = streaming
                .take()
                .and_then(|s| s.stop(sources, Some(e), false));
            lock(control).saves.extend(handle);
        }
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
    if let Some(s) = streaming.take() {
        // Quitting or failed: do not wait for a slow network.
        let handle = s.stop(sources, None, true);
        lock(control).saves.extend(handle);
    } else if lock(control).stream.take().is_some() {
        // Requested, never started: the UI waits for its end.
        on_event(Event::Stream(StreamState::Ended(None)));
    }
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
            Err(tr!("No frame was captured").into())
        }
        None => Ok(()),
    }
}

/// How long the overlay thread and its compositor stay after leaving a
/// composed scene: switching back soon reuses them.
const OVERLAYS_WARM: Duration = Duration::from_secs(10);

/// Threads freeing a compositor (retired overlay threads, failed
/// compositors), and since when: no new compositor starts before they end,
/// so two Vulkan devices never coexist, across pipelines too.
static RETIRING: Mutex<Vec<(Duration, JoinHandle<()>)>> = Mutex::new(Vec::new());

/// A thread freeing a compositor this long is stuck in the driver: new
/// compositors start anyway.
pub(crate) const RETIRING_STUCK: Duration = Duration::from_secs(10);

fn retire_thread(t: JoinHandle<()>) {
    lock(&RETIRING).push((monotonic_now(), t));
}

/// How long the oldest thread still freeing a compositor has been at it;
/// joins the finished ones.
pub(crate) fn retiring() -> Option<Duration> {
    let mut list = lock(&RETIRING);
    let (done, running): (Vec<_>, Vec<_>) = list.drain(..).partition(|(_, t)| t.is_finished());
    *list = running;
    let oldest = list.iter().map(|(since, _)| *since).min();
    drop(list);
    for (_, t) in done {
        let _ = t.join();
    }
    oldest.map(|t| monotonic_now().saturating_sub(t))
}

/// What the compositor gave this tick.
enum Out {
    /// A composition, ready for the encoder.
    Composed(VideoFrame),
    /// No composition: this capture frame goes to the encoder as is.
    Direct(VideoFrame),
    /// Nothing new: the last picture repeats.
    Nothing,
}

impl Drop for Stage {
    /// The overlay thread frees its compositor on its own; a next pipeline
    /// waits for it (see [`RETIRING`]).
    fn drop(&mut self) {
        self.next = None;
        self.retire();
    }
}

/// Turns the tick's capture into the encoder's picture. A scene that shows
/// the capture alone is a passthrough: frames go to the encoder as without
/// scenes, and no compositor (no Vulkan device) exists for it. Otherwise
/// the compositor draws the scene, only when the capture or an overlay
/// changed. Capture frames go to the compositor by value: their producer
/// gets them back as soon as the GPU read them. An overlay change without a
/// new capture frame is drawn from the compositor's copy of the last one
/// for damage-driven sources (the portal); a game sends frames
/// continuously, so its redraw waits for the next one (the copy costs
/// ~0.07 ms of GPU per 4K frame). The tick never waits for the GPU more
/// than half a frame: a composition still running then is picked up at a
/// later tick, the last picture repeating meanwhile. Any compositor failure
/// shows the capture alone (with a notice) and retries later: a recording
/// never fails because of an overlay.
struct Stage {
    /// Longest wait for a composition on the tick.
    budget: Duration,
    scene_name: String,
    passthrough: bool,
    shows_main: bool,
    /// Where frames come from: a game window from the portal is
    /// damage-driven too.
    game: Option<GameFrames>,
    draws: Vec<Draw>,
    want: Vec<(u32, Key)>,
    /// Texture id of each overlay source wanted (pruned with `want`), never
    /// reused: the overlay thread may still hold an old id's texture.
    ids: Vec<(u32, Key)>,
    next_id: u32,
    overlays: Option<Overlays>,
    /// Camera id of each webcam seen (its node name), stable across scenes.
    camera_ids: Vec<String>,
    /// Started by the first scene with a webcam, for the pipeline's life.
    cameras: Option<Cameras>,
    /// The user was told a webcam picture could not be drawn.
    camera_told: bool,
    /// Since when a passthrough scene leaves the overlay thread unused.
    idle_since: Option<Duration>,
    /// The encoder's size, once open.
    canvas: Option<(u32, u32)>,
    /// A capture frame waiting while the compositor is busy (a texture
    /// copy being queued, or the GPU still on the previous composition).
    next: Option<VideoFrame>,
    /// The picture must be drawn again.
    dirty: bool,
    /// A capture frame went to the encoder directly since the composition
    /// in flight was queued: that composition is older, drop it.
    stale: bool,
    /// The encoder has a picture to repeat.
    pictured: bool,
    /// A new picture went to the encoder since [`Stage::fresh`].
    encoded: bool,
    /// When to try the compositor again after a failure.
    retry: Option<Duration>,
    backoff: Duration,
    /// The user was told overlays are off; reset by a composition.
    told: bool,
    /// The user was told a previous compositor is stuck.
    stuck_told: bool,
    /// When the next preview is due.
    preview_at: Duration,
    /// The scene shown, to skip the same one sent again.
    shown: Option<Scene>,
    /// Draws still showing their previous texture until the new one is
    /// uploaded: (draw, new texture id).
    swaps: Vec<(usize, u32)>,
    on_event: OnEvent,
    #[cfg(test)]
    probe: Option<Arc<Probe>>,
}

/// The overlay texture a layer source shows.
fn texture_key(source: &LayerSource) -> Option<Key> {
    match source {
        LayerSource::Image { path } => Some(Key::Image(path.clone())),
        LayerSource::Text(style) => Some(Key::Text(style.clone())),
        LayerSource::Main | LayerSource::Camera { .. } => None,
    }
}

/// Whether the layer that showed `old` became `new`: same kind, and `old`
/// shown by no layer of the new scene (`keys`), so not moved elsewhere.
fn edited(old: &Key, new: &LayerSource, keys: &[Key]) -> bool {
    let kind = matches!(
        (old, new),
        (Key::Image(_), LayerSource::Image { .. }) | (Key::Text(_), LayerSource::Text(_))
    );
    kind && !keys.contains(old)
}

impl Stage {
    fn new(settings: &Settings, on_event: &OnEvent) -> Self {
        let mut stage = Self {
            budget: Duration::from_nanos(500_000_000 / u64::from(settings.fps.max(1))),
            scene_name: String::new(),
            passthrough: true,
            shows_main: true,
            game: settings.game.clone(),
            draws: Vec::new(),
            want: Vec::new(),
            ids: Vec::new(),
            next_id: 0,
            overlays: None,
            camera_ids: Vec::new(),
            cameras: None,
            camera_told: false,
            idle_since: None,
            canvas: None,
            next: None,
            dirty: true,
            stale: false,
            pictured: false,
            encoded: false,
            retry: None,
            backoff: RETRY_FIRST,
            told: false,
            stuck_told: false,
            preview_at: Duration::ZERO,
            shown: None,
            swaps: Vec::new(),
            on_event: on_event.clone(),
            #[cfg(test)]
            probe: settings.game.as_ref().map(|g| g.0.probe.clone()),
        };
        stage.set_scene(settings.scene.clone());
        stage
    }

    fn set_scene(&mut self, scene: Scene) {
        // The editor sends the scene on every change: the same again is
        // nothing new.
        if self.shown.as_ref() == Some(&scene) {
            return;
        }
        let renamed = scene.name != self.scene_name;
        if renamed && (!self.scene_name.is_empty() || !scene.is_passthrough()) {
            log!("scene: {:?}", scene.name);
        }
        self.passthrough = scene.is_passthrough();
        self.shows_main = scene.shows_main();
        let old = std::mem::take(&mut self.draws);
        self.want.clear();
        let mut cameras: Vec<(u32, String)> = Vec::new();
        self.swaps.clear();
        let done = self
            .overlays
            .as_ref()
            .map(|o| lock(&o.shared.done).clone())
            .unwrap_or_default();
        // An edit keeps the layers' count; a layer added, removed or shown
        // is no edit.
        let same_shape = scene.drawn().count() == old.len();
        let keys: Vec<Key> = scene
            .drawn()
            .filter_map(|l| texture_key(&l.source))
            .collect();
        for l in scene.drawn() {
            let source = match &l.source {
                LayerSource::Main => DrawSource::Main,
                LayerSource::Image { path } => self.texture(Key::Image(path.clone())),
                LayerSource::Text(style) => self.texture(Key::Text(style.clone())),
                LayerSource::Camera { node } => {
                    let id = match self.camera_ids.iter().position(|n| n == node) {
                        Some(i) => i,
                        None => {
                            self.camera_ids.push(node.clone());
                            self.camera_ids.len() - 1
                        }
                    } as u32;
                    if !cameras.iter().any(|(i, _)| *i == id) {
                        if cameras.len() == vesper_compose::MAX_CAMERAS {
                            log!("scene {:?}: webcam {node:?} left out", scene.name);
                            continue;
                        }
                        cameras.push((id, node.clone()));
                    }
                    DrawSource::Camera(id)
                }
            };
            // An edited image or text keeps its old texture until the new
            // one is uploaded: an edit never blanks it in the recording.
            let n = self.draws.len();
            let prev = match (source, old.get(n)) {
                (DrawSource::Texture(new), Some(d))
                    if !renamed && same_shape && d.rect == l.rect && !done.contains(&new) =>
                {
                    match d.source {
                        DrawSource::Texture(prev) if prev != new => self.key_of(prev),
                        _ => None,
                    }
                    .filter(|(_, k)| edited(k, &l.source, &keys))
                }
                _ => None,
            };
            let source = match (source, prev) {
                (DrawSource::Texture(new), Some((prev, key))) => {
                    self.swaps.push((n, new));
                    self.want.push((prev, key));
                    DrawSource::Texture(prev)
                }
                (source, _) => source,
            };
            self.draws.push(Draw {
                source,
                rect: l.rect,
                crop: l.crop,
                fit: l.fit,
                opacity: l.opacity,
            });
        }
        self.prune_ids();
        self.scene_name.clone_from(&scene.name);
        if !cameras.is_empty() && self.cameras.is_none() {
            match Cameras::start(self.on_event.clone()) {
                Ok(c) => {
                    c.set_overlays(self.overlays.as_ref().map(|o| o.shared.clone()));
                    self.cameras = Some(c);
                }
                Err(e) => log!("no camera thread: {e}"),
            }
        }
        if let Some(c) = &self.cameras {
            c.want(cameras);
        }
        self.shown = Some(scene);
        if !self.shows_main {
            self.next = None;
        }
        if self.passthrough {
            // Kept a while: a quick switch back reuses it.
            self.idle_since = self.overlays.as_ref().map(|_| monotonic_now());
        } else {
            self.idle_since = None;
            self.request();
        }
        self.dirty = true;
    }

    /// The texture id of `key`, added to this scene's wanted textures.
    fn texture(&mut self, key: Key) -> DrawSource {
        let id = match self.ids.iter().find(|(_, k)| *k == key) {
            Some(&(id, _)) => id,
            None => {
                let id = self.next_id;
                self.next_id = self.next_id.wrapping_add(1);
                self.ids.push((id, key.clone()));
                id
            }
        };
        if !self.want.iter().any(|(i, _)| *i == id) {
            self.want.push((id, key));
        }
        DrawSource::Texture(id)
    }

    fn key_of(&self, id: u32) -> Option<(u32, Key)> {
        self.ids.iter().find(|(i, _)| *i == id).cloned()
    }

    /// Forgets the ids of textures no longer wanted (each edit of a text
    /// makes one).
    fn prune_ids(&mut self) {
        let want = &self.want;
        self.ids.retain(|(id, _)| want.iter().any(|(w, _)| w == id));
    }

    /// The encoder (re)opened at this size: it has no picture yet.
    fn set_canvas(&mut self, width: u32, height: u32) {
        self.canvas = Some((width, height));
        self.pictured = false;
        self.dirty = true;
        self.request();
    }

    /// Asks the overlay thread for this scene's compositor and textures.
    fn request(&mut self) {
        let Some(canvas) = self.canvas else { return };
        if self.passthrough || self.retry.is_some() {
            return;
        }
        if self.overlays.is_none() {
            // A retired thread first frees its device (`poll` asks again),
            // unless it is stuck in the driver.
            match retiring() {
                Some(age) if age < RETIRING_STUCK => return,
                Some(_) if !std::mem::replace(&mut self.stuck_told, true) => {
                    log!("scene compositor: a previous one is stuck freeing its device");
                    (self.on_event)(Event::Notice(
                        tr!("Scene overlays may be unavailable: the GPU is busy").into(),
                    ));
                }
                _ => {}
            }
            match Overlays::start() {
                Ok(o) => {
                    if let Some(c) = &self.cameras {
                        c.set_overlays(Some(o.shared.clone()));
                    }
                    self.overlays = Some(o);
                }
                Err(e) => return self.fail(format!("no overlay thread: {e}")),
            }
        }
        if let Some(o) = &self.overlays {
            o.request(canvas, self.want.clone());
        }
    }

    /// Shows the capture alone until a retry after the backoff.
    fn fail(&mut self, why: String) {
        log!("scene compositor: {why}; retrying in {:?}", self.backoff);
        if !std::mem::replace(&mut self.told, true) {
            (self.on_event)(Event::Notice(
                tr!("Scene overlays are off for now: the GPU compositor failed").into(),
            ));
        }
        if let Some(o) = &self.overlays {
            o.suspend(self.backoff);
        }
        self.retry = Some(monotonic_now() + self.backoff);
        self.backoff = (self.backoff * 2).min(RETRY_MAX);
        self.dirty = true;
    }

    /// News from the overlay thread, a retry when due, an idle thread retired.
    fn poll(&mut self) {
        if let Some(o) = &self.overlays {
            #[cfg(test)]
            if let Some(p) = &self.probe {
                let n = o.shared.created.load(Ordering::Relaxed);
                p.compositors.fetch_max(n, Ordering::Relaxed);
            }
            if o.changed() {
                self.dirty = true;
            }
            if self.cameras.as_ref().is_some_and(Cameras::news) {
                self.dirty = true;
            }
            if let Some(e) = o.error() {
                self.fail(format!("could not start: {e}"));
            }
        }
        let done = self
            .overlays
            .as_ref()
            .filter(|_| !self.swaps.is_empty())
            .map(|o| lock(&o.shared.done).clone());
        if let Some(done) = done {
            let before = self.swaps.len();
            let draws = &mut self.draws;
            self.swaps.retain(|&(n, new)| {
                let ready = done.contains(&new);
                if ready && let Some(d) = draws.get_mut(n) {
                    d.source = DrawSource::Texture(new);
                }
                !ready
            });
            if self.swaps.len() != before {
                // The old textures go.
                let (draws, swaps) = (&self.draws, &self.swaps);
                self.want.retain(|(id, _)| {
                    draws.iter().any(|d| d.source == DrawSource::Texture(*id))
                        || swaps.iter().any(|s| s.1 == *id)
                });
                self.prune_ids();
                self.dirty = true;
                self.request();
            }
        }
        if self
            .idle_since
            .is_some_and(|t| monotonic_now() - t >= OVERLAYS_WARM)
        {
            self.idle_since = None;
            self.next = None;
            self.retire();
        }
        if self.retry.is_some_and(|t| monotonic_now() >= t) {
            self.retry = None;
            self.request();
        } else if self.overlays.is_none() && !self.passthrough {
            // Waiting for a retired thread to end.
            self.request();
        }
    }

    fn retire(&mut self) {
        if let Some(c) = &self.cameras {
            c.set_overlays(None);
        }
        if let Some(o) = self.overlays.take() {
            retire_thread(o.retire());
        }
    }

    /// Picks up and drops a composition still in flight (a direct frame
    /// now follows it): its capture frame and ring slot go back.
    fn drain(&mut self) {
        let Some(o) = &self.overlays else { return };
        if let Ok(mut slot) = o.shared.comp.try_lock()
            && let Some(c) = slot.as_mut()
            && c.in_flight()
        {
            let _ = c.poll(Duration::ZERO);
        }
    }

    /// Whether the compositor holds a copy of the capture to redraw from.
    fn has_base(&self) -> bool {
        let Some(o) = &self.overlays else {
            return false;
        };
        o.shared
            .comp
            .try_lock()
            .ok()
            .and_then(|c| c.as_ref().map(Compositor::has_base))
            .unwrap_or(false)
    }

    /// The tick's packets: `frame` (the new capture, if any) drawn into the
    /// scene, or the last picture again.
    fn picture(
        &mut self,
        enc: &mut Encoder,
        frame: Option<VideoFrame>,
        pts: Duration,
    ) -> Result<Vec<EncodedPacket>, vesper_encode::Error> {
        self.poll();
        // Back to the capture alone on a static source: the compositor's
        // copy shows it until the source's next frame.
        let redraw = self.passthrough && frame.is_none() && self.dirty && self.has_base();
        if self.passthrough && !redraw {
            self.dirty = false;
            self.next = None;
            self.drain();
            return match frame {
                Some(mut frame) => {
                    frame.pts = pts;
                    self.stale = true;
                    self.encode(enc, frame)
                }
                None => self.repeat(enc, pts),
            };
        }
        // The newest frame only: a waiting older one goes back now.
        let waiting = self.next.take();
        let frame = match frame {
            Some(f) if self.shows_main => {
                drop(waiting);
                self.dirty = true;
                Some(f)
            }
            // Not shown: back to its producer at once.
            Some(_) => None,
            None => waiting,
        };
        match self.compose(frame) {
            Out::Composed(mut f) | Out::Direct(mut f) => {
                f.pts = pts;
                self.encode(enc, f)
            }
            Out::Nothing => self.repeat(enc, pts),
        }
    }

    fn compose(&mut self, frame: Option<VideoFrame>) -> Out {
        // Direct: the frame (if any) as is, no composition.
        let direct = |stage: &mut Self, frame: Option<VideoFrame>| match frame {
            Some(f) => {
                stage.stale = true;
                stage.dirty = false;
                Out::Direct(f)
            }
            None => Out::Nothing,
        };
        let Some(shared) = self.overlays.as_ref().map(|o| o.shared.clone()) else {
            return direct(self, frame);
        };
        #[cfg(test)]
        let busy = self
            .probe
            .as_ref()
            .is_some_and(|p| p.busy.load(Ordering::Relaxed));
        #[cfg(not(test))]
        let busy = false;
        let locked = if busy {
            Err(std::sync::TryLockError::WouldBlock)
        } else {
            shared.comp.try_lock()
        };
        let mut slot = match locked {
            Ok(slot) => slot,
            Err(std::sync::TryLockError::WouldBlock) => {
                // A texture copy is being queued: next tick.
                self.next = frame;
                return Out::Nothing;
            }
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        };
        // Missing, or another size (the overlay thread is replacing it).
        let Some(comp) = slot
            .as_mut()
            .filter(|c| Some((c.width(), c.height())) == self.canvas)
        else {
            drop(slot);
            return direct(self, frame);
        };
        if PREVIEW.load(Ordering::Relaxed) && monotonic_now() >= self.preview_at {
            self.preview_at = monotonic_now() + PREVIEW_PERIOD;
            if let Some(px) = comp.preview() {
                let (w, h) = (
                    vesper_compose::PREVIEW_WIDTH,
                    vesper_compose::PREVIEW_HEIGHT,
                );
                let buffer = slint::SharedPixelBuffer::clone_from_slice(px, w, h);
                (self.on_event)(Event::Preview(buffer));
            }
            // The next composition carries the preview pass, even of a
            // picture that would otherwise repeat.
            self.dirty = true;
        }
        let mut out = None;
        if comp.in_flight() {
            match comp.poll(Duration::ZERO) {
                Ok(Some(f)) if !std::mem::take(&mut self.stale) => out = Some(f),
                Ok(_) => {}
                Err(e) => return self.broken(slot, e.to_string(), frame),
            }
        }
        if comp.in_flight() {
            // The GPU is still on it: this frame waits (the older one goes).
            if frame.is_some() {
                self.next = frame;
            }
            return Out::Nothing;
        }
        let can_draw = frame.is_some() || !self.shows_main || comp.has_base();
        if self.dirty && can_draw {
            #[cfg(test)]
            if self
                .probe
                .as_ref()
                .is_some_and(|p| p.fail.swap(false, Ordering::Relaxed))
            {
                return self.broken(slot, "injected failure".into(), frame);
            }
            #[cfg(test)]
            if let (Some(p), Some(f)) = (&self.probe, &frame) {
                let t = f.pts.as_nanos() as u64;
                if p.last_main.fetch_max(t, Ordering::Relaxed) > t {
                    p.went_back.store(true, Ordering::Relaxed);
                }
            }
            #[cfg(test)]
            let budget = if self
                .probe
                .as_ref()
                .is_some_and(|p| p.no_wait.load(Ordering::Relaxed))
            {
                Duration::ZERO
            } else {
                self.budget
            };
            #[cfg(not(test))]
            let budget = self.budget;
            // The latest webcam pictures, copied by this composition.
            for (id, feed) in self.cameras.as_ref().map(Cameras::take).unwrap_or_default() {
                let r = match feed {
                    Feed::Gpu(f) => comp.set_camera_frame(id, f),
                    Feed::Yuv(s) if s.is_for(comp) => comp.set_camera_yuv(id, s),
                    // Staged for a replaced compositor: freed off the tick.
                    Feed::Yuv(s) => {
                        if let Some(c) = &self.cameras {
                            c.discard(s);
                        }
                        Ok(())
                    }
                    Feed::Gone => {
                        comp.remove_camera(id);
                        Ok(())
                    }
                };
                if let Err(e) = r {
                    log!("webcam {id}: picture dropped: {e}");
                }
            }
            // Damage-driven sources keep a copy for redraws without a frame.
            let keep = frame.is_some() && self.damage_driven();
            match comp.submit(frame, &self.draws, keep) {
                Ok(()) => {
                    (self.dirty, self.stale) = (false, false);
                    match comp.poll(budget) {
                        Ok(Some(f)) => out = Some(f),
                        Ok(None) => {}
                        Err(e) => return self.broken(slot, e.to_string(), None),
                    }
                }
                Err(e) => return self.broken(slot, e.to_string(), None),
            }
            // A webcam picture the GPU could not take hides that webcam;
            // the next one may work.
            for (id, why) in comp.take_camera_failures() {
                log!("webcam {id}: hidden: {why}");
                if !std::mem::replace(&mut self.camera_told, true) {
                    (self.on_event)(Event::Notice(
                        tr!("A webcam is hidden: the GPU could not use its picture").into(),
                    ));
                }
            }
        } else if frame.is_some() {
            // Cannot happen (a frame makes the picture dirty): released.
            drop(frame);
        }
        let Some(f) = out else { return Out::Nothing };
        #[cfg(test)]
        if let Some(p) = &self.probe {
            p.composed.fetch_add(1, Ordering::Relaxed);
            let ns = comp.gpu_time().map_or(0, |t| t.as_nanos() as u64);
            p.gpu_ns.fetch_add(ns, Ordering::Relaxed);
        }
        if std::mem::take(&mut self.told) {
            log!("scene compositor: back");
        }
        self.backoff = RETRY_FIRST;
        Out::Composed(f)
    }

    /// Frames come on damage only (portal): nothing may follow the last one.
    fn damage_driven(&self) -> bool {
        #[cfg(test)]
        if self
            .probe
            .as_ref()
            .is_some_and(|p| p.base.load(Ordering::Relaxed))
        {
            return true;
        }
        self.game
            .as_ref()
            .is_none_or(|g| g.0.window.load(Ordering::Relaxed))
    }

    /// Drops a failed compositor (its teardown waits for the GPU: never on
    /// the tick) and shows `frame` alone.
    fn broken(
        &mut self,
        mut slot: MutexGuard<'_, Option<Compositor>>,
        why: String,
        frame: Option<VideoFrame>,
    ) -> Out {
        let lost = slot.as_ref().is_some_and(Compositor::is_lost);
        let old = slot.take();
        drop(slot);
        if let Ok(t) = std::thread::Builder::new()
            .name("vesper-compose-drop".into())
            .spawn(move || drop(old))
        {
            retire_thread(t);
        }
        self.fail(if lost {
            format!("the GPU stopped answering ({why})")
        } else {
            why
        });
        match frame {
            Some(f) => {
                self.stale = true;
                Out::Direct(f)
            }
            None => Out::Nothing,
        }
    }

    fn encode(
        &mut self,
        enc: &mut Encoder,
        frame: VideoFrame,
    ) -> Result<Vec<EncodedPacket>, vesper_encode::Error> {
        (self.pictured, self.encoded) = (true, true);
        enc.encode(frame)
    }

    /// The last tick encoded a new picture (not a repeat): a stream with
    /// its own encoder scales it.
    fn fresh(&mut self) -> bool {
        std::mem::take(&mut self.encoded)
    }

    fn repeat(
        &self,
        enc: &mut Encoder,
        pts: Duration,
    ) -> Result<Vec<EncodedPacket>, vesper_encode::Error> {
        // Nothing to show yet (a fixed canvas waiting for its compositor).
        if !self.pictured {
            return Ok(Vec::new());
        }
        enc.repeat(pts)
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

/// First tick of the shared grid ([`protocol::tick_ns`]) at or after `t`.
fn next_tick(t: Duration, fps: u32) -> Duration {
    let t = t.as_nanos() as u64;
    let n = protocol::tick_at(t, fps);
    let n = if protocol::tick_ns(n, fps) < t {
        n + 1
    } else {
        n
    };
    Duration::from_nanos(protocol::tick_ns(n, fps))
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

/// vesper-encode's GPU test-pattern producer: synthetic game frames for the
/// pipeline tests below (skipped without a VAAPI GPU).
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../vesper-encode/tests/common/mod.rs"]
mod producer;

#[cfg(test)]
#[path = "recorder_gpu_tests.rs"]
mod gpu_tests;

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
    fn mailbox_wait_ends_on_a_request_an_end_or_the_timeout() {
        let mailbox = Arc::new(Mailbox::default());
        let t = monotonic_now();
        mailbox.wait(Some(Duration::from_millis(20)));
        assert!(monotonic_now() - t >= Duration::from_millis(20));

        let m = mailbox.clone();
        let poker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            m.poke();
        });
        mailbox.wait(None);
        poker.join().unwrap();
        // The request is consumed: no busy loop afterwards.
        assert!(!lock(&mailbox.slot).poked);

        mailbox.park(Err("gone".into()));
        mailbox.wait(None);
        assert_eq!(mailbox.take().1.as_deref(), Some("gone"));
    }

    #[test]
    fn ticks_start_on_the_shared_grid() {
        let t = Duration::from_nanos(1_000_000_000_007);
        let s = next_tick(t, 144);
        assert!(s >= t && s - t < frame_offset(1, 144));
        let n = protocol::tick_at(s.as_nanos() as u64, 144);
        assert_eq!(protocol::tick_ns(n, 144), s.as_nanos() as u64);
        assert_eq!(next_tick(s, 144), s);
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
    fn recording_without_any_frame_fails_after_the_timeout() {
        let settings = Settings {
            codec: Codec::Hevc,
            fps: 60,
            quality: 28,
            max_bits_per_pixel: 0.1,
            tracks: Vec::new(),
            mix: false,
            game: None,
            scene: Scene::main(),
            canvas: Canvas::Auto,
        };
        let control = Mutex::new(Control {
            record: Some(std::env::temp_dir()),
            ..Control::default()
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = events.clone();
        let on_event: OnEvent = Arc::new(move |e| lock(&seen).push(format!("{e:?}")));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        // No frame ever comes: the first wait must already know the deadline.
        std::thread::spawn(move || {
            let result = pump(
                &settings,
                &Mailbox::default(),
                &control,
                &SharedSinks::default(),
                &AudioSources::default(),
                &on_event,
            );
            let _ = done_tx.send(result);
        });
        let result = done_rx
            .recv_timeout(FIRST_FRAME_TIMEOUT + Duration::from_secs(2))
            .expect("the pipeline waits forever for a first frame");
        assert_eq!(result, Ok(()));
        assert_eq!(
            *lock(&events),
            [r#"Finished(Err("No frame from the source"))"#]
        );
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
    fn ring_keeps_its_content_while_video_stops() {
        // The game crashed at 4 s: audio goes on, video does not.
        let mut ring = filled(3, 4000);
        let before = (ring.video.clone(), ring.audio[0].len());
        for ms in (4100..60_000).step_by(100) {
            ring.push_audio(0, packet(ms, false));
        }
        assert!(
            ring.video
                .iter()
                .map(|p| p.pts)
                .eq(before.0.iter().map(|p| p.pts))
        );
        let last = ring.audio[0].back().unwrap().pts;
        assert_eq!(last, Duration::from_millis(4000) + AUDIO_LEAD);
        assert_eq!(ring.audio[0].len(), before.1 + 10);
    }

    #[test]
    fn ring_restarts_after_a_gap_longer_than_its_span() {
        // The game was gone from 4 s to 60 s, then comes back.
        let mut ring = filled(3, 4000);
        ring.push_video(packet(60_000, false));
        assert!(ring.video.is_empty(), "waits for a keyframe");
        ring.push_audio(0, packet(60_000, false));
        assert!(ring.audio[0].is_empty());
        ring.push_video(packet(60_100, true));
        ring.push_audio(0, packet(60_100, false));
        assert_eq!(ring.video.len(), 1);
        assert_eq!(ring.audio[0].len(), 1);
        // A short pause keeps the ring.
        let mut ring = filled(3, 4000);
        ring.push_video(packet(6000, false));
        assert_eq!(ring.video.back().unwrap().pts, Duration::from_millis(6000));
    }

    #[test]
    fn missing_audio_is_one_short_notice() {
        assert_eq!(missing_audio(&[], false), None);
        assert_eq!(
            missing_audio(&[], true).as_deref(),
            Some("Recording without the mix track")
        );
        assert_eq!(
            missing_audio(&["Mic"], true).as_deref(),
            Some("Recording without the audio track Mic")
        );
        assert_eq!(
            missing_audio(&["Mic", "Game"], false).as_deref(),
            Some("Recording without the audio tracks Mic, Game")
        );
    }

    #[test]
    fn ring_snapshot_is_in_pts_order() {
        let ring = filled(3, 4000);
        let snap = interleave(ring.snapshot());
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
    fn tracks_move_up_when_the_mix_is_dropped() {
        // Mix + 2 tracks, then one track failed: the mix goes, the other
        // track must write stream 0, the only one its files have.
        let first = Arc::new(AtomicUsize::new(1));
        let tracks: Vec<_> = (0..2).map(|k| StreamIndex::new(&first, k)).collect();
        assert_eq!(
            tracks.iter().map(StreamIndex::get).collect::<Vec<_>>(),
            [1, 2]
        );
        first.store(0, Ordering::Relaxed);
        assert_eq!(tracks[0].get(), 0);
    }

    #[test]
    fn timestamp_is_file_name_safe() {
        let ts = local_timestamp();
        assert_eq!(ts.len(), 19, "{ts}");
        assert!(!ts.contains(':') && !ts.contains('/'), "{ts}");
    }

    // --- robustness (audit): the pipeline state machine without GPU or
    // PipeWire (a game route nobody feeds, no audio tracks) ---

    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};

    fn settings() -> Settings {
        Settings {
            codec: Codec::Hevc,
            fps: 60,
            quality: 28,
            max_bits_per_pixel: 0.1,
            tracks: Vec::new(),
            mix: false,
            game: Some(GameFrames::default()),
            scene: Scene::main(),
            canvas: Canvas::Auto,
        }
    }

    fn start(
        replay: Option<Duration>,
        record: Option<PathBuf>,
        settings: Settings,
    ) -> (Pipeline, Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        let p = Pipeline::start(settings, replay, record, None, move |e| drop(tx.send(e))).unwrap();
        (p, rx)
    }

    /// Next event within `secs`, or a failure naming what hung.
    fn next(rx: &Receiver<Event>, secs: u64) -> Event {
        match rx.recv_timeout(Duration::from_secs(secs)) {
            Ok(e) => e,
            Err(RecvTimeoutError::Timeout) => panic!("no event within {secs} s: pipeline hung"),
            Err(RecvTimeoutError::Disconnected) => panic!("pipeline ended without Stopped"),
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vesper-rec-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn quitting_an_idle_pipeline_is_prompt_and_clean() {
        let (p, rx) = start(Some(Duration::from_secs(30)), None, settings());
        assert!(p.record(temp_dir("quit")), "pipeline accepts requests");
        let t = std::time::Instant::now();
        p.join();
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "join took {:?}",
            t.elapsed()
        );
        // The pending recording request never started: no file, no Finished
        // (it ends as "No frame was captured", which the UI ignores on quit).
        assert!(matches!(next(&rx, 2), Event::Stopped(_)));
    }

    #[test]
    fn a_dead_capture_ends_the_pipeline_and_refuses_new_requests() {
        let s = settings();
        let route = s.game.clone().unwrap();
        let (p, rx) = start(Some(Duration::from_secs(30)), None, s);
        // The game capture server died: its last event is an error. Pushed
        // until the pipeline has attached and reports it (a push before
        // that is dropped).
        let mut stopped = None;
        for _ in 0..100 {
            route.push(Err("boom".into()));
            if let Ok(e) = rx.recv_timeout(Duration::from_millis(50)) {
                stopped = Some(e);
                break;
            }
        }
        match stopped {
            Some(Event::Stopped(Some(e))) => assert!(e.contains("boom"), "{e}"),
            other => panic!("{other:?}"),
        }
        // The UI must start a new pipeline: requests report the end.
        assert!(!p.record(temp_dir("dead")));
        assert!(!p.save_replay(temp_dir("dead")));
        assert!(!p.set_replay(None));
        p.join();
    }

    #[test]
    fn a_recording_without_frames_fails_instead_of_waiting_forever() {
        let dir = temp_dir("noframe");
        let (p, rx) = start(None, Some(dir.clone()), settings());
        match next(&rx, FIRST_FRAME_TIMEOUT.as_secs() + 3) {
            Event::Finished(Err(e)) => assert!(e.contains("No frame"), "{e}"),
            other => panic!("{other:?}"),
        }
        // Nothing runs any more: the pipeline ends on its own, cleanly.
        assert!(matches!(next(&rx, 3), Event::Stopped(None)));
        assert!(
            !dir.exists(),
            "no folder or file for a recording that never started"
        );
        p.join();
    }

    #[test]
    fn a_recording_waits_while_the_game_window_is_picked() {
        let dir = temp_dir("picking");
        let s = settings();
        // Shares its route with the pipeline's.
        let game = s.game.clone().unwrap();
        game.set_picking(true);
        let (p, rx) = start(None, Some(dir.clone()), s);
        // Well past the first-frame timeout: the user is still picking.
        let waited = rx.recv_timeout(FIRST_FRAME_TIMEOUT * 5);
        assert!(waited.is_err(), "{waited:?}");
        // Cancelled: the usual timeout applies again.
        game.set_picking(false);
        match next(&rx, FIRST_FRAME_TIMEOUT.as_secs() + 3) {
            Event::Finished(Err(e)) => assert!(e.contains("No frame"), "{e}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(next(&rx, 3), Event::Stopped(None)));
        p.join();
    }

    #[test]
    fn a_game_window_is_never_gone() {
        let game = GameFrames::default();
        game.0.gone.store(true, Ordering::Relaxed);
        assert!(game.gone());
        game.set_window(true);
        assert!(!game.gone(), "a static window repeats its picture");
    }

    fn stream_request() -> StreamRequest {
        StreamRequest {
            settings: crate::stream::StreamSettings {
                service: crate::stream::Service::Custom,
                custom_url: "rtmp://127.0.0.1:1/live".into(),
                ..Default::default()
            },
            key: None,
        }
    }

    #[test]
    fn a_stream_without_frames_ends_and_so_does_the_pipeline() {
        // Passed at start like the app does: a pipeline started with nothing
        // to do may end before a later start_stream request reaches it.
        let (tx, rx) = mpsc::channel();
        let p = Pipeline::start(settings(), None, None, Some(stream_request()), move |e| {
            drop(tx.send(e))
        })
        .unwrap();
        match next(&rx, FIRST_FRAME_TIMEOUT.as_secs() + 3) {
            Event::Stream(StreamState::Ended(Some(e))) => assert!(e.contains("No frame"), "{e}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(next(&rx, 3), Event::Stopped(None)));
        p.join();
    }

    #[test]
    fn ending_a_stream_that_waits_for_a_frame_reports_its_end() {
        let (p, rx) = start(Some(Duration::from_secs(30)), None, settings());
        assert!(p.start_stream(stream_request()));
        assert!(p.stop_stream());
        assert!(matches!(
            next(&rx, 3),
            Event::Stream(StreamState::Ended(None))
        ));
        p.join();
    }

    #[test]
    fn ending_a_stream_of_an_ended_pipeline_says_so() {
        let s = settings();
        let route = s.game.clone().unwrap();
        let (p, rx) = start(Some(Duration::from_secs(30)), None, s);
        for _ in 0..100 {
            route.push(Err("boom".into()));
            if rx.recv_timeout(Duration::from_millis(50)).is_ok() {
                break;
            }
        }
        // The UI then resets the Live card itself.
        assert!(!p.stop_stream());
        p.join();
    }

    #[test]
    fn saving_an_empty_replay_buffer_reports_it() {
        let (p, rx) = start(Some(Duration::from_secs(30)), None, settings());
        assert!(p.save_replay(temp_dir("empty")));
        match next(&rx, 3) {
            Event::ReplaySaved(Err(e)) => assert!(e.contains("empty"), "{e}"),
            other => panic!("{other:?}"),
        }
        p.join();
    }

    #[test]
    fn unusable_output_folders_are_reported_with_their_path() {
        let video = VideoTrack {
            codec: Codec::Hevc,
            width: 64,
            height: 64,
            fps: 30,
            extradata: Vec::new(),
        };
        // A file where the folder should be.
        let base = temp_dir("badfolder");
        std::fs::create_dir_all(&base).unwrap();
        let file = base.join("not-a-folder");
        std::fs::write(&file, b"x").unwrap();
        let err = create_file(&file, "Vesper", Duration::ZERO, &video, &[])
            .err()
            .unwrap();
        assert!(err.contains("not-a-folder"), "{err}");
        // The header cannot be written (HEVC needs its parameter sets): the
        // reserved, empty file must not stay behind.
        let folder = base.join("clips");
        let err = create_file(&folder, "Vesper", Duration::ZERO, &video, &[])
            .err()
            .unwrap();
        assert!(err.contains("Could not create"), "{err}");
        let left: Vec<_> = std::fs::read_dir(&folder).unwrap().collect();
        assert!(left.is_empty(), "left behind: {left:?}");
        std::fs::remove_dir_all(base).unwrap();
    }

    // --- texture swaps on a scene edit, without an overlay thread ---

    fn stage_with(scene: Scene) -> Stage {
        let settings = Settings {
            game: None,
            scene,
            ..settings()
        };
        let on_event: OnEvent = Arc::new(|_| {});
        Stage::new(&settings, &on_event)
    }

    fn image_layer(path: &str) -> crate::scenes::Layer {
        crate::scenes::Layer {
            source: LayerSource::Image { path: path.into() },
            rect: [0.5, 0.5, 0.25, 0.25],
            ..crate::scenes::Layer::main()
        }
    }

    fn texture_of(stage: &Stage, path: &str) -> DrawSource {
        let id = stage
            .ids
            .iter()
            .find(|(_, k)| *k == Key::Image(path.into()));
        DrawSource::Texture(id.expect("an id for the image").0)
    }

    fn scene_of(layers: Vec<crate::scenes::Layer>) -> Scene {
        Scene {
            name: "S".into(),
            layers,
        }
    }

    #[test]
    fn an_edit_keeps_the_old_texture_and_ids_stay_few() {
        let main = crate::scenes::Layer::main();
        let mut stage = stage_with(scene_of(vec![main.clone(), image_layer("/a.png")]));
        let a = texture_of(&stage, "/a.png");
        for i in 0..100 {
            let path = format!("/edit{i}.png");
            stage.set_scene(scene_of(vec![main.clone(), image_layer(&path)]));
            assert_eq!(stage.draws[1].source, a, "edit {i} shows the old texture");
        }
        // The old texture and the newest edit's.
        assert_eq!(stage.ids.len(), 2, "{:?}", stage.ids);
        assert_eq!(stage.want.len(), 2);
    }

    #[test]
    fn a_new_or_moved_layer_never_shows_another_layers_texture() {
        let main = crate::scenes::Layer::main();
        let (a, b, c) = (
            image_layer("/a.png"),
            image_layer("/b.png"),
            image_layer("/c.png"),
        );
        let mut stage = stage_with(scene_of(vec![main.clone(), a.clone(), b.clone()]));
        // Added under the others, at the same rect.
        stage.set_scene(scene_of(vec![
            main.clone(),
            c.clone(),
            a.clone(),
            b.clone(),
        ]));
        assert!(stage.swaps.is_empty());
        assert_eq!(stage.draws[1].source, texture_of(&stage, "/c.png"));
        // Moved: each keeps its own texture.
        stage.set_scene(scene_of(vec![main.clone(), b, c, a]));
        assert!(stage.swaps.is_empty());
        assert_eq!(stage.draws[1].source, texture_of(&stage, "/b.png"));
        assert_eq!(stage.draws[3].source, texture_of(&stage, "/a.png"));
        // A text never takes an image's place.
        let text = crate::scenes::Layer {
            source: LayerSource::Text(crate::scenes::TextStyle::default()),
            ..image_layer("")
        };
        stage.set_scene(scene_of(vec![main, text.clone(), text.clone(), text]));
        assert!(stage.swaps.is_empty());
    }
}
