//! Fake backend state used until capture, encode and audio are wired in.

use std::time::{Duration, Instant};

/// Level meter ballistics: instant attack, smooth release, peak hold.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct MeterState {
    pub level: f32,
    pub peak: f32,
    hold: Duration,
}

impl MeterState {
    const RELEASE_PER_SEC: f32 = 1.6;
    const PEAK_HOLD: Duration = Duration::from_millis(900);
    const PEAK_FALL_PER_SEC: f32 = 0.5;

    /// Feeds one instantaneous sample (linear 0..1) covering `dt`.
    pub fn feed(&mut self, sample: f32, dt: Duration) {
        let sample = sample.clamp(0.0, 1.0);
        let secs = dt.as_secs_f32();
        self.level = sample
            .max(self.level - Self::RELEASE_PER_SEC * secs)
            .max(0.0);
        if self.level >= self.peak {
            self.peak = self.level;
            self.hold = Self::PEAK_HOLD;
        } else if let Some(left) = self.hold.checked_sub(dt) {
            self.hold = left;
        } else {
            self.hold = Duration::ZERO;
            self.peak = (self.peak - Self::PEAK_FALL_PER_SEC * secs).max(self.level);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordingStats {
    pub elapsed: Duration,
    pub fps: f32,
    pub dropped: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Snapshot {
    pub desktop: MeterState,
    pub mic: MeterState,
    pub recording: Option<RecordingStats>,
}

#[derive(Debug)]
pub struct Sim {
    pub desktop_muted: bool,
    pub mic_muted: bool,
    desktop: MeterState,
    mic: MeterState,
    recording: Option<RecordingStats>,
    started: Option<Instant>,
    last_tick: Option<Instant>,
    target_fps: f32,
    t: f32,
    rng: u32,
}

impl Default for Sim {
    fn default() -> Self {
        Self {
            desktop_muted: false,
            mic_muted: false,
            desktop: MeterState::default(),
            mic: MeterState::default(),
            recording: None,
            started: None,
            last_tick: None,
            target_fps: 60.0,
            t: 0.0,
            rng: 0x9E37_79B9,
        }
    }
}

impl Sim {
    pub fn start_recording(&mut self, fps: f32, now: Instant) {
        self.target_fps = fps;
        self.started = Some(now);
        self.recording = Some(RecordingStats {
            elapsed: Duration::ZERO,
            fps,
            dropped: 0,
        });
    }

    pub fn stop_recording(&mut self) {
        self.recording = None;
        self.started = None;
    }

    /// Advances to `now` (wall clock) and returns the state to display.
    pub fn tick(&mut self, now: Instant) -> Snapshot {
        let dt = self
            .last_tick
            .map_or(Duration::ZERO, |last| now.saturating_duration_since(last));
        self.last_tick = Some(now);
        let mut snap = self.step(dt);
        if let (Some(started), Some(rec)) = (self.started, self.recording.as_mut()) {
            rec.elapsed = now.saturating_duration_since(started);
            snap.recording = Some(*rec);
        }
        snap
    }

    /// Advances the simulation by `dt`.
    fn step(&mut self, dt: Duration) -> Snapshot {
        self.t += dt.as_secs_f32();
        let t = self.t;

        // Desktop: a steady "music" bed with a beat.
        let beat = (t * 2.0 * std::f32::consts::TAU).sin().max(0.0).powi(4);
        let desktop = 0.45 + 0.12 * (t * 0.7).sin() + 0.3 * beat + 0.08 * self.noise();
        // Mic: speech-like bursts separated by silence.
        let talking = (t * 0.45).sin() > 0.1;
        let mic = if talking {
            0.25 + 0.35 * (t * 9.0).sin().abs() * (t * 2.3).sin().abs() + 0.15 * self.noise()
        } else {
            0.02 * self.noise()
        };

        self.desktop
            .feed(if self.desktop_muted { 0.0 } else { desktop }, dt);
        self.mic.feed(if self.mic_muted { 0.0 } else { mic }, dt);

        let jitter = self.noise() - 0.5;
        let target = self.target_fps;
        let drop_roll = self.noise();
        if let Some(rec) = self.recording.as_mut() {
            rec.fps = target - 0.2 * jitter.abs();
            // A rare dropped frame keeps the counter honest-looking.
            if drop_roll > 0.9993 {
                rec.dropped += 1;
            }
        }

        Snapshot {
            desktop: self.desktop,
            mic: self.mic,
            recording: self.recording,
        }
    }

    /// Uniform noise in 0..1 (xorshift32, no dependency needed).
    fn noise(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x >> 8) as f32 / (1u32 << 24) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: Duration = Duration::from_millis(33);

    #[test]
    fn meter_attacks_instantly_and_releases_slowly() {
        let mut m = MeterState::default();
        m.feed(0.8, TICK);
        assert_eq!(m.level, 0.8);
        m.feed(0.0, TICK);
        assert!(m.level > 0.7 && m.level < 0.8, "level = {}", m.level);
    }

    #[test]
    fn meter_holds_peak_then_falls() {
        let mut m = MeterState::default();
        m.feed(0.9, TICK);
        for _ in 0..20 {
            m.feed(0.0, TICK);
        }
        assert_eq!(m.peak, 0.9, "peak is held for ~0.9 s");
        for _ in 0..60 {
            m.feed(0.0, TICK);
        }
        assert!(m.peak < 0.9);
        assert!(m.peak >= m.level);
    }

    #[test]
    fn muted_track_goes_silent() {
        let mut sim = Sim {
            mic_muted: true,
            ..Sim::default()
        };
        let mut last = None;
        for _ in 0..120 {
            last = Some(sim.step(TICK));
        }
        let snap = last.unwrap();
        assert_eq!(snap.mic.level, 0.0);
        assert!(snap.desktop.level > 0.0);
    }

    #[test]
    fn recording_tracks_wall_clock_time() {
        let t0 = Instant::now();
        let mut sim = Sim::default();
        assert!(sim.tick(t0).recording.is_none());
        sim.start_recording(60.0, t0);
        // Irregular ticks: elapsed follows the clock, not the tick count.
        sim.tick(t0 + Duration::from_millis(10));
        let snap = sim.tick(t0 + Duration::from_millis(1500));
        let rec = snap.recording.expect("recording");
        assert_eq!(rec.elapsed, Duration::from_millis(1500));
        assert!(rec.fps > 59.0 && rec.fps <= 60.0);
        sim.stop_recording();
        assert!(sim.tick(t0 + Duration::from_secs(2)).recording.is_none());
    }
}
