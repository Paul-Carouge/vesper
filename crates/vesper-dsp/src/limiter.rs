//! Lookahead peak limiter.
//!
//! Per frame: the loudest channel's peak, including 4x interpolated
//! intersample peaks, gives the gain needed to stay under the ceiling. A
//! minimum over the lookahead window then a moving average of the same
//! length turn it into a smooth gain that is down to that value by the time
//! the delayed peak plays (Signalsmith's design), and a release one-pole
//! brings it back up. A final clamp catches rounding errors.

use crate::{FS, Filter, Glide, coef, db_to_lin, lin_to_db};

/// Taps per phase of the intersample peak interpolator.
const TAPS: usize = 8;
/// Phases between two samples (4x oversampling has 3).
const PHASES: usize = 3;
/// Delay of the interpolator: it looks at the segment between the samples
/// `TP_DELAY` and `TP_DELAY - 1` frames old.
const TP_DELAY: usize = TAPS / 2;

pub(crate) struct Limiter {
    ch: usize,
    /// Lookahead, frames.
    n: usize,
    ceiling: Glide,
    release: f32,
    taps: [[f32; TAPS]; PHASES],
    /// Last `TAPS` input samples per channel, newest first.
    hist: Vec<[f32; TAPS]>,
    /// Audio delay line, `n + TP_DELAY - 1` frames.
    line: Vec<f32>,
    line_pos: usize,
    hold: MinHold,
    avg: Vec<f32>,
    avg_pos: usize,
    avg_sum: f64,
    gain: f32,
}

impl Limiter {
    pub fn new(ch: usize, ceiling_db: f32, release_ms: f32, lookahead_ms: f32) -> Self {
        let n = ((lookahead_ms * 1e-3 * FS).round() as usize).max(1);
        // Hann-windowed sinc, normalized to unity gain.
        let taps = std::array::from_fn(|p| {
            let frac = (p + 1) as f32 / (PHASES + 1) as f32;
            let mut t: [f32; TAPS] = std::array::from_fn(|k| {
                let u = TP_DELAY as f32 - k as f32 - frac;
                let sinc = if u == 0.0 {
                    1.0
                } else {
                    (std::f32::consts::PI * u).sin() / (std::f32::consts::PI * u)
                };
                let window =
                    0.5 * (1.0 + (std::f32::consts::PI * u / (TP_DELAY as f32 + 0.5)).cos());
                sinc * window
            });
            let sum: f32 = t.iter().sum();
            t.iter_mut().for_each(|v| *v /= sum);
            t
        });
        let mut limiter = Self {
            ch,
            n,
            ceiling: Glide::new(db_to_lin(ceiling_db), 20.0, 1),
            release: coef(release_ms),
            taps,
            hist: vec![[0.0; TAPS]; ch],
            line: vec![0.0; (n + TP_DELAY - 1) * ch],
            line_pos: 0,
            hold: MinHold::new(n),
            avg: vec![1.0; n],
            avg_pos: 0,
            avg_sum: 0.0,
            gain: 1.0,
        };
        limiter.reset();
        limiter
    }

    /// Largest sample or interpolated value between the samples
    /// `TP_DELAY` and `TP_DELAY - 1` frames old, over the channels.
    fn true_peak(&self) -> f32 {
        let mut peak = 0.0f32;
        for h in &self.hist {
            peak = peak.max(h[TP_DELAY].abs());
            for taps in &self.taps {
                let v: f32 = taps.iter().zip(h).map(|(t, x)| t * x).sum();
                peak = peak.max(v.abs());
            }
        }
        peak
    }
}

impl Filter for Limiter {
    fn process(&mut self, buf: &mut [f32]) {
        for frame in buf.chunks_exact_mut(self.ch) {
            for (h, &x) in self.hist.iter_mut().zip(frame.iter()) {
                h.copy_within(..TAPS - 1, 1);
                h[0] = x;
            }
            let ceiling = self.ceiling.step();
            let peak = self.true_peak();
            // Lowering: aim at the target now, so audio already in the lookahead
            // is not clipped by the gliding ceiling.
            let aim = ceiling.min(self.ceiling.target);
            let needed = if peak > aim { aim / peak } else { 1.0 };
            let held = self.hold.push(needed);
            self.avg_sum += f64::from(held) - f64::from(self.avg[self.avg_pos]);
            self.avg[self.avg_pos] = held;
            self.avg_pos = (self.avg_pos + 1) % self.n;
            let smooth = (self.avg_sum / self.n as f64) as f32;
            // Down at once (already smooth), up with the release.
            self.gain = if smooth < self.gain {
                smooth
            } else {
                smooth + (self.gain - smooth) * self.release
            };
            let delayed = &mut self.line[self.line_pos * self.ch..][..self.ch];
            for (s, d) in frame.iter_mut().zip(delayed) {
                let x = std::mem::replace(d, *s);
                *s = (x * self.gain).clamp(-ceiling, ceiling);
            }
            self.line_pos = (self.line_pos + 1) % (self.n + TP_DELAY - 1);
        }
    }

    fn reset(&mut self) {
        self.hist.fill([0.0; TAPS]);
        self.line.fill(0.0);
        self.line_pos = 0;
        self.hold.reset();
        self.avg.fill(1.0);
        (self.avg_pos, self.avg_sum, self.gain) = (0, self.n as f64, 1.0);
    }

    fn latency(&self) -> usize {
        self.n + TP_DELAY - 1
    }

    fn reduction_db(&self) -> f32 {
        -lin_to_db(self.gain)
    }

    fn set_param(&mut self, index: usize, value: f32) {
        match index {
            0 => self.ceiling.target = db_to_lin(value),
            1 => self.release = coef(value),
            // The lookahead sets the latency: it takes a new limiter.
            _ => {}
        }
    }
}

/// Minimum of the last `len` values: a monotonic queue in a ring buffer,
/// O(1) amortized.
struct MinHold {
    value: Vec<f32>,
    at: Vec<u64>,
    head: usize,
    len: usize,
    t: u64,
}

impl MinHold {
    fn new(len: usize) -> Self {
        Self {
            value: vec![0.0; len],
            at: vec![0; len],
            head: 0,
            len: 0,
            t: 0,
        }
    }

    fn reset(&mut self) {
        (self.head, self.len, self.t) = (0, 0, 0);
    }

    fn push(&mut self, v: f32) -> f32 {
        let cap = self.value.len();
        // Older values that are not smaller can never be the minimum again.
        while self.len > 0 && self.value[(self.head + self.len - 1) % cap] >= v {
            self.len -= 1;
        }
        if self.len > 0 && self.at[self.head] + cap as u64 <= self.t {
            self.head = (self.head + 1) % cap;
            self.len -= 1;
        }
        let back = (self.head + self.len) % cap;
        (self.value[back], self.at[back]) = (v, self.t);
        self.len += 1;
        self.t += 1;
        self.value[self.head]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FilterSpec;
    use crate::test_util::*;

    fn limiter() -> Box<dyn Filter + Send> {
        FilterSpec::default_for("limiter").unwrap().build(2)
    }

    /// Peak of `buf` 4x oversampled with a long windowed sinc (64 taps).
    fn true_peak(buf: &[f32], ch: usize) -> f32 {
        let mut peak = 0.0f32;
        for c in 0..ch {
            let x: Vec<f32> = buf.iter().skip(c).step_by(ch).copied().collect();
            for i in 32..x.len().saturating_sub(32) {
                for p in 0..4 {
                    let t = i as f32 + p as f32 / 4.0;
                    let v: f32 = (i - 31..i + 33)
                        .map(|k| {
                            let u = t - k as f32;
                            let sinc = if u == 0.0 {
                                1.0
                            } else {
                                (std::f32::consts::PI * u).sin() / (std::f32::consts::PI * u)
                            };
                            let w = 0.5 * (1.0 + (std::f32::consts::PI * u / 33.0).cos());
                            x[k] * sinc * w
                        })
                        .sum();
                    peak = peak.max(v.abs());
                }
            }
        }
        peak
    }

    #[test]
    fn min_hold_matches_brute_force() {
        let input = noise(1_000, 1.0, 9);
        let mut hold = MinHold::new(7);
        for i in 0..input.len() {
            let want = input[i.saturating_sub(6)..=i]
                .iter()
                .copied()
                .fold(f32::MAX, f32::min);
            assert_eq!(hold.push(input[i]), want, "at {i}");
        }
    }

    #[test]
    fn limiter_never_exceeds_the_ceiling() {
        let ceiling = db_to_lin(-1.0);
        // +12 dB sine with full-scale x8 clicks and a hot noise burst.
        let mut input = sine(997.0, 4.0, 48_000, 2);
        for i in (1_000..96_000).step_by(7_919) {
            input[i] = 8.0;
            input[i + 3] = -8.0;
        }
        input[60_000..70_000].copy_from_slice(&noise(10_000, 6.0, 5));
        let out = apply(FilterSpec::default_for("limiter").unwrap(), &input);
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak <= ceiling, "{peak}");
        // No clamp needed beyond rounding: the gain alone keeps it there.
        assert!(peak > ceiling * 0.9, "{peak}");
        let tp = true_peak(&out[..60_000], 2);
        assert!(
            lin_to_db_test(tp) < -1.0 + 0.5,
            "true peak {} dB",
            lin_to_db_test(tp)
        );
    }

    fn lin_to_db_test(x: f32) -> f32 {
        20.0 * x.log10()
    }

    #[test]
    fn limiter_catches_intersample_peaks() {
        // fs/4 at 45 degrees: samples at +-0.707, true peak 1.0 (0 dBFS).
        let input: Vec<f32> = (0..9_600 * 2)
            .map(|i| {
                (std::f32::consts::FRAC_PI_2 * (i / 2) as f32 + std::f32::consts::FRAC_PI_4).sin()
            })
            .collect();
        let out = apply(FilterSpec::default_for("limiter").unwrap(), &input);
        let tail = &out[4_800 * 2..];
        let tp = true_peak(tail, 2);
        assert!(
            lin_to_db_test(tp) < -1.0 + 0.3,
            "true peak {} dB",
            lin_to_db_test(tp)
        );
    }

    #[test]
    fn limiter_is_transparent_below_the_ceiling() {
        let mut l = limiter();
        let latency = l.latency();
        assert_eq!(latency, 96 + 3);
        let input = sine(440.0, 0.5, 9_600, 2);
        let mut buf = input.clone();
        run(l.as_mut(), &mut buf, 2);
        for (o, i) in buf[latency * 2..].iter().zip(&input) {
            assert!((o - i).abs() < 1e-6);
        }
    }
}
