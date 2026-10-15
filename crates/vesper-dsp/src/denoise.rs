//! RNNoise noise suppression (nnnoiseless, a pure-Rust port).
//!
//! RNNoise is mono: the channels are mixed down, denoised once and the result
//! goes to every channel. A voice track is one voice in one place (PipeWire
//! upmixes mono mics to identical channels), so nothing is lost, it costs
//! half of a per-channel run, and gains computed per channel could not shift
//! the image of a stereo mic. The dry signal, delayed to match, is mixed back
//! in to bound the attenuation: that keeps the stereo of whatever noise is
//! left, and the voice is never removed more than `max_attenuation_db`.

use nnnoiseless::DenoiseState;

use crate::{Filter, Glide, db_to_lin};

/// RNNoise frame: 10 ms.
pub(crate) const FRAME: usize = DenoiseState::FRAME_SIZE;
/// RNNoise works on 16-bit sample values.
const SCALE: f32 = 32_768.0;
/// Buffering one frame, plus RNNoise's own overlap-add frame.
const LATENCY: usize = 2 * FRAME;

pub(crate) struct Denoise {
    ch: usize,
    state: Box<DenoiseState<'static>>,
    input: Box<[f32; FRAME]>,
    output: Box<[f32; FRAME]>,
    pos: usize,
    /// Dry signal delayed by `LATENCY` frames.
    dry: Vec<f32>,
    dry_pos: usize,
    /// Level of the dry signal in the output, linear.
    dry_mix: Glide,
}

impl Denoise {
    pub fn new(ch: usize, max_attenuation_db: f32) -> Self {
        Self {
            ch,
            state: new_state(),
            input: Box::new([0.0; FRAME]),
            output: Box::new([0.0; FRAME]),
            pos: 0,
            dry: vec![0.0; LATENCY * ch],
            dry_pos: 0,
            dry_mix: Glide::new(dry_mix(max_attenuation_db), 20.0, 1),
        }
    }
}

/// Above 99 dB the output is fully wet.
fn dry_mix(max_attenuation_db: f32) -> f32 {
    if max_attenuation_db >= 99.0 {
        0.0
    } else {
        db_to_lin(-max_attenuation_db)
    }
}

fn new_state() -> Box<DenoiseState<'static>> {
    let mut state = DenoiseState::new();
    // The first frame sets up nnnoiseless's global tables and this thread's
    // FFT caches, which allocates (the audio thread gets its own caches
    // through `Chain::warm_up`). Silence leaves the state as new.
    state.process_frame(&mut [0.0; FRAME], &[0.0; FRAME]);
    state
}

impl Filter for Denoise {
    fn process(&mut self, buf: &mut [f32]) {
        let ch = self.ch;
        for frame in buf.chunks_exact_mut(ch) {
            let wet = self.output[self.pos] / SCALE;
            self.input[self.pos] = frame.iter().sum::<f32>() / ch as f32 * SCALE;
            let mix = self.dry_mix.step();
            let dry = &mut self.dry[self.dry_pos * ch..][..ch];
            for (s, d) in frame.iter_mut().zip(dry) {
                let x = std::mem::replace(d, *s);
                *s = wet + mix * (x - wet);
            }
            self.dry_pos = (self.dry_pos + 1) % LATENCY;
            self.pos += 1;
            if self.pos == FRAME {
                self.state
                    .process_frame(&mut self.output[..], &self.input[..]);
                self.pos = 0;
            }
        }
    }

    fn reset(&mut self) {
        self.state = new_state();
        self.input.fill(0.0);
        self.output.fill(0.0);
        self.dry.fill(0.0);
        (self.pos, self.dry_pos) = (0, 0);
    }

    fn latency(&self) -> usize {
        LATENCY
    }

    fn set_param(&mut self, index: usize, value: f32) {
        if index == 0 {
            self.dry_mix.target = dry_mix(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FS;
    use crate::test_util::*;

    fn spec(max_attenuation_db: f32) -> crate::FilterSpec {
        crate::FilterSpec::NoiseSuppression { max_attenuation_db }
    }

    /// A voice-like stand-in: 120-160 Hz glottal pulse train (harmonics at
    /// 1/k, shaped by two formant-ish peaks), 4 Hz syllables. Not speech:
    /// RNNoise was trained on real voices, so this only checks that a
    /// harmonic, pitched signal mostly survives.
    fn voice(frames: usize) -> Vec<f32> {
        let mut phase = 0.0f32;
        (0..frames)
            .flat_map(|i| {
                let t = i as f32 / FS;
                let f0 = 140.0 + 20.0 * (std::f32::consts::TAU * 0.7 * t).sin();
                phase += std::f32::consts::TAU * f0 / FS;
                let mut v = 0.0;
                for k in 1..=40 {
                    let f = f0 * k as f32;
                    let formants = 1.0 / (1.0 + ((f - 700.0) / 300.0).powi(2))
                        + 0.5 / (1.0 + ((f - 1_800.0) / 400.0).powi(2));
                    v += (phase * k as f32).sin() * formants / k as f32;
                }
                let syllables = 0.6 + 0.4 * (std::f32::consts::TAU * 4.0 * t).sin();
                let s = 0.3 * v * syllables;
                [s, s]
            })
            .collect()
    }

    /// Mic-like hiss: Gaussian-ish (sum of 4 uniforms), low-passed at ~2.7
    /// kHz, at `level` dB RMS, the same on both channels (an upmixed mic).
    fn hiss(frames: usize, level: f32, seed: u32) -> Vec<f32> {
        let mut lp = 0.0f32;
        let mono: Vec<f32> = noise(frames * 4, 1.0, seed)
            .chunks(4)
            .map(|u| {
                lp = 0.7 * lp + 0.3 * u.iter().sum::<f32>();
                lp
            })
            .collect();
        let g = db_to_lin(level - rms_db(&mono));
        mono.iter().flat_map(|s| [s * g, s * g]).collect()
    }

    /// Noise reduction (dB) per 0.5 s window, from 2 s on.
    fn reductions(max_attenuation_db: f32, seed: u32) -> Vec<f32> {
        let input = hiss(48_000 * 10, -40.0, seed);
        let out = apply(spec(max_attenuation_db), &input);
        (4..20)
            .map(|k| {
                let r = k * 48_000..(k + 1) * 48_000;
                rms_db(&input[r.clone()]) - rms_db(&out[r])
            })
            .collect()
    }

    /// RNNoise's model is not steady on synthetic stationary noise: over a
    /// few seconds it swings between ~60 dB and a few dB of reduction,
    /// depending on the noise sequence (nnnoiseless 0.5.1 and 0.5.2 give the
    /// same numbers). So this checks the typical reduction over several
    /// sequences, and that it never adds noise.
    #[test]
    fn removes_noise() {
        let mut all: Vec<f32> = [2, 3, 11]
            .into_iter()
            .flat_map(|s| reductions(100.0, s))
            .collect();
        assert!(all.iter().all(|r| *r > 1.0), "{all:?}");
        all.sort_by(f32::total_cmp);
        let median = all[all.len() / 2];
        assert!(median > 15.0, "median reduction {median} dB: {all:?}");
        // The dry mix bounds the reduction.
        let bounded = reductions(20.0, 2);
        assert!(bounded.iter().all(|r| *r < 20.5), "{bounded:?}");
        assert!(bounded.iter().any(|r| *r > 19.0), "{bounded:?}");
    }

    #[test]
    fn keeps_a_voice_like_signal() {
        let input = voice(48_000 * 3);
        let out = apply(spec(100.0), &input);
        let tail = 48_000 * 2..48_000 * 6 - 2 * LATENCY;
        let loss = rms_db(&input[tail.clone()])
            - rms_db(&out[tail.start + 2 * LATENCY..tail.end + 2 * LATENCY]);
        assert!(loss.abs() < 1.0, "voice changed by {loss} dB");
        // The voice comes out `LATENCY` frames late: best correlation there.
        let corr = |lag: usize| -> f32 {
            (48_000..96_000)
                .map(|i| input[2 * i] * out[2 * (i + lag)])
                .sum()
        };
        let best = (0..2_000)
            .max_by(|a, b| corr(*a).total_cmp(&corr(*b)))
            .unwrap();
        assert!(best.abs_diff(LATENCY) <= 2, "best lag {best}");
    }

    #[test]
    fn dry_path_is_aligned_with_the_latency() {
        let input = noise(9_600 * 2, 0.5, 1);
        let out = apply(spec(0.0), &input);
        for (o, i) in out[LATENCY * 2..].iter().zip(&input) {
            assert!((o - i).abs() < 1e-6);
        }
    }
}
