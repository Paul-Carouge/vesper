//! Audio capture through PipeWire (desktop, microphone, a device, or an
//! application's streams), encoded to Opus.
//!
//! [`AudioCapture::start`] runs one PipeWire stream (its own thread) and one
//! encode thread per source. Packets carry `CLOCK_MONOTONIC` pts like the
//! video frames, so the muxer can put both on one time base. Tracks started
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
//! 1024, ~488 ppm at 2048).

mod mix;
mod nodes;
mod pw;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

use ffmpeg::format::Sample;
use ffmpeg::format::sample::Type;
use ffmpeg::{ChannelLayout, codec, encoder, frame};
use ffmpeg_next as ffmpeg;
use tracing::{error, warn};
use vesper_core::EncodedPacket;

pub use mix::{Mix, MixInput};
pub use nodes::{Node, NodeWatch};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

/// Per-track processing of each 20 ms block (interleaved stereo F32), run on
/// the encode thread before the volume and the encoder. Returns its current
/// delay in frames: the encoder drops that much of its output (or inserts
/// silence when it shrinks), so the track and its pts stay on the timeline
/// and the tracks of a mix stay aligned.
pub type Process = Box<dyn FnMut(&mut [f32]) -> usize + Send>;

/// Output sample rate of every track.
pub const SAMPLE_RATE: u32 = 48_000;
/// Output channels of every track (mono sources are upmixed by PipeWire).
pub const CHANNELS: u16 = 2;
/// Opus frame: 20 ms.
const FRAME: usize = 960;
const BIT_RATE: usize = 160_000;
/// Peak meter window: ~33 ms.
const PEAK_WINDOW: usize = 1_600;
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
}

/// State shared with the encode thread.
struct Shared {
    muted: AtomicBool,
    /// `f32` bits of the linear gain.
    gain: AtomicU32,
    /// `f32` bits of the last window's peak.
    peak: AtomicU32,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            muted: AtomicBool::new(false),
            gain: AtomicU32::new(1f32.to_bits()),
            peak: AtomicU32::new(0),
        }
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
        // ~1 s of buffers at the default quantum.
        let (chunk_tx, chunk_rx) = mpsc::sync_channel(50);
        let mut enc = Encoder::new(encoder, options.anchor, shared.clone(), on_packet);
        enc.process = options.process;
        enc.mix = options.mix;
        let encode_thread = std::thread::Builder::new()
            .name("vesper-audio-enc".into())
            .spawn(move || encode_loop(enc, chunk_rx))?;
        let (stop, stop_rx) = ::pipewire::channel::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let pw_thread = std::thread::Builder::new()
            .name("vesper-audio-pw".into())
            .spawn(move || pw::run(source, chunk_tx, stop_rx, ready_tx))?;
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

    /// Peak level of the last ~33 ms, linear 0..=1 (0 while muted).
    pub fn peak(&self) -> f32 {
        f32::from_bits(self.shared.peak.load(Ordering::Relaxed))
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
        // The PipeWire thread drops the chunk sender on exit, which ends the
        // encode thread after it flushed.
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
    mix: Option<MixInput>,
    peak: f32,
    peak_samples: usize,
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
            // Room for a block and a delay change, so it rarely grows.
            ready: Vec::with_capacity(4 * FRAME * usize::from(CHANNELS)),
            delay: 0,
            skip: 0,
            sent: 0,
            anchor,
            gain: f32::from_bits(shared.gain.load(Ordering::Relaxed)),
            shared,
            process: None,
            mix: None,
            peak: 0.0,
            peak_samples: 0,
            on_packet,
        }
    }

    fn push(&mut self, samples: &[f32]) {
        self.pending.extend_from_slice(samples);
        let ch = usize::from(CHANNELS);
        let len = FRAME * ch;
        while self.pending.len() >= len {
            self.block.clear();
            self.block.extend(self.pending.drain(..len));
            // The DSP seam: a no-op unless a processor was given.
            let delay = match &mut self.process {
                Some(process) => process(&mut self.block),
                None => 0,
            };
            self.set_delay(delay);
            let skip = self.skip.min(FRAME);
            self.skip -= skip;
            self.ready.extend_from_slice(&self.block[skip * ch..]);
            while self.ready.len() >= len {
                self.block.clear();
                self.block.extend(self.ready.drain(..len));
                self.encode_block();
            }
        }
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

    /// Volume, mix, meter and encoder for the block in `self.block`.
    fn encode_block(&mut self) {
        let gain = if self.shared.muted.load(Ordering::Relaxed) {
            0.0
        } else {
            f32::from_bits(self.shared.gain.load(Ordering::Relaxed))
        };
        apply_gain(&mut self.block, self.gain, gain);
        self.gain = gain;
        if let Some(mix) = &self.mix {
            mix.push(&self.block);
        }
        let out = self.frame.data_mut(0);
        for (dst, &s) in out.as_chunks_mut::<4>().0.iter_mut().zip(&self.block) {
            dst.copy_from_slice(&s.to_le_bytes());
            self.peak = self.peak.max(s.abs());
            self.peak_samples += 1;
            if self.peak_samples == PEAK_WINDOW * usize::from(CHANNELS) {
                let peak = self.peak.min(1.0);
                self.shared.peak.store(peak.to_bits(), Ordering::Relaxed);
                (self.peak, self.peak_samples) = (0.0, 0);
            }
        }
        self.frame.set_pts(Some(self.sent));
        self.sent += FRAME as i64;
        if let Err(e) = self.encoder.send_frame(&self.frame) {
            error!("opus encode failed: {e}");
        }
        self.drain();
    }

    fn drain(&mut self) {
        let mut packet = ffmpeg::Packet::empty();
        while self.encoder.receive_packet(&mut packet).is_ok() {
            // libavcodec convention: the first packet starts `pre_skip`
            // samples early (negative pts), so decoded audio lands on `anchor`.
            let pts_ns = packet.pts().unwrap_or(0) as i128 * 1_000_000_000 / SAMPLE_RATE as i128;
            let pts = self.anchor.as_nanos() as i128 + pts_ns;
            (self.on_packet)(EncodedPacket {
                data: Arc::from(packet.data().unwrap_or_default()),
                pts: Duration::from_nanos(pts.max(0) as u64),
                keyframe: true,
            });
        }
    }

    fn finish(mut self) {
        // Pad the last partial frame and flush the processing's delay so no
        // captured audio is lost.
        let len = FRAME * usize::from(CHANNELS);
        let tail = self.pending.len() + self.delay * usize::from(CHANNELS);
        self.push(&vec![0.0; tail.next_multiple_of(len) - self.pending.len()]);
        if !self.ready.is_empty() {
            self.ready.resize(len, 0.0);
            self.block.clear();
            self.block.append(&mut self.ready);
            self.encode_block();
        }
        if let Err(e) = self.encoder.send_eof() {
            error!("opus flush failed: {e}");
        }
        self.drain();
        self.shared.peak.store(0, Ordering::Relaxed);
    }
}

fn encode_loop(mut enc: Encoder<impl FnMut(EncodedPacket)>, rx: mpsc::Receiver<pw::Chunk>) {
    let mut timeline = Timeline::new(enc.anchor);
    let ch = usize::from(CHANNELS);
    loop {
        let (pad, skip, repeat, samples) = match rx.recv_timeout(STALL) {
            Ok(chunk) => {
                let (pad, skip, repeat) = timeline.place(chunk.start, chunk.samples.len() / ch);
                (pad, skip, repeat, chunk.samples)
            }
            // Stalled source (no device, suspended): keep the track continuous.
            Err(RecvTimeoutError::Timeout) => {
                let (pad, _, _) = timeline.place(monotonic_now().saturating_sub(STALL), 0);
                (pad, 0, 0, Vec::new())
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if pad > 0 {
            warn!("audio timeline gap: {pad} frames of silence inserted");
            enc.push(&vec![0.0; pad * ch]);
        }
        for _ in 0..repeat {
            enc.push(samples.get(..ch).unwrap_or_default());
        }
        enc.push(samples.get(skip * ch..).unwrap_or_default());
    }
    enc.finish();
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn encoder_emits_continuous_20ms_packets_with_head() {
        let encoder = open_encoder().unwrap();
        let head = opus_head(&encoder).unwrap();
        assert_eq!(&head[..8], b"OpusHead");
        assert_eq!(head[9], CHANNELS as u8);
        let (tx, rx) = mpsc::channel();
        let anchor = Duration::from_secs(10);
        let (chunk_tx, chunk_rx) = mpsc::channel();
        // 1 s of a full-scale square wave, in odd-sized chunks.
        let samples: Vec<f32> = (0..SAMPLE_RATE as usize * 2)
            .map(|i| if (i / 200) % 2 == 0 { 0.5 } else { -0.5 })
            .collect();
        for (i, c) in samples.chunks(1_234).enumerate() {
            let start = anchor + frames_to_duration(i * 1_234 / 2);
            chunk_tx
                .send(pw::Chunk {
                    start,
                    samples: c.to_vec(),
                })
                .unwrap();
        }
        drop(chunk_tx);
        let enc = Encoder::new(encoder, anchor, Arc::default(), move |p| {
            tx.send(p).unwrap()
        });
        encode_loop(enc, chunk_rx);
        let packets: Vec<_> = rx.iter().collect();
        assert!(packets.len() >= 50, "{} packets", packets.len());
        for w in packets.windows(2) {
            assert_eq!(w[1].pts - w[0].pts, Duration::from_millis(20));
        }
        assert!(packets[0].pts <= anchor);
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
        let (chunk_tx, chunk_rx) = mpsc::channel();
        for (i, c) in samples.chunks(1_024 * 2).enumerate() {
            let start = anchor + frames_to_duration(i * 1_024);
            chunk_tx
                .send(pw::Chunk {
                    start,
                    samples: c.to_vec(),
                })
                .unwrap();
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
        encode_loop(enc, chunk_rx);
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
        f32::from_bits(shared.peak.load(Ordering::Relaxed))
    }

    #[test]
    fn muted_track_reports_zero_peak() {
        let shared = Arc::new(Shared::default());
        let mut enc = test_encoder(&shared);
        enc.push(&vec![0.75; PEAK_WINDOW * 2 * 2]);
        assert_eq!(peak(&shared), 0.75);
        shared.muted.store(true, Ordering::Relaxed);
        // The first muted block ramps down; the windows after it are silent.
        enc.push(&vec![0.75; PEAK_WINDOW * 2 * 4]);
        assert_eq!(peak(&shared), 0.0);
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
        enc.push(&vec![0.5; PEAK_WINDOW * 2 * 2]);
        // 0.5 doubled by the processor, then -6 dB.
        assert!(
            (peak(&shared) - db_to_gain(-6.0)).abs() < 1e-4,
            "{}",
            peak(&shared)
        );
    }
}
