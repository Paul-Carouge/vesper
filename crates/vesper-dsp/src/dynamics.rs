//! Compressor, expander and de-esser: feed-forward, log domain, stereo-linked.
//!
//! Each one turns the level of the loudest channel into a gain change in dB
//! (the static curve), smooths that gain with separate attack and release
//! one-poles (Giannoulis, Massberg, Reiss, "Digital Dynamic Range Compressor
//! Design", JAES 2012), and applies one gain to every channel.

use crate::biquad::{Coefs, FreqGlide, State};
use crate::{Filter, Glide, coef, db_to_lin, lin_to_db, peak};

/// Smooths the gain change `gc` (dB) into `gs`: `fall` coefficient while the
/// gain goes down, `rise` while it goes up.
#[inline]
fn smooth(gs: f32, gc: f32, fall: f32, rise: f32) -> f32 {
    let a = if gc < gs { fall } else { rise };
    gc + a * (gs - gc)
}

pub(crate) struct Compressor {
    ch: usize,
    threshold: f32,
    ratio: f32,
    knee: f32,
    attack: f32,
    release: f32,
    /// In dB.
    makeup: Glide,
    /// Smoothed gain change, dB.
    gs: f32,
}

impl Compressor {
    pub fn new(
        ch: usize,
        threshold: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        knee: f32,
        makeup: f32,
    ) -> Self {
        Self {
            ch,
            threshold,
            ratio,
            knee,
            attack: coef(attack_ms),
            release: coef(release_ms),
            makeup: Glide::new(makeup, 10.0, 1),
            gs: 0.0,
        }
    }

    /// Gain change (dB, <= 0) at input level `x` dB, soft knee of `knee` dB.
    fn curve(&self, x: f32) -> f32 {
        let over = x - self.threshold;
        let slope = 1.0 / self.ratio - 1.0;
        if 2.0 * over <= -self.knee {
            0.0
        } else if 2.0 * over < self.knee {
            slope * (over + self.knee / 2.0).powi(2) / (2.0 * self.knee)
        } else {
            slope * over
        }
    }
}

impl Filter for Compressor {
    fn process(&mut self, buf: &mut [f32]) {
        for frame in buf.chunks_exact_mut(self.ch) {
            let gc = self.curve(lin_to_db(peak(frame)));
            self.gs = smooth(self.gs, gc, self.attack, self.release);
            let g = db_to_lin(self.gs + self.makeup.step());
            frame.iter_mut().for_each(|s| *s *= g);
        }
        if self.gs > -1e-6 {
            self.gs = 0.0;
        }
    }

    fn reset(&mut self) {
        self.gs = 0.0;
    }

    fn latency(&self) -> usize {
        0
    }

    fn set_param(&mut self, index: usize, value: f32) {
        match index {
            0 => self.threshold = value,
            1 => self.ratio = value,
            2 => self.attack = coef(value),
            3 => self.release = coef(value),
            4 => self.knee = value,
            5 => self.makeup.target = value,
            _ => {}
        }
    }
}

/// Release of the expander's level detector: keeps the gain from following
/// the waveform of low voices.
const DETECT_RELEASE_MS: f32 = 30.0;

pub(crate) struct Expander {
    ch: usize,
    threshold: f32,
    ratio: f32,
    range: f32,
    attack: f32,
    release: f32,
    detect: f32,
    /// Peak envelope, linear.
    env: f32,
    gs: f32,
}

impl Expander {
    pub fn new(
        ch: usize,
        threshold: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        range: f32,
    ) -> Self {
        Self {
            ch,
            threshold,
            ratio,
            range,
            attack: coef(attack_ms),
            release: coef(release_ms),
            detect: coef(DETECT_RELEASE_MS),
            env: 0.0,
            gs: 0.0,
        }
    }
}

impl Filter for Expander {
    fn process(&mut self, buf: &mut [f32]) {
        for frame in buf.chunks_exact_mut(self.ch) {
            self.env = peak(frame).max(self.env * self.detect);
            let under = lin_to_db(self.env) - self.threshold;
            let gc = if under < 0.0 {
                (under * (self.ratio - 1.0)).max(-self.range)
            } else {
                0.0
            };
            // Attack opens (gain up), release closes.
            self.gs = smooth(self.gs, gc, self.release, self.attack);
            let g = db_to_lin(self.gs);
            frame.iter_mut().for_each(|s| *s *= g);
        }
        if self.env < 1e-15 {
            self.env = 0.0;
        }
        if self.gs > -1e-6 {
            self.gs = 0.0;
        }
    }

    fn reset(&mut self) {
        (self.env, self.gs) = (0.0, 0.0);
    }

    fn latency(&self) -> usize {
        0
    }

    fn set_param(&mut self, index: usize, value: f32) {
        match index {
            0 => self.threshold = value,
            1 => self.ratio = value,
            2 => self.attack = coef(value),
            3 => self.release = coef(value),
            4 => self.range = value,
            _ => {}
        }
    }
}

/// Width of the sibilance band.
const DS_Q: f64 = 1.0;
const DS_RATIO: f32 = 4.0;
const DS_ATTACK_MS: f32 = 1.0;
const DS_RELEASE_MS: f32 = 60.0;

/// Split-band de-esser. The band is `bandpass(x)`, the rest `x - band`, so
/// the output `x + (g - 1) * band` is exactly the input while `g` is 1. The
/// detector runs a second band-pass on the band (4th order): low, loud
/// content does not trigger it.
pub(crate) struct DeEsser {
    ch: usize,
    freq: FreqGlide,
    c: Coefs,
    band: Vec<State>,
    side: Vec<State>,
    /// This frame's band samples.
    scratch: Vec<f64>,
    threshold: f32,
    range: f32,
    attack: f32,
    release: f32,
    gs: f32,
}

impl DeEsser {
    pub fn new(ch: usize, freq: f32, threshold: f32, range: f32) -> Self {
        Self {
            ch,
            freq: FreqGlide::new(freq),
            c: Coefs::bandpass(freq, DS_Q),
            band: vec![State::default(); ch],
            side: vec![State::default(); ch],
            scratch: vec![0.0; ch],
            threshold,
            range,
            attack: coef(DS_ATTACK_MS),
            release: coef(DS_RELEASE_MS),
            gs: 0.0,
        }
    }
}

impl Filter for DeEsser {
    fn process(&mut self, buf: &mut [f32]) {
        for frame in buf.chunks_exact_mut(self.ch) {
            if let Some(freq) = self.freq.next() {
                self.c = Coefs::bandpass(freq, DS_Q);
            }
            let mut level = 0.0f64;
            let states = self.band.iter_mut().zip(&mut self.side);
            for ((x, (band, side)), b) in frame.iter().zip(states).zip(&mut self.scratch) {
                *b = band.run(&self.c, f64::from(*x));
                level = level.max(side.run(&self.c, *b).abs());
            }
            let over = lin_to_db(level as f32) - self.threshold;
            let gc = -(over * (1.0 - 1.0 / DS_RATIO)).clamp(0.0, self.range);
            self.gs = smooth(self.gs, gc, self.attack, self.release);
            let g = f64::from(db_to_lin(self.gs)) - 1.0;
            for (s, b) in frame.iter_mut().zip(&self.scratch) {
                *s = (f64::from(*s) + g * b) as f32;
            }
        }
        self.band
            .iter_mut()
            .chain(&mut self.side)
            .for_each(State::flush);
        if self.gs > -1e-6 {
            self.gs = 0.0;
        }
    }

    fn reset(&mut self) {
        self.band.fill(State::default());
        self.side.fill(State::default());
        self.gs = 0.0;
    }

    fn latency(&self) -> usize {
        0
    }

    fn set_param(&mut self, index: usize, value: f32) {
        match index {
            0 => self.freq.glide.target = value,
            1 => self.threshold = value,
            2 => self.range = value,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FilterSpec;
    use crate::test_util::*;

    fn compressor(attack_ms: f32, release_ms: f32, knee_db: f32) -> Compressor {
        Compressor::new(2, -20.0, 4.0, attack_ms, release_ms, knee_db, 0.0)
    }

    #[test]
    fn compressor_static_curve() {
        // Threshold -20, ratio 4, 6 dB knee: (input, output) in dB.
        for (input, output) in [
            (-40.0, -40.0),
            (-23.0, -23.0),
            (-20.0, -20.5625),
            (-10.0, -17.5),
            (0.0, -15.0),
        ] {
            let mut c = compressor(1.0, 10.0, 6.0);
            let mut buf = vec![db_to_lin(input); 4_800 * 2];
            run(&mut c, &mut buf, 2);
            let got = lin_to_db(*buf.last().unwrap());
            assert!(
                (got - output).abs() < 0.01,
                "{input} dB in: {got} dB out, want {output}"
            );
        }
    }

    #[test]
    fn compressor_timing() {
        let mut c = compressor(10.0, 100.0, 0.0);
        let (quiet, loud) = (db_to_lin(-40.0), 1.0);
        // 0.5 s quiet, 0.5 s at 0 dB (15 dB of reduction), 0.5 s quiet.
        let mut buf: Vec<f32> = [quiet, loud, quiet]
            .iter()
            .flat_map(|&v| std::iter::repeat_n(v, 24_000 * 2))
            .collect();
        let input = buf.clone();
        run(&mut c, &mut buf, 2);
        let reduction = |t_ms: usize| {
            let i = (24_000 + t_ms * 48) * 2;
            lin_to_db(input[i]) - lin_to_db(buf[i])
        };
        // One time constant: 1 - 1/e of the way.
        let after_attack = reduction(10);
        assert!(
            (after_attack - 15.0 * (1.0 - (-1.0f32).exp())).abs() < 0.3,
            "{after_attack}"
        );
        assert!((reduction(499) - 15.0).abs() < 0.01);
        let i = (48_000 + 100 * 48) * 2;
        let after_release = lin_to_db(input[i]) - lin_to_db(buf[i]);
        assert!(
            (after_release - 15.0 * (-1.0f32).exp()).abs() < 0.3,
            "{after_release}"
        );
    }

    #[test]
    fn compressor_makeup_and_linking() {
        let mut c = Compressor::new(2, -20.0, 4.0, 1.0, 10.0, 0.0, 6.0);
        // Loud left, silent right: the right channel gets the same gain.
        let mut buf: Vec<f32> = (0..9_600).flat_map(|_| [1.0, 0.1]).collect();
        run(&mut c, &mut buf, 2);
        let (l, r) = (buf[buf.len() - 2], buf[buf.len() - 1]);
        assert!((lin_to_db(l) - (-15.0 + 6.0)).abs() < 0.01, "{l}");
        assert!((l / r - 10.0).abs() < 1e-3);
    }

    #[test]
    fn expander_attenuates_below_threshold() {
        let spec = FilterSpec::default_for("expander").unwrap();
        // Threshold -50, ratio 4, range 24: -70 dB gets the full 24 dB.
        let tail = 72_000 * 2..;
        for (level, want) in [(-70.0, -24.0), (-30.0, 0.0)] {
            let input = sine(1_000.0, db_to_lin(level), 96_000, 2);
            let out = apply(spec, &input);
            let got = rms_db(&out[tail.clone()]) - rms_db(&input[tail.clone()]);
            assert!((got - want).abs() < 0.1, "{level} dB: {got}");
        }
        // 5 dB under: 15 dB down, within the detector's ripple.
        let input = sine(1_000.0, db_to_lin(-55.0), 96_000, 2);
        let got = rms_db(&apply(spec, &input)[tail.clone()]) - rms_db(&input[tail]);
        assert!((got + 15.0).abs() < 1.0, "{got}");
    }

    /// Band-limited noise around 7 kHz at -12 dB RMS.
    fn sibilance() -> Vec<f32> {
        let c = Coefs::bandpass(7_000.0, 2.0);
        let mut z = [State::default(); 4];
        let mut out: Vec<f32> = noise(9_600 * 2, 1.0, 3)
            .chunks(2)
            .flat_map(|f| {
                let l = z[0].run(&c, f64::from(f[0]));
                let l = z[1].run(&c, l);
                let r = z[2].run(&c, f64::from(f[1]));
                let r = z[3].run(&c, r);
                [l as f32, r as f32]
            })
            .collect();
        let g = db_to_lin(-12.0 - rms_db(&out));
        out.iter_mut().for_each(|s| *s *= g);
        out
    }

    #[test]
    fn de_esser_cuts_sibilance_not_a_tone() {
        let spec = FilterSpec::default_for("de_esser").unwrap();
        let input = sibilance();
        let out = apply(spec, &input);
        let cut = rms_db(&input) - rms_db(&out);
        assert!(cut > 5.0, "sibilance cut by {cut} dB");
        let input = sine(1_000.0, db_to_lin(-6.0), 48_000, 2);
        let out = apply(spec, &input);
        let change = rms_db(&out) - rms_db(&input);
        assert!(change.abs() < 0.01, "1 kHz changed by {change} dB");
    }
}
