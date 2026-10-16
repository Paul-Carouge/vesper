//! Level meter ballistics and scale for the audio tracks.
//!
//! A sample peak meter in dBFS: instant attack, a release of 20 dB per
//! 300 ms, a peak hold marker that stays 1.5 s then falls, and a clip
//! indicator latched until cleared. The scale follows IEC 60268-18 above
//! -50 dB (-20 dB at half the width), so the top 20 dB, where levels are
//! set, get half of it. Positions are quantized: a meter at rest, or moving
//! less than a step, produces equal values and so no redraw.

use std::time::Duration;

use vesper_audio::Peaks;

use crate::TrackMeter;

/// Lowest level shown: quieter is silence, the meter rests.
pub const FLOOR_DB: f32 = -60.0;
/// Release: 20 dB in 300 ms.
const RELEASE_DB_PER_SEC: f32 = 20.0 / 0.3;
const HOLD: Duration = Duration::from_millis(1500);
/// Fall of the held peak once its hold is over.
const HOLD_FALL_DB_PER_SEC: f32 = 30.0;
/// Positions move by 1/200 of the meter's width (~1.5 px on a 300 px bar).
const STEPS: f32 = 200.0;

/// `peak` (linear, 1.0 is full scale) in dBFS, at most 0, at least the floor.
pub fn peak_db(peak: f32) -> f32 {
    if peak > 0.0 {
        (20.0 * peak.log10()).clamp(FLOOR_DB, 0.0)
    } else {
        FLOOR_DB
    }
}

/// Where `db` sits on the meter, 0..=1: IEC 60268-18 above -50 dB, linear
/// down to the floor below it. The UI's ticks use the same points.
pub fn position(db: f32) -> f32 {
    const POINTS: [(f32, f32); 6] = [
        (FLOOR_DB, 0.0),
        (-50.0, 0.075),
        (-40.0, 0.15),
        (-30.0, 0.3),
        (-20.0, 0.5),
        (0.0, 1.0),
    ];
    if db.is_nan() || db <= FLOOR_DB {
        return 0.0;
    }
    for pair in POINTS.windows(2) {
        let ((d0, p0), (d1, p1)) = (pair[0], pair[1]);
        if db <= d1 {
            return p0 + (db - d0) / (d1 - d0) * (p1 - p0);
        }
    }
    1.0
}

/// `position` rounded to a step, so small moves compare equal.
fn quantize(position: f32) -> f32 {
    (position * STEPS).round() / STEPS
}

/// One channel's ballistics, in dBFS.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Channel {
    level: f32,
    hold: f32,
    /// Hold time left before the held peak falls.
    held: Duration,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            level: FLOOR_DB,
            hold: FLOOR_DB,
            held: Duration::ZERO,
        }
    }
}

impl Channel {
    fn feed(&mut self, db: f32, dt: Duration) {
        let secs = dt.as_secs_f32();
        let released = self.level - RELEASE_DB_PER_SEC * secs;
        self.level = db.max(released).clamp(FLOOR_DB, 0.0);
        if self.level >= self.hold {
            self.hold = self.level;
            self.held = HOLD;
        } else if let Some(left) = self.held.checked_sub(dt) {
            self.held = left;
        } else {
            self.held = Duration::ZERO;
            self.hold = (self.hold - HOLD_FALL_DB_PER_SEC * secs).max(self.level);
        }
    }

    fn rested(&self) -> bool {
        self.level <= FLOOR_DB && self.hold <= FLOOR_DB
    }
}

/// A track's meter: both channels, the clip latch, and whether it shows two
/// bars.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct MeterState {
    left: Channel,
    right: Channel,
    clip: bool,
    stereo: bool,
    /// Linear peaks of the last block seen, for ticks that got none.
    last: (f32, f32),
}

impl MeterState {
    /// A meter for a stereo source (two bars from the start), or one that
    /// splits once its channels differ.
    pub fn new(stereo: bool) -> Self {
        Self {
            stereo,
            ..Self::default()
        }
    }

    /// Feeds the peaks taken over the last `dt`. Without a new block, the
    /// last one's peaks still stand: a steady tone stays still.
    pub fn feed(&mut self, peaks: Peaks, dt: Duration) {
        let (left, right) = if peaks.fresh {
            (peaks.left, peaks.right)
        } else {
            // Read while a block was half stored: keep the louder.
            (peaks.left.max(self.last.0), peaks.right.max(self.last.1))
        };
        self.last = (left, right);
        self.left.feed(peak_db(left), dt);
        self.right.feed(peak_db(right), dt);
        self.clip |= peaks.clipped;
        self.stereo |= peaks.stereo;
    }

    pub fn clear_clip(&mut self) {
        self.clip = false;
    }

    /// Nothing moves: no need to tick until a signal comes.
    pub fn rested(&self) -> bool {
        self.left.rested() && self.right.rested()
    }

    /// What the UI shows. Equal values for moves under a step.
    pub fn shown(&self) -> TrackMeter {
        let (left, right) = if self.stereo {
            (self.left, self.right)
        } else {
            // One bar: the louder channel.
            let louder = if self.right.level > self.left.level {
                self.right
            } else {
                self.left
            };
            let hold = self.left.hold.max(self.right.hold);
            (Channel { hold, ..louder }, Channel { hold, ..louder })
        };
        TrackMeter {
            left: quantize(position(left.level)),
            right: quantize(position(right.level)),
            hold_left: quantize(position(left.hold)),
            hold_right: quantize(position(right.hold)),
            clip: self.clip,
            stereo: self.stereo,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: Duration = Duration::from_millis(33);

    fn peaks(left: f32, right: f32) -> Peaks {
        Peaks {
            left,
            right,
            fresh: true,
            ..Peaks::default()
        }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn decibels_clamp_to_the_scale() {
        assert_eq!(peak_db(1.0), 0.0);
        assert_eq!(peak_db(2.0), 0.0, "overs show as full scale");
        assert!(close(peak_db(0.5), -6.0206));
        assert!(close(peak_db(0.1), -20.0));
        assert_eq!(peak_db(0.0), FLOOR_DB);
        assert_eq!(peak_db(1e-6), FLOOR_DB);
        assert_eq!(peak_db(f32::NAN), FLOOR_DB);
    }

    #[test]
    fn scale_follows_iec_points() {
        assert_eq!(position(FLOOR_DB), 0.0);
        assert_eq!(position(-90.0), 0.0);
        assert!(close(position(-40.0), 0.15));
        assert!(close(position(-20.0), 0.5));
        assert!(close(position(-12.0), 0.7));
        assert!(close(position(-6.0), 0.85));
        assert!(close(position(-3.0), 0.925));
        assert_eq!(position(0.0), 1.0);
        assert_eq!(position(3.0), 1.0);
        assert_eq!(position(f32::NAN), 0.0);
        // Monotonic.
        let mut last = 0.0;
        for db in -70..=0 {
            let p = position(db as f32);
            assert!(p >= last, "{db} dB");
            last = p;
        }
    }

    #[test]
    fn attack_is_instant_and_release_takes_300_ms_per_20_db() {
        let mut m = MeterState::new(false);
        m.feed(peaks(0.1, 0.1), TICK);
        assert!(close(m.left.level, -20.0));
        // 300 ms of silence: down 20 dB.
        for _ in 0..10 {
            m.feed(peaks(0.0, 0.0), Duration::from_millis(30));
        }
        assert!(close(m.left.level, -40.0), "{}", m.left.level);
        // And at rest once the hold is over too.
        for _ in 0..100 {
            m.feed(peaks(0.0, 0.0), TICK);
        }
        assert!(m.rested());
        assert_eq!(m.shown(), MeterState::new(false).shown());
    }

    #[test]
    fn peak_holds_for_1_5_s_then_falls() {
        let mut m = MeterState::new(true);
        m.feed(peaks(0.5, 0.0), TICK);
        let held = m.shown().hold_left;
        for _ in 0..44 {
            m.feed(peaks(0.0, 0.0), TICK);
        }
        assert_eq!(m.shown().hold_left, held, "held ~1.45 s");
        for _ in 0..4 {
            m.feed(peaks(0.0, 0.0), TICK);
        }
        assert!(m.shown().hold_left < held, "falling after 1.5 s");
        assert!(m.left.hold >= m.left.level);
        // A louder peak replaces it at once.
        m.feed(peaks(1.0, 0.0), TICK);
        assert_eq!(m.shown().hold_left, 1.0);
    }

    #[test]
    fn clip_latches_until_cleared() {
        let mut m = MeterState::new(true);
        m.feed(
            Peaks {
                clipped: true,
                ..peaks(1.0, 1.0)
            },
            TICK,
        );
        for _ in 0..200 {
            m.feed(peaks(0.0, 0.0), TICK);
        }
        assert!(m.rested() && m.shown().clip, "still latched at rest");
        m.clear_clip();
        assert!(!m.shown().clip);
    }

    #[test]
    fn mono_shows_the_louder_channel_until_stereo_is_seen() {
        let mut m = MeterState::new(false);
        m.feed(peaks(0.1, 0.5), TICK);
        let shown = m.shown();
        assert!(!shown.stereo);
        assert_eq!(shown.left, shown.right);
        assert_eq!(shown.left, quantize(position(peak_db(0.5))));
        m.feed(
            Peaks {
                stereo: true,
                ..peaks(0.1, 0.5)
            },
            TICK,
        );
        let shown = m.shown();
        assert!(shown.stereo && shown.left < shown.right);
        // Sticky: silence does not fold it back.
        m.feed(peaks(0.0, 0.0), TICK);
        assert!(m.shown().stereo);
    }

    #[test]
    fn a_tick_without_a_block_keeps_the_last_peaks() {
        let mut m = MeterState::new(true);
        m.feed(peaks(0.1, 0.1), TICK);
        let shown = m.shown();
        m.feed(Peaks::default(), TICK);
        assert_eq!(m.shown(), shown, "no dip between blocks");
        m.feed(peaks(0.0, 0.0), TICK);
        assert!(m.shown().left < shown.left, "real silence releases");
    }

    #[test]
    fn small_moves_do_not_change_what_is_shown() {
        let mut a = MeterState::new(true);
        let mut b = a;
        a.feed(peaks(0.5, 0.5), TICK);
        b.feed(peaks(0.5005, 0.5005), TICK);
        assert_eq!(a.shown(), b.shown());
    }
}
