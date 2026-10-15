//! Video recording: screen capture -> constant-frame-rate VAAPI encode -> MKV.
//!
//! Threads:
//! - PipeWire thread (`vesper-capture`): `on_frame` only parks the newest
//!   frame in a one-slot mailbox. A frame it replaces is dropped at once
//!   (released to PipeWire) and counted as dropped.
//! - Recorder thread (here): opens the capture (blocks on the portal picker),
//!   then ticks on an absolute `CLOCK_MONOTONIC` grid `start + k / fps`. Each
//!   tick encodes the parked frame, or repeats the last picture when the
//!   compositor sent none (Wayland sends frames on damage only), so the file
//!   has exactly `fps` frames per second. Packets go straight to the muxer.
//! - Audio threads (`vesper-audio`, two per source): Opus packets of the
//!   desktop and microphone tracks go to the same muxer, behind a mutex.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use vesper_audio::{AudioCapture, CHANNELS, SAMPLE_RATE, Source};
use vesper_capture::Capture;
use vesper_core::{Codec, EncodedPacket, VideoFrame};
use vesper_encode::{Encoder, RateControl};
use vesper_mux::{AudioTrack, Muxer, VideoTrack};

/// How often `Event::Stats` is sent.
const STATS_PERIOD: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct Settings {
    pub codec: Codec,
    pub fps: u32,
    /// Target bitrate per pixel per frame; scaled by the captured size and `fps`.
    pub bits_per_pixel: f64,
    pub folder: PathBuf,
    pub desktop_muted: bool,
    pub mic_muted: bool,
}

#[derive(Debug)]
pub enum Event {
    /// The capture runs (the picker, if any, is closed).
    Started,
    Stats {
        width: u32,
        height: u32,
        /// Recorded media time.
        elapsed: Duration,
        /// Frames written per wall-clock second over the last period.
        fps: f32,
        /// Captured frames replaced by a newer one before a tick took them.
        dropped: u64,
    },
    /// The recording ended. On a mid-recording error the file is still
    /// finalized up to that point.
    Finished(Result<PathBuf, String>),
}

/// Audio captures and mute state, shared with the UI thread. One lock, so a
/// mute toggled while the picker is open applies once the captures start.
#[derive(Default)]
struct Audio {
    running: Vec<(Source, AudioCapture)>,
    muted: Vec<Source>,
}

type AudioSources = Arc<Mutex<Audio>>;

pub struct Recorder {
    stop: Arc<AtomicBool>,
    audio: AudioSources,
    thread: JoinHandle<()>,
}

impl Recorder {
    /// Starts recording on a new thread. `on_event` runs on that thread.
    pub fn start(
        settings: Settings,
        mut on_event: impl FnMut(Event) + Send + 'static,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let audio = AudioSources::default();
        for (source, muted) in [
            (Source::Desktop, settings.desktop_muted),
            (Source::Microphone, settings.mic_muted),
        ] {
            if muted {
                audio.lock().unwrap().muted.push(source);
            }
        }
        let (flag, sources) = (stop.clone(), audio.clone());
        let thread = std::thread::Builder::new()
            .name("vesper-recorder".into())
            .spawn(move || {
                let result = run(&settings, &flag, &sources, &mut on_event);
                on_event(Event::Finished(result));
            })?;
        Ok(Self {
            stop,
            audio,
            thread,
        })
    }

    /// Asks the recording to end; `Event::Finished` follows within a frame
    /// interval plus the encoder flushes.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Mutes a track (it records silence).
    pub fn set_muted(&self, source: Source, muted: bool) {
        let mut audio = self.audio.lock().unwrap();
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
        let audio = self.audio.lock().unwrap();
        audio
            .running
            .iter()
            .find(|(s, _)| *s == source)
            .map_or(0.0, |(_, capture)| capture.peak())
    }

    /// Stops and waits for the file to be finalized.
    pub fn join(self) {
        self.stop();
        let _ = self.thread.join();
    }
}

/// Mailbox between the PipeWire thread and the recorder thread.
#[derive(Default)]
struct Slot {
    frame: Option<VideoFrame>,
    dropped: u64,
    ended: Option<String>,
}

/// The muxer, created on the first video frame, shared with the audio threads.
type SharedMuxer = Arc<Mutex<Option<Muxer>>>;

/// Starts the desktop and microphone captures. A source that fails to start
/// is left out of the file (e.g. no microphone).
fn start_audio(sources: &AudioSources, mux: &SharedMuxer) -> Vec<AudioTrack> {
    let mut tracks = Vec::new();
    for source in [Source::Desktop, Source::Microphone] {
        let (index, mux) = (tracks.len(), mux.clone());
        let capture = AudioCapture::start(source, move |packet| {
            // Packets before the muxer exists are before the start anyway.
            if let Some(m) = mux.lock().unwrap().as_mut()
                && let Err(e) = m.write_audio(index, &packet)
            {
                log!("{source:?} audio: {e}");
            }
        });
        match capture {
            Ok(capture) => {
                // Started outside the lock (it blocks until PipeWire is
                // ready); mute applied under it, so no toggle is missed.
                let mut audio = sources.lock().unwrap();
                capture.set_muted(audio.muted.contains(&source));
                tracks.push(AudioTrack {
                    sample_rate: SAMPLE_RATE,
                    channels: u32::from(CHANNELS),
                    extradata: capture.extradata().to_vec(),
                });
                audio.running.push((source, capture));
            }
            Err(e) => log!("recording without {source:?} audio: {e}"),
        }
    }
    tracks
}

fn run(
    settings: &Settings,
    stop: &AtomicBool,
    sources: &AudioSources,
    on_event: &mut impl FnMut(Event),
) -> Result<PathBuf, String> {
    let slot = Arc::new(Mutex::new(Slot::default()));
    let s = slot.clone();
    let capture = Capture::start(load_restore_token().as_deref(), move |event| {
        let mut slot = s.lock().unwrap();
        let old = match event {
            Ok(frame) => slot.frame.replace(frame),
            Err(e) => {
                slot.ended = Some(e.to_string());
                slot.frame.take()
            }
        };
        slot.dropped += u64::from(old.is_some());
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
    let mux = SharedMuxer::default();
    let audio = start_audio(sources, &mux);
    on_event(Event::Started);
    let result = record(settings, &slot, stop, &mux, sources, &audio, on_event);
    drop(capture);
    // Normally done by `record`; covers its early returns.
    let running = std::mem::take(&mut sources.lock().unwrap().running);
    drop(running);
    result
}

/// An open output: created from the first frame.
struct Output {
    enc: Encoder,
    mux: SharedMuxer,
    path: PathBuf,
    start: Duration,
    width: u32,
    height: u32,
}

impl Output {
    fn open(
        settings: &Settings,
        first: &VideoFrame,
        shared: &SharedMuxer,
        audio: &[AudioTrack],
    ) -> Result<Self, String> {
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
        std::fs::create_dir_all(&settings.folder)
            .map_err(|e| format!("Could not create {}: {e}", settings.folder.display()))?;
        let path = settings
            .folder
            .join(format!("Vesper {}.mkv", local_timestamp()));
        let track = VideoTrack {
            codec: settings.codec,
            width,
            height,
            fps,
            extradata: enc.extradata().to_vec(),
        };
        // The grid starts now, not at the frame's own pts: opening the encoder
        // takes a while, and a producer clock off by seconds must not stall
        // or burst the ticks. The first frame gets this pts.
        let start = monotonic_now();
        let mux = Muxer::create(&path, start, track, audio)
            .map_err(|e| format!("Could not create {}: {e}", path.display()))?;
        *shared.lock().unwrap() = Some(mux);
        log!("recording to {} at {bitrate} bit/s", path.display());
        Ok(Self {
            enc,
            mux: shared.clone(),
            path,
            start,
            width,
            height,
        })
    }

    fn write(&mut self, packets: Vec<EncodedPacket>) -> Result<(), String> {
        let mut mux = self.mux.lock().unwrap();
        let mux = mux.as_mut().ok_or("muxer already closed")?;
        for p in &packets {
            mux.write_video(p).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// Offset of frame `k` from the start: exact multiples of `1 / fps`.
fn frame_offset(k: u64, fps: u32) -> Duration {
    Duration::from_nanos(k * 1_000_000_000 / u64::from(fps))
}

fn record(
    settings: &Settings,
    slot: &Mutex<Slot>,
    stop: &AtomicBool,
    mux: &SharedMuxer,
    sources: &AudioSources,
    audio: &[AudioTrack],
    on_event: &mut impl FnMut(Event),
) -> Result<PathBuf, String> {
    let fps = settings.fps;
    let mut out: Option<Output> = None;
    let mut k = 0u64;
    let mut dropped_base = 0;
    let mut skipped = 0;
    let mut deadline = monotonic_now();
    let mut window = (monotonic_now(), 0u32);
    let mut error = None;

    loop {
        sleep_until(deadline);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let (frame, ended, dropped) = {
            let mut s = slot.lock().unwrap();
            (s.frame.take(), s.ended.take(), s.dropped)
        };
        if let Some(e) = ended {
            error = Some(format!("Capture ended: {e}"));
            break;
        }
        let step = match (&mut out, frame) {
            // Waiting for the first frame: poll once per interval.
            (None, None) => {
                deadline += frame_offset(1, fps);
                continue;
            }
            (None, Some(mut frame)) => {
                dropped_base = dropped;
                Output::open(settings, &frame, mux, audio).and_then(|o| {
                    let o = out.insert(o);
                    frame.pts = o.start;
                    let packets = o.enc.encode(frame).map_err(|e| e.to_string())?;
                    o.write(packets)
                })
            }
            (Some(o), frame) => {
                k += 1;
                let pts = o.start + frame_offset(k, fps);
                let packets = match frame {
                    Some(mut frame) => {
                        frame.pts = pts;
                        o.enc.encode(frame)
                    }
                    None => o.enc.repeat(pts),
                };
                packets
                    .map_err(|e| format!("Encoding failed: {e}"))
                    .and_then(|p| o.write(p))
            }
        };
        if let Err(e) = step {
            error = Some(e);
            break;
        }
        let Some(o) = &out else { break };
        deadline = o.start + frame_offset(k + 1, fps);
        let now = monotonic_now();
        if now >= deadline + frame_offset(1, fps) {
            // More than a frame behind (GPU too busy): skip the missed ticks,
            // so video keeps its place against the audio, and count them.
            let behind = (now - o.start).as_nanos() * u128::from(fps) / 1_000_000_000;
            let behind = (behind as u64).saturating_sub(k + 1);
            k += behind;
            skipped += behind;
            deadline = o.start + frame_offset(k + 1, fps);
        }

        window.1 += 1;
        let span = now.saturating_sub(window.0);
        if k == 0 || span >= STATS_PERIOD {
            on_event(Event::Stats {
                width: o.width,
                height: o.height,
                elapsed: frame_offset(k, fps),
                fps: if k == 0 {
                    fps as f32
                } else {
                    window.1 as f32 / span.as_secs_f32()
                },
                dropped: dropped - dropped_base + skipped,
            });
            window = (now, 0);
        }
    }

    let Some(mut o) = out else {
        return Err(error.unwrap_or_else(|| "No frame was captured".into()));
    };
    let flushed = o.enc.flush().map_err(|e| e.to_string());
    let flushed = flushed.and_then(|p| o.write(p));
    // Stopping the audio flushes its last packets into the muxer, so do it
    // before closing, without holding the muxer lock the audio threads need.
    let running = std::mem::take(&mut sources.lock().unwrap().running);
    drop(running);
    let mux = o.mux.lock().unwrap().take();
    let finished = mux.map_or(Ok(()), |m| m.finish().map_err(|e| e.to_string()));
    match error.or(flushed.err()).or(finished.err()) {
        None => Ok(o.path),
        Some(e) => Err(format!("{e} (saved up to there: {})", o.path.display())),
    }
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
    std::fs::write(path, token)
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
    fn timestamp_is_file_name_safe() {
        let ts = local_timestamp();
        assert_eq!(ts.len(), 19, "{ts}");
        assert!(!ts.contains(':') && !ts.contains('/'), "{ts}");
    }
}
