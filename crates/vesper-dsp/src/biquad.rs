//! RBJ biquads, the high-pass and the gain.

use crate::{FS, Filter, Glide, db_to_lin};

/// Frames between two coefficient updates while a frequency glides.
pub(crate) const CHUNK: usize = 32;
/// Time constant of frequency glides.
pub(crate) const FREQ_GLIDE_MS: f32 = 30.0;

/// Normalized biquad coefficients (`a0 = 1`), from the RBJ Audio EQ Cookbook.
#[derive(Clone, Copy, Default)]
pub(crate) struct Coefs {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Coefs {
    fn new(freq: f32, q: f64, b: impl FnOnce(f64, f64) -> [f64; 3]) -> Self {
        let w0 = std::f64::consts::TAU * f64::from(freq) / f64::from(FS);
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        let [b0, b1, b2] = b(cos, alpha);
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha) / a0,
        }
    }

    pub fn highpass(freq: f32, q: f64) -> Self {
        Self::new(freq, q, |cos, _| {
            [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0]
        })
    }

    /// Band-pass with a 0 dB peak: `x - bandpass(x)` is the matching notch.
    pub fn bandpass(freq: f32, q: f64) -> Self {
        Self::new(freq, q, |_, alpha| [alpha, 0.0, -alpha])
    }

    /// Peaking EQ (bell) of `db` at `freq`.
    pub fn peaking(freq: f32, q: f64, db: f32) -> Self {
        let a = 10f64.powf(f64::from(db) / 40.0);
        let w0 = std::f64::consts::TAU * f64::from(freq) / f64::from(FS);
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha / a;
        Self {
            b0: (1.0 + alpha * a) / a0,
            b1: -2.0 * cos / a0,
            b2: (1.0 - alpha * a) / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha / a) / a0,
        }
    }

    /// The two stages of ITU-R BS.1770 K-weighting at 48 kHz (high shelf,
    /// then high-pass), the coefficients of the standard.
    pub fn k_weighting() -> [Self; 2] {
        [
            Self {
                b0: 1.535_124_859_586_97,
                b1: -2.691_696_189_406_38,
                b2: 1.198_392_810_852_85,
                a1: -1.690_659_293_182_41,
                a2: 0.732_480_774_215_85,
            },
            Self {
                b0: 1.0,
                b1: -2.0,
                b2: 1.0,
                a1: -1.990_047_454_833_98,
                a2: 0.990_072_250_366_21,
            },
        ]
    }
}

/// Transposed direct form II state, in `f64` (low cutoffs need the precision).
#[derive(Clone, Copy, Default)]
pub(crate) struct State {
    z1: f64,
    z2: f64,
}

impl State {
    #[inline]
    pub fn run(&mut self, c: &Coefs, x: f64) -> f64 {
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y + self.z2;
        self.z2 = c.b2 * x - c.a2 * y;
        y
    }

    #[cfg(test)]
    pub fn raw(&self) -> (f64, f64) {
        (self.z1, self.z2)
    }

    /// Zeroes a decayed state before it turns denormal.
    pub fn flush(&mut self) {
        if self.z1.abs() < 1e-30 && self.z2.abs() < 1e-30 {
            *self = Self::default();
        }
    }
}

const BUTTERWORTH_Q: f64 = std::f64::consts::FRAC_1_SQRT_2;

/// A filter frequency gliding to its target, stepped every `CHUNK` frames
/// whatever the block sizes.
pub(crate) struct FreqGlide {
    pub glide: Glide,
    tick: usize,
}

impl FreqGlide {
    pub fn new(freq: f32) -> Self {
        Self {
            glide: Glide::new(freq, FREQ_GLIDE_MS, CHUNK),
            tick: 0,
        }
    }

    /// Call once per frame: the new frequency when the coefficients need an
    /// update.
    #[inline]
    pub fn next(&mut self) -> Option<f32> {
        self.tick = (self.tick + 1) % CHUNK;
        (self.tick == 0 && !self.glide.settled()).then(|| self.glide.step())
    }
}

pub(crate) struct HighPass {
    ch: usize,
    freq: FreqGlide,
    c: Coefs,
    z: Vec<State>,
}

impl HighPass {
    pub fn new(ch: usize, freq: f32) -> Self {
        Self {
            ch,
            freq: FreqGlide::new(freq),
            c: Coefs::highpass(freq, BUTTERWORTH_Q),
            z: vec![State::default(); ch],
        }
    }
}

impl Filter for HighPass {
    fn process(&mut self, buf: &mut [f32]) {
        for frame in buf.chunks_exact_mut(self.ch) {
            if let Some(freq) = self.freq.next() {
                self.c = Coefs::highpass(freq, BUTTERWORTH_Q);
            }
            for (s, z) in frame.iter_mut().zip(&mut self.z) {
                *s = z.run(&self.c, f64::from(*s)) as f32;
            }
        }
        self.z.iter_mut().for_each(State::flush);
    }

    fn reset(&mut self) {
        self.z.fill(State::default());
    }

    fn latency(&self) -> usize {
        0
    }

    fn set_param(&mut self, index: usize, value: f32) {
        if index == 0 {
            self.freq.glide.target = value;
        }
    }
}

pub(crate) struct Gain {
    ch: usize,
    /// In dB.
    gain: Glide,
}

impl Gain {
    pub fn new(ch: usize, db: f32) -> Self {
        Self {
            ch,
            gain: Glide::new(db, 10.0, 1),
        }
    }
}

impl Filter for Gain {
    fn process(&mut self, buf: &mut [f32]) {
        if self.gain.settled() {
            let g = db_to_lin(self.gain.cur);
            if g != 1.0 {
                buf.iter_mut().for_each(|s| *s *= g);
            }
            return;
        }
        for frame in buf.chunks_exact_mut(self.ch) {
            let g = db_to_lin(self.gain.step());
            frame.iter_mut().for_each(|s| *s *= g);
        }
    }

    fn reset(&mut self) {}

    fn latency(&self) -> usize {
        0
    }

    fn set_param(&mut self, index: usize, value: f32) {
        if index == 0 {
            self.gain.target = value;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::FilterSpec;
    use crate::test_util::*;

    /// Steady-state gain of the high-pass at `freq`, in dB.
    fn response(freq: f32) -> f32 {
        let input = sine(freq, 0.5, 48_000, 2);
        let out = apply(FilterSpec::HighPass { freq_hz: 80.0 }, &input);
        let tail = 24_000 * 2..;
        rms_db(&out[tail.clone()]) - rms_db(&input[tail])
    }

    #[test]
    fn highpass_is_3db_down_at_cutoff() {
        assert!((response(80.0) + 3.01).abs() < 0.05, "{}", response(80.0));
        assert!(response(1_000.0).abs() < 0.05, "{}", response(1_000.0));
        // 12 dB/octave: two octaves below, ~24 dB down.
        assert!((response(20.0) + 24.1).abs() < 0.5, "{}", response(20.0));
    }
}
