//! Desktop and microphone audio capture through PipeWire, encoded to Opus.
//!
//! [`AudioCapture::start`] runs one PipeWire stream (its own thread) and one
//! encode thread per source. Packets carry `CLOCK_MONOTONIC` pts like the
//! video frames, so the muxer can put both on one time base.
//!
//! Timestamps come from a sample counter anchored at the start time, so a
//! track has no gaps. Each PipeWire buffer is also placed on the monotonic
//! clock (graph cycle time minus device delay minus buffer length); when the
//! counter drifts from that by more than [`TOLERANCE`] (lost buffers, a
//! suspended device, clock drift), silence is inserted or input dropped.

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

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

/// Output sample rate of every track.
pub const SAMPLE_RATE: u32 = 48_000;
/// Output channels of every track (mono sources are upmixed by PipeWire).
pub const CHANNELS: u16 = 2;
/// Opus frame: 20 ms.
const FRAME: usize = 960;
const BIT_RATE: usize = 160_000;
/// Peak meter window: ~33 ms.
const PEAK_WINDOW: usize = 1_600;
/// Timeline error allowed before inserting silence or dropping input.
pub const TOLERANCE: Duration = Duration::from_millis(30);
/// With no buffer for this long, the source is stalled: fill with silence.
const STALL: Duration = Duration::from_millis(250);

/// Which audio to record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Monitor of the default output device.
    Desktop,
    /// Default input device.
    Microphone,
}

/// State shared with the encode thread.
#[derive(Default)]
struct Shared {
    muted: AtomicBool,
    /// `f32` bits of the last window's peak.
    peak: AtomicU32,
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
        on_packet: impl FnMut(EncodedPacket) + Send + 'static,
    ) -> Result<Self> {
        let anchor = monotonic_now();
        let encoder = open_encoder()?;
        // SAFETY: the opened encoder owns valid extradata of that size.
        let extradata = unsafe {
            let ctx = encoder.as_ptr();
            if (*ctx).extradata.is_null() {
                return Err("libopus produced no OpusHead".into());
            }
            std::slice::from_raw_parts((*ctx).extradata, (*ctx).extradata_size as usize).to_vec()
        };
        let shared = Arc::new(Shared::default());
        let (chunk_tx, chunk_rx) = mpsc::channel();
        let encode_thread = std::thread::Builder::new()
            .name("vesper-audio-enc".into())
            .spawn({
                let shared = shared.clone();
                move || encode_loop(encoder, anchor, chunk_rx, &shared, on_packet)
            })?;
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

    /// Peak level of the last ~33 ms, linear 0..=1 (0 while muted).
    pub fn peak(&self) -> f32 {
        f32::from_bits(self.shared.peak.load(Ordering::Relaxed))
    }

    /// `OpusHead` for the container's codec private data.
    pub fn extradata(&self) -> &[u8] {
        &self.extradata
    }

    pub fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    pub fn channels(&self) -> u16 {
        CHANNELS
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

pub(crate) fn monotonic_now() -> Duration {
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
}

impl Timeline {
    /// Places `len` frames measured to start at `start`. Returns how many
    /// silent frames to insert before them, and how many of them to drop.
    fn place(&mut self, start: Duration, len: usize) -> (usize, usize) {
        let expected = self.anchor + frames_to_duration(self.frames);
        let (pad, skip) = if start > expected + TOLERANCE {
            (duration_to_frames(start - expected), 0)
        } else if start + TOLERANCE < expected {
            (0, duration_to_frames(expected - start).min(len))
        } else {
            (0, 0)
        };
        self.frames += pad + len - skip;
        (pad, skip)
    }
}

/// Feeds samples to the encoder 20 ms at a time and tracks the peak.
struct Encoder<'a, F> {
    encoder: encoder::Audio,
    frame: frame::Audio,
    pending: Vec<f32>,
    /// Frames sent to the encoder, the frame pts in 1/48000 s.
    sent: i64,
    anchor: Duration,
    shared: &'a Shared,
    peak: f32,
    peak_samples: usize,
    on_packet: F,
}

impl<F: FnMut(EncodedPacket)> Encoder<'_, F> {
    fn push(&mut self, samples: &[f32]) {
        self.pending.extend_from_slice(samples);
        let len = FRAME * usize::from(CHANNELS);
        while self.pending.len() >= len {
            let muted = self.shared.muted.load(Ordering::Relaxed);
            let out = self.frame.data_mut(0);
            for (dst, src) in out
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(self.pending.drain(..len))
            {
                let s = if muted { 0.0 } else { src };
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
        // Pad the last partial frame so no captured audio is lost.
        let rem = self.pending.len() % (FRAME * usize::from(CHANNELS));
        if rem != 0 {
            self.push(&vec![0.0; FRAME * usize::from(CHANNELS) - rem]);
        }
        if let Err(e) = self.encoder.send_eof() {
            error!("opus flush failed: {e}");
        }
        self.drain();
        self.shared.peak.store(0, Ordering::Relaxed);
    }
}

fn encode_loop(
    encoder: encoder::Audio,
    anchor: Duration,
    rx: mpsc::Receiver<pw::Chunk>,
    shared: &Shared,
    on_packet: impl FnMut(EncodedPacket),
) {
    let mut timeline = Timeline { anchor, frames: 0 };
    let mut enc = Encoder {
        encoder,
        frame: frame::Audio::new(Sample::F32(Type::Packed), FRAME, ChannelLayout::STEREO),
        pending: Vec::new(),
        sent: 0,
        anchor,
        shared,
        peak: 0.0,
        peak_samples: 0,
        on_packet,
    };
    enc.frame.set_rate(SAMPLE_RATE);
    let ch = usize::from(CHANNELS);
    loop {
        let (pad, samples) = match rx.recv_timeout(STALL) {
            Ok(chunk) => {
                let (pad, skip) = timeline.place(chunk.start, chunk.samples.len() / ch);
                (
                    pad,
                    chunk.samples.get(skip * ch..).unwrap_or_default().to_vec(),
                )
            }
            // Stalled source (no device, suspended): keep the track continuous.
            Err(RecvTimeoutError::Timeout) => {
                let (pad, _) = timeline.place(monotonic_now().saturating_sub(STALL), 0);
                (pad, Vec::new())
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if pad > 0 {
            warn!("audio timeline gap: {pad} frames of silence inserted");
            enc.push(&vec![0.0; pad * ch]);
        }
        enc.push(&samples);
    }
    enc.finish();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_pads_gaps_and_drops_overlaps() {
        let anchor = Duration::from_secs(100);
        let mut t = Timeline { anchor, frames: 0 };
        // Within tolerance: taken as is.
        assert_eq!(t.place(anchor + Duration::from_millis(10), 960), (0, 0));
        assert_eq!(t.frames, 960);
        // 100 ms gap after the expected 20 ms: 4800 frames of silence.
        assert_eq!(
            t.place(anchor + Duration::from_millis(120), 960),
            (4_800, 0)
        );
        assert_eq!(t.frames, 960 + 4_800 + 960);
        // Expected at 140 ms, buffer claims 90 ms: drop 50 ms of it.
        assert_eq!(
            t.place(anchor + Duration::from_millis(90), 4_800),
            (0, 2_400)
        );
        assert_eq!(t.frames, 960 + 4_800 + 960 + 2_400);
        // Drops never exceed the buffer.
        assert_eq!(t.place(anchor, 10), (0, 10));
    }

    #[test]
    fn encoder_emits_continuous_20ms_packets_with_head() {
        let encoder = open_encoder().unwrap();
        let head = unsafe {
            let ctx = encoder.as_ptr();
            std::slice::from_raw_parts((*ctx).extradata, (*ctx).extradata_size as usize).to_vec()
        };
        assert_eq!(&head[..8], b"OpusHead");
        assert_eq!(head[9], CHANNELS as u8);
        let (tx, rx) = mpsc::channel();
        let shared = Shared::default();
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
        encode_loop(encoder, anchor, chunk_rx, &shared, move |p| {
            tx.send(p).unwrap()
        });
        let packets: Vec<_> = rx.iter().collect();
        assert!(packets.len() >= 50, "{} packets", packets.len());
        for w in packets.windows(2) {
            assert_eq!(w[1].pts - w[0].pts, Duration::from_millis(20));
        }
        assert!(packets[0].pts <= anchor);
    }

    #[test]
    fn muted_track_reports_zero_peak() {
        let shared = Shared::default();
        let mut enc = Encoder {
            encoder: open_encoder().unwrap(),
            frame: frame::Audio::new(Sample::F32(Type::Packed), FRAME, ChannelLayout::STEREO),
            pending: Vec::new(),
            sent: 0,
            anchor: Duration::ZERO,
            shared: &shared,
            peak: 0.0,
            peak_samples: 0,
            on_packet: |_| {},
        };
        enc.frame.set_rate(SAMPLE_RATE);
        enc.push(&vec![0.75; PEAK_WINDOW * 2 * 2]);
        assert_eq!(f32::from_bits(shared.peak.load(Ordering::Relaxed)), 0.75);
        shared.muted.store(true, Ordering::Relaxed);
        enc.push(&vec![0.75; PEAK_WINDOW * 2 * 2]);
        assert_eq!(f32::from_bits(shared.peak.load(Ordering::Relaxed)), 0.0);
    }
}
