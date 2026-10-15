//! Level meter ballistics for the audio tracks.

use std::time::Duration;

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
}
