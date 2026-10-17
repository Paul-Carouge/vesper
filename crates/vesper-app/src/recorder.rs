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
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use vesper_audio::{
    Aac, AudioCapture, CHANNELS, DuckSettings, Exclusions, Mix, Options, Peaks, SAMPLE_RATE,
    Source, TrackState, VoiceKey,
};
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

mod audio;
mod clock;
mod pump;
mod reconnect;
mod ring;
mod sinks;
mod stage;
mod streaming;

pub use audio::*;
use clock::*;
use pump::*;
use reconnect::{Change, Portal, Starter};
use ring::*;
use sinks::*;
pub(crate) use stage::*;
use streaming::*;

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

/// Share of the GPU encoder's time ([`Encoder::vcn_load`]) above which the
/// user is told frames may be dropped. Quality is never lowered for it.
const VCN_OVERLOAD: f64 = 0.9;

/// Recording and own-encode stream together above this share of the GPU
/// encoder's time: the stream opens at the faster quality level (in CBR,
/// measured as good as the slower one).
const VCN_STREAM_FAST: f64 = 0.85;

/// What the user can do when the GPU encoder cannot keep up with a
/// recording in `codec`. `stream`: the codec of a stream that, with the
/// recording, is too much.
fn overload_notice(codec: Codec, stream: Option<Codec>) -> String {
    let what = if stream.is_some() {
        tr!(
            "The GPU encoder cannot keep up with both the recording and the stream: frames may be dropped."
        )
    } else {
        tr!("The GPU encoder cannot keep up with this size and frame rate: frames may be dropped.")
    };
    // AV1 has no faster level: an HEVC stream takes far less of the encoder.
    let advice = if stream == Some(Codec::Av1) {
        tr!("Choose HEVC for the stream, or a lower frame rate or size.")
    } else if codec == Codec::Hevc {
        tr!("Choose a lower frame rate or size.")
    } else {
        tr!("Choose HEVC, or a lower frame rate or size.")
    };
    format!("{what} {advice}")
}

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
    /// The desktop and output tracks now leave out these apps (their app
    /// tracks started, lost their source or got it back).
    DesktopExcept(Vec<String>),
    /// A track got louder than the meters' floor after they went to rest
    /// (see [`Pipeline::wake_meters_on_signal`]).
    AudioSignal,
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

/// Queues the H.264 retry of a stream whose codec the server rejected,
/// unless the user stopped it (the stop then sends its one `Ended`) or
/// started another one meanwhile.
fn queue_retry(c: &mut Control, request: StreamRequest, notice: String, on_event: &OnEvent) {
    if !c.stop_stream && c.stream.is_none() {
        on_event(Event::Notice(notice));
        c.stream = Some(request);
    }
}

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
        Self::spawn(settings, replay, record, stream, None, on_event)
    }

    /// [`Pipeline::start`], capturing with `capture` instead of the portal
    /// (when not in game mode).
    fn spawn(
        settings: Settings,
        replay: Option<Duration>,
        record: Option<PathBuf>,
        stream: Option<StreamRequest>,
        capture: Option<Starter>,
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
                    run(
                        &settings,
                        capture,
                        &slot,
                        &ctl,
                        &started_flag,
                        &sources,
                        &on_event,
                    )
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

    /// Sets the ducking of track `index`, live.
    pub fn set_ducking(&self, index: usize, ducking: Ducking) {
        let mut audio = lock(&self.audio);
        let (track, capture) = audio.track(index);
        if let Some(t) = track {
            t.ducking = ducking;
        }
        if let Some(c) = capture {
            c.set_ducking(ducking.settings());
            c.set_speaks(ducking.voice);
        }
    }

    /// How much the duck lowers track `index` now, dB; 0 when it does not run.
    pub fn ducking_db(&self, index: usize) -> f32 {
        let audio = lock(&self.audio);
        audio
            .running
            .iter()
            .find(|(i, _)| *i == index)
            .map_or(0.0, |(_, capture)| capture.ducking_db())
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

    /// Peaks of track `index` since the last call; silence when it does not
    /// run.
    pub fn take_peaks(&self, index: usize) -> Peaks {
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
            .map_or(Peaks::SILENT, |(_, capture)| capture.take_peaks())
    }

    /// The meters rest: the next track louder than their floor sends
    /// [`Event::AudioSignal`], once. Returns true when a track was already
    /// louder since the last take: the meters must keep ticking.
    pub fn wake_meters_on_signal(&self) -> bool {
        let audio = lock(&self.audio);
        // Every track is asked, even after a loud one.
        audio
            .running
            .iter()
            .fold(false, |loud, (_, capture)| capture.wake_on_signal() | loud)
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
    // Private (0600, the token lets anyone running as this user record the
    // source) and atomic: a crash mid-write must not leave an empty token.
    crate::prefs::write_file(&path, token.as_bytes(), true)
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
    #[test]
    fn a_stop_during_the_h264_retry_wins() {
        use crate::stream::{Service, StreamSettings};
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let on_event: OnEvent = Arc::new(move |e| lock(&s).push(format!("{e:?}")));
        let request = || StreamRequest {
            settings: StreamSettings {
                service: Service::Custom,
                ..StreamSettings::default()
            },
            key: None,
        };
        // Stopped meanwhile: no retry, and nothing sent here (the stop
        // sends the one `Ended`).
        let mut c = Control {
            stop_stream: true,
            ..Control::default()
        };
        queue_retry(&mut c, request(), "n".into(), &on_event);
        assert!(c.stream.is_none() && lock(&seen).is_empty());
        // Another stream asked for meanwhile: it stays.
        let mut c = Control {
            stream: Some(StreamRequest {
                key: crate::stream::StreamKey::new("other"),
                ..request()
            }),
            ..Control::default()
        };
        queue_retry(&mut c, request(), "n".into(), &on_event);
        assert!(c.stream.as_ref().unwrap().key.is_some() && lock(&seen).is_empty());
        // Otherwise: retried, with the notice.
        let mut c = Control::default();
        queue_retry(&mut c, request(), "n".into(), &on_event);
        assert!(c.stream.is_some());
        assert_eq!(lock(&seen).len(), 1);
    }

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
                None,
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
    fn desktop_leaves_out_the_enabled_app_tracks_that_ask() {
        let app = |name: &str| Track::new(name, Source::App(name.into()));
        let tracks = [
            Track::new("Desktop", Source::Desktop),
            Track::new("Mic", Source::Microphone),
            app("Firefox"),
            Track {
                leave_desktop: false,
                ..app("mpv")
            },
            Track {
                enabled: false,
                ..app("Spotify")
            },
            // Muted still records nothing of it: still left out.
            Track {
                muted: true,
                ..app("Discord")
            },
        ];
        assert_eq!(desktop_except(&tracks), ["Firefox", "Discord"]);
        assert!(desktop_except(&tracks[..2]).is_empty());
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
