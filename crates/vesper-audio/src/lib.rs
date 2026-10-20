//! Audio capture through PipeWire (desktop, microphone, a device, or an
//! application's streams), encoded to Opus.
//!
//! [`AudioCapture::start`] runs one PipeWire stream (its own thread, its
//! samples copied on PipeWire's real-time thread into a lock-free ring) and
//! one encode thread per source, woken once per 20 ms block whatever the
//! graph quantum. Packets carry `CLOCK_MONOTONIC` pts like the video frames,
//! so the muxer can put both on one time base. Tracks started
//! with the same anchor are sample-aligned, so a [`Mix`] can sum them.
//! [`NodeWatch`] lists the devices and applications that can be recorded.
//!
//! Timestamps come from a sample counter anchored at the start time, so a
//! track has no gaps. Each PipeWire buffer is also placed on the monotonic
//! clock (graph cycle time, minus the mic's capture delay or plus the desktop
//! sink's playback delay, minus queued buffers); when the
//! counter is off from that by more than [`TOLERANCE`] (startup offset, lost
//! buffers, a suspended device), silence is inserted or input dropped at once.
//! Slow device clock drift is followed instead: the low-pass filtered error is
//! corrected by repeating or dropping single frames, at most one per buffer
//! and per 480 frames (~2000 ppm ceiling up to 480-frame buffers, ~977 ppm at
//! 1024, ~488 ppm at 2048), each faded in over up to 100 ms ([`Slew`]) so it
//! does not click.
//!
//! Voice ducking: tracks started with the same [`VoiceKey`] talk through it.
//! A voice track ([`Options::speaks`]) tells it when it speaks, from its raw
//! input (before its filters and volume, so neither moves the detector's
//! thresholds; a muted voice does not listen at all, its detector keeps its
//! state for the unmute); every other track ducks under it as
//! its [`DuckSettings`] say, before its volume, so the duck is in the
//! track's own file and in the mix alike. They meet lock-free on the
//! timeline: the voice marks its voiced windows at their timeline frames
//! (its raw input sits there already: its capture delay is taken off when
//! it is placed, and its filters come after), and a track that ducks holds
//! its audio back by [`LOOKAHEAD_FRAMES`] (a processing latency, taken off
//! its timeline like a plugin's) to read the voice that far past each
//! block. A playback track's input runs ahead of a microphone's in real
//! time by the output and input delays: past what the voice has told,
//! nothing is known, so it eats into the lookahead (a Bluetooth headset
//! takes all of it), but no track ever waits for another.

mod aac;
mod exclusions;
mod mix;
mod nodes;
mod pw;
mod ring;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use ffmpeg::format::Sample;
use ffmpeg::format::sample::Type;
use ffmpeg::{ChannelLayout, codec, encoder, frame};
use ffmpeg_next as ffmpeg;
use tracing::{error, warn};
use vesper_core::EncodedPacket;
use vesper_dsp::{Ducker, LOOKAHEAD_FRAMES, VoiceDetector};

pub use aac::Aac;
pub use exclusions::{Exclusions, Hold, OnExclusions};
pub use mix::{Mix, MixInput};
pub use nodes::{Node, NodeWatch};
pub use vesper_dsp::{DuckSettings, VoiceKey};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

/// Per-track processing of each 20 ms block (interleaved stereo F32), run on
/// the encode thread before the volume and the encoder. Returns its current
/// delay in frames: the encoder drops that much of its output (or inserts
/// silence when it shrinks), so the track and its pts stay on the timeline
/// and the tracks of a mix stay aligned.
pub type Process = Box<dyn FnMut(&mut [f32]) -> usize + Send>;

/// Gets a track's samples as they are encoded: 20 ms blocks of interleaved
/// stereo F32, each with the pts of its first sample. Runs on the track's
/// encode thread (for a [`Mix`], whichever input thread completes a block).
pub type Tap = Box<dyn FnMut(Duration, &[f32]) + Send>;

/// Output sample rate of every track.
pub const SAMPLE_RATE: u32 = 48_000;
/// Output channels of every track (mono sources are upmixed by PipeWire).
pub const CHANNELS: u16 = 2;
/// Opus frame: 20 ms.
const FRAME: usize = 960;
const BIT_RATE: usize = 160_000;
/// Peak at or above which a block counts as clipped: full scale (an integer
/// source's loudest sample is a hair under 1.0).
const CLIP_PEAK: f32 = 0.999;
/// Peak above which a block wakes a resting meter: -60 dBFS, the meter's
/// floor.
const SIGNAL_PEAK: f32 = 0.001;
/// Left and right differing by more than this (-60 dBFS) is stereo content,
/// not an upmixed mono source.
const STEREO_DIFF: f32 = 0.001;
/// Timeline error allowed before inserting silence or dropping input at once:
/// above timestamp jitter (sub-ms graph clock), far above a slow drift step.
pub const TOLERANCE: Duration = Duration::from_millis(5);
/// Filtered timeline error that triggers a one-frame drift correction.
const DRIFT_THRESHOLD_NS: f64 = 1_000_000.0;
/// Frames between two drift corrections at least (10 ms). There is also at
/// most one per buffer: the ceiling is 1 / max(buffer, 480) frames, ~2000 ppm
/// up to 480-frame buffers, ~977 ppm at 1024 and ~488 ppm at 2048.
const DRIFT_SPACING: usize = 480;
/// Weight of each buffer's error in the drift filter (~0.6 s at 20 ms buffers).
const DRIFT_ALPHA: f64 = 1.0 / 32.0;

/// With no buffer for this long, the source is stalled: fill with silence.
const STALL: Duration = Duration::from_millis(250);
/// How often a stalled track is then filled: the mix and a live stream wait
/// for its slowest track.
const STALL_FILL: Duration = Duration::from_millis(20);
/// Longest crossfade of a drift correction (100 ms): artefacts of a
/// correction every 0.4 s (50 ppm) fall from -24 dB to under -60 dB on an
/// 8 kHz tone.
const SLEW_FRAMES: usize = 4_800;

/// Lowest volume: at or below it a track is silent.
pub const MIN_DB: f32 = -60.0;
/// Highest volume.
pub const MAX_DB: f32 = 12.0;

/// Linear gain of a volume in dB, clamped to `MIN_DB..=MAX_DB`; `MIN_DB` is silence.
pub fn db_to_gain(db: f32) -> f32 {
    if db.is_nan() || db <= MIN_DB {
        0.0
    } else {
        10f32.powf(db.min(MAX_DB) / 20.0)
    }
}

/// A track's side of a [`VoiceKey`]: it speaks into it or ducks under it,
/// as `Shared::speaks` says (live).
struct Voice {
    key: Arc<VoiceKey>,
    detector: VoiceDetector,
    ducker: Box<Ducker>,
    /// Was a voice at the last block (counted in `key`'s voices).
    speaks: bool,
    /// The lookahead's delay line, interleaved, while `ahead`: the
    /// processed audio waits [`LOOKAHEAD_FRAMES`] in it.
    delay: Box<[f32]>,
    pos: usize,
    ahead: bool,
}

impl Voice {
    fn new(key: Arc<VoiceKey>) -> Self {
        let ch = usize::from(CHANNELS);
        Self {
            key,
            detector: VoiceDetector::new(ch),
            ducker: Box::new(Ducker::new(ch)),
            speaks: false,
            delay: vec![0.0; LOOKAHEAD_FRAMES * ch].into(),
            pos: 0,
            ahead: false,
        }
    }
}

impl Drop for Voice {
    fn drop(&mut self) {
        if self.speaks {
            self.key.remove_voice();
        }
    }
}

/// Which audio to record. Devices are named by their PipeWire `node.name`,
/// which stays the same across reboots.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    /// Monitor of the default output device (follows the default).
    Desktop,
    /// Default input device (follows the default).
    Microphone,
    /// Monitor of this output device (sink).
    Output(String),
    /// This input device (source).
    Input(String),
    /// Every playback stream of this application (`application.name`, or
    /// `application.process.binary`), including streams it opens later.
    App(String),
}

impl Source {
    /// Whether it records an input (its buffers were captured in the past)
    /// rather than playback (heard after the output's delay).
    fn is_input(&self) -> bool {
        matches!(self, Self::Microphone | Self::Input(_))
    }
}

/// How a capture starts.
pub struct Options {
    /// `CLOCK_MONOTONIC` start of the track's timeline. Tracks meant to be
    /// mixed share it: their samples then line up one to one.
    pub anchor: Duration,
    pub muted: bool,
    /// Volume in dB, see [`db_to_gain`].
    pub volume_db: f32,
    /// Also feeds this mix with the track's samples, after volume.
    pub mix: Option<MixInput>,
    /// Processing before volume and encoding; `None` leaves samples as is.
    pub process: Option<Process>,
    /// Told when the source is lost and when it is back, on the capture's
    /// PipeWire thread.
    pub on_state: Option<OnState>,
    /// Called on the encode thread when a block is louder than the meter's
    /// floor after [`AudioCapture::wake_on_signal`]; once per call of it.
    pub on_signal: Option<OnSignal>,
    /// Voice ducking (see the crate docs): a track that `speaks` speaks
    /// into it, the others duck under it.
    pub voice: Option<Arc<VoiceKey>>,
    /// The track is a voice (a microphone): see [`AudioCapture::set_speaks`].
    pub speaks: bool,
    /// Ducking and background music of a track that is not a voice.
    pub ducking: DuckSettings,
    /// Apps with their own track, left out of the desktop: a
    /// [`Source::App`] track holds its app there while it captures; a
    /// [`Source::Desktop`] or [`Source::Output`] track leaves out the apps
    /// held, following changes. It then links the sink's other playback
    /// streams to itself instead of reading its monitor; with no app held
    /// it reads the monitor, as without this. `None`: neither.
    pub exclusions: Option<Arc<Exclusions>>,
}

/// See [`Options::on_signal`].
pub type OnSignal = Box<dyn Fn() + Send>;

/// Peak levels of a track since they were last taken, after volume and mute,
/// for a level meter. See [`AudioCapture::take_peaks`].
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Peaks {
    /// Linear sample peaks: 1.0 is full scale, more is over.
    pub left: f32,
    pub right: f32,
    /// A block reached full scale.
    pub clipped: bool,
    /// Left and right have differed since the capture started: not a mono
    /// source upmixed to both.
    pub stereo: bool,
    /// At least one block came since the last take. Blocks are 20 ms and
    /// come in bursts: a meter polling at 30 Hz sometimes gets none, which
    /// is not silence.
    pub fresh: bool,
}

impl Peaks {
    /// Silence, as a block would report it: what a track that does not run
    /// shows (`Default` is "no block came", which keeps the last peaks).
    pub const SILENT: Self = Self {
        left: 0.0,
        right: 0.0,
        clipped: false,
        stereo: false,
        fresh: true,
    };
}

/// Where a running track's source is at, for [`Options::on_state`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackState {
    /// Capturing again after a reconnection.
    Running,
    /// The source is gone (PipeWire restarted, device removed), for this
    /// reason: reconnecting until it is back, recording silence meanwhile.
    Reconnecting(String),
}

/// See [`Options::on_state`].
pub type OnState = Box<dyn Fn(TrackState) + Send>;

/// State shared with the encode thread.
struct Shared {
    muted: AtomicBool,
    /// `f32` bits of the linear gain.
    gain: AtomicU32,
    /// `f32` bits of the peaks since they were last taken. Non-negative
    /// floats order like their bits, so `fetch_max` keeps the loudest.
    left: AtomicU32,
    right: AtomicU32,
    clipped: AtomicBool,
    stereo: AtomicBool,
    fresh: AtomicBool,
    /// The next block above [`SIGNAL_PEAK`] calls `on_signal`.
    wake: AtomicBool,
    /// `f32` bits of [`DuckSettings::duck_db`].
    duck_db: AtomicU32,
    /// `f32` bits of [`DuckSettings::music_lufs`], 0 for `None`.
    music_lufs: AtomicU32,
    /// `f32` bits of the duck's current reduction, dB.
    ducking: AtomicU32,
    /// A voice: it speaks into the [`VoiceKey`] instead of ducking.
    speaks: AtomicBool,
}

impl Shared {
    fn set_ducking(&self, settings: DuckSettings) {
        let (duck, music) = (
            settings.duck_db.max(0.0),
            settings.music_lufs.unwrap_or(0.0),
        );
        self.duck_db.store(duck.to_bits(), Ordering::Relaxed);
        self.music_lufs.store(music.to_bits(), Ordering::Relaxed);
    }

    fn ducking(&self) -> DuckSettings {
        let music = f32::from_bits(self.music_lufs.load(Ordering::Relaxed));
        DuckSettings {
            duck_db: f32::from_bits(self.duck_db.load(Ordering::Relaxed)),
            music_lufs: (music < 0.0).then_some(music),
        }
    }
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            muted: AtomicBool::new(false),
            gain: AtomicU32::new(1f32.to_bits()),
            left: AtomicU32::new(0),
            right: AtomicU32::new(0),
            clipped: AtomicBool::new(false),
            stereo: AtomicBool::new(false),
            fresh: AtomicBool::new(false),
            wake: AtomicBool::new(false),
            duck_db: AtomicU32::new(0),
            music_lufs: AtomicU32::new(0),
            ducking: AtomicU32::new(0),
            speaks: AtomicBool::new(false),
        }
    }
}

impl Shared {
    fn take_peaks(&self) -> Peaks {
        let take = |a: &AtomicU32| f32::from_bits(a.swap(0, Ordering::Relaxed));
        Peaks {
            left: take(&self.left),
            right: take(&self.right),
            clipped: self.clipped.swap(false, Ordering::Relaxed),
            stereo: self.stereo.load(Ordering::Relaxed),
            fresh: self.fresh.swap(false, Ordering::Relaxed),
        }
    }

    /// See [`AudioCapture::wake_on_signal`].
    fn wake_on_signal(&self) -> bool {
        self.wake.store(true, Ordering::Relaxed);
        // A block stored between the last take and the store above did not
        // see the request: look at what it left.
        let loud = |a: &AtomicU32| f32::from_bits(a.load(Ordering::Relaxed)) > SIGNAL_PEAK;
        // Taking the request back: the encode thread may have just fired it,
        // then the event wakes the meter anyway.
        (loud(&self.left) || loud(&self.right)) && self.wake.swap(false, Ordering::Relaxed)
    }
}

/// A running audio capture. Dropping it stops the capture.
pub struct AudioCapture {
    shared: Arc<Shared>,
    extradata: Vec<u8>,
    stop: ::pipewire::channel::Sender<()>,
    pw_thread: Option<JoinHandle<()>>,
    encode_thread: Option<JoinHandle<()>>,
}

impl AudioCapture {
    /// Starts capturing `source`. `on_packet` runs on the encode thread with
    /// one Opus packet per 20 ms, without gaps, until the capture stops.
    pub fn start(
        source: Source,
        options: Options,
        on_packet: impl FnMut(EncodedPacket) + Send + 'static,
    ) -> Result<Self> {
        let encoder = open_encoder()?;
        let extradata = opus_head(&encoder)?;
        let shared = Arc::new(Shared::default());
        shared.muted.store(options.muted, Ordering::Relaxed);
        shared
            .gain
            .store(db_to_gain(options.volume_db).to_bits(), Ordering::Relaxed);
        shared.set_ducking(options.ducking);
        // A time's worth of audio, whatever the quantum.
        let (chunk_tx, chunk_rx) = ring::ring();
        let mut enc = Encoder::new(encoder, options.anchor, shared.clone(), on_packet);
        enc.process = options.process;
        enc.mix = options.mix;
        enc.on_signal = options.on_signal;
        shared.speaks.store(options.speaks, Ordering::Relaxed);
        enc.voice = options.voice.map(Voice::new);
        let encode_thread = std::thread::Builder::new()
            .name("vesper-audio-enc".into())
            .spawn({
                let input = source.is_input();
                move || encode_loop(enc, chunk_rx, input)
            })?;
        let (stop, stop_rx) = ::pipewire::channel::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (on_state, except) = (options.on_state, options.exclusions);
        let pw_thread = std::thread::Builder::new()
            .name("vesper-audio-pw".into())
            .spawn(move || pw::run(source, except, chunk_tx, stop_rx, ready_tx, on_state))?;
        let capture = Self {
            shared,
            extradata,
            stop,
            pw_thread: Some(pw_thread),
            encode_thread: Some(encode_thread),
        };
        // On error, dropping `capture` joins both threads.
        ready_rx.recv().map_err(|_| "pipewire thread died")??;
        Ok(capture)
    }

    /// Muted tracks keep producing packets, of silence.
    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Relaxed);
    }

    /// Volume in dB, see [`db_to_gain`]. Ramped over the next 20 ms.
    pub fn set_volume_db(&self, db: f32) {
        self.shared
            .gain
            .store(db_to_gain(db).to_bits(), Ordering::Relaxed);
    }

    /// Peaks since the last call (0 while muted), and whether a block
    /// clipped since then. Cheap: a few atomics.
    pub fn take_peaks(&self) -> Peaks {
        self.shared.take_peaks()
    }

    /// The next block louder than the meter's floor calls
    /// [`Options::on_signal`]: a resting meter needs no polling. Returns
    /// true, without arming, when such a block already came since the
    /// peaks were last taken: the meter must not rest.
    pub fn wake_on_signal(&self) -> bool {
        self.shared.wake_on_signal()
    }

    /// Ducking and background music, live (see [`Options::ducking`]).
    pub fn set_ducking(&self, settings: DuckSettings) {
        self.shared.set_ducking(settings);
    }

    /// Whether the track is a voice, live: it then speaks into the
    /// [`VoiceKey`] instead of ducking under it.
    pub fn set_speaks(&self, speaks: bool) {
        self.shared.speaks.store(speaks, Ordering::Relaxed);
    }

    /// How much the duck lowers the track now, dB (>= 0).
    pub fn ducking_db(&self) -> f32 {
        f32::from_bits(self.shared.ducking.load(Ordering::Relaxed))
    }

    /// `OpusHead` for the container's codec private data.
    pub fn extradata(&self) -> &[u8] {
        &self.extradata
    }

    /// Stops the capture and flushes the encoder. Same as dropping it.
    pub fn stop(self) {}
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        // The PipeWire thread drops the ring's writer on exit, which ends the
        // encode thread after it drained and flushed.
        for thread in [self.pw_thread.take(), self.encode_thread.take()]
            .into_iter()
            .flatten()
        {
            if thread.join().is_err() {
                warn!("audio thread panicked");
            }
        }
    }
}

/// The encoder's `OpusHead`.
fn opus_head(encoder: &encoder::Audio) -> Result<Vec<u8>> {
    // SAFETY: the opened encoder owns valid extradata of that size.
    unsafe {
        let ctx = encoder.as_ptr();
        if (*ctx).extradata.is_null() {
            return Err("libopus produced no OpusHead".into());
        }
        Ok(std::slice::from_raw_parts((*ctx).extradata, (*ctx).extradata_size as usize).to_vec())
    }
}

fn open_encoder() -> Result<encoder::Audio> {
    let codec = encoder::find_by_name("libopus").ok_or("FFmpeg has no libopus encoder")?;
    let mut ctx = codec::Context::new_with_codec(codec).encoder().audio()?;
    ctx.set_rate(SAMPLE_RATE as i32);
    ctx.set_channel_layout(ChannelLayout::STEREO);
    ctx.set_format(Sample::F32(Type::Packed));
    ctx.set_bit_rate(BIT_RATE);
    ctx.set_time_base((1, SAMPLE_RATE as i32));
    let mut opts = ffmpeg::Dictionary::new();
    opts.set("frame_duration", "20");
    let encoder = ctx.open_as_with(codec, opts)?;
    if encoder.frame_size() as usize != FRAME {
        return Err(format!("unexpected Opus frame size {}", encoder.frame_size()).into());
    }
    Ok(encoder)
}

/// Now on `CLOCK_MONOTONIC`, the clock of every pts.
pub fn monotonic_now() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid pointer; CLOCK_MONOTONIC always exists on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

pub(crate) fn frames_to_duration(frames: usize) -> Duration {
    Duration::from_nanos(frames as u64 * 1_000_000_000 / u64::from(SAMPLE_RATE))
}

fn duration_to_frames(d: Duration) -> usize {
    (d.as_nanos() * u128::from(SAMPLE_RATE) / 1_000_000_000) as usize
}

/// Sample-counted position of a track on the monotonic clock.
struct Timeline {
    anchor: Duration,
    /// Frames placed so far.
    frames: usize,
    /// Low-pass filtered `start - expected` of the recent buffers, in ns.
    drift_ns: f64,
    /// Frames placed since the last drift correction.
    since_fix: usize,
}

impl Timeline {
    fn new(anchor: Duration) -> Self {
        Self {
            anchor,
            frames: 0,
            drift_ns: 0.0,
            since_fix: 0,
        }
    }

    /// Places `len` frames measured to start at `start`. Returns how many
    /// silent frames to insert before them, how many of them to drop, and how
    /// many times to repeat their first frame (drift correction, 0 or 1).
    fn place(&mut self, start: Duration, len: usize) -> (usize, usize, usize) {
        let expected = self.anchor + frames_to_duration(self.frames);
        let (pad, skip, repeat) = if start > expected + TOLERANCE {
            (self.drift_ns, self.since_fix) = (0.0, 0);
            (duration_to_frames(start - expected), 0, 0)
        } else if start + TOLERANCE < expected {
            (self.drift_ns, self.since_fix) = (0.0, 0);
            (0, duration_to_frames(expected - start).min(len), 0)
        } else if len == 0 {
            (0, 0, 0)
        } else {
            let err_ns = (start.as_nanos() as i128 - expected.as_nanos() as i128) as f64;
            self.drift_ns += (err_ns - self.drift_ns) * DRIFT_ALPHA;
            self.since_fix += len;
            if self.since_fix >= DRIFT_SPACING && self.drift_ns.abs() > DRIFT_THRESHOLD_NS {
                self.since_fix = 0;
                // The correction moves the error by one frame right away.
                let frame_ns = 1e9 / f64::from(SAMPLE_RATE);
                if self.drift_ns > 0.0 {
                    self.drift_ns -= frame_ns;
                    (0, 0, 1)
                } else {
                    self.drift_ns += frame_ns;
                    (0, 1, 0)
                }
            } else {
                (0, 0, 0)
            }
        };
        self.frames += pad + repeat + len - skip;
        (pad, skip, repeat)
    }
}

/// Ramps the gain of interleaved stereo `samples` from `from` to `to` over
/// the block, so a volume change or a mute does not click.
fn apply_gain(samples: &mut [f32], from: f32, to: f32) {
    if from == to {
        if to != 1.0 {
            samples.iter_mut().for_each(|s| *s *= to);
        }
        return;
    }
    let ch = usize::from(CHANNELS);
    let frames = samples.len() / ch;
    for (i, frame) in samples.chunks_mut(ch).enumerate() {
        let g = from + (to - from) * (i + 1) as f32 / frames as f32;
        frame.iter_mut().for_each(|s| *s *= g);
    }
}

/// Appends processed `samples` to `ready`, less the first `skip` frames
/// (the processing's delay still to remove).
fn emit(ready: &mut Vec<f32>, skip: &mut usize, samples: &[f32]) {
    let ch = usize::from(CHANNELS);
    let n = (*skip).min(samples.len() / ch);
    *skip -= n;
    ready.extend_from_slice(&samples[n * ch..]);
}

/// Feeds samples to the encoder 20 ms at a time and tracks the peak.
struct Encoder<F> {
    encoder: encoder::Audio,
    frame: frame::Audio,
    pending: Vec<f32>,
    /// The 20 ms block being processed.
    block: Vec<f32>,
    /// Processed samples, delay removed, waiting for a whole block.
    ready: Vec<f32>,
    /// Delay of the processing that `ready` accounts for, frames.
    delay: usize,
    /// Processed frames still to drop to remove that delay.
    skip: usize,
    /// Frames sent to the encoder, the frame pts in 1/48000 s.
    sent: i64,
    anchor: Duration,
    shared: Arc<Shared>,
    /// Gain reached at the end of the last block.
    gain: f32,
    process: Option<Process>,
    /// Voice ducking, see [`Options::voice`].
    voice: Option<Voice>,
    /// Frames taken from `pending` (the input's timeline position), and
    /// the anchor in frames: a [`VoiceKey`]'s timeline frames are their sum.
    fed: u64,
    origin: u64,
    /// In `finish`: the lookahead stays as it is, so its line drains
    /// through the padding instead of coming out with an extra block.
    ending: bool,
    mix: Option<MixInput>,
    /// Gets each block as encoded, with its pts (a live stream's audio).
    tap: Option<Tap>,
    /// Off: blocks go to `tap` only, nothing is encoded (a mix kept for a
    /// stream while the files have none).
    opus: bool,
    on_signal: Option<OnSignal>,
    on_packet: F,
}

impl<F: FnMut(EncodedPacket)> Encoder<F> {
    fn new(encoder: encoder::Audio, anchor: Duration, shared: Arc<Shared>, on_packet: F) -> Self {
        let mut frame = frame::Audio::new(Sample::F32(Type::Packed), FRAME, ChannelLayout::STEREO);
        frame.set_rate(SAMPLE_RATE);
        Self {
            encoder,
            frame,
            pending: Vec::new(),
            block: Vec::with_capacity(FRAME * usize::from(CHANNELS)),
            // Room for a block and a delay change (the lookahead's line
            // let out at once), so it rarely grows.
            ready: Vec::with_capacity((4 * FRAME + LOOKAHEAD_FRAMES) * usize::from(CHANNELS)),
            delay: 0,
            skip: 0,
            sent: 0,
            anchor,
            gain: f32::from_bits(shared.gain.load(Ordering::Relaxed)),
            shared,
            process: None,
            voice: None,
            fed: 0,
            origin: duration_to_frames(anchor) as u64,
            ending: false,
            mix: None,
            tap: None,
            opus: true,
            on_signal: None,
            on_packet,
        }
    }

    /// Frames waiting for a whole block: the next one needs `FRAME` minus that.
    fn pending_frames(&self) -> usize {
        self.pending.len() / usize::from(CHANNELS)
    }

    fn push(&mut self, samples: &[f32]) {
        self.pending.extend_from_slice(samples);
        let ch = usize::from(CHANNELS);
        let len = FRAME * ch;
        while self.pending.len() >= len {
            self.block.clear();
            self.block.extend(self.pending.drain(..len));
            self.detect();
            self.fed += FRAME as u64;
            // The DSP seam: a no-op unless a processor was given.
            let delay = match &mut self.process {
                Some(process) => process(&mut self.block),
                None => 0,
            };
            let ahead = self.lookahead();
            self.set_delay(delay + ahead);
            emit(&mut self.ready, &mut self.skip, &self.block);
            while self.ready.len() >= len {
                self.block.clear();
                self.block.extend(self.ready.drain(..len));
                self.encode_block();
            }
        }
    }

    /// Voice detection on the raw block in `self.block`, before the filters
    /// and volume. Muted, it does not listen: a mute is not silence to learn.
    fn detect(&mut self) {
        let speaks = self.shared.speaks.load(Ordering::Relaxed);
        let Some(v) = &mut self.voice else { return };
        if speaks && !v.speaks {
            // A voice from now: it starts listening afresh.
            v.detector.reset();
            v.key.add_voice();
        } else if !speaks && v.speaks {
            v.key.remove_voice();
        }
        v.speaks = speaks;
        if !speaks {
            v.detector.pause();
            return;
        }
        // The raw block sits at its timeline position: the capture delay
        // was taken off when it was placed, the filters come after.
        let at = self.origin + self.fed;
        if self.shared.muted.load(Ordering::Relaxed) {
            v.detector.pause();
        } else {
            let key = &v.key;
            v.detector.process_windows(&self.block, |end, voiced| {
                if voiced {
                    key.voiced(at + end as u64);
                }
            });
        }
        // Muted, it tells it does not speak.
        v.key.heard(at + FRAME as u64);
    }

    /// The lookahead of a track that ducks: delays the processed block in
    /// `self.block` by [`LOOKAHEAD_FRAMES`] while the track ducks under a
    /// voice, and returns that delay (0 otherwise). Turned on, the line
    /// starts silent and `set_delay` drops that silence; turned off, what
    /// it held comes out before the block. Either way the output goes on
    /// sample for sample: no gap, no jump.
    fn lookahead(&mut self) -> usize {
        let Some(v) = &mut self.voice else { return 0 };
        let set = self.shared.ducking();
        let ducks =
            !v.speaks && v.key.has_voice() && (set.duck_db > 0.0 || set.music_lufs.is_some());
        if ducks != v.ahead && !self.ending {
            v.ahead = ducks;
            if ducks {
                v.delay.fill(0.0);
                v.pos = 0;
            } else {
                let (newest, oldest) = v.delay.split_at(v.pos);
                emit(&mut self.ready, &mut self.skip, oldest);
                emit(&mut self.ready, &mut self.skip, newest);
                // Out already: not to be filled in by `set_delay`.
                self.delay -= LOOKAHEAD_FRAMES;
            }
        }
        if !v.ahead {
            return 0;
        }
        // The block (shorter than the line) swaps places with its oldest
        // samples, wrapping around.
        let (n, len) = (self.block.len(), v.delay.len());
        let first = n.min(len - v.pos);
        let (a, b) = self.block.split_at_mut(first);
        a.swap_with_slice(&mut v.delay[v.pos..v.pos + first]);
        b.swap_with_slice(&mut v.delay[..n - first]);
        v.pos = (v.pos + n) % len;
        LOOKAHEAD_FRAMES
    }

    /// Starts over at frame `at` of the timeline (a mix that was idle):
    /// what waits is dropped, and so is the processing's delay of old audio.
    fn restart_at(&mut self, at: u64) {
        self.pending.clear();
        self.ready.clear();
        self.skip = self.delay;
        self.sent = at as i64;
        self.fed = at;
    }

    /// Follows a change of the processing's delay. Output frame `n` must be
    /// input frame `n`: a longer delay drops output, a shorter one inserts
    /// silence where the old processing still held audio.
    fn set_delay(&mut self, delay: usize) {
        if delay >= self.delay {
            self.skip += delay - self.delay;
        } else {
            let mut short = self.delay - delay;
            let cancel = short.min(self.skip);
            (self.skip, short) = (self.skip - cancel, short - cancel);
            let ch = usize::from(CHANNELS);
            self.ready.resize(self.ready.len() + short * ch, 0.0);
        }
        self.delay = delay;
    }

    /// Ducking, volume, voice, mix, meter and encoder for the block in
    /// `self.block`.
    fn encode_block(&mut self) {
        let at = self.anchor + frames_to_duration(self.sent as usize);
        let speaks = self.shared.speaks.load(Ordering::Relaxed);
        if let Some(v) = &mut self.voice {
            // A voice lets a duck it was in come back up smoothly (a no-op
            // once idle): a jump would click.
            let (settings, speaking) = if speaks {
                (DuckSettings::default(), false)
            } else {
                let ahead = if v.ahead { LOOKAHEAD_FRAMES } else { 0 };
                let at = self.origin + self.sent as u64;
                let speaking = v.key.has_voice() && v.key.ducks(at, FRAME, ahead);
                (self.shared.ducking(), speaking)
            };
            v.ducker.process(&mut self.block, settings, speaking);
            let db = v.ducker.ducking_db();
            self.shared.ducking.store(db.to_bits(), Ordering::Relaxed);
        }
        let gain = if self.shared.muted.load(Ordering::Relaxed) {
            0.0
        } else {
            f32::from_bits(self.shared.gain.load(Ordering::Relaxed))
        };
        apply_gain(&mut self.block, self.gain, gain);
        self.gain = gain;
        self.meter();
        if let Some(mix) = &self.mix {
            mix.push(&self.block, self.sent as u64);
        }
        if let Some(tap) = &mut self.tap {
            tap(at, &self.block);
        }
        if !self.opus {
            self.sent += FRAME as i64;
            return;
        }
        let out = self.frame.data_mut(0);
        for (dst, &s) in out.as_chunks_mut::<4>().0.iter_mut().zip(&self.block) {
            dst.copy_from_slice(&s.to_le_bytes());
        }
        self.frame.set_pts(Some(self.sent));
        self.sent += FRAME as i64;
        if let Err(e) = self.encoder.send_frame(&self.frame) {
            error!("opus encode failed: {e}");
        }
        self.drain();
    }

    /// Feeds the level meter from the block: per channel max-abs, a few
    /// atomics per 20 ms, no allocation. `f32::max` drops NaN.
    fn meter(&self) {
        let (mut left, mut right, mut side) = (0f32, 0f32, 0f32);
        for &[l, r] in self.block.as_chunks::<2>().0 {
            left = left.max(l.abs());
            right = right.max(r.abs());
            side = side.max((l - r).abs());
        }
        let shared = &self.shared;
        shared.left.fetch_max(left.to_bits(), Ordering::Relaxed);
        shared.right.fetch_max(right.to_bits(), Ordering::Relaxed);
        shared.fresh.store(true, Ordering::Relaxed);
        let peak = left.max(right);
        if peak >= CLIP_PEAK {
            shared.clipped.store(true, Ordering::Relaxed);
        }
        if side > STEREO_DIFF && !shared.stereo.load(Ordering::Relaxed) {
            shared.stereo.store(true, Ordering::Relaxed);
        }
        if peak > SIGNAL_PEAK
            && shared.wake.load(Ordering::Relaxed)
            && shared.wake.swap(false, Ordering::Relaxed)
            && let Some(on_signal) = &self.on_signal
        {
            on_signal();
        }
    }

    fn drain(&mut self) {
        let mut packet = ffmpeg::Packet::empty();
        while self.encoder.receive_packet(&mut packet).is_ok() {
            // libavcodec convention: the first packet starts `pre_skip`
            // samples early (negative pts), so decoded audio lands on `anchor`.
            let pts_ns = packet.pts().unwrap_or(0) as i128 * 1_000_000_000 / SAMPLE_RATE as i128;
            let pts = self.anchor.as_nanos() as i128 + pts_ns;
            (self.on_packet)(EncodedPacket::new(
                Arc::from(packet.data().unwrap_or_default()),
                Duration::from_nanos(pts.max(0) as u64),
                true,
            ));
        }
    }

    fn finish(mut self) {
        // Pad the last partial frame and flush the processing's delay so no
        // captured audio is lost, and no silence added: zeros go in until
        // the output reaches the input's end (the delay read again per
        // block), the lookahead held as it is.
        self.ending = true;
        let ch = usize::from(CHANNELS);
        let len = FRAME * ch;
        let end = self.fed as i64 + self.pending_frames() as i64;
        let zeros = vec![0.0; len];
        self.push(&zeros[..(len - self.pending.len() % len) % len]);
        while self.sent + ((self.ready.len() / ch) as i64) < end {
            self.push(&zeros);
        }
        if !self.ready.is_empty() {
            self.ready.resize(len, 0.0);
            self.block.clear();
            self.block.append(&mut self.ready);
            self.encode_block();
        }
        if self.opus {
            if let Err(e) = self.encoder.send_eof() {
                error!("opus flush failed: {e}");
            }
            self.drain();
        }
        self.shared.take_peaks();
    }
}

// `Slew` works on stereo frames.
const _: () = assert!(CHANNELS == 2);

/// Applies the timeline's drift corrections (one frame repeated or dropped)
/// as a raised-cosine crossfade from the stream to its copy one frame later
/// (a repeat) or earlier (a drop), instead of a jump that clicks on tonal
/// sounds. Each fades over the interval between the timeline's last two
/// corrections (shared with any still owed), up to [`SLEW_FRAMES`], so they
/// keep pace with the drift. The frame count comes
/// out as the timeline counts it, so pts stay exact; the audio is up to one
/// frame (21 µs) off them while a fade runs.
#[derive(Default)]
struct Slew {
    /// Corrections not started yet: + repeats, - drops.
    owed: i32,
    /// The one fading: +1 repeat, -1 drop, 0 none.
    dir: i32,
    pos: usize,
    len: usize,
    /// Input frames since the last `correct`.
    gap: usize,
    /// Input frames between the last two `correct`s (the first: since the start).
    every: usize,
    /// The previous input frame.
    prev: [f32; 2],
    /// Input frames still to drop (a drop applied at once by `flush`).
    drop: usize,
}

impl Slew {
    fn correct(&mut self, dir: i32) {
        (self.every, self.gap) = (self.gap, 0);
        self.owed += dir;
    }

    /// Interleaved stereo `input` into `out`, corrections faded in.
    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        out.clear();
        if self.dir == 0 && self.owed == 0 && self.drop == 0 {
            // Nearly always: nothing to fade.
            self.gap += input.len() / 2;
            self.prev = input.last_chunk().copied().unwrap_or(self.prev);
            out.extend_from_slice(input);
            return;
        }
        for &x in input.as_chunks::<2>().0 {
            self.gap += 1;
            if self.drop > 0 {
                self.drop -= 1;
                self.prev = x;
                continue;
            }
            if self.dir == 0 && self.owed != 0 {
                self.dir = self.owed.signum();
                self.owed -= self.dir;
                let share = self.every / (self.owed.unsigned_abs() as usize + 1);
                (self.pos, self.len) = (0, share.clamp(DRIFT_SPACING, SLEW_FRAMES));
                if self.dir < 0 {
                    // Held back: each frame now comes out blended with the next.
                    self.prev = x;
                    continue;
                }
            }
            if self.dir == 0 {
                out.extend_from_slice(&x);
            } else {
                self.pos += 1;
                let t = self.pos as f32 / self.len as f32;
                let w = 0.5 - 0.5 * (std::f32::consts::PI * t).cos();
                let (a, b) = if self.dir > 0 {
                    (x, self.prev)
                } else {
                    (self.prev, x)
                };
                out.extend_from_slice(&[a[0] + w * (b[0] - a[0]), a[1] + w * (b[1] - a[1])]);
                if self.pos == self.len {
                    if self.dir > 0 {
                        // Now one frame late: this one comes out next.
                        out.extend_from_slice(&x);
                    }
                    self.dir = 0;
                }
            }
            self.prev = x;
        }
    }

    /// Completes every correction at once: a gap or the end follows, which
    /// breaks the waveform anyway. Repeats go to `out`; returns the frames
    /// still to drop, for the caller to take off its silence or else
    /// (`drop`) off the next input.
    fn flush(&mut self, out: &mut Vec<f32>) -> usize {
        out.clear();
        // A drop in progress is already a frame short.
        let owed = self.owed + i32::from(self.dir > 0);
        (self.owed, self.dir) = (0, 0);
        for _ in 0..owed.max(0) {
            out.extend_from_slice(&self.prev);
        }
        owed.min(0).unsigned_abs() as usize
    }
}

fn encode_loop(mut enc: Encoder<impl FnMut(EncodedPacket)>, mut rx: ring::Reader, input: bool) {
    if let Some(v) = &mut enc.voice {
        v.detector.warm_up();
    }
    let mut timeline = Timeline::new(enc.anchor);
    let mut slew = Slew::default();
    let mut out = Vec::new();
    let mut samples = Vec::new();
    let ch = usize::from(CHANNELS);
    let mut stalled = false;
    // Length of the last buffer: about a quantum.
    let mut quantum = Duration::ZERO;
    // When the last buffer reached the ring (not when it was read: the
    // last few may wait there for a whole block), or the last fill: a
    // stall is counted from there.
    let mut since = monotonic_now();
    loop {
        // Before draining: what was pushed before the close is drained.
        let closed = rx.closed();
        let (mut pad, skip, repeat) = if let Some((start, pushed)) = rx.next(&mut samples) {
            (stalled, since) = (false, pushed);
            quantum = frames_to_duration(samples.len() / ch);
            timeline.place(start, samples.len() / ch)
        } else {
            // Drained: what the real-time side could not take, once per burst.
            let dropped = rx.take_dropped();
            if dropped > 0 {
                warn!("audio encoder is behind: {dropped} frames dropped");
            }
            let wait = if stalled { STALL_FILL } else { STALL };
            let waited = monotonic_now().saturating_sub(since);
            if closed {
                // A stalled track lags by up to `STALL` and one fill: fill
                // it up to the stop, so it ends with the video.
                if stalled {
                    let drops = slew.flush(&mut out);
                    enc.push(&out);
                    let (pad, _, _) = timeline.place(monotonic_now(), 0);
                    enc.push(&vec![0.0; pad.saturating_sub(drops) * ch]);
                }
                break;
            }
            if waited < wait {
                rx.wait(FRAME.saturating_sub(enc.pending_frames()), wait - waited);
                continue;
            }
            // Stalled source (no device, suspended, nothing playing to an
            // app track or a sink): keep the track continuous. A playback
            // buffer is stamped when heard, about `now` or later, but may
            // come a quantum early (a backlog, no graph time): silence up to
            // the tolerance and a quantum (or a fill, if longer) ago does not
            // cover audio that resumes, and the track keeps up with the
            // others. An input's is stamped its capture delay ago.
            (stalled, since) = (true, monotonic_now());
            let lag = if input {
                STALL
            } else {
                TOLERANCE + STALL_FILL.max(quantum)
            };
            samples.clear();
            timeline.place(monotonic_now().saturating_sub(lag), 0)
        };
        // One frame repeated or dropped is a drift correction, faded in;
        // anything else jumps.
        let drift = pad == 0 && skip + repeat == 1;
        if !drift && pad + skip > 0 {
            let drops = slew.flush(&mut out);
            enc.push(&out);
            let cut = drops.min(pad);
            pad -= cut;
            slew.drop += drops - cut;
        }
        if pad > 0 {
            // A stall fills every `STALL_FILL`: not worth a line each time.
            if !stalled {
                warn!("audio timeline gap: {pad} frames of silence inserted");
            }
            enc.push(&vec![0.0; pad * ch]);
        }
        let kept = if drift {
            slew.correct(if repeat == 1 { 1 } else { -1 });
            &samples[..]
        } else {
            samples.get(skip * ch..).unwrap_or_default()
        };
        slew.process(kept, &mut out);
        enc.push(&out);
    }
    slew.flush(&mut out);
    enc.push(&out);
    enc.finish();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pushes a buffer of interleaved stereo `samples` captured from `start`.
    fn push(w: &ring::Writer, start: Duration, samples: &[f32]) {
        let mut producer = w.producer().unwrap();
        assert!(producer.push(start, samples.len() / 2, |i| samples[i]));
    }

    #[test]
    fn timeline_pads_gaps_and_drops_overlaps() {
        let anchor = Duration::from_secs(100);
        let mut t = Timeline::new(anchor);
        // Within tolerance: taken as is.
        assert_eq!(t.place(anchor + Duration::from_millis(3), 960), (0, 0, 0));
        assert_eq!(t.frames, 960);
        // 100 ms gap after the expected 20 ms: 4800 frames of silence.
        assert_eq!(
            t.place(anchor + Duration::from_millis(120), 960),
            (4_800, 0, 0)
        );
        assert_eq!(t.frames, 960 + 4_800 + 960);
        // Expected at 140 ms, buffer claims 90 ms: drop 50 ms of it.
        assert_eq!(
            t.place(anchor + Duration::from_millis(90), 4_800),
            (0, 2_400, 0)
        );
        assert_eq!(t.frames, 960 + 4_800 + 960 + 2_400);
        // Drops never exceed the buffer.
        assert_eq!(t.place(anchor, 10), (0, 10, 0));
    }

    /// Outcome of one hour of simulated capture.
    struct DriftRun {
        t: Timeline,
        /// True start of the buffer after the last one, in ns.
        end_ns: f64,
        /// Worst |pts - true capture time| of a buffer's first kept frame.
        worst_ns: f64,
        pads: Vec<usize>,
        skips: usize,
        repeats: usize,
    }

    /// Feeds 1 h of `len`-frame buffers from a device clock `ppm` fast
    /// against the monotonic clock (so the timeline drops frames when
    /// `ppm > 0`), with ±1 ms of timestamp jitter. The first buffer starts
    /// `offset` after the anchor; buffer `lost`, if any, never arrives.
    fn run_drift(ppm: f64, len: usize, offset: Duration, lost: Option<u64>) -> DriftRun {
        let anchor = Duration::from_secs(1_000);
        let mut t = Timeline::new(anchor);
        let frame_ns = 1e9 / f64::from(SAMPLE_RATE);
        let buf_ns = len as f64 * frame_ns / (1.0 + ppm * 1e-6);
        let (mut worst_ns, mut pads, mut skips, mut repeats) = (0.0f64, Vec::new(), 0, 0);
        let mut true_start = (anchor + offset).as_nanos() as f64;
        for i in 0..3_600 * u64::from(SAMPLE_RATE) / len as u64 {
            if Some(i) != lost {
                let jitter_ns = ((i * 7_919) % 2_001) as f64 * 1_000.0 - 1e6;
                let start = Duration::from_nanos((true_start + jitter_ns) as u64);
                let (pad, skip, repeat) = t.place(start, len);
                assert!(skip <= 1, "cut of {skip} frames at buffer {i}");
                if pad > 0 {
                    pads.push(pad);
                }
                (skips, repeats) = (skips + skip, repeats + repeat);
                // pts of the first kept frame against its true capture time.
                let pts = anchor.as_nanos() as f64 + (t.frames - len + skip) as f64 * frame_ns;
                let truth = true_start + skip as f64 * buf_ns / len as f64;
                worst_ns = worst_ns.max((pts - truth).abs());
            }
            true_start += buf_ns;
        }
        DriftRun {
            t,
            end_ns: true_start,
            worst_ns,
            pads,
            skips,
            repeats,
        }
    }

    /// 100 ppm of an hour: 0.36 s.
    const DRIFT_FRAMES: usize = 17_280;

    #[test]
    fn timeline_follows_clock_drift_without_jumps() {
        for len in [960, 1_024, 2_048] {
            for ppm in [100.0, -100.0] {
                let run = run_drift(ppm, len, Duration::ZERO, None);
                let what = format!("{len} frames, {ppm} ppm");
                assert!(run.pads.is_empty(), "{what}: pads {:?}", run.pads);
                assert!(
                    run.worst_ns < 2e6,
                    "{what}: worst {} ms",
                    run.worst_ns / 1e6
                );
                // A fast device clock only drops frames, a slow one only repeats.
                let (fixes, wrong) = if ppm > 0.0 {
                    (run.skips, run.repeats)
                } else {
                    (run.repeats, run.skips)
                };
                assert_eq!(wrong, 0, "{what}: corrections both ways");
                assert!(fixes.abs_diff(DRIFT_FRAMES) < 100, "{what}: {fixes} fixes");
            }
        }
    }

    #[test]
    fn timeline_steps_over_a_startup_offset_at_once() {
        let run = run_drift(100.0, 960, Duration::from_millis(15), None);
        // One 15 ms pad (±1 ms jitter), then only the drift's own drops.
        assert_eq!(run.pads.len(), 1, "{:?}", run.pads);
        assert!((672..=768).contains(&run.pads[0]), "{:?}", run.pads);
        assert_eq!(run.repeats, 0);
        assert!(run.skips.abs_diff(DRIFT_FRAMES) < 100, "{}", run.skips);
        assert!(run.worst_ns < 2e6, "worst {} ms", run.worst_ns / 1e6);
    }

    #[test]
    fn timeline_pads_a_lost_buffer_at_once() {
        let run = run_drift(-100.0, 960, Duration::ZERO, Some(10_000));
        // One 20 ms pad (±1 ms jitter), not a train of repeated frames.
        assert_eq!(run.pads.len(), 1, "{:?}", run.pads);
        assert!((912..=1_008).contains(&run.pads[0]), "{:?}", run.pads);
        assert_eq!(run.skips, 0);
        assert!(run.repeats.abs_diff(DRIFT_FRAMES) < 100, "{}", run.repeats);
        assert!(run.worst_ns < 2e6, "worst {} ms", run.worst_ns / 1e6);
    }

    #[test]
    fn timeline_still_pads_a_real_gap_after_drift() {
        let mut run = run_drift(100.0, 960, Duration::ZERO, None);
        let gap = Duration::from_nanos(run.end_ns as u64) + Duration::from_millis(200);
        let (pad, skip, repeat) = run.t.place(gap, 960);
        assert_eq!((skip, repeat), (0, 0));
        // 200 ms of silence, give or take the 1 ms tracking error.
        assert!((9_550..=9_650).contains(&pad), "{pad} frames");
    }

    /// Stereo frames of a sine at `freq`, from frame `from`.
    fn sine(freq: f32, from: usize, frames: usize) -> Vec<f32> {
        (from..from + frames)
            .flat_map(|i| {
                let s = (std::f64::consts::TAU * f64::from(freq) * i as f64 / 48_000.0).sin();
                [s as f32 * 0.5; 2]
            })
            .collect()
    }

    /// Runs `slew` over `frames` of a sine at `freq` in 1024-frame chunks,
    /// with a correction (`dir`) every `every` frames; the left channel.
    fn slewed(freq: f32, frames: usize, every: usize, dir: i32, flush: bool) -> Vec<f32> {
        let (mut slew, mut out, mut left) = (Slew::default(), Vec::new(), Vec::new());
        for at in (0..frames).step_by(1_024) {
            if at > 0 && at % every < 1_024 {
                slew.correct(dir);
            }
            slew.process(&sine(freq, at, 1_024), &mut out);
            left.extend(out.iter().step_by(2));
        }
        if flush {
            assert_eq!(slew.flush(&mut out), 0, "drops are applied by the caller");
            left.extend(out.iter().step_by(2));
        }
        left
    }

    /// In-place radix-2 FFT of (re, im), length a power of two.
    fn fft(re: &mut [f64], im: &mut [f64]) {
        let n = re.len();
        let mut j = 0;
        for i in 1..n {
            let mut bit = n >> 1;
            while j & bit != 0 {
                j ^= bit;
                bit >>= 1;
            }
            j |= bit;
            if i < j {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let ang = -std::f64::consts::TAU / len as f64;
            for start in (0..n).step_by(len) {
                for k in 0..len / 2 {
                    let (wr, wi) = ((ang * k as f64).cos(), (ang * k as f64).sin());
                    let (a, b) = (start + k, start + k + len / 2);
                    let (xr, xi) = (re[b] * wr - im[b] * wi, re[b] * wi + im[b] * wr);
                    (re[b], im[b]) = (re[a] - xr, im[a] - xi);
                    (re[a], im[a]) = (re[a] + xr, im[a] + xi);
                }
            }
            len <<= 1;
        }
    }

    /// Energy more than 30 Hz away from `freq` against the total, in dB
    /// (Hann window over the first 2^17 frames).
    fn off_tone_db(signal: &[f32], freq: f32) -> f64 {
        let n = 1 << 17;
        let mut re: Vec<f64> = signal[..n]
            .iter()
            .enumerate()
            .map(|(i, &s)| {
                let w = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos();
                f64::from(s) * w
            })
            .collect();
        let mut im = vec![0.0; n];
        fft(&mut re, &mut im);
        let (mut off, mut all) = (0.0, 0.0);
        for k in 0..n / 2 {
            let e = re[k] * re[k] + im[k] * im[k];
            all += e;
            if (k as f64 * 48_000.0 / n as f64 - f64::from(freq)).abs() > 30.0 {
                off += e;
            }
        }
        10.0 * (off / all).log10()
    }

    #[test]
    fn drift_corrections_are_faded_in_without_clicks() {
        // 50 ppm: a correction every 20 000 frames (0.42 s), both ways.
        for dir in [1, -1] {
            let db = off_tone_db(&slewed(8_000.0, 140_000, 20_000, dir, false), 8_000.0);
            assert!(db < -60.0, "8 kHz, dir {dir}: {db:.1} dB off the tone");
        }
        // The same corrections as one-frame jumps, as before: clicks.
        let mut jumped = Vec::new();
        for at in (0..140_000).step_by(1_024) {
            let chunk = sine(8_000.0, at, 1_024);
            let skip = usize::from(at > 0 && at % 20_000 < 1_024);
            jumped.extend(chunk[skip * 2..].iter().step_by(2));
        }
        let db = off_tone_db(&jumped, 8_000.0);
        assert!(db > -30.0, "jumps: {db:.1} dB");
    }

    #[test]
    fn drift_corrections_keep_the_frame_count_and_alignment() {
        // Fast corrections (every 2048 frames) overlap the fades: still
        // one frame each, the count exact once flushed.
        for dir in [1, -1] {
            let frames = 1_024 * 100;
            let fixes = (1..100).filter(|k| (k * 1_024) % 2_048 < 1_024).count();
            let out = slewed(440.0, frames, 2_048, dir, true);
            let want = frames as i64 + i64::from(dir) * fixes as i64;
            assert_eq!(out.len() as i64, want, "dir {dir}");
        }
        // A long first interval, then a correction every 1000 frames: the
        // first fade is long, then they shorten and catch up instead of
        // owing more and more.
        let (mut slew, mut out) = (Slew::default(), Vec::new());
        let block = sine(440.0, 0, 500);
        for _ in 0..20 {
            slew.process(&block, &mut out);
        }
        for k in 0..200 {
            if k % 2 == 0 {
                slew.correct(-1);
            }
            slew.process(&block, &mut out);
            let most = if k < 100 { 6 } else { 1 };
            assert!(slew.owed.abs() <= most, "{} owed at block {k}", slew.owed);
        }
        // Once a repeat is faded in, the output is the input one frame late.
        let (mut slew, mut out) = (Slew::default(), Vec::new());
        let input = sine(440.0, 0, 20_000);
        slew.process(&input[..2_048], &mut out);
        slew.correct(1);
        let mut all = out.clone();
        slew.process(&input[2_048..], &mut out);
        all.extend_from_slice(&out);
        assert_eq!(all.len(), input.len() + 2);
        assert_eq!(all[all.len() - 4_000..], input[input.len() - 4_000..]);
    }

    #[test]
    fn encoder_emits_continuous_20ms_packets_with_head() {
        let encoder = open_encoder().unwrap();
        let head = opus_head(&encoder).unwrap();
        assert_eq!(&head[..8], b"OpusHead");
        assert_eq!(head[9], CHANNELS as u8);
        let (tx, rx) = mpsc::channel();
        let anchor = Duration::from_secs(10);
        let (chunk_tx, chunk_rx) = ring::ring();
        // 1 s of a full-scale square wave, in odd-sized chunks.
        let samples: Vec<f32> = (0..SAMPLE_RATE as usize * 2)
            .map(|i| if (i / 200) % 2 == 0 { 0.5 } else { -0.5 })
            .collect();
        for (i, c) in samples.chunks(1_234).enumerate() {
            let start = anchor + frames_to_duration(i * 1_234 / 2);
            push(&chunk_tx, start, c);
        }
        drop(chunk_tx);
        let enc = Encoder::new(encoder, anchor, Arc::default(), move |p| {
            tx.send(p).unwrap()
        });
        encode_loop(enc, chunk_rx, false);
        let packets: Vec<_> = rx.iter().collect();
        assert!(packets.len() >= 50, "{} packets", packets.len());
        for w in packets.windows(2) {
            assert_eq!(w[1].pts - w[0].pts, Duration::from_millis(20));
        }
        assert!(packets[0].pts <= anchor);
    }

    #[test]
    fn small_buffers_are_encoded_in_whole_20ms_blocks() {
        // 1 s of a 1 kHz tone in 64-frame buffers (a 64 quantum), pushed by
        // another thread in bursts while the encode thread waits for them.
        let anchor = Duration::from_secs(10);
        let tone: Vec<f32> = (0..SAMPLE_RATE as usize)
            .flat_map(|i| [0.5 * (std::f32::consts::TAU * i as f32 / 48.0).sin(); 2])
            .collect();
        let (chunk_tx, chunk_rx) = ring::ring();
        let producer = std::thread::spawn(move || {
            for (k, c) in tone.chunks(64 * 2).enumerate() {
                push(&chunk_tx, anchor + frames_to_duration(k * 64), c);
                if k % 15 == 14 {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        });
        let (tx, rx) = mpsc::channel();
        let encoder = open_encoder().unwrap();
        let head = opus_head(&encoder).unwrap();
        let enc = Encoder::new(encoder, anchor, Arc::default(), move |p| {
            tx.send(p).unwrap()
        });
        encode_loop(enc, chunk_rx, false);
        producer.join().unwrap();
        let packets: Vec<_> = rx.iter().collect();
        // 50 blocks and the encoder's padding, back to back.
        assert!((51..=53).contains(&packets.len()), "{}", packets.len());
        for w in packets.windows(2) {
            assert_eq!(w[1].pts - w[0].pts, Duration::from_millis(20));
        }
        // No hole: the tone is in every 20 ms of the second (the last fades
        // out in the codec).
        let pre_skip = usize::from(u16::from_le_bytes([head[10], head[11]]));
        let decoded = &decode(&head, &packets)[pre_skip..];
        for (i, block) in decoded.chunks(FRAME).take(49).enumerate() {
            let rms = (block.iter().map(|s| s * s).sum::<f32>() / FRAME as f32).sqrt();
            assert!(rms > 0.3, "block {i}: rms {rms}");
        }
    }

    #[test]
    fn stalled_track_is_filled_up_to_the_stop() {
        let anchor = monotonic_now();
        let (chunk_tx, chunk_rx) = ring::ring();
        push(&chunk_tx, anchor, &vec![0.0; FRAME * 2]);
        // The source then stalls for 700 ms before the capture stops.
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            drop(chunk_tx);
        });
        let (tx, rx) = mpsc::channel();
        let enc = Encoder::new(open_encoder().unwrap(), anchor, Arc::default(), move |p| {
            tx.send(p).unwrap()
        });
        encode_loop(enc, chunk_rx, false);
        stopper.join().unwrap();
        let end = rx.iter().last().unwrap().pts + Duration::from_millis(20);
        // Not `STALL` (or two) short of the stop.
        assert!(
            end >= anchor + Duration::from_millis(680),
            "{:?}",
            end - anchor
        );
    }

    /// Worst `now - pts` of the packets out of a track that stalls after
    /// its first buffer of `frames`, once it is stalled (past 450 ms).
    fn stalled_lag(input: bool, frames: usize) -> Duration {
        let anchor = monotonic_now();
        let (chunk_tx, chunk_rx) = ring::ring();
        push(&chunk_tx, anchor, &vec![0.0; frames * 2]);
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(900));
            drop(chunk_tx);
        });
        let (tx, rx) = mpsc::channel();
        let enc = Encoder::new(open_encoder().unwrap(), anchor, Arc::default(), move |p| {
            tx.send((monotonic_now(), p.pts)).unwrap()
        });
        encode_loop(enc, chunk_rx, input);
        stopper.join().unwrap();
        rx.iter()
            .filter(|(at, _)| *at > anchor + Duration::from_millis(450))
            .map(|(at, pts)| at.saturating_sub(pts))
            .max()
            .unwrap()
    }

    #[test]
    fn a_stall_counts_from_the_last_buffer_pushed_not_read() {
        // Less than a block arrives while the encode thread waits for one:
        // it stays in the ring, and the stall must still be seen `STALL`
        // after it came, not `STALL` after it was read.
        let anchor = monotonic_now();
        let (chunk_tx, chunk_rx) = ring::ring();
        let (tx, rx) = mpsc::channel();
        let enc = Encoder::new(open_encoder().unwrap(), anchor, Arc::default(), move |_| {
            tx.send(monotonic_now()).unwrap()
        });
        let encoder = std::thread::spawn(move || encode_loop(enc, chunk_rx, false));
        std::thread::sleep(Duration::from_millis(50));
        let pushed = monotonic_now();
        // Kept, as a stream keeps it: dropping it would wake the reader.
        let mut producer = chunk_tx.producer().unwrap();
        // Captured at the anchor: no gap, so only the fill makes packets.
        assert!(producer.push(anchor, 100, |_| 0.0));
        std::thread::sleep(Duration::from_millis(700));
        drop(producer);
        drop(chunk_tx);
        encoder.join().unwrap();
        let first = rx.iter().next().unwrap();
        // The first fill makes the first packet.
        let margin = Duration::from_millis(30);
        assert!(
            first < pushed + STALL + STALL_FILL + margin,
            "first packet {:?} after the buffer",
            first - pushed
        );
    }

    #[test]
    fn a_stalled_playback_track_keeps_up_with_now() {
        // Filled every 20 ms up to 25 ms ago (a fill and the tolerance): the
        // mix and a live stream do not wait for it. Packets lag by that, a
        // block and the encoder.
        // Waiting for it would lag at least `STALL`: the bound has room for a loaded CI runner.
        let lag = stalled_lag(false, FRAME);
        let margin = TOLERANCE + STALL_FILL;
        assert!(lag >= margin && lag < STALL, "{lag:?}");
        // A larger quantum: a resume can come that early, so that far back.
        let lag = stalled_lag(false, 2_048);
        let margin = TOLERANCE + frames_to_duration(2_048);
        assert!(lag >= margin && lag < STALL, "{lag:?}");
        // An input's buffers are stamped in the past: it stays `STALL` back.
        let lag = stalled_lag(true, FRAME);
        assert!(lag >= STALL, "{lag:?}");
        assert!(lag < 2 * STALL, "{lag:?}");
    }

    /// Decodes Opus packets back to channel 0's samples.
    pub(crate) fn decode(head: &[u8], packets: &[EncodedPacket]) -> Vec<f32> {
        use ffmpeg_next as ffmpeg;
        let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::OPUS).unwrap();
        let mut ctx = ffmpeg::codec::Context::new_with_codec(codec);
        unsafe {
            let raw = ctx.as_mut_ptr();
            let buf = ffmpeg::ffi::av_mallocz(head.len() + 64) as *mut u8;
            std::ptr::copy_nonoverlapping(head.as_ptr(), buf, head.len());
            (*raw).extradata = buf;
            (*raw).extradata_size = head.len() as i32;
        }
        let mut dec = ctx.decoder().audio().unwrap();
        let mut out = Vec::new();
        let mut frame = ffmpeg::frame::Audio::empty();
        for p in packets {
            dec.send_packet(&ffmpeg::Packet::copy(&p.data)).unwrap();
            while dec.receive_frame(&mut frame).is_ok() {
                // libopus decodes to planar or packed float: read channel 0.
                let plane: &[f32] = frame.plane(0);
                out.extend_from_slice(&plane[..frame.samples()]);
            }
        }
        out
    }

    #[test]
    fn processing_delay_is_removed_from_the_timeline() {
        let anchor = Duration::from_secs(10);
        // 1 s of silence with a 10 ms, 2 kHz burst starting at frame `at`.
        let at = 24_123;
        let samples: Vec<f32> = (0..SAMPLE_RATE as usize)
            .flat_map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                let on = (at..at + 480).contains(&i);
                [if on {
                    0.5 * (std::f32::consts::TAU * 2_000.0 * t).sin()
                } else {
                    0.0
                }; 2]
            })
            .collect();
        let (chunk_tx, chunk_rx) = ring::ring();
        for (i, c) in samples.chunks(1_024 * 2).enumerate() {
            let start = anchor + frames_to_duration(i * 1_024);
            push(&chunk_tx, start, c);
        }
        drop(chunk_tx);
        let (tx, rx) = mpsc::channel();
        let encoder = open_encoder().unwrap();
        let head = opus_head(&encoder).unwrap();
        let mut enc = Encoder::new(encoder, anchor, Arc::default(), move |p| {
            tx.send(p).unwrap()
        });
        // Delayed by RNNoise's 20 ms and the limiter's lookahead (the
        // expander is left out: its attack would blur the onset).
        let specs = ["high_pass", "noise_suppression", "limiter"]
            .map(|n| vesper_dsp::FilterSpec::default_for(n).unwrap());
        let (mut chain, _) = vesper_dsp::live(&specs, 2);
        assert_eq!(chain.latency(), 960 + 99);
        enc.process = Some(Box::new(move |buf: &mut [f32]| {
            chain.process(buf);
            chain.latency()
        }));
        encode_loop(enc, chunk_rx, false);
        let packets: Vec<_> = rx.iter().collect();
        let decoded = decode(&head, &packets);
        let onset = decoded.iter().position(|s| s.abs() > 0.05).unwrap();
        // The decoder drops the pre-skip: its first sample is at the anchor.
        let pre_skip = usize::from(u16::from_le_bytes([head[10], head[11]]));
        let heard = packets[0].pts + frames_to_duration(pre_skip + onset);
        let truth = anchor + frames_to_duration(at);
        let error_ms = (heard.as_secs_f64() - truth.as_secs_f64()) * 1e3;
        assert!(error_ms.abs() < 0.1, "burst {error_ms} ms off");
        // Nothing lost at the end: the delay was flushed.
        assert!(packets.len() >= 50, "{} packets", packets.len());
    }

    fn test_encoder(shared: &Arc<Shared>) -> Encoder<impl FnMut(EncodedPacket)> {
        Encoder::new(
            open_encoder().unwrap(),
            Duration::ZERO,
            shared.clone(),
            |_| {},
        )
    }

    fn peak(shared: &Shared) -> f32 {
        let peaks = shared.take_peaks();
        peaks.left.max(peaks.right)
    }

    #[test]
    fn muted_track_reports_zero_peak() {
        let shared = Arc::new(Shared::default());
        let mut enc = test_encoder(&shared);
        enc.push(&vec![0.75; FRAME * 2 * 2]);
        assert_eq!(peak(&shared), 0.75);
        shared.muted.store(true, Ordering::Relaxed);
        // The first muted block ramps down; the blocks after it are silent.
        enc.push(&vec![0.75; FRAME * 2]);
        peak(&shared);
        enc.push(&vec![0.75; FRAME * 2 * 3]);
        assert_eq!(peak(&shared), 0.0);
    }

    #[test]
    fn peaks_keep_the_loudest_per_channel_until_taken() {
        let shared = Arc::new(Shared::default());
        let mut enc = test_encoder(&shared);
        let block = |l: f32, r: f32| [l, r].repeat(FRAME);
        enc.push(&block(0.5, -0.25));
        enc.push(&block(0.1, 0.1));
        let peaks = shared.take_peaks();
        assert_eq!((peaks.left, peaks.right), (0.5, 0.25));
        assert!(peaks.stereo && !peaks.clipped && peaks.fresh);
        let peaks = shared.take_peaks();
        assert_eq!(peaks.left, 0.0, "taken");
        assert!(!peaks.fresh, "no block since");
        enc.push(&block(-1.0, 1.0));
        assert!(shared.take_peaks().clipped);
        assert!(!shared.take_peaks().clipped, "taken");
    }

    #[test]
    fn a_signal_before_the_request_is_not_lost() {
        let shared = Arc::new(Shared::default());
        let mut enc = test_encoder(&shared);
        enc.push(&vec![0.0; FRAME * 2]);
        shared.take_peaks();
        assert!(!shared.wake_on_signal(), "silent: armed");
        assert!(shared.wake.load(Ordering::Relaxed));
        shared.take_peaks();
        shared.wake.store(false, Ordering::Relaxed);
        // A block lands between the last take and the request.
        enc.push(&vec![0.5; FRAME * 2]);
        assert!(shared.wake_on_signal(), "the meter keeps ticking");
        assert!(!shared.wake.load(Ordering::Relaxed), "not armed");
    }

    #[test]
    fn upmixed_mono_is_not_stereo() {
        let shared = Arc::new(Shared::default());
        let mut enc = test_encoder(&shared);
        enc.push(&vec![0.3; FRAME * 2 * 3]);
        assert!(!shared.take_peaks().stereo);
    }

    #[test]
    fn signal_wakes_once_per_request() {
        use std::sync::atomic::AtomicUsize;
        let shared = Arc::new(Shared::default());
        let mut enc = test_encoder(&shared);
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        enc.on_signal = Some(Box::new(move || {
            c.fetch_add(1, Ordering::Relaxed);
        }));
        enc.push(&vec![0.5; FRAME * 2]);
        assert_eq!(calls.load(Ordering::Relaxed), 0, "not asked");
        shared.wake.store(true, Ordering::Relaxed);
        enc.push(&vec![0.0; FRAME * 2]);
        assert_eq!(calls.load(Ordering::Relaxed), 0, "silence");
        enc.push(&vec![0.5; FRAME * 2 * 3]);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn volume_converts_db_and_clamps() {
        assert_eq!(db_to_gain(0.0), 1.0);
        assert!((db_to_gain(-6.0) - 0.501).abs() < 1e-3);
        assert!((db_to_gain(12.0) - 3.981).abs() < 1e-3);
        assert_eq!(db_to_gain(40.0), db_to_gain(MAX_DB));
        assert_eq!(db_to_gain(MIN_DB), 0.0);
        assert_eq!(db_to_gain(-200.0), 0.0);
        assert_eq!(db_to_gain(f32::NAN), 0.0);
    }

    #[test]
    fn gain_ramps_over_the_block_then_holds() {
        let mut block = vec![1.0; 8];
        apply_gain(&mut block, 1.0, 0.0);
        assert_eq!(block, [0.75, 0.75, 0.5, 0.5, 0.25, 0.25, 0.0, 0.0]);
        let mut block = vec![0.5; 4];
        apply_gain(&mut block, 2.0, 2.0);
        assert_eq!(block, [1.0; 4]);
    }

    #[test]
    fn volume_and_process_apply_before_the_encoder() {
        let shared = Arc::new(Shared::default());
        shared
            .gain
            .store(db_to_gain(-6.0).to_bits(), Ordering::Relaxed);
        let mut enc = test_encoder(&shared);
        // Ramps from 1.0 towards -6 dB during the first block only.
        enc.push(&vec![0.5; FRAME * 2]);
        enc.process = Some(Box::new(|s: &mut [f32]| {
            s.iter_mut().for_each(|x| *x *= 2.0);
            0
        }));
        peak(&shared);
        enc.push(&vec![0.5; FRAME * 2 * 2]);
        // 0.5 doubled by the processor, then -6 dB.
        assert!(
            (peak(&shared) - db_to_gain(-6.0)).abs() < 1e-4,
            "{}",
            peak(&shared)
        );
    }

    /// Amplitude of the `freq` sine in a 20 ms block (whole periods) of
    /// interleaved stereo, channel 0.
    fn tone(block: &[f32], freq: f32) -> f32 {
        let (mut re, mut im) = (0.0, 0.0);
        for (i, frame) in block.chunks(2).enumerate() {
            let w = std::f32::consts::TAU * freq * i as f32 / SAMPLE_RATE as f32;
            (re, im) = (re + frame[0] * w.cos(), im + frame[0] * w.sin());
        }
        2.0 * (re * re + im * im).sqrt() / (block.len() / 2) as f32
    }

    /// A track on `key` at `anchor` that speaks, or ducks as `ducking` says.
    fn voice_track(
        key: &Arc<VoiceKey>,
        anchor: Duration,
        speaks: bool,
        ducking: DuckSettings,
    ) -> Encoder<impl FnMut(EncodedPacket)> {
        let shared = Arc::new(Shared::default());
        shared.set_ducking(ducking);
        shared.speaks.store(speaks, Ordering::Relaxed);
        let mut enc = Encoder::new(open_encoder().unwrap(), anchor, shared, |_| {});
        enc.voice = Some(Voice::new(key.clone()));
        enc
    }

    const DUCK_12: DuckSettings = DuckSettings {
        duck_db: 12.0,
        music_lufs: None,
    };

    /// Block `k` of a `freq` sine (whole periods in 20 ms), stereo.
    fn sine_block(freq: f32, amp: f32, k: usize) -> Vec<f32> {
        (k * FRAME..(k + 1) * FRAME)
            .flat_map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                [amp * (std::f32::consts::TAU * freq * t).sin(); 2]
            })
            .collect()
    }

    /// A LibriSpeech utterance (vesper-dsp's test data, 4.8 s) at 48 kHz
    /// (linear upsampling) over a -65 dB hiss: block `k` of it, stereo, the
    /// clip starting at block 50 (1 s).
    fn speech_block(k: usize) -> Vec<f32> {
        static SPEECH: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
        let speech = SPEECH.get_or_init(|| {
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../vesper-dsp/testdata/speech/1272-128104-0001.s16"
            );
            let raw = std::fs::read(path).unwrap();
            let x: Vec<f32> = raw
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| f32::from(i16::from_le_bytes(*b)) / 32_768.0)
                .collect();
            let mut out = vec![0.0; 50 * FRAME];
            // shortcut: linear upsampling (images above 8 kHz, about -30 dB);
            // on the bench this clip's detection was the same as with a
            // windowed sinc. Use the bench's sinc if a test here gets close.
            for w in x.windows(2) {
                out.extend((0..3).map(|i| w[0] + (w[1] - w[0]) * i as f32 / 3.0));
            }
            out
        });
        (k * FRAME..(k + 1) * FRAME)
            .flat_map(|i| [speech.get(i).copied().unwrap_or(0.0) + hiss(i as u32); 2])
            .collect()
    }

    /// Deterministic white noise at about -65 dBFS RMS.
    fn hiss(i: u32) -> f32 {
        let x = i.wrapping_mul(2_654_435_761) ^ 0x9e37_79b9;
        let x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((x >> 8) as f32 / (1 << 24) as f32 - 0.5) * 2e-3
    }

    /// Blocks of [`speech_block`] with words: the clip opens with 0.5 s of
    /// its room.
    const WORDS: std::ops::Range<usize> = 77..267;

    /// The mic speaks (see [`speech_block`]) over a 150 Hz song: the song's
    /// blocks `0..lead` come first, then mic block `k` with song block
    /// `k + lead`, one after the other.
    fn run_voice<A: FnMut(EncodedPacket), B: FnMut(EncodedPacket)>(
        mic: &mut Encoder<A>,
        music: &mut Encoder<B>,
        lead: usize,
        mut each: impl FnMut(usize, &Shared),
    ) {
        for k in 0..lead {
            music.push(&sine_block(150.0, 0.25, k));
        }
        for k in 0..WORDS.end + 65 {
            mic.push(&speech_block(k));
            music.push(&sine_block(150.0, 0.25, k + lead));
            each(k, &music.shared);
        }
    }

    /// Checks the song's level per block against the voice of `run_voice`,
    /// both on one timeline (`levels[k]` at the mic's block `k`).
    fn check_duck(levels: &[f32]) {
        let db = |k: usize| 20.0 * (levels[k] / 0.25).log10();
        assert!(db(49).abs() < 0.1, "{}", db(49));
        // The lookahead starts it less than 160 ms before the first word.
        let before = WORDS.start - 8;
        assert!(db(before).abs() < 0.1, "{}", db(before));
        // Down by the depth at the first word, and held through the gaps
        // between words, then the 300 ms hold.
        for k in WORDS.start..WORDS.end + 12 {
            assert!((db(k) + 12.0).abs() < 0.3, "block {k}: {} dB", db(k));
        }
        // Back after the 600 ms release.
        let end = WORDS.end + 50;
        assert!(db(end).abs() < 0.1, "{}", db(end));
    }

    #[test]
    fn music_ducks_under_the_microphone_in_the_mix() {
        let anchor = Duration::from_secs(5);
        let mix = Mix::start(anchor, |_| {}).unwrap();
        mix.set_encoding(false);
        let (tx, rx) = mpsc::channel();
        mix.set_tap(Some(Box::new(move |pts, b: &[f32]| {
            let _ = tx.send((pts, tone(b, 150.0)));
        })));
        let key = Arc::new(VoiceKey::default());
        let mut mic = voice_track(&key, anchor, true, DuckSettings::default());
        let mut music = voice_track(&key, anchor, false, DUCK_12);
        // The mic stays out of the mix: its speech would blur the song's
        // 150 Hz measured there.
        music.mix = Some(mix.input());
        let mut during = 0.0;
        run_voice(&mut mic, &mut music, 0, |k, shared| {
            if k == 150 {
                during = f32::from_bits(shared.ducking.load(Ordering::Relaxed));
            }
        });
        let got: Vec<_> = rx.try_iter().collect();
        // The ducking's lookahead holds 6 back, the limiter's one.
        let held = LOOKAHEAD_FRAMES / FRAME + 1;
        assert!(got.len() >= WORDS.end + 65 - held, "{}", got.len());
        assert_eq!(got[60].0, anchor + Duration::from_millis(1_200));
        check_duck(&got.iter().map(|g| g.1).collect::<Vec<_>>());
        // The duck shows for a meter, and goes.
        assert!((during - 12.0).abs() < 0.01, "{during}");
        let after = f32::from_bits(music.shared.ducking.load(Ordering::Relaxed));
        assert_eq!(after, 0.0);
    }

    /// The song's level per block as `music` taps it.
    fn tap_levels<F: FnMut(EncodedPacket)>(
        music: &mut Encoder<F>,
    ) -> Arc<std::sync::Mutex<Vec<f32>>> {
        let levels = Arc::new(std::sync::Mutex::new(Vec::new()));
        let l = levels.clone();
        music.tap = Some(Box::new(move |_, b: &[f32]| {
            l.lock().unwrap().push(tone(b, 150.0));
        }));
        levels
    }

    #[test]
    fn a_track_ahead_on_the_timeline_ducks_in_time() {
        // The song's track started 500 ms before the mic's (its anchor),
        // more than the hold: they meet on the timeline, not on block counts.
        let key = Arc::new(VoiceKey::default());
        let anchor = Duration::from_secs(5);
        let mut mic = voice_track(&key, anchor, true, DuckSettings::default());
        let early = anchor - Duration::from_millis(500);
        let mut music = voice_track(&key, early, false, DUCK_12);
        let levels = tap_levels(&mut music);
        run_voice(&mut mic, &mut music, 25, |_, _| {});
        check_duck(&levels.lock().unwrap()[25..]);
    }

    #[test]
    fn a_track_ahead_in_real_time_still_ducks() {
        // Playback heard 500 ms after it is processed (a Bluetooth headset):
        // the song's block at a timeline frame comes 500 ms before the
        // mic's. The voice is not known there yet: the song ducks as the
        // voice speaks now, late by those 500 ms less the lookahead.
        let key = Arc::new(VoiceKey::default());
        let anchor = Duration::from_secs(5);
        let mut mic = voice_track(&key, anchor, true, DuckSettings::default());
        let early = anchor - Duration::from_millis(500);
        let mut music = voice_track(&key, early, false, DUCK_12);
        let levels = tap_levels(&mut music);
        // Song block `k + 25` (on the mic's block `k`) comes with mic
        // block `k - 25`.
        run_voice(&mut mic, &mut music, 50, |_, _| {});
        let levels = levels.lock().unwrap();
        let db = |k: usize| 20.0 * (levels[k + 25] / 0.25).log10();
        let late = (500 * 48 - LOOKAHEAD_FRAMES) / FRAME;
        let before = WORDS.start + late - 8;
        assert!(db(before).abs() < 0.1, "{}", db(before));
        for k in WORDS.start + late + 5..WORDS.end + late + 12 {
            assert!((db(k) + 12.0).abs() < 0.3, "block {k}: {} dB", db(k));
        }
        let end = WORDS.end + late + 50;
        assert!(db(end).abs() < 0.1, "{}", db(end));
    }

    #[test]
    fn ducking_on_and_off_keeps_the_audio_continuous_and_in_place() {
        // Turned on, the lookahead delays the track; turned off, it lets
        // its line out: the output is the input sample for sample, on the
        // same timeline frames, with no gap and no jump.
        let key = Arc::new(VoiceKey::default());
        key.add_voice();
        let mut track = voice_track(&key, Duration::ZERO, false, DuckSettings::default());
        let out = Arc::new(std::sync::Mutex::new(Vec::<f32>::new()));
        let o = out.clone();
        track.tap = Some(Box::new(move |_, b: &[f32]| {
            o.lock().unwrap().extend_from_slice(b);
        }));
        let mut k = 0;
        for (blocks, ducks) in [(10, false), (3, true), (2, false), (20, true), (10, false)] {
            let set = if ducks {
                DUCK_12
            } else {
                DuckSettings::default()
            };
            track.shared.set_ducking(set);
            for _ in 0..blocks {
                track.push(&sine_block(441.0, 0.25, k));
                k += 1;
            }
        }
        assert_eq!(track.delay, 0);
        let input: Vec<f32> = (0..k).flat_map(|k| sine_block(441.0, 0.25, k)).collect();
        assert_eq!(*out.lock().unwrap(), input);
    }

    #[test]
    fn a_ducking_track_ends_with_its_input_whether_its_voice_stays_or_not() {
        for voice_left in [false, true] {
            let key = Arc::new(VoiceKey::default());
            key.add_voice();
            let mut track = voice_track(&key, Duration::ZERO, false, DUCK_12);
            let out = Arc::new(std::sync::Mutex::new(Vec::<f32>::new()));
            let o = out.clone();
            track.tap = Some(Box::new(move |_, b: &[f32]| {
                o.lock().unwrap().extend_from_slice(b);
            }));
            let input: Vec<f32> = (0..50).flat_map(|k| sine_block(441.0, 0.25, k)).collect();
            track.push(&input);
            if voice_left {
                // Its voice is gone: the lookahead lets its line out.
                key.remove_voice();
            }
            track.finish();
            // 1 s in, 1 s out: nothing lost, no silence added.
            let out = out.lock().unwrap();
            assert_eq!(out.len(), input.len(), "voice left: {voice_left}");
            assert!(*out == input, "voice left: {voice_left}");
        }
    }

    #[test]
    fn becoming_a_voice_mid_duck_comes_back_up_smoothly() {
        let key = Arc::new(VoiceKey::default());
        let mut track = voice_track(&key, Duration::ZERO, false, DUCK_12);
        let out = Arc::new(std::sync::Mutex::new(Vec::<f32>::new()));
        let o = out.clone();
        track.tap = Some(Box::new(move |_, b: &[f32]| {
            o.lock().unwrap().extend(b.iter().step_by(2));
        }));
        // Another voice speaks for 10 s.
        key.add_voice();
        for end in (480..480_000).step_by(480) {
            key.voiced(end);
        }
        key.heard(480_000);
        track.push(&vec![0.25; FRAME * 2 * 10]);
        track.shared.speaks.store(true, Ordering::Relaxed);
        track.push(&vec![0.25; FRAME * 2 * 80]);
        let out = out.lock().unwrap();
        let step = out
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!(step < 1e-3, "{step}");
        // Out before it became a voice: the lookahead held the rest back.
        assert!((out[FRAME * 3] - 0.25 * db_to_gain(-12.0)).abs() < 1e-3);
        assert_eq!(*out.last().unwrap(), 0.25);
        assert_eq!(track.shared.ducking.load(Ordering::Relaxed), 0);
    }

    /// Pushes blocks `blocks` of `signal` into `mic` (anchored at zero);
    /// whether the key said speaking in any of them.
    fn spoke<F: FnMut(EncodedPacket)>(
        mic: &mut Encoder<F>,
        key: &VoiceKey,
        blocks: std::ops::Range<usize>,
        signal: impl Fn(usize) -> Vec<f32>,
    ) -> bool {
        let mut spoke = false;
        for k in blocks {
            mic.push(&signal(k));
            spoke |= key.speaking((k * FRAME) as u64, ((k + 1) * FRAME) as u64);
        }
        spoke
    }

    #[test]
    fn a_muted_or_ducking_track_does_not_speak() {
        let key = Arc::new(VoiceKey::default());
        let mut mic = voice_track(&key, Duration::ZERO, true, DuckSettings::default());
        mic.shared.muted.store(true, Ordering::Relaxed);
        assert!(!spoke(&mut mic, &key, 0..150, speech_block));
        // Not a voice (an input device playing music): it ducks instead.
        mic.shared.muted.store(false, Ordering::Relaxed);
        mic.shared.speaks.store(false, Ordering::Relaxed);
        assert!(!spoke(&mut mic, &key, 150..200, speech_block));
        mic.shared.speaks.store(true, Ordering::Relaxed);
        assert!(spoke(&mut mic, &key, 200..250, speech_block));
        // Voiced in its last block, on the timeline.
        assert!(key.speaking(249 * FRAME as u64, 250 * FRAME as u64));
        // Only voices count: one that stops speaking is not one any more.
        assert!(key.has_voice());
        mic.shared.speaks.store(false, Ordering::Relaxed);
        mic.push(&speech_block(250));
        assert!(!key.has_voice());
        mic.shared.speaks.store(true, Ordering::Relaxed);
        mic.push(&speech_block(251));
        assert!(key.has_voice());
        drop(mic);
        assert!(!key.has_voice());
    }

    #[test]
    fn the_detector_hears_the_raw_input() {
        // Before the filters and the volume: a chain that silences the
        // track, at the lowest volume, still lets the voice speak.
        let key = Arc::new(VoiceKey::default());
        let mut mic = voice_track(&key, Duration::ZERO, true, DuckSettings::default());
        mic.process = Some(Box::new(|b: &mut [f32]| {
            b.fill(0.0);
            0
        }));
        let silent = db_to_gain(MIN_DB);
        mic.shared.gain.store(silent.to_bits(), Ordering::Relaxed);
        mic.gain = silent;
        assert!(spoke(&mut mic, &key, 0..100, speech_block));
    }

    #[test]
    fn muting_freezes_the_detector() {
        // A -65 dB room, then muted for 3 s while a loud steady noise
        // plays: the detector does not hear it, so its floor stays put and
        // the speech after the unmute is voice from its first words.
        let key = Arc::new(VoiceKey::default());
        let mut mic = voice_track(&key, Duration::ZERO, true, DuckSettings::default());
        assert!(!spoke(&mut mic, &key, 0..50, speech_block));
        let floor = mic.voice.as_ref().unwrap().detector.floor_db();
        assert!(floor < -60.0, "{floor}");
        mic.shared.muted.store(true, Ordering::Relaxed);
        let loud = |k: usize| -> Vec<f32> {
            (k * FRAME..(k + 1) * FRAME)
                .flat_map(|i| [300.0 * hiss(i as u32); 2])
                .collect()
        };
        assert!(!spoke(&mut mic, &key, 50..200, loud));
        assert_eq!(mic.voice.as_ref().unwrap().detector.floor_db(), floor);
        mic.shared.muted.store(false, Ordering::Relaxed);
        // The clip again: its first word in block 277.
        assert!(spoke(&mut mic, &key, 200..285, |k| speech_block(k - 200)));
    }
}
