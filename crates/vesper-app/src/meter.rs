//! Level meter ballistics and scale for the audio tracks.
//!
//! A sample peak meter in dBFS: instant attack, a release of 20 dB per
//! 300 ms, a peak hold marker that stays 1.5 s then falls, and a clip
//! indicator latched until cleared. The scale follows IEC 60268-18 above
//! -50 dB (-20 dB at half the width), so the top 20 dB, where levels are
//! set, get half of it. Positions are quantized: a meter at rest, or moving
//! less than a step, produces equal values and so no redraw.

use std::time::Duration;

use vesper_audio::{DuckState, Peaks};

use crate::{TrackDuck, TrackMeter};

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

    /// Back to rest, levels and held peaks dropped (the window was away);
    /// the clip latch and the bar count stay.
    pub fn reset(&mut self) {
        *self = Self {
            clip: self.clip,
            stereo: self.stereo,
            ..Self::default()
        };
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

/// A track lowered by more than this (dB) says so.
const LOWERED_DB: f32 = 1.0;
/// The caption's dB follow the gain with this time constant: the
/// compressor's quick moves and the duck's ramps do not flicker it.
const GAIN_SMOOTH: Duration = Duration::from_millis(300);
/// The dB shown change once the gain is this far from them, so a gain
/// sitting between two whole dB does not flap.
const GAIN_STEP_DB: f32 = 1.0;
/// "Voice detected" stays this long after the last voiced window: it does
/// not blink between words.
const VOICE_HOLD: Duration = Duration::from_millis(600);

/// A track's ducking caption: a voice that speaks, the duck that lowers a
/// track, and the whole gain of the duck, leveler and compressor in whole
/// dB, fed at the meters' tick.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct DuckCaption {
    /// The gain, dB, smoothed, and where it goes.
    gain: f32,
    target: f32,
    /// What is shown of it, whole dB.
    db: i32,
    lowered: bool,
    /// Time left showing the voice.
    voice: Duration,
}

impl DuckCaption {
    /// Feeds the state taken over the last `dt`.
    pub fn feed(&mut self, state: DuckState, dt: Duration) {
        self.voice = if state.voiced {
            VOICE_HOLD
        } else {
            self.voice.saturating_sub(dt)
        };
        self.lowered = state.lowered_db > LOWERED_DB;
        // Lowered, the drop shown is at least the duck's: a quiet song the
        // leveler raised more than the duck lowers it is still lowered.
        self.target = if self.lowered {
            state.gain_db.min(-state.lowered_db)
        } else {
            state.gain_db
        };
        let k = (dt.as_secs_f32() / GAIN_SMOOTH.as_secs_f32()).min(1.0);
        self.gain += (self.target - self.gain) * k;
        // Moving, the dB follow the gain by steps; settled, they round it
        // (a gain at x.5 keeps the dB it had).
        let off = (self.gain - self.db as f32).abs();
        let settled = (self.gain - self.target).abs() < 0.05;
        if off >= GAIN_STEP_DB || (settled && off > 0.6) {
            self.db = self.gain.round() as i32;
        }
    }

    /// Back to the state as it is, without the past (the window was away,
    /// the track list changed): the gain at once, and no voice, which may
    /// have spoken long ago.
    pub fn reset(&mut self, state: DuckState) {
        *self = Self::default();
        let state = DuckState {
            voiced: false,
            ..state
        };
        self.feed(state, GAIN_SMOOTH);
    }

    /// Nothing to tick for: no voice shown, not lowered, the gain settled.
    pub fn rested(&self) -> bool {
        self.voice.is_zero() && !self.lowered && (self.gain - self.target).abs() < 0.1
    }

    pub fn shown(&self) -> TrackDuck {
        TrackDuck {
            speaking: !self.voice.is_zero(),
            lowered: self.lowered,
            // Lowered says by how much, at least a dB.
            db: if self.lowered {
                self.db.min(-1)
            } else {
                self.db
            },
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
    fn a_track_that_stops_running_comes_to_rest() {
        let mut m = MeterState::new(true);
        m.feed(peaks(0.5, 0.5), TICK);
        // What the pipeline reports for a track it does not run.
        for _ in 0..100 {
            m.feed(Peaks::SILENT, TICK);
        }
        assert!(m.rested());
    }

    #[test]
    fn reset_drops_levels_but_keeps_the_clip() {
        let mut m = MeterState::new(false);
        m.feed(
            Peaks {
                clipped: true,
                stereo: true,
                ..peaks(1.0, 0.5)
            },
            TICK,
        );
        m.reset();
        assert!(m.rested());
        let shown = m.shown();
        assert!(shown.clip && shown.stereo);
        // The last peaks are gone too: a tick without a block stays at rest.
        m.feed(Peaks::default(), TICK);
        assert!(m.rested());
    }

    #[test]
    fn small_moves_do_not_change_what_is_shown() {
        let mut a = MeterState::new(true);
        let mut b = a;
        a.feed(peaks(0.5, 0.5), TICK);
        b.feed(peaks(0.5005, 0.5005), TICK);
        assert_eq!(a.shown(), b.shown());
    }

    fn duck(lowered_db: f32, gain_db: f32, voiced: bool) -> DuckState {
        DuckState {
            lowered_db,
            gain_db,
            voiced,
        }
    }

    #[test]
    fn the_voice_shows_through_pauses_between_words() {
        let mut c = DuckCaption::default();
        c.feed(duck(0.0, 0.0, true), TICK);
        assert!(c.shown().speaking);
        // A 300 ms pause between words: still speaking.
        for _ in 0..9 {
            c.feed(duck(0.0, 0.0, false), TICK);
        }
        assert!(c.shown().speaking && !c.rested());
        c.feed(duck(0.0, 0.0, true), TICK);
        for _ in 0..18 {
            c.feed(duck(0.0, 0.0, false), TICK);
        }
        assert!(c.shown().speaking);
        for _ in 0..2 {
            c.feed(duck(0.0, 0.0, false), TICK);
        }
        assert!(!c.shown().speaking && c.rested());
    }

    #[test]
    fn the_duck_shows_its_whole_drop_in_whole_db() {
        let mut c = DuckCaption::default();
        // Ducked 12 dB under a leveler at -3 dB: -15 dB once settled.
        for _ in 0..60 {
            c.feed(duck(12.0, -15.2, false), TICK);
        }
        let shown = c.shown();
        assert!(shown.lowered && !shown.speaking);
        assert_eq!(shown.db, -15);
        assert!(!c.rested());
        // A quiet song raised 8 dB, ducked 6: lowered by the duck at least.
        for _ in 0..60 {
            c.feed(duck(6.0, 2.0, false), TICK);
        }
        assert_eq!(c.shown().db, -6);
        // Back up: not lowered, the leveler's +2 dB show, then rest.
        for _ in 0..60 {
            c.feed(duck(0.0, 2.0, false), TICK);
        }
        assert_eq!(
            c.shown(),
            TrackDuck {
                speaking: false,
                lowered: false,
                db: 2
            }
        );
        assert!(c.rested());
        // Under a dB of duck is not lowered.
        c.feed(duck(0.9, 2.0, false), TICK);
        assert!(!c.shown().lowered);
    }

    #[test]
    fn a_gain_between_two_db_or_a_quick_peak_does_not_flicker() {
        let mut c = DuckCaption::default();
        for _ in 0..60 {
            c.feed(duck(0.0, -9.5, false), TICK);
        }
        let first = c.shown().db;
        // Wobbling around -9.5 dB, then a compressor peak of 3 dB for 30 ms.
        let mut seen = vec![first];
        for i in 0..60 {
            let g = if i % 2 == 0 { -9.4 } else { -9.6 };
            c.feed(duck(0.0, g, false), TICK);
            seen.push(c.shown().db);
        }
        c.feed(duck(0.0, -12.5, false), TICK);
        seen.push(c.shown().db);
        c.feed(duck(0.0, -9.5, false), TICK);
        seen.push(c.shown().db);
        assert!(seen.iter().all(|&db| db == first), "{seen:?}");
    }

    #[test]
    fn reset_shows_the_state_at_once() {
        let mut c = DuckCaption::default();
        c.feed(duck(0.0, 0.0, true), TICK);
        // A voice told while away is not shown on return.
        c.reset(duck(0.0, -12.0, true));
        assert_eq!(
            c.shown(),
            TrackDuck {
                speaking: false,
                lowered: false,
                db: -12
            }
        );
    }

    #[test]
    fn a_new_caption_reset_to_its_track_does_not_count_up() {
        // A track list changed: the music at -8 dB shows -8 dB on the
        // first tick, then stays, instead of -1, -2... from 0 dB.
        let mut c = DuckCaption::default();
        c.reset(duck(0.0, -8.0, false));
        assert_eq!(c.shown().db, -8);
        assert!(c.rested());
        for _ in 0..30 {
            c.feed(duck(0.0, -8.0, false), TICK);
            assert_eq!(c.shown().db, -8);
        }
    }
}
