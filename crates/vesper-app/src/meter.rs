//! Level meter ballistics for the audio tracks.

use std::time::Duration;

/// Smallest change worth a redraw: half a percent of the meter's width.
pub const VISIBLE_STEP: f32 = 0.005;

/// Whether a meter showing `shown` must be redrawn to show `value`: a
/// visible step, or reaching or leaving zero.
pub fn visible_change(shown: f32, value: f32) -> bool {
    (shown - value).abs() >= VISIBLE_STEP || (shown == 0.0) != (value == 0.0)
}

/// Level meter ballistics: instant attack, ~300 ms release, peak hold.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct MeterState {
    pub level: f32,
    pub peak: f32,
    hold: Duration,
}

impl MeterState {
    /// Time constant of the exponential release.
    const RELEASE: Duration = Duration::from_millis(300);
    const PEAK_HOLD: Duration = Duration::from_secs(1);
    const PEAK_FALL_PER_SEC: f32 = 0.5;

    /// Feeds one instantaneous sample (linear 0..1) covering `dt`.
    pub fn feed(&mut self, sample: f32, dt: Duration) {
        let sample = sample.clamp(0.0, 1.0);
        let secs = dt.as_secs_f32();
        let released = self.level * (-secs / Self::RELEASE.as_secs_f32()).exp();
        // Below a visible step, rest at 0 instead of decaying forever.
        self.level = if released < VISIBLE_STEP {
            sample
        } else {
            sample.max(released)
        };
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
        // Down to a visible step in ~1.5 s (5 time constants), then at rest.
        for _ in 0..50 {
            m.feed(0.0, TICK);
        }
        assert_eq!(m.level, 0.0);
    }

    #[test]
    fn only_visible_steps_redraw() {
        assert!(!visible_change(0.5, 0.503));
        assert!(visible_change(0.5, 0.506));
        assert!(visible_change(0.002, 0.0));
        assert!(visible_change(0.0, 0.001));
        assert!(!visible_change(0.0, 0.0));
    }

    #[test]
    fn meter_holds_peak_then_falls() {
        let mut m = MeterState::default();
        m.feed(0.9, TICK);
        for _ in 0..20 {
            m.feed(0.0, TICK);
        }
        assert_eq!(m.peak, 0.9, "peak is held for ~1 s");
        for _ in 0..60 {
            m.feed(0.0, TICK);
        }
        assert!(m.peak < 0.9);
        assert!(m.peak >= m.level);
    }
}
