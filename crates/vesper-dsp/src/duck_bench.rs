//! Ducking bench: scenarios fed through [`VoiceDetector`] and [`Ducker`]
//! as `vesper-audio` wires them, and the metrics that matter to a listener.
//!
//! The voice track and the ducked track run 20 ms blocks side by side. As in
//! `vesper-audio`'s `Encoder::push`, the detector hears the voice's raw
//! block (before its chain) and marks its voiced windows in a [`VoiceKey`]
//! at their timeline frames; the ducked track's audio is held back by the
//! lookahead, and each of its blocks asks the key about the hold before
//! its end and the lookahead after it ([`VoiceKey::ducks`]). The voice
//! tells its block in the same step as the ducked track gets its input
//! (the best case of the app's threads), or `late` after it. A
//! [`VoiceMeter`] measures the voice after its chain and tells its loudness
//! through the key, as `Encoder::encode_block` does.
//!
//! The duck's timing is read off a probe: a [`Ducker`] (duck only, the same
//! depth) on a constant 1.0, so its output is its gain, sample by sample.
//!
//! Full table: `cargo test --release -p vesper-dsp -- --ignored --nocapture
//! ducking_report`. The other tests pin what the detector and the ducker
//! do well (with a margin), so it does not regress.

use super::*;
use crate::biquad::{Coefs, State};
use crate::test_util::{noise, rms_db};
use crate::{Chain, FilterSpec};

const CH: usize = 2;
/// 20 ms blocks, as the tracks run them.
const BLK: usize = 960;
/// Frames per ms.
const MS: usize = 48;

/// Deterministic xorshift noise in [-1, 1].
struct Rng(u32);

impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0 as f32 / u32::MAX as f32 * 2.0 - 1.0
    }
}

fn ms(frames: usize) -> f32 {
    frames as f32 / MS as f32
}

/// Scales `x` so its RMS over `active` frames (all if empty) is `db` dBFS.
fn set_rms(x: &mut [f32], active: &[(usize, usize)], db: f32) {
    let now = if active.is_empty() {
        rms_db(x)
    } else {
        let (p, n) = active.iter().fold((0.0, 0), |(p, n), &(a, b)| {
            let s = &x[a..b];
            (
                p + s.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>(),
                n + s.len(),
            )
        });
        10.0 * (p / n as f64).log10() as f32
    };
    let g = db_to_lin(db - now);
    x.iter_mut().for_each(|s| *s *= g);
}

/// One-pole low-pass at `hz`, in place.
fn lowpass(x: &mut [f32], hz: f32) {
    let a = (-std::f32::consts::TAU * hz / FS).exp();
    let mut y = 0.0;
    for s in x {
        y = *s + a * (y - *s);
        *s = y;
    }
}

/// Band-pass (RBJ, 0 dB peak) at `hz`, in place.
fn bandpass(x: &mut [f32], hz: f32, q: f64) {
    let c = Coefs::bandpass(hz, q);
    let mut z = State::default();
    x.iter_mut()
        .for_each(|s| *s = z.run(&c, f64::from(*s)) as f32);
}

/// Steady room noise (white, gently low-passed) at RMS `db`.
fn room(n: usize, db: f32, seed: u32) -> Vec<f32> {
    let mut x = noise(n, 1.0, seed);
    lowpass(&mut x, 4_000.0);
    set_rms(&mut x, &[], db);
    x
}

/// A fan: low rumble with a 100 Hz hum, RMS `db`.
fn fan(n: usize, db: f32, seed: u32) -> Vec<f32> {
    let mut x = noise(n, 1.0, seed);
    lowpass(&mut x, 600.0);
    for (i, s) in x.iter_mut().enumerate() {
        *s += 0.3 * (std::f32::consts::TAU * 100.0 * i as f32 / FS).sin();
    }
    set_rms(&mut x, &[], db);
    x
}

fn add(x: &mut [f32], y: &[f32], at: usize) {
    let at = at.min(x.len());
    for (a, b) in x[at..].iter_mut().zip(y) {
        *a += b;
    }
}

/// Synthetic speech: words at `(start ms, length ms)`, glottal pulses at
/// a falling pitch around `f0` through three moving formants, an unvoiced
/// onset per word, syllables every ~180 ms. Returns the signal (RMS
/// `db` dBFS over the words) and the words in frames.
fn synth_speech(len_ms: usize, words: &[(usize, usize)], f0: f32, db: f32, seed: u32) -> Speech {
    let n = len_ms * MS;
    let mut out = vec![0.0; n];
    let mut rng = Rng(seed);
    let active: Vec<(usize, usize)> = words.iter().map(|&(a, l)| (a * MS, (a + l) * MS)).collect();
    for (w, &(a, b)) in active.iter().enumerate() {
        let len = b - a;
        // Unvoiced onset (a fricative / plosive), 40 ms, then voicing.
        let fric = 40 * MS;
        let mut f = noise(fric, 1.0, seed ^ ((w as u32 + 1) * 977));
        bandpass(&mut f, 4_500.0, 1.0);
        for (i, s) in f.iter_mut().enumerate() {
            *s *= 0.25 * (std::f32::consts::PI * i as f32 / fric as f32).sin();
        }
        add(&mut out, &f, a);
        let mut src = vec![0.0; len];
        let mut phase = 0.0f32;
        for (i, s) in src.iter_mut().enumerate() {
            let t = i as f32 / len as f32;
            let pitch = f0 * (1.15 - 0.3 * t) * (1.0 + 0.01 * rng.next());
            phase += pitch / FS;
            if phase >= 1.0 {
                phase -= 1.0;
                *s = 1.0;
            }
            *s += 0.02 * rng.next();
        }
        lowpass(&mut src, 800.0);
        let mut voiced = vec![0.0; len];
        // Formants change per syllable: one vowel per ~180 ms.
        let syl = 180 * MS;
        for (k, part) in src.chunks(syl).enumerate() {
            let vowels = [
                (700.0, 1_200.0),
                (400.0, 2_000.0),
                (550.0, 900.0),
                (300.0, 2_300.0),
            ];
            let (f1, f2) = vowels[(k + w) % vowels.len()];
            let mut sum = vec![0.0; part.len()];
            for (hz, g) in [(f1, 1.0), (f2, 0.5), (2_700.0, 0.25)] {
                let mut p = part.to_vec();
                bandpass(&mut p, hz, 6.0);
                sum.iter_mut().zip(&p).for_each(|(s, v)| *s += g * v);
            }
            // Syllable envelope: 15 ms in, 40 ms out, a dip between.
            let l = part.len();
            for (i, s) in sum.iter_mut().enumerate() {
                let in_ = (i as f32 / (15 * MS) as f32).min(1.0);
                let out_ = ((l - i) as f32 / (40 * MS) as f32).min(1.0);
                *s *= in_ * out_;
            }
            voiced[k * syl..k * syl + l].copy_from_slice(&sum);
        }
        add(&mut out, &voiced, a + fric / 2);
    }
    set_rms(&mut out, &active, db);
    Speech { x: out, active }
}

/// A speech signal and where it is active (frames).
struct Speech {
    x: Vec<f32>,
    active: Vec<(usize, usize)>,
}

impl Speech {
    fn onset(&self) -> usize {
        self.active[0].0
    }
    fn offset(&self) -> usize {
        self.active.last().unwrap().1
    }
}

/// A LibriSpeech clip of `testdata/speech`, at 48 kHz, its active RMS at
/// `db`, starting `lead_ms` in and followed by `tail_ms` of silence. The
/// active parts are where its 10 ms RMS is within 35 dB of its loudest.
fn clip(name: &str, db: f32, lead_ms: usize, tail_ms: usize) -> Speech {
    let path = format!("{}/testdata/speech/{name}.s16", env!("CARGO_MANIFEST_DIR"));
    let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let x16: Vec<f32> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| f32::from(i16::from_le_bytes(*b)) / 32_768.0)
        .collect();
    // 16 to 48 kHz: polyphase windowed sinc (Blackman, 21 taps per phase,
    // cut at 7.6 kHz), so RNNoise does not hear images above 8 kHz.
    const TAPS: usize = 21;
    let h = |t: f32| {
        // t in input samples from the output frame, |t| < TAPS / 2.
        let w = 0.42
            + 0.5 * (std::f32::consts::PI * t / (TAPS as f32 / 2.0)).cos()
            + 0.08 * (std::f32::consts::TAU * t / (TAPS as f32 / 2.0)).cos();
        let fc = 7_600.0 / 16_000.0;
        let a = std::f32::consts::TAU * fc * t;
        2.0 * fc * w * if t == 0.0 { 1.0 } else { a.sin() / a }
    };
    let mut x = vec![0.0; lead_ms * MS];
    for i in 0..x16.len() * 3 {
        let pos = i as f32 / 3.0;
        let c = i / 3;
        let lo = (c + 1).saturating_sub(TAPS / 2);
        let hi = (c + TAPS / 2 + 1).min(x16.len());
        let taps = x16[lo..hi].iter().zip(lo..);
        x.push(taps.map(|(v, j)| v * h(pos - j as f32)).sum());
    }
    x.resize(x.len() + tail_ms * MS, 0.0);
    let win = 10 * MS;
    let levels: Vec<f32> = x.chunks(win).map(rms_db).collect();
    let top = levels.iter().copied().fold(f32::MIN, f32::max);
    let mut active: Vec<(usize, usize)> = Vec::new();
    for (i, l) in levels.iter().enumerate() {
        if *l > top - 35.0 {
            match active.last_mut() {
                Some(r) if r.1 == i * win => r.1 = (i + 1) * win,
                _ => active.push((i * win, (i + 1) * win)),
            }
        }
    }
    let end = x.len();
    active.iter_mut().for_each(|r| r.1 = r.1.min(end));
    set_rms(&mut x, &active, db);
    Speech { x, active }
}

/// A test song: kick, hat, bass, a chord pad and a lead at 120 bpm, 2 bars
/// quiet (verse, -8 dB, no drums) then 2 bars loud (chorus) if `dynamic`.
/// Mono; scaled to `lufs` (ungated, as stereo copies).
fn song(secs: f32, lufs: f32, dynamic: bool, seed: u32) -> Vec<f32> {
    let n = (secs * FS) as usize;
    let beat = (FS / 2.0) as usize;
    let mut rng = Rng(seed);
    let roots = [110.0, 87.31, 130.81, 98.0];
    let mut x = vec![0.0; n];
    let mut lead = 440.0f32;
    for (i, s) in x.iter_mut().enumerate() {
        let t = i as f32 / FS;
        let b = i / beat;
        let bar = b / 4;
        let in_beat = (i % beat) as f32 / FS;
        let chorus = !dynamic || (bar / 2) % 2 == 1;
        let root = roots[bar % 4];
        if i % (beat / 2) == 0 {
            lead = root * 4.0 * [1.0, 1.25, 1.5, 2.0][(rng.next().abs() * 3.99) as usize];
        }
        let tau = std::f32::consts::TAU;
        let mut v = 0.0;
        for k in [1.0, 1.25, 1.5] {
            v += 0.12 * (tau * root * 2.0 * k * t).sin();
        }
        v += 0.25 * ((tau * root / 2.0 * t).sin() + 0.3 * (tau * root * t).sin());
        v += 0.08 * (tau * lead * t).sin() * (-in_beat * 6.0).exp();
        if chorus {
            v += 0.6
                * (tau * 55.0 * in_beat * (1.0 + 2.0 * (-in_beat * 30.0).exp())).sin()
                * (-in_beat * 18.0).exp();
            let off = ((i + beat / 2) % beat) as f32 / FS;
            v += 0.15 * rng.next() * (-off * 120.0).exp();
        }
        *s = v * if chorus { 1.0 } else { db_to_lin(-8.0) };
    }
    let now = loudness(&x, &[]);
    let g = db_to_lin(lufs - now);
    x.iter_mut().for_each(|s| *s *= g);
    x
}

/// K-weighted loudness of mono `x` played on both channels, LUFS, over
/// `ranges` (all if empty), ungated.
fn loudness(x: &[f32], ranges: &[(usize, usize)]) -> f32 {
    let [c0, c1] = Coefs::k_weighting();
    let mut z = [State::default(); 2];
    let y: Vec<f64> = x
        .iter()
        .map(|&s| {
            let k = z[0].run(&c0, f64::from(s));
            z[1].run(&c1, k)
        })
        .collect();
    let (p, n) = if ranges.is_empty() {
        (y.iter().map(|v| v * v).sum::<f64>(), y.len())
    } else {
        ranges.iter().fold((0.0, 0), |(p, n), &(a, b)| {
            let s = &y[a.min(y.len())..b.min(y.len())];
            (p + s.iter().map(|v| v * v).sum::<f64>(), n + s.len())
        })
    };
    // Two channels: twice the power of one.
    -0.691 + 10.0 * (2.0 * p / n.max(1) as f64).max(1e-30).log10() as f32
}

/// What a run gives.
struct Sim {
    /// The voice key said "speaking" at each ducked block.
    speaking: Vec<bool>,
    /// The duck's gain per frame, dB (the probe).
    gain: Vec<f32>,
    /// The ducked track's output (left channel), with its leveler and
    /// compressor gains per block, dB.
    out: Vec<f32>,
    level: Vec<f32>,
    comp: Vec<f32>,
    /// The duck's gain per block, dB.
    duck: Vec<f32>,
}

/// Runs the voice `mic` (mono, through the voice chain first if `chain`)
/// against `music` (mono; silence if None) with `settings`, as the app
/// does ([`LOOKAHEAD_FRAMES`], the voice told in time).
fn simulate(mic: &[f32], chain: bool, music: Option<&[f32]>, settings: DuckSettings) -> Sim {
    simulate_with(mic, chain, music, settings, LOOKAHEAD_FRAMES, 0)
}

/// [`simulate`] with a ducked track `lookahead` frames behind its input
/// and a voice that tells its blocks `late` frames after the ducked track
/// got them (both multiples of [`BLK`]).
fn simulate_with(
    mic: &[f32],
    chain: bool,
    music: Option<&[f32]>,
    settings: DuckSettings,
    lookahead: usize,
    late: usize,
) -> Sim {
    let depth = if settings.duck_db > 0.0 {
        settings.duck_db
    } else if settings.music_lufs.is_some() {
        MUSIC_DUCK_DB
    } else {
        0.0
    };
    let mut chain = chain.then(|| Chain::new(&FilterSpec::voice_chain(), CH).0);
    let mut det = VoiceDetector::new(CH);
    let mut probe = Ducker::new(CH);
    // The duck's loudness envelope (its spectral split aside).
    probe.broadband = true;
    let mut duck = Ducker::new(CH);
    let probe_set = duck_only(depth);
    let mut meter = VoiceMeter::new(CH);
    let blocks = mic.len() / BLK;
    let mut sim = Sim {
        speaking: Vec::with_capacity(blocks),
        gain: Vec::with_capacity(blocks * BLK),
        out: Vec::with_capacity(blocks * BLK),
        level: Vec::with_capacity(blocks),
        comp: Vec::with_capacity(blocks),
        duck: Vec::with_capacity(blocks),
    };
    let (mut v, mut m, mut p) = (
        vec![0.0; BLK * CH],
        vec![0.0; BLK * CH],
        vec![1.0; BLK * CH],
    );
    let key = VoiceKey::default();
    let (look, late) = (lookahead / BLK, late / BLK);
    for step in 0..blocks + look.max(late) {
        // The voice's block `late` steps late, as the app's mic runs
        // behind a playback track by its capture and playback delays.
        if let Some(at) = step
            .checked_sub(late)
            .filter(|&j| j < blocks)
            .map(|j| j * BLK)
        {
            for (i, f) in v.as_chunks_mut::<CH>().0.iter_mut().enumerate() {
                f.fill(mic[at + i]);
            }
            // As `Encoder::push`: the detector hears the raw input, then
            // the chain runs (it does not change what the detector hears).
            det.process_windows(&v, |end, voiced| {
                if voiced {
                    key.voiced((at + end) as u64);
                }
            });
            key.heard((at + BLK) as u64);
            if let Some(c) = &mut chain {
                c.process(&mut v);
            }
            // As `Encoder::encode_block`: the voice as recorded is measured.
            let at = at as u64;
            if let Some(lufs) = meter.process(&v, |a, b| key.speaking(at + a as u64, at + b as u64))
            {
                key.set_loudness(0, lufs);
            }
        }
        // As `Encoder::push`: the ducked track measures its input as it
        // comes in, before the lookahead holds it back.
        if step < blocks {
            let at = step * BLK;
            for (i, f) in m.as_chunks_mut::<CH>().0.iter_mut().enumerate() {
                f.fill(music.map_or(0.0, |x| x[at + i]));
            }
            duck.look_ahead(&m, at as u64, settings, key.loudness());
        }
        // The ducked track's block, its audio held back by the lookahead.
        let Some(at) = step
            .checked_sub(look)
            .filter(|&k| k < blocks)
            .map(|k| k * BLK)
        else {
            continue;
        };
        let speaking = key.ducks(at as u64, BLK, lookahead);
        sim.speaking.push(speaking);
        p.fill(1.0);
        probe.process(&mut p, probe_set, speaking, None);
        sim.gain
            .extend(p.as_chunks::<CH>().0.iter().map(|f| lin_to_db(f[0])));
        for (i, f) in m.as_chunks_mut::<CH>().0.iter_mut().enumerate() {
            f.fill(music.map_or(0.0, |x| x[at + i]));
        }
        duck.process_at(&mut m, at as u64, settings, speaking, key.loudness());
        sim.out.extend(m.as_chunks::<CH>().0.iter().map(|f| f[0]));
        sim.level.push(duck.leveled());
        sim.comp.push(duck.comp);
        sim.duck.push(duck.duck);
    }
    sim
}

/// Duck timing of one phrase, ms: onset to -3 dB, to full depth (negative:
/// down before the onset); offset to back over -3 dB, to over -0.5 dB.
/// Err: "n/a" when already there 500 ms before the onset or at the offset
/// (ducked early, released early), or "never".
struct Timing {
    to_3db: Result<f32, &'static str>,
    to_full: Result<f32, &'static str>,
    back_3db: Result<f32, &'static str>,
    back_full: Result<f32, &'static str>,
}

fn timing(sim: &Sim, onset: usize, offset: usize, depth: f32) -> Timing {
    let g = &sim.gain;
    // From `lead` frames before `at`: negative when it was there early.
    let from = |at: usize, lead: usize, f: &dyn Fn(f32) -> bool| match g[(at - lead).min(g.len())..]
        .iter()
        .position(|&x| f(x))
    {
        Some(0) => Err("n/a"),
        Some(i) => Ok(ms(i) - ms(lead)),
        None => Err("never"),
    };
    let after = |at: usize, f: &dyn Fn(f32) -> bool| from(at, 0, f);
    // A lookahead ducks before the onset, by less than this.
    let lead = (500 * MS).min(onset);
    Timing {
        to_3db: from(onset, lead, &|x| x <= -3.0),
        to_full: from(onset, lead, &|x| x <= -(depth - 0.5)),
        back_3db: after(offset, &|x| x > -3.0),
        back_full: after(offset, &|x| x > -0.5),
    }
}

/// Pumping within a phrase: from the depth reached after the onset to the
/// offset, how far the duck comes back up (dB) and its gain's standard
/// deviation (dB).
fn pumping(sim: &Sim, onset: usize, offset: usize, depth: f32) -> (f32, f32) {
    let g = &sim.gain;
    let Some(full) = g[onset..offset].iter().position(|&x| x <= -depth + 0.01) else {
        return (depth, 0.0);
    };
    let s = &g[onset + full..offset];
    let rise = s.iter().copied().fold(f32::MIN, f32::max) + depth;
    let mean = s.iter().sum::<f32>() / s.len() as f32;
    let var = s.iter().map(|x| (x - mean).powi(2)).sum::<f32>() / s.len() as f32;
    (rise, var.sqrt())
}

/// From frame `from`: share of the time ducked by more than 1 dB, the
/// number of ducks (crossing -3 dB downwards), deepest duck (dB).
fn false_ducks(sim: &Sim, from: usize) -> (f32, usize, f32) {
    let g = &sim.gain[from..];
    let ducked = g.iter().filter(|&&x| x < -1.0).count() as f32 / g.len() as f32;
    let events = g.windows(2).filter(|w| w[0] > -3.0 && w[1] <= -3.0).count();
    let deepest = 0.0 - g.iter().copied().fold(0.0, f32::min);
    (ducked * 100.0, events, deepest)
}

/// Share of the active speech (10 ms windows) during which the duck is
/// at least 3 dB down, %.
fn recall(sim: &Sim, speech: &Speech) -> f32 {
    let (mut hit, mut n) = (0, 0);
    for &(a, b) in &speech.active {
        for w in (a..b.min(sim.gain.len())).step_by(10 * MS) {
            n += 1;
            hit += usize::from(sim.gain[(w + 10 * MS - 1).min(sim.gain.len() - 1)] <= -3.0);
        }
    }
    hit as f32 * 100.0 / n.max(1) as f32
}

/// Time after the speech during which the duck stays more than 1 dB down
/// away from any speech (more than 2 s after it), %.
fn ducked_away(sim: &Sim, speech: &Speech) -> f32 {
    let far = |i: usize| {
        speech
            .active
            .iter()
            .all(|&(a, b)| i + 2_000 * MS < a || i > b + 2_000 * MS)
    };
    let (mut d, mut n) = (0, 0);
    for (i, &g) in sim.gain.iter().enumerate().step_by(MS) {
        if far(i) {
            n += 1;
            d += usize::from(g < -1.0);
        }
    }
    d as f32 * 100.0 / n.max(1) as f32
}

fn duck_only(db: f32) -> DuckSettings {
    DuckSettings {
        duck_db: db,
        ..DuckSettings::default()
    }
}

fn music(lufs: f32) -> DuckSettings {
    DuckSettings {
        music_lufs: Some(lufs),
        ..DuckSettings::default()
    }
}

/// The synthetic phrase: five words with gaps of 80 to 300 ms, 1 s in.
fn phrase_words() -> Vec<(usize, usize)> {
    vec![
        (1_000, 320),
        (1_400, 260),
        (1_960, 420),
        (2_560, 300),
        (2_940, 380),
    ]
}

fn speech_with_floor(s: Speech, floor_db: f32) -> Speech {
    let mut x = s.x;
    let n = x.len();
    add(&mut x, &room(n, floor_db, 99), 0);
    Speech {
        x,
        active: s.active,
    }
}

fn opt(x: Result<f32, &str>) -> String {
    x.map_or_else(String::from, |v| format!("{v:.0}"))
}

/// One row of the timing table.
fn timing_row(name: &str, speech: &Speech, chain: bool, depth: f32) -> String {
    let sim = simulate(&speech.x, chain, None, duck_only(depth));
    let t = timing(&sim, speech.onset(), speech.offset(), depth);
    let (rise, sd) = pumping(&sim, speech.onset(), speech.offset(), depth);
    // Ducked before the speech starts (a breath or lip noise the detector
    // took for voice, or the timeline offset).
    let pre = speech.onset().saturating_sub(1_000 * MS);
    let early = ms(sim.gain[pre..speech.onset()]
        .iter()
        .filter(|&&g| g < -1.0)
        .count());
    format!(
        "| {name} | {} | {} | {} | {} | {rise:.1} | {sd:.2} | {:.0} | {early:.0} |",
        opt(t.to_3db),
        opt(t.to_full),
        opt(t.back_3db),
        opt(t.back_full),
        recall(&sim, speech),
    )
}

/// One row of the false trigger table: `mic` has no speech, from `from` ms.
fn false_row(name: &str, mic: &[f32], from_ms: usize) -> String {
    let sim = simulate(mic, false, None, duck_only(12.0));
    let (pct, n, deep) = false_ducks(&sim, from_ms * MS);
    format!("| {name} | {pct:.1} | {n} | {deep:.1} |")
}

/// Keyboard typing: a click (2 ms tick and a 150 Hz body) every ~110 to
/// 220 ms, peak around `db` dBFS, over a -60 dB room.
fn keyboard(secs: usize, db: f32, seed: u32) -> Vec<f32> {
    let n = secs * 1_000 * MS;
    let mut x = room(n, -60.0, seed);
    let mut rng = Rng(seed);
    let mut at = 500 * MS;
    let amp = db_to_lin(db);
    while at < n {
        for i in 0..(30 * MS).min(n - at) {
            let t = i as f32 / FS;
            x[at + i] += amp
                * (rng.next() * (-t / 0.002).exp()
                    + 0.5 * (std::f32::consts::TAU * 150.0 * t).sin() * (-t / 0.008).exp());
        }
        at += (165.0 + 55.0 * rng.next()) as usize * MS;
    }
    x
}

/// Breaths: 500 ms of soft band-limited noise every 3 s, RMS `db` dBFS.
fn breaths(secs: usize, db: f32) -> Vec<f32> {
    let n = secs * 1_000 * MS;
    let mut x = room(n, -60.0, 5);
    let mut b = noise(500 * MS, 1.0, 6);
    bandpass(&mut b, 1_200.0, 0.7);
    let l = b.len();
    for (i, s) in b.iter_mut().enumerate() {
        *s *= (std::f32::consts::PI * i as f32 / l as f32).sin().powi(2);
    }
    set_rms(&mut b, &[], db);
    for at in (1_000..secs * 1_000).step_by(3_000) {
        add(&mut x, &b, at * MS);
    }
    x
}

/// Coughs: two 120 ms broadband bursts 60 ms apart, RMS `db`, every 4 s.
fn coughs(secs: usize, db: f32) -> Vec<f32> {
    let n = secs * 1_000 * MS;
    let mut x = room(n, -60.0, 7);
    let mut c = noise(120 * MS, 1.0, 8);
    lowpass(&mut c, 3_000.0);
    let l = c.len();
    for (i, s) in c.iter_mut().enumerate() {
        *s *= (-(i as f32) / l as f32 * 3.0).exp();
    }
    set_rms(&mut c, &[], db);
    for at in (1_000..secs * 1_000).step_by(4_000) {
        add(&mut x, &c, at * MS);
        add(&mut x, &c, (at + 180) * MS);
    }
    x
}

/// Steady noise at `db` from `at_ms` on, over a `before_db` floor (digital
/// silence if None: a muted or gated microphone).
fn appearing(secs: usize, at_ms: usize, before_db: Option<f32>, db: f32) -> Vec<f32> {
    let n = secs * 1_000 * MS;
    let mut x = before_db.map_or(vec![0.0; n], |d| room(n, d, 3));
    let f = fan(n - at_ms * MS, db, 4);
    add(&mut x, &f, at_ms * MS);
    x
}

/// Song bleeding into the microphone (speakers, a game) at RMS `db`.
fn bleed(secs: usize, db: f32) -> Vec<f32> {
    let mut x = song(secs as f32, -20.0, true, 21);
    set_rms(&mut x, &[], db);
    let n = x.len();
    add(&mut x, &room(n, -60.0, 22), 0);
    x
}

/// Voice and music: `voice` speaks over `song`, music mode at `target`.
/// Returns (voice LUFS, music LUFS during the voice, LU gap).
fn lu_gap(voice: &Speech, song: &[f32], target: f32) -> (f32, f32, f32) {
    let sim = simulate(&voice.x, false, Some(song), music(target));
    // Past the leveler's first measurement.
    let ranges: Vec<_> = voice
        .active
        .iter()
        .copied()
        .filter(|&(a, _)| a > 3_000 * MS)
        .collect();
    let v = loudness(&voice.x, &ranges);
    let m = loudness(&sim.out, &ranges);
    (v, m, v - m)
}

/// The voice clips one after the other, 1 s apart, from 4 s in, `db`.
fn talk(db: f32, total_ms: usize) -> Speech {
    let a = clip("1272-128104-0001", db, 4_000, 1_000);
    let b = clip("1272-135031-0004", db, 0, 0);
    let off = a.x.len();
    let mut x = a.x;
    x.extend(&b.x);
    x.resize(total_ms * MS, 0.0);
    let mut active = a.active;
    active.extend(b.active.iter().map(|&(s, e)| (s + off, e + off)));
    active.retain(|&(s, _)| s < total_ms * MS);
    let n = x.len();
    add(&mut x, &room(n, -65.0, 31), 0);
    Speech { x, active }
}

/// The settings of the app's `Level` (0 Low, 1 Medium, 2 High) for a
/// ducked track, or a background music one.
fn level_settings(music_mode: bool, level: usize) -> DuckSettings {
    if music_mode {
        DuckSettings::at_levels(None, Some(level))
    } else {
        DuckSettings::at_levels(Some(level), None)
    }
}

/// Both clips twice, 1 s apart, from 2 s in, at `db`, over a -65 dB room:
/// about 21 s of talk in 26 s.
fn long_talk(db: f32) -> Speech {
    let mut x = vec![0.0; 2_000 * MS];
    let mut active = Vec::new();
    for name in ["1272-128104-0001", "1272-135031-0004"].repeat(2) {
        let c = clip(name, db, 0, 1_000);
        let off = x.len();
        active.extend(c.active.iter().map(|&(s, e)| (s + off, e + off)));
        x.extend(c.x);
    }
    x.resize(26_000 * MS, 0.0);
    active.retain(|&(s, _)| s < x.len());
    let n = x.len();
    add(&mut x, &room(n, -65.0, 31), 0);
    Speech { x, active }
}

/// `voice` speaks over `song` with `settings`. Returns (voice LUFS, track
/// LUFS during the voice, LU gap, how far the duck's gain moves there in
/// dB: pumping), from 10 s in: the voice's loudness and the leveler are
/// measured by then.
fn gap_by_mic(voice: &Speech, song: &[f32], settings: DuckSettings) -> (f32, f32, f32, f32) {
    let sim = simulate(&voice.x, false, Some(&song[..voice.x.len()]), settings);
    // The music where the voice speaks and the duck is down (60 ms into
    // the voice's run): how deep it goes, not its attack and release (other
    // matters).
    let (ranges, ducked) = ducked_ranges(&sim, voice);
    let v = loudness(&voice.x, &ranges);
    let m = loudness(&sim.out, &ducked);
    let (lo, hi) = ducked
        .iter()
        .map(|&(a, _)| sim.duck[a / BLK])
        .fold((f32::MAX, f32::MIN), |(lo, hi), d| (lo.min(d), hi.max(d)));
    (v, m, v - m, hi - lo)
}

/// Time until the leveler is within 1 dB of where it ends (its mean over
/// the last 2 s), from block `from`, in ms; and that final gain. `level`
/// and `from` are in 20 ms blocks.
fn convergence(level: &[f32], from: usize) -> (f32, f32) {
    let end = level.len();
    assert!(
        end >= 100 && from < end,
        "convergence needs a run of 2 s or more past block {from}, got {end} blocks"
    );
    let last = level[end - 100..].iter().sum::<f32>() / 100.0;
    let settled = level[from..]
        .iter()
        .rposition(|x| (x - last).abs() > 1.0)
        .map_or(0, |i| i + 1);
    ((settled * 20) as f32, last)
}

/// Momentary loudness (400 ms) of mono `x` peaks this far over `target`
/// after frame `from`, and stays over target + 3 LU this long (ms).
fn overshoot(x: &[f32], from: usize, target: f32) -> (f32, f32) {
    let win = 400 * MS;
    let mut top = f32::MIN;
    let mut over = 0;
    for a in (from..x.len().saturating_sub(win)).step_by(100 * MS) {
        let l = loudness(&x[a..a + win], &[]);
        top = top.max(l);
        over += usize::from(l > target + 3.0);
    }
    (top - target, (over * 100) as f32)
}

/// Mono `x` through the cascade `cs`, from rest.
fn filtered(x: &[f32], cs: &[Coefs]) -> Vec<f32> {
    let mut z = vec![State::default(); cs.len()];
    x.iter()
        .map(|&s| {
            let mut v = f64::from(s);
            for (c, z) in cs.iter().zip(&mut z) {
                v = z.run(c, v);
            }
            v as f32
        })
        .collect()
}

const BUTTERWORTH: f64 = std::f64::consts::FRAC_1_SQRT_2;

/// The voice band of mono `x`: 300 Hz to 4 kHz, 24 dB/octave each side.
fn voice_band(x: &[f32]) -> Vec<f32> {
    let (hp, lp) = (
        Coefs::highpass(300.0, BUTTERWORTH),
        Coefs::lowpass(4_000.0, BUTTERWORTH),
    );
    filtered(x, &[hp, hp, lp, lp])
}

/// Under 150 Hz, 24 dB/octave.
fn lows(x: &[f32]) -> Vec<f32> {
    let lp = Coefs::lowpass(150.0, BUTTERWORTH);
    filtered(x, &[lp, lp])
}

/// Over 6 kHz, 24 dB/octave.
fn highs(x: &[f32]) -> Vec<f32> {
    let hp = Coefs::highpass(6_000.0, BUTTERWORTH);
    filtered(x, &[hp, hp])
}

/// Mean power of mono `x` over `ranges`, dB.
fn power_db(x: &[f32], ranges: &[(usize, usize)]) -> f32 {
    let (p, n) = ranges.iter().fold((0.0f64, 0usize), |(p, n), &(a, b)| {
        let s = &x[a.min(x.len())..b.min(x.len())];
        (
            p + s.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>(),
            n + s.len(),
        )
    });
    10.0 * (p / n.max(1) as f64).max(1e-30).log10() as f32
}

/// Level (dB) and phase (rad) of `hz` in mono `x`, Hann-windowed (its
/// leakage far from `hz` is far under any artefact measured here).
fn tone_at(x: &[f32], hz: f32) -> (f32, f64) {
    let tau = std::f64::consts::TAU;
    let w = tau * f64::from(hz) / f64::from(FS);
    let n = x.len() as f64;
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (i, &v) in x.iter().enumerate() {
        let i = i as f64;
        let h = 0.5 - 0.5 * (tau * i / n).cos();
        let a = w * i;
        re += h * f64::from(v) * a.cos();
        im -= h * f64::from(v) * a.sin();
    }
    (
        10.0 * (re * re + im * im).max(1e-300).log10() as f32,
        im.atan2(re),
    )
}

/// The tones of [`tone_gains`].
const TONES: [f32; 5] = [80.0, 300.0, 1_100.0, 3_000.0, 10_000.0];

/// Five equal tones (-37 dBFS RMS each) under `set`: each tone's gain
/// (dB, the leveler's and compressor's gains taken out) while the voice is
/// silent (1.5 to 4.5 s) and while it speaks (one phrase from 5 s to
/// 9.5 s, measured from 6.5 to 9 s).
fn tone_gains(set: DuckSettings) -> ([f32; 5], [f32; 5]) {
    let x: Vec<f32> = (0..10 * 48_000)
        .map(|i| {
            let t = i as f32 / FS;
            TONES
                .iter()
                .map(|f| 0.02 * (std::f32::consts::TAU * f * t).sin())
                .sum()
        })
        .collect();
    let voice = synth_speech(10_000, &[(5_000, 4_500)], 130.0, -26.0, 2);
    let sim = simulate(&voice.x, false, Some(&x), set);
    let gains = |a: usize, b: usize| {
        let (fa, fb) = (a * MS, b * MS);
        let blocks = fa / BLK..fb / BLK;
        let n = blocks.len() as f32;
        let lc = blocks.map(|k| sim.level[k] + sim.comp[k]).sum::<f32>() / n;
        TONES.map(|f| tone_at(&sim.out[fa..fb], f).0 - tone_at(&x[fa..fb], f).0 - lc)
    };
    (gains(1_500, 4_500), gains(6_500, 9_000))
}

/// Peak of `out - inp` against `inp`'s peak, dB, or "exact".
fn null(out: &[f32], inp: &[f32]) -> String {
    let d = out
        .iter()
        .zip(inp)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f32::max);
    if d == 0.0 {
        return "exact (bit-identical)".into();
    }
    let peak = inp.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    format!("{:.0}", lin_to_db(d) - lin_to_db(peak))
}

/// Duck 12 dB, a phrase from 1 to 2.5 s: null against the input with the
/// voice silent and from 6 s (well after the release); 2nd and 3rd
/// harmonics of a 1 kHz sine at full depth (dBc); the worst spur at 1 kHz
/// +/- 1.5 kHz (the chunk rate) through the attack and the release (dBc).
fn transparency() -> (String, String, (f32, f32), f32) {
    let x = song(8.0, -20.0, true, 31);
    let sim = simulate(&vec![0.0; x.len()], false, Some(&x), duck_only(12.0));
    let idle = null(&sim.out, &x);
    let voice = synth_speech(8_000, &[(1_000, 1_500)], 130.0, -26.0, 4);
    let sim = simulate(&voice.x, false, Some(&x), duck_only(12.0));
    let after = null(&sim.out[6_000 * MS..], &x[6_000 * MS..]);
    let sine: Vec<f32> = (0..x.len())
        .map(|i| 0.1 * (std::f32::consts::TAU * 1_000.0 * i as f32 / FS).sin())
        .collect();
    let sim = simulate(&voice.x, false, Some(&sine), duck_only(12.0));
    let full = &sim.out[1_800 * MS..2_400 * MS];
    let f0 = tone_at(full, 1_000.0).0;
    let thd = (tone_at(full, 2_000.0).0 - f0, tone_at(full, 3_000.0).0 - f0);
    let zip = [(800, 1_400), (2_400, 4_400)]
        .iter()
        .map(|&(a, b)| {
            let w = &sim.out[a * MS..b * MS];
            let c = tone_at(w, 1_000.0).0;
            tone_at(w, 2_500.0).0.max(tone_at(w, 500.0).0) - c
        })
        .fold(f32::MIN, f32::max);
    (idle, after, thd, zip)
}

/// Ways to cut the voice band.
#[derive(Clone, Copy)]
enum Design {
    /// `x + (g - 1) bp(x)`, `g` per frame (the ducker's).
    Bell,
    /// An RBJ peaking bell, its coefficients per chunk (the old dip's).
    Peaking,
    /// A 3-band Linkwitz-Riley (4th order) split at 300 Hz and 4 kHz, the
    /// low band all-passed at 4 kHz to sum flat, the mid band's gain per
    /// frame.
    Split,
}

impl Design {
    fn name(self) -> &'static str {
        match self {
            Design::Bell => "bell, x + (g - 1) bp(x), gain per frame (chosen)",
            Design::Peaking => "RBJ peaking bell, coefficients per 32-frame chunk",
            Design::Split => "LR4 3-band split 300 Hz / 4 kHz, mid gain per frame",
        }
    }
}

/// A [`Design`] on mono audio.
struct Voiceband {
    d: Design,
    c: Vec<Coefs>,
    z: Vec<State>,
    /// The cut (dB) at the end of the last chunk.
    db: f32,
}

impl Voiceband {
    fn new(d: Design, db: f32) -> Self {
        let c = match d {
            Design::Bell => vec![Coefs::bandpass(1_100.0, 0.15)],
            Design::Peaking => vec![Coefs::peaking(1_100.0, 0.3, db)],
            Design::Split => {
                let (lo, hi) = (
                    Coefs::lowpass(300.0, BUTTERWORTH),
                    Coefs::highpass(300.0, BUTTERWORTH),
                );
                let (lo4, hi4) = (
                    Coefs::lowpass(4_000.0, BUTTERWORTH),
                    Coefs::highpass(4_000.0, BUTTERWORTH),
                );
                let ap = Coefs::allpass(4_000.0, BUTTERWORTH);
                vec![lo, lo, hi, hi, lo4, lo4, hi4, hi4, ap]
            }
        };
        Self {
            d,
            z: vec![State::default(); c.len()],
            c,
            db,
        }
    }

    /// Runs `x` (one chunk), the cut gliding to `db` across it.
    fn chunk(&mut self, x: &mut [f32], db: f32) {
        let n = x.len() as f32;
        let (g0, g1) = (db_to_lin(self.db), db_to_lin(db));
        let (c, z) = (&self.c, &mut self.z);
        match self.d {
            Design::Bell => {
                for (i, s) in x.iter_mut().enumerate() {
                    let k = f64::from(g0 + (g1 - g0) * (i + 1) as f32 / n - 1.0);
                    let v = f64::from(*s);
                    *s = (v + k * z[0].run(&c[0], v)) as f32;
                }
            }
            Design::Peaking => {
                if db != self.db {
                    self.c[0] = Coefs::peaking(1_100.0, 0.3, db);
                }
                for s in x.iter_mut() {
                    *s = self.z[0].run(&self.c[0], f64::from(*s)) as f32;
                }
            }
            Design::Split => {
                for (i, s) in x.iter_mut().enumerate() {
                    let g = f64::from(g0 + (g1 - g0) * (i + 1) as f32 / n);
                    let v = f64::from(*s);
                    let lo = z[0].run(&c[0], v);
                    let lo = z[1].run(&c[1], lo);
                    let lo = z[8].run(&c[8], lo);
                    let hi = z[2].run(&c[2], v);
                    let hi = z[3].run(&c[3], hi);
                    let mid = z[4].run(&c[4], hi);
                    let mid = z[5].run(&c[5], mid);
                    let top = z[6].run(&c[6], hi);
                    let top = z[7].run(&c[7], top);
                    *s = (lo + g * mid + top) as f32;
                }
            }
        }
        self.db = db;
    }

    /// Mono `x` through `d` at a steady cut of `db`.
    fn steady(d: Design, db: f32, x: &[f32]) -> Vec<f32> {
        let mut v = Self::new(d, db);
        let mut y = x.to_vec();
        y.chunks_mut(32).for_each(|c| v.chunk(c, db));
        y
    }
}

struct DesignRow {
    null: String,
    delay0: f32,
    delay12: f32,
    phase12: f32,
    zipper: f32,
    cpu: f64,
}

fn design_row(d: Design) -> DesignRow {
    let x = noise(48_000, 0.3, 77);
    let y = Voiceband::steady(d, 0.0, &x);
    let r = y
        .iter()
        .zip(&x)
        .map(|(a, b)| f64::from(a - b).powi(2))
        .sum::<f64>();
    let null = if r == 0.0 {
        "exact (bit-identical)".to_string()
    } else {
        let p = x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
        format!("{:.0}", 10.0 * (r / p).log10())
    };
    let phase = |db: f32, f: f32| {
        let x: Vec<f32> = (0..48_000)
            .map(|i| 0.1 * (std::f32::consts::TAU * f * i as f32 / FS).sin())
            .collect();
        let y = Voiceband::steady(d, db, &x);
        let a = tone_at(&x[24_000..], f).1;
        let b = tone_at(&y[24_000..], f).1;
        let tau = std::f64::consts::TAU;
        (b - a + tau * 1.5).rem_euclid(tau) - tau / 2.0
    };
    let freqs = [
        50.0, 100.0, 200.0, 300.0, 500.0, 1_000.0, 2_000.0, 4_000.0, 8_000.0, 16_000.0,
    ];
    let delay = |db: f32| {
        freqs
            .iter()
            .map(|&f| {
                let dp = phase(db, f * 1.02) - phase(db, f);
                (dp / (std::f64::consts::TAU * 0.02 * f64::from(f))).abs() as f32 * 1e3
            })
            .fold(0.0, f32::max)
    };
    let phase12 = freqs
        .iter()
        .map(|&f| phase(-12.0, f).abs().to_degrees() as f32)
        .fold(0.0, f32::max);
    // A 1 kHz sine, the cut gliding in at 0.5 s (12 dB in 50 ms) and out
    // at 1.2 s (in 250 ms), per chunk as the ducker does.
    let sine: Vec<f32> = (0..2 * 48_000)
        .map(|i| 0.1 * (std::f32::consts::TAU * 1_000.0 * i as f32 / FS).sin())
        .collect();
    let mut y = sine.clone();
    let mut v = Voiceband::new(d, 0.0);
    let mut db = 0.0f32;
    for (i, c) in y.chunks_mut(32).enumerate() {
        let t = (i * 32) as f32 / FS;
        let target = if (0.5..1.2).contains(&t) { -12.0 } else { 0.0 };
        db = slew(
            db,
            target,
            12.0 * 32.0 / 48.0 / 50.0,
            12.0 * 32.0 / 48.0 / 250.0,
        );
        v.chunk(c, db);
    }
    let w = &y[19_200..76_800];
    let carrier = tone_at(w, 1_000.0).0;
    let zipper = tone_at(w, 2_500.0).0.max(tone_at(w, 500.0).0) - carrier;
    // CPU: 30 s of noise at a steady -12 dB cut, 20 ms stereo blocks (as
    // 1920 mono samples).
    let x = noise(30 * 48_000 * CH, 0.3, 78);
    let mut v = Voiceband::new(d, -12.0);
    let mut buf = x.clone();
    let t = std::time::Instant::now();
    for b in buf.chunks_mut(BLK * CH) {
        b.chunks_mut(32 * CH).for_each(|c| v.chunk(c, -12.0));
    }
    std::hint::black_box(&buf);
    let cpu = t.elapsed().as_nanos() as f64 / (x.len() / (BLK * CH)) as f64;
    DesignRow {
        null,
        delay0: delay(0.0),
        delay12: delay(-12.0),
        phase12,
        zipper,
        cpu,
    }
}

/// Frame ranges.
type Ranges = Vec<(usize, usize)>;

/// `voice`'s active parts from 10 s in, and their blocks where the duck
/// is down (60 ms into the voice's run), as frame ranges.
fn ducked_ranges(sim: &Sim, voice: &Speech) -> (Ranges, Ranges) {
    let ranges: Vec<_> = voice
        .active
        .iter()
        .copied()
        .filter(|&(a, _)| a > 10_000 * MS)
        .collect();
    let down =
        |k: usize| k >= 3 && k < sim.speaking.len() && sim.speaking[k - 3..=k].iter().all(|&s| s);
    let ducked = ranges
        .iter()
        .flat_map(|&(a, b)| (a / BLK..b.div_ceil(BLK)).map(|k| (k * BLK, (k + 1) * BLK)))
        .filter(|&(a, _)| down(a / BLK))
        .collect();
    (ranges, ducked)
}

struct Bands {
    gap: f32,
    snr: f32,
    lows: f32,
    mids: f32,
    highs: f32,
}

/// `voice` over `song` with `settings`, where the duck is down: the LU gap,
/// the voice-band SNR of the voice against the music (dB), and the music's
/// gain in its lows, voice band and highs (dB, the leveler's and the
/// compressor's gains taken out).
fn bands_under_voice(voice: &Speech, song: &[f32], settings: DuckSettings) -> Bands {
    let song = &song[..voice.x.len()];
    let sim = simulate(&voice.x, false, Some(song), settings);
    let (ranges, ducked) = ducked_ranges(&sim, voice);
    let gap = loudness(&voice.x, &ranges) - loudness(&sim.out, &ducked);
    let n = ducked.len() as f32;
    let lc = ducked
        .iter()
        .map(|&(a, _)| sim.level[a / BLK] + sim.comp[a / BLK])
        .sum::<f32>()
        / n;
    let gain = |f: fn(&[f32]) -> Vec<f32>| {
        power_db(&f(&sim.out), &ducked) - power_db(&f(song), &ducked) - lc
    };
    Bands {
        gap,
        snr: power_db(&voice_band(&voice.x), &ducked) - power_db(&voice_band(&sim.out), &ducked),
        lows: gain(lows),
        mids: gain(voice_band),
        highs: gain(highs),
    }
}

/// Speech of several shapes, for the release.
fn phrase_types() -> Vec<(&'static str, Speech)> {
    let synth = |words: &[(usize, usize)]| {
        speech_with_floor(synth_speech(6_000, words, 130.0, -26.0, 1), -65.0)
    };
    vec![
        ("lone word", synth(&[(1_000, 300)])),
        (
            "two words, 120 ms apart",
            synth(&[(1_000, 300), (1_420, 350)]),
        ),
        ("synthetic phrase, 5 words", synth(&phrase_words())),
        (
            "clip 1272-128104-0001",
            speech_with_floor(clip("1272-128104-0001", -26.0, 1_000, 3_000), -65.0),
        ),
        (
            "clip 1272-135031-0004",
            speech_with_floor(clip("1272-135031-0004", -26.0, 1_000, 3_000), -65.0),
        ),
    ]
}

/// The whole table: `cargo test --release -p vesper-dsp -- --ignored
/// --nocapture ducking_report`.
#[test]
#[ignore = "report"]
fn ducking_report() {
    println!("\n### Duck timing (duck 12 dB unless said; ms from voice onset / offset)\n");
    println!(
        "| Scenario | onset to -3 dB | onset to full | offset to -3 dB | offset to -0.5 dB | rise in phrase (dB) | gain SD in phrase (dB) | speech ducked (%) | ducked in the 1 s before onset (ms) |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    let synth = |db: f32, floor: f32| {
        speech_with_floor(synth_speech(5_000, &phrase_words(), 130.0, db, 1), floor)
    };
    let clip1 =
        |db: f32, floor: f32| speech_with_floor(clip("1272-128104-0001", db, 1_000, 2_000), floor);
    let clip2 =
        |db: f32, floor: f32| speech_with_floor(clip("1272-135031-0004", db, 1_000, 2_000), floor);
    let rows: Vec<(&str, Speech, bool, f32)> = vec![
        (
            "synthetic phrase, -26 dBFS, room -65",
            synth(-26.0, -65.0),
            false,
            12.0,
        ),
        (
            "synthetic phrase, duck 6 dB (music default)",
            synth(-26.0, -65.0),
            false,
            6.0,
        ),
        (
            "synthetic phrase, duck 20 dB",
            synth(-26.0, -65.0),
            false,
            20.0,
        ),
        (
            "clip 1272-128104-0001, -26 dBFS",
            clip1(-26.0, -65.0),
            false,
            12.0,
        ),
        (
            "clip 1272-135031-0004, -26 dBFS",
            clip2(-26.0, -65.0),
            false,
            12.0,
        ),
        (
            "clip 1, soft: -40 dBFS, room -65",
            clip1(-40.0, -65.0),
            false,
            12.0,
        ),
        (
            "clip 1, very soft: -48 dBFS, room -65",
            clip1(-48.0, -65.0),
            false,
            12.0,
        ),
        (
            "clip 1, -30 dBFS over noise -45 (15 dB SNR)",
            clip1(-30.0, -45.0),
            false,
            12.0,
        ),
        (
            "clip 1, -36 dBFS over noise -45 (9 dB SNR)",
            clip1(-36.0, -45.0),
            false,
            12.0,
        ),
        (
            "clip 1, -26 dBFS, through the voice chain",
            clip1(-26.0, -55.0),
            true,
            12.0,
        ),
        (
            "clip 2, -30 dBFS over noise -45, voice chain",
            clip2(-30.0, -45.0),
            true,
            12.0,
        ),
    ];
    for (name, s, chain, depth) in &rows {
        println!("{}", timing_row(name, s, *chain, *depth));
    }
    // What the lookahead buys, and what a voice told late (its capture
    // delay plus the ducked track's playback delay, in the app) takes back.
    println!(
        "\n### Lookahead: onset to -3 dB / to full depth (ms), by how late the voice is told\n"
    );
    println!("| Scenario | lookahead (ms) | told in time | 20 ms late | 40 ms late |");
    println!("|---|---|---|---|---|");
    for i in [0, 3, 4, 8, 9, 10] {
        let (name, s, chain, depth) = &rows[i];
        for look_ms in [0, 40, 60, 80, 100, 120] {
            let cells: Vec<String> = [0, 20, 40]
                .iter()
                .map(|late_ms| {
                    let sim = simulate_with(
                        &s.x,
                        *chain,
                        None,
                        duck_only(*depth),
                        look_ms * MS,
                        late_ms * MS,
                    );
                    let t = timing(&sim, s.onset(), s.offset(), *depth);
                    format!("{} / {}", opt(t.to_3db), opt(t.to_full))
                })
                .collect();
            println!("| {name} | {look_ms} | {} |", cells.join(" | "));
        }
    }
    // Speech over a fan that is already there: the detector must have
    // learnt the floor; how much of the 6 s away from speech is ducked.
    println!("\n### Speech over steady noise: ducked away from speech\n");
    println!("| Scenario | speech ducked (%) | ducked > 2 s away from speech (%) |");
    println!("|---|---|---|");
    for (name, floor, db) in [
        ("fan -50, voice -30", -50.0, -30.0),
        ("fan -40, voice -26", -40.0, -26.0),
        ("fan -35, voice -26", -35.0, -26.0),
    ] {
        let mut s = clip("1272-128104-0001", db, 6_000, 6_000);
        let n = s.x.len();
        add(&mut s.x, &fan(n, floor, 41), 0);
        let sim = simulate(&s.x, false, None, duck_only(12.0));
        println!(
            "| {name} | {:.0} | {:.1} |",
            recall(&sim, &s),
            ducked_away(&sim, &s)
        );
    }

    println!("\n### False triggers (no speech; duck 12 dB)\n");
    println!("| Scenario | time ducked > 1 dB (%) | ducks | deepest (dB) |");
    println!("|---|---|---|---|");
    println!(
        "{}",
        false_row("keyboard, clicks -30 dBFS peak", &keyboard(10, -30.0, 1), 0)
    );
    println!(
        "{}",
        false_row("keyboard, clicks -15 dBFS peak", &keyboard(10, -15.0, 2), 0)
    );
    println!(
        "{}",
        false_row("breaths -45 dBFS RMS every 3 s", &breaths(10, -45.0), 0)
    );
    println!(
        "{}",
        false_row("breaths -35 dBFS RMS every 3 s", &breaths(10, -35.0), 0)
    );
    println!(
        "{}",
        false_row("cough -20 dBFS RMS every 4 s", &coughs(12, -20.0), 0)
    );
    for (name, before, db) in [
        (
            "fan -45 appears over room -70 (from appearance)",
            Some(-70.0),
            -45.0,
        ),
        (
            "fan -30 appears over room -70 (from appearance)",
            Some(-70.0),
            -30.0,
        ),
        (
            "room -48 after 3 s of digital silence (unmute)",
            None,
            -48.0,
        ),
        (
            "room -40 after 3 s of digital silence (unmute)",
            None,
            -40.0,
        ),
    ] {
        println!(
            "{}",
            false_row(name, &appearing(33, 3_000, before, db), 3_000)
        );
    }
    for db in [-50.0, -40.0, -30.0, -20.0] {
        println!(
            "{}",
            false_row(
                &format!("music bleeding into the mic, {db} dBFS RMS"),
                &bleed(20, db),
                0
            )
        );
    }
    // The voice chain (noise suppression, expander, compressor with makeup)
    // runs before the detector: what it makes of a room with no speech.
    for db in [-65.0, -55.0, -45.0] {
        let sim = simulate(&room(20 * 1_000 * MS, db, 51), true, None, duck_only(12.0));
        let (pct, n, deep) = false_ducks(&sim, 0);
        println!("| room {db} dBFS through the voice chain | {pct:.1} | {n} | {deep:.1} |");
    }

    println!("\n### Background music (music mode)\n");
    println!(
        "| Scenario | song in (LUFS) | leveler to within 1 dB (ms) | leveler gain (dB) | out (LUFS, no voice) | loud start: momentary over target (LU) | time > target+3 (ms) | comp mean / max (dB) |"
    );
    println!("|---|---|---|---|---|---|---|---|");
    let silent = vec![0.0; 20 * 1_000 * MS];
    for (name, lufs, dynamic, target) in [
        ("steady pad+drums, loud", -8.0, false, -26.0),
        ("steady pad+drums, quiet", -40.0, false, -26.0),
        ("steady, at target", -26.0, false, -26.0),
        ("verse/chorus (8 dB) song, loud", -10.0, true, -26.0),
        ("loud song, Low (-30)", -8.0, false, -30.0),
        ("loud song, High (-22)", -8.0, false, -22.0),
    ] {
        let x = song(20.0, lufs, dynamic, 11);
        let sim = simulate(&silent, false, Some(&x), music(target));
        let (conv, gain) = convergence(&sim.level, 0);
        let out = loudness(&sim.out[10_000 * MS..], &[]);
        let (top, over) = overshoot(&sim.out, 0, target);
        let comp_mean = sim.comp.iter().sum::<f32>() / sim.comp.len() as f32;
        let comp_max = sim.comp.iter().copied().fold(0.0, f32::min);
        println!(
            "| {name} | {lufs:.0} | {conv:.0} | {gain:.1} | {out:.1} | {top:.1} | {over:.0} | {comp_mean:.1} / {comp_max:.1} |"
        );
    }
    // Song change mid-way: quiet to loud and back.
    for (name, a, b) in [
        ("quiet (-40) to loud (-8) at 15 s", -40.0, -8.0),
        ("loud (-8) to quiet (-40) at 15 s", -8.0, -40.0),
    ] {
        let mut x = song(15.0, a, false, 12);
        x.extend(song(15.0, b, false, 13));
        let sim = simulate(&vec![0.0; x.len()], false, Some(&x), music(-26.0));
        let (conv, gain) = convergence(&sim.level, 750);
        let (top, over) = overshoot(&sim.out, 15_000 * MS, -26.0);
        println!(
            "| {name} | {b:.0} | {conv:.0} after the change | {gain:.1} | {:.1} | {top:.1} | {over:.0} | - |",
            loudness(&sim.out[25_000 * MS..], &[])
        );
    }
    // Leveler pumping on a verse/chorus song: how much of the 8 dB the
    // leveler takes away (it should leave the song's dynamics).
    {
        let x = song(40.0, -14.0, true, 14);
        let sim = simulate(&vec![0.0; x.len()], false, Some(&x), music(-26.0));
        let l = &sim.level[500..];
        let (lo, hi) = l
            .iter()
            .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
        let sec = |s: usize| loudness(&sim.out[s * 48_000..(s + 4) * 48_000], &[]);
        let sec_in = |s: usize| loudness(&x[s * 48_000..(s + 4) * 48_000], &[]);
        println!(
            "\nVerse/chorus song (-14 LUFS, 8 dB apart), after 10 s: leveler gain swings {:.1} dB ({lo:.1} to {hi:.1}); chorus minus verse in {:.1} LU, out {:.1} LU.",
            hi - lo,
            sec_in(20) - sec_in(16),
            sec(20) - sec(16),
        );
        let (swing, top) = arranged_swing();
        println!(
            "\nArranged song (quiet intro, fade-in, bridge 10 LU lower): leveler gain swings {swing:.1} dB from 12 s; momentary peak {top:.1} LU over target after the intro."
        );
        let (perc, _) = bursts(20, 14.0, 500);
        let (perc20, _) = bursts(20, 20.0, 500);
        println!(
            "\nPercussive: 20 ms hits over a steady bed every 500 ms: leveler gain moves {perc:.2} dB (14 LU over), {perc20:.2} dB (20 LU over)."
        );
        for ms in [400, 800] {
            let (_, back) = bursts(ms, 14.0, 0);
            println!(
                "Burst: {ms} ms, 14 LU over a steady bed: leveler back within 1 dB {back:.0} ms after it ends."
            );
        }
        let (moves, over) = stop_time();
        println!(
            "Stop-time song (10 dB louder, 250 ms stops every 1.5 s): over its last 10 s the leveler moves {moves:.1} dB, {over:.0} ms over target + 3 LU."
        );
        for ms in [2_000, 3_000, 6_000] {
            let (top, over) = pause_menu(ms);
            println!(
                "Pause menu: loud song 20 dB lower for {ms} ms: once back, {top:.1} LU over target at most, {over:.0} ms over target + 3 LU."
            );
        }
        let duck = chorus_under_duck(DuckSettings::at_levels(Some(1), None));
        let bg = chorus_under_duck(DuckSettings::at_levels(None, Some(1)));
        println!(
            "\nChorus while the voice speaks: music in its first second (from 200 ms) over settled: duck Medium {duck:.1} LU, music Normal {bg:.1} LU."
        );
    }

    println!(
        "\n### Voice over music: LU gap (voice minus music, while the voice speaks; absolute music levels, the fallback until the voice is measured)\n"
    );
    println!(
        "| Voice (dBFS RMS) | song in (LUFS) | music level | voice (LUFS) | music under voice (LUFS) | gap (LU) |"
    );
    println!("|---|---|---|---|---|---|");
    for (vdb, lufs, target) in [
        (-20.0, -10.0, -26.0),
        (-26.0, -10.0, -26.0),
        (-32.0, -10.0, -26.0),
        (-26.0, -40.0, -26.0),
        (-26.0, -10.0, -22.0),
        (-26.0, -10.0, -30.0),
    ] {
        let voice = talk(vdb, 16_000);
        let s = song(16.0, lufs, false, 15);
        let (v, m, gap) = lu_gap(&voice, &s, target);
        println!("| {vdb:.0} | {lufs:.0} | {target:.0} | {v:.1} | {m:.1} | {gap:.1} |");
    }

    println!(
        "\n### Voice over the track: LU gap by mic level (while the voice speaks, from 10 s in)\n"
    );
    println!(
        "| Mode, level | song in (LUFS) | gap at -20 dBFS | gap at -26 dBFS | gap at -32 dBFS | spread (LU) | duck moves (dB, -26 dBFS) |"
    );
    println!("|---|---|---|---|---|---|---|");
    for (mode, song_lufs) in [("duck", -14.0), ("music", -10.0)] {
        for (name, level) in [("Low", 0), ("Medium", 1), ("High", 2)] {
            let set = level_settings(mode == "music", level);
            let s = song(30.0, song_lufs, false, 15);
            let runs: Vec<_> = [-20.0, -26.0, -32.0]
                .iter()
                .map(|&vdb| gap_by_mic(&long_talk(vdb), &s, set))
                .collect();
            let gaps: Vec<f32> = runs.iter().map(|r| r.2).collect();
            let (lo, hi) = gaps
                .iter()
                .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
            println!(
                "| {mode}, {name} | {song_lufs:.0} | {:.1} | {:.1} | {:.1} | {:.1} | {:.2} |",
                gaps[0],
                gaps[1],
                gaps[2],
                hi - lo,
                runs[1].3
            );
        }
    }

    println!("\n### Spectral duck: tone gains (dB, leveler and compressor taken out)\n");
    println!(
        "Five equal tones; the voice silent from 1.5 to 4.5 s, speaking (one 4.5 s phrase) from 6.5 to 9 s.\n"
    );
    println!("| Mode | voice | 80 Hz | 300 Hz | 1.1 kHz | 3 kHz | 10 kHz |");
    println!("|---|---|---|---|---|---|---|");
    for (name, set) in [
        ("duck 12 dB (fixed)", duck_only(12.0)),
        ("music -26, duck 6 dB", music(-26.0)),
    ] {
        let (silent, talking) = tone_gains(set);
        for (voice, g) in [("silent", silent), ("speaking", talking)] {
            let cells: Vec<String> = g.iter().map(|v| format!("{v:.1}")).collect();
            println!("| {name} | {voice} | {} |", cells.join(" | "));
        }
    }

    println!("\n### Transparency and artefacts (duck 12 dB)\n");
    {
        let (idle, after, thd, zip) = transparency();
        println!("| Measure | value |\n|---|---|");
        println!("| voice silent: output minus input, peak (dB re input peak) | {idle} |");
        println!("| 2 s after a phrase's release: output minus input, peak (dB) | {after} |");
        println!(
            "| 1 kHz at full duck: 2nd / 3rd harmonic (dBc) | {:.0} / {:.0} |",
            thd.0, thd.1
        );
        println!(
            "| 1 kHz through the attack and release: spur at 1 kHz +/- 1.5 kHz (the chunk rate), dBc | {:.0} |",
            zip
        );
    }

    println!("\n### Voice band: design choice (bell vs. Linkwitz-Riley split)\n");
    {
        println!(
            "| Design | null vs input at 0 dB (dB) | group delay at 0 dB, worst 50 Hz-16 kHz (ms) | at -12 dB band cut (ms) | phase at -12 dB, worst (deg) | spur on a 1 kHz cut in/out (dBc) | CPU per 20 ms stereo block (ns) |"
        );
        println!("|---|---|---|---|---|---|---|");
        for d in [Design::Bell, Design::Peaking, Design::Split] {
            let r = design_row(d);
            println!(
                "| {} | {} | {:.2} | {:.2} | {:.0} | {:.0} | {:.0} |",
                d.name(),
                r.null,
                r.delay0,
                r.delay12,
                r.phase12,
                r.zipper,
                r.cpu
            );
        }
    }

    println!(
        "\n### Voice over the track: intelligibility proxy and the music's body (Medium, voice -26 dBFS, from 10 s, while ducked)\n"
    );
    println!(
        "| Mode | gap (LU, K-weighted) | voice-band SNR, voice vs music (dB) | music lows < 150 Hz (dB vs input) | music voice band (dB vs input) | music highs > 6 kHz (dB vs input) |"
    );
    println!("|---|---|---|---|---|---|");
    for (mode, song_lufs) in [("duck", -14.0), ("music", -10.0)] {
        let s = song(30.0, song_lufs, false, 15);
        let r = bands_under_voice(&long_talk(-26.0), &s, level_settings(mode == "music", 1));
        println!(
            "| {mode} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} |",
            r.gap, r.snr, r.lows, r.mids, r.highs
        );
    }

    println!("\n### Release by phrase type (duck 12 dB; ms from the voice's offset)\n");
    println!(
        "| Phrase | length (ms) | offset to -3 dB | offset to -0.5 dB | rise in phrase (dB) |"
    );
    println!("|---|---|---|---|---|");
    for (name, s) in phrase_types() {
        let sim = simulate(&s.x, false, None, duck_only(12.0));
        let t = timing(&sim, s.onset(), s.offset(), 12.0);
        let (rise, _) = pumping(&sim, s.onset(), s.offset(), 12.0);
        println!(
            "| {name} | {:.0} | {} | {} | {rise:.2} |",
            ms(s.offset() - s.onset()),
            opt(t.back_3db),
            opt(t.back_full)
        );
    }

    println!("\n### CPU per 20 ms stereo block (ns; meaningful in --release)\n");
    let audio = {
        let mut a = song(30.0, -14.0, true, 16);
        add(
            &mut a,
            &synth_speech(30_000, &[(1_000, 5_000), (12_000, 6_000)], 130.0, -20.0, 3).x,
            0,
        );
        let mut s = Vec::with_capacity(a.len() * CH);
        a.iter().for_each(|v| s.extend([*v; CH]));
        s
    };
    let time = |f: &mut dyn FnMut(&mut [f32], usize)| {
        let mut buf = audio.clone();
        let t = std::time::Instant::now();
        let n = buf.len() / (BLK * CH);
        for (i, b) in buf.chunks_mut(BLK * CH).enumerate() {
            f(b, i);
        }
        std::hint::black_box(&buf);
        t.elapsed().as_nanos() as f64 / n as f64
    };
    let mut v = VoiceDetector::new(CH);
    let det = time(&mut |b, _| {
        std::hint::black_box(v.process(b));
    });
    let mut d = Ducker::new(CH);
    let idle = time(&mut |b, _| d.process(b, duck_only(12.0), false, None));
    let mut d = Ducker::new(CH);
    let ducking = time(&mut |b, i| d.process(b, duck_only(12.0), i % 100 < 50, None));
    let mut d = Ducker::new(CH);
    let bg = time(&mut |b, i| d.process(b, music(-26.0), i % 100 < 50, None));
    println!("| Stage | ns per block |\n|---|---|");
    println!("| VoiceDetector | {det:.0} |");
    println!("| Ducker, idle | {idle:.0} |");
    println!("| Ducker, ducking 12 dB | {ducking:.0} |");
    println!("| Ducker, background music | {bg:.0} |");
    // With the voice measured: the duck measures the track all along.
    let duck_m = DuckSettings::at_levels(Some(1), None);
    let mut d = Ducker::new(CH);
    let adaptive = time(&mut |b, i| d.process(b, duck_m, i % 100 < 50, Some(Voices::one(-20.0))));
    let music_m = DuckSettings::at_levels(None, Some(1));
    let mut d = Ducker::new(CH);
    let bg_rel = time(&mut |b, i| d.process(b, music_m, i % 100 < 50, Some(Voices::one(-20.0))));
    let mut m = VoiceMeter::new(CH);
    let meter = time(&mut |b, _| {
        std::hint::black_box(m.process(b, |a, _| a % 960 == 0));
    });
    println!("| Ducker, adaptive duck (voice measured) | {adaptive:.0} |");
    println!("| Ducker, background music under the voice | {bg_rel:.0} |");
    println!("| VoiceMeter | {meter:.0} |");
}

// Quick checks, so later changes do not regress.

#[test]
fn a_spoken_phrase_ducks_fast_and_holds_through_its_gaps() {
    let s = speech_with_floor(synth_speech(5_000, &phrase_words(), 130.0, -26.0, 1), -65.0);
    let sim = simulate(&s.x, false, None, duck_only(12.0));
    let t = timing(&sim, s.onset(), s.offset(), 12.0);
    // RNNoise needs the voiced part: each synthetic word opens with a 40 ms
    // unvoiced onset, so the lookahead leaves -28 / 8 ms here (92 / 128
    // without it; real speech: below).
    assert!(t.to_3db.is_ok_and(|v| v <= 0.0), "{:?}", t.to_3db);
    assert!(t.to_full.is_ok_and(|v| v <= 20.0), "{:?}", t.to_full);
    // A long phrase is let go gently: 400 ms hold, 900 ms release (1262).
    assert!(
        t.back_full.is_ok_and(|v| (1_100.0..=1_400.0).contains(&v)),
        "{:?}",
        t.back_full
    );
    let (rise, _) = pumping(&sim, s.onset(), s.offset(), 12.0);
    // 0.0; a gap that lets the duck up would show as tenths of a dB.
    assert!(rise < 0.25, "pumps {rise} dB between words");
}

#[test]
fn real_speech_is_ducked_throughout() {
    let s = speech_with_floor(clip("1272-128104-0001", -26.0, 1_000, 2_000), -65.0);
    let sim = simulate(&s.x, false, None, duck_only(12.0));
    assert!(recall(&sim, &s) >= 95.0, "{}", recall(&sim, &s));
    let t = timing(&sim, s.onset(), s.offset(), 12.0);
    // Down at full depth before the first syllable (-78 / -42 ms; 42 / 78
    // without the lookahead), released gently, a long phrase (1162 ms;
    // 795 before the release followed the phrase).
    assert!(t.to_3db.is_ok_and(|v| v <= -40.0), "{:?}", t.to_3db);
    assert!(t.to_full.is_ok_and(|v| v <= 0.0), "{:?}", t.to_full);
    assert!(t.back_full.is_ok_and(|v| v <= 1_300.0), "{:?}", t.back_full);
    let (rise, _) = pumping(&sim, s.onset(), s.offset(), 12.0);
    assert!(rise < 0.25, "pumps {rise} dB");
}

#[test]
fn real_speech_is_ducked_in_time_even_told_late() {
    // The app's mic tells its blocks after a playback track got its own,
    // by their capture and playback delays: 40 ms still leaves the duck
    // at full depth by the first syllable (-2 ms).
    let s = speech_with_floor(clip("1272-128104-0001", -26.0, 1_000, 2_000), -65.0);
    let sim = simulate_with(
        &s.x,
        false,
        None,
        duck_only(12.0),
        LOOKAHEAD_FRAMES,
        40 * MS,
    );
    let t = timing(&sim, s.onset(), s.offset(), 12.0);
    assert!(t.to_full.is_ok_and(|v| v <= 0.0), "{:?}", t.to_full);
}

#[test]
fn speech_over_a_steady_fan_ducks_only_while_speaking() {
    let mut s = clip("1272-128104-0001", -26.0, 6_000, 6_000);
    let n = s.x.len();
    add(&mut s.x, &fan(n, -40.0, 41), 0);
    let sim = simulate(&s.x, false, None, duck_only(12.0));
    assert!(recall(&sim, &s) >= 95.0, "{}", recall(&sim, &s));
    assert_eq!(ducked_away(&sim, &s), 0.0);
}

#[test]
fn a_loud_song_is_leveled_within_a_second() {
    let x = song(6.0, -8.0, false, 11);
    let sim = simulate(&vec![0.0; x.len()], false, Some(&x), music(-26.0));
    let (top, over) = overshoot(&sim.out, 0, -26.0);
    assert!(over <= 600.0, "{over} ms over target + 3 LU, peak {top}");
    let out = loudness(&sim.out[3 * 48_000..], &[]);
    assert!((out + 26.0).abs() < 2.0, "{out} LUFS");
}

#[test]
fn noises_alone_do_not_duck() {
    // Before the RNNoise detector: 28 to 100 % of the time ducked.
    let cases: [(&str, Vec<f32>, usize); 9] = [
        ("keyboard -30", keyboard(10, -30.0, 1), 0),
        ("keyboard -15", keyboard(10, -15.0, 2), 0),
        ("breaths -45", breaths(10, -45.0), 0),
        ("breaths -35", breaths(10, -35.0), 0),
        ("cough -20", coughs(12, -20.0), 0),
        ("fan -45", appearing(12, 3_000, Some(-70.0), -45.0), 3_000),
        ("fan -30", appearing(12, 3_000, Some(-70.0), -30.0), 3_000),
        ("unmute -48", appearing(12, 3_000, None, -48.0), 3_000),
        ("unmute -40", appearing(12, 3_000, None, -40.0), 3_000),
    ];
    // One thread each: RNNoise is slow in a debug build.
    std::thread::scope(|s| {
        for (name, mic, from) in cases {
            s.spawn(move || {
                let sim = simulate(&mic, false, None, duck_only(12.0));
                let (pct, _, _) = false_ducks(&sim, from * MS);
                assert!(pct <= 2.0, "{name}: ducked {pct} % of the time");
            });
        }
    });
}

#[test]
fn music_bleeding_into_the_mic_ducks_less() {
    // Before: 23.5 % at -50, 44.1 % from -40 on. Now 0 and 13 to 27 %.
    for (db, most) in [(-50.0, 5.0), (-40.0, 35.0), (-20.0, 35.0)] {
        let sim = simulate(&bleed(20, db), false, None, duck_only(12.0));
        let (pct, _, _) = false_ducks(&sim, 0);
        assert!(pct <= most, "bleed at {db}: ducked {pct} % of the time");
    }
}

#[test]
fn soft_or_noisy_speech_is_ducked_throughout() {
    // Very soft, and at 9 dB SNR (before: 56 %, the duck came up 12 dB
    // mid-phrase).
    for (db, floor) in [(-48.0, -65.0), (-36.0, -45.0)] {
        let s = speech_with_floor(clip("1272-128104-0001", db, 1_000, 2_000), floor);
        let sim = simulate(&s.x, false, None, duck_only(12.0));
        let r = recall(&sim, &s);
        assert!(r >= 93.0, "{db} over {floor}: {r} %");
        let (rise, _) = pumping(&sim, s.onset(), s.offset(), 12.0);
        assert!(rise < 1.0, "{db} over {floor}: comes up {rise} dB");
    }
}

#[test]
fn the_voice_chain_does_not_lift_room_noise_into_voice() {
    // The detector hears the raw input: the chain's makeup gain does not
    // turn a -45 dB room into voice (before: 22 % of the time ducked).
    let sim = simulate(
        &room(10 * 1_000 * MS, -45.0, 51),
        true,
        None,
        duck_only(12.0),
    );
    let (pct, _, _) = false_ducks(&sim, 0);
    assert!(pct <= 1.0, "{pct} %");
}

#[test]
fn noise_right_after_speech_does_not_hold_the_voice() {
    // Speech, then a steady noise from 50 ms after its last word: the
    // hold's bridge must not keep the voice on through the noise.
    for db in [-50.0, -30.0] {
        let mut s = speech_with_floor(clip("1272-128104-0001", -26.0, 1_000, 4_000), -65.0);
        let at = s.offset() + 50 * MS;
        let n = s.x.len() - at;
        add(&mut s.x, &room(n, db, 61), at);
        let sim = simulate(&s.x, false, None, duck_only(12.0));
        let last = sim.speaking.iter().rposition(|&v| v).unwrap();
        let held = ms((last + 1) * BLK).max(0.0) - ms(s.offset());
        assert!(held < 600.0, "noise at {db}: voice held {held} ms");
    }
}

#[test]
fn the_voice_over_music_gap_does_not_depend_on_the_mic_level() {
    // Medium: 12 LU ducking, 14 LU in music mode (8 under, ducked 6 more).
    // Before the voice was measured: 8.4 / 2.4 / -3.6 and 14.6 / 8.6 / 2.6
    // LU at -20 / -26 / -32 dBFS. Now within 0.6 LU of the target.
    let cases = [
        (false, -14.0, -20.0, 12.0),
        (false, -14.0, -32.0, 12.0),
        (true, -10.0, -20.0, 14.0),
        (true, -10.0, -32.0, 14.0),
    ];
    std::thread::scope(|s| {
        for (music_mode, song_lufs, vdb, want) in cases {
            s.spawn(move || {
                let x = song(26.0, song_lufs, false, 15);
                let (_, _, gap, _) = gap_by_mic(&long_talk(vdb), &x, level_settings(music_mode, 1));
                assert!(
                    (gap - want).abs() <= 1.5,
                    "music {music_mode}, voice {vdb} dBFS: gap {gap} LU, want {want}"
                );
            });
        }
    });
}

/// A song as played: a quiet intro (-12 dB, 6 s), a 4 s fade-in, 12 s
/// full, a bridge 10 LU lower (6 s), 12 s full again; -14 LUFS when full.
fn arranged_song() -> Vec<f32> {
    let mut x = song(40.0, -14.0, false, 17);
    for (i, s) in x.iter_mut().enumerate() {
        let t = i as f32 / FS;
        let db = match t {
            t if t < 6.0 => -12.0,
            t if t < 10.0 => -12.0 + 3.0 * (t - 6.0),
            t if (22.0..28.0).contains(&t) => -10.0,
            _ => 0.0,
        };
        *s *= db_to_lin(db);
    }
    x
}

/// [`arranged_song`] in music mode at -26 LUFS, no voice: how far the
/// leveler's gain swings once the song is in (from 12 s), and how far
/// the music goes over the target after the intro (momentary, LU).
fn arranged_swing() -> (f32, f32) {
    let x = arranged_song();
    let sim = simulate(&vec![0.0; x.len()], false, Some(&x), music(-26.0));
    let l = &sim.level[600..];
    let (lo, hi) = l
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    let (top, _) = overshoot(&sim.out, 6_000 * MS, -26.0);
    (hi - lo, top)
}

/// A voice talking all along (300 ms words, 100 ms apart, -26 dBFS) over
/// a verse/chorus song (-14 LUFS, chorus 8 dB louder, from 4 s every
/// 8 s) with `settings`: how much louder the music is in the chorus's
/// first second (from 200 ms) than once settled (2 to 4 s in), LU, over
/// the choruses at 12 and 20 s. 0: ducked as deep at once.
fn chorus_under_duck(settings: DuckSettings) -> f32 {
    let words: Vec<_> = (0..74).map(|i| (300 + i * 400, 300)).collect();
    let voice = synth_speech(30_000, &words, 130.0, -26.0, 7);
    let x = song(30.0, -14.0, true, 18);
    let sim = simulate(&voice.x, false, Some(&x), settings);
    let at = |s: usize, a: usize, b: usize| {
        loudness(&sim.out, &[((s * 1_000 + a) * MS, (s * 1_000 + b) * MS)])
    };
    [12, 20]
        .iter()
        .map(|&s| at(s, 200, 1_000) - at(s, 2_000, 4_000))
        .sum::<f32>()
        / 2.0
}

/// A steady bed (the steady song at -26 LUFS, music mode at -26) with
/// noise bursts `len_ms` long, `over` LU louder than the bed, every
/// `period_ms` from 1 s, or once at 10 s (`period_ms` 0); 20 s. Returns
/// how far the leveler's gain (its guard included) moves from 10 s (the hits measured), dB,
/// and how long after the last burst it stays more than 1 dB off where it
/// was before the first, ms.
fn bursts(len_ms: usize, over: f32, period_ms: usize) -> (f32, f32) {
    let mut x = song(20.0, -26.0, false, 19);
    let len = len_ms * MS;
    let mut hit = noise(len, 1.0, 23);
    let g = db_to_lin(-26.0 + over - loudness(&hit, &[]));
    hit.iter_mut().for_each(|s| *s *= g);
    let starts: Vec<usize> = if period_ms == 0 {
        vec![10_000 * MS]
    } else {
        (1_000..19_000).step_by(period_ms).map(|t| t * MS).collect()
    };
    for &a in &starts {
        add(&mut x, &hit, a);
    }
    let sim = simulate(&vec![0.0; x.len()], false, Some(&x), music(-26.0));
    let first = starts[0] / BLK;
    let before = sim.level[first - 50..first].iter().sum::<f32>() / 50.0;
    let l = &sim.level[500..];
    let (lo, hi) = l
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    let end = (starts.last().unwrap() + len) / BLK;
    let off = sim.level[end..]
        .iter()
        .rposition(|v| (v - before).abs() > 1.0)
        .map_or(0, |i| i + 1);
    (hi - lo, (off * 20) as f32)
}

/// A -26 LUFS song for 10 s, then one 10 dB louder that stops for 250 ms
/// every 1.5 s, 30 s, music mode at -26: how far the leveler's gain moves
/// over its last 10 s (dB), and how long the music is over target + 3 LU
/// there (ms).
fn stop_time() -> (f32, f32) {
    let mut x = song(10.0, -26.0, false, 21);
    let mut b = song(20.0, -16.0, false, 22);
    for (i, s) in b.iter_mut().enumerate() {
        if i % (1_500 * MS) >= 1_250 * MS {
            *s = 0.0;
        }
    }
    x.extend(b);
    let sim = simulate(&vec![0.0; x.len()], false, Some(&x), music(-26.0));
    let l = &sim.level[1_000..];
    let (lo, hi) = l
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    let (_, over) = overshoot(&sim.out, 20_000 * MS, -26.0);
    (hi - lo, over)
}

/// A loud song (-10 LUFS) paused 20 dB lower for `pause_ms` at 10 s
/// (a game's pause menu), then loud again, music mode at -26: how far and
/// how long (ms) it goes over target + 3 LU once back.
fn pause_menu(pause_ms: usize) -> (f32, f32) {
    let mut x = song(25.0, -10.0, false, 20);
    let (a, b) = (10_000 * MS, (10_000 + pause_ms) * MS);
    x[a..b].iter_mut().for_each(|s| *s *= 0.1);
    let sim = simulate(&vec![0.0; x.len()], false, Some(&x), music(-26.0));
    overshoot(&sim.out, b, -26.0)
}

/// Song `a` then song `b` (LUFS), 15 s each, in music mode at -26 LUFS:
/// the leveler's convergence after the change (ms, final gain), and how
/// far and how long (ms) the music goes over target + 3 LU after it.
fn song_change(a: f32, b: f32) -> (f32, f32, f32, f32) {
    let mut x = song(15.0, a, false, 12);
    x.extend(song(15.0, b, false, 13));
    let sim = simulate(&vec![0.0; x.len()], false, Some(&x), music(-26.0));
    let (conv, gain) = convergence(&sim.level, 750);
    let (top, over) = overshoot(&sim.out, 15_000 * MS, -26.0);
    (conv, gain, top, over)
}

#[test]
fn a_song_change_is_leveled_in_both_directions() {
    // Before: 1.9 s over target + 3 LU (peak 14.9 LU), and 11.8 s to
    // converge 9 LU under. Now 100 ms, peak 3.6 LU: the guard holds the
    // louder song down from its first sample.
    let (_, _, top, over) = song_change(-40.0, -8.0);
    assert!(over <= 200.0, "{over} ms over target + 3 LU, peak {top}");
    assert!(top <= 6.0, "peak {top} LU over target");
    let (conv, gain, _, _) = song_change(-8.0, -40.0);
    assert!(conv <= 9_000.0, "{conv} ms to converge");
    assert!(gain >= 11.0, "raised {gain} dB only");
}

#[test]
fn the_leveler_keeps_a_songs_dynamics() {
    // Verse and chorus 8 dB apart: before, the leveler swung 8 dB.
    let x = song(40.0, -14.0, true, 14);
    let sim = simulate(&vec![0.0; x.len()], false, Some(&x), music(-26.0));
    let l = &sim.level[500..];
    let (lo, hi) = l
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    assert!(hi - lo <= 3.0, "swings {} dB", hi - lo);
}

#[test]
fn an_arranged_song_does_not_pump() {
    // Quiet intro, fade-in, a bridge 10 LU lower: 1.5 dB, 4.7 LU.
    let (swing, top) = arranged_swing();
    assert!(swing <= 3.0, "swings {swing} dB");
    assert!(top <= 7.0, "peak {top} LU over target");
}

#[test]
fn a_louder_section_is_ducked_as_it_starts() {
    // Before (depth from the 3 s loudness): the chorus's first second was
    // 3.9 (duck) and 6.3 LU (music) louder than settled; now 0.4 and 0.9.
    std::thread::scope(|s| {
        for (duck, music) in [(Some(1), None), (None, Some(1))] {
            s.spawn(move || {
                let under = chorus_under_duck(DuckSettings::at_levels(duck, music));
                assert!(under <= 2.0, "{duck:?} {music:?}: {under} LU");
            });
        }
    });
}

#[test]
fn hits_and_bursts_do_not_move_the_leveler() {
    // A 20 ms hit 14 LU over the bed, every 500 ms: before, the guard dipped
    // 3.3 dB per hit.
    for over in [14.0, 20.0] {
        let (moves, _) = bursts(20, over, 500);
        assert!(moves < 0.5, "{over} LU over: moves {moves} dB");
    }
    // A 400 ms burst: before, the bed stayed about 14 dB down for 7 s.
    let (_, back) = bursts(400, 14.0, 0);
    assert!(back <= 1_500.0, "back after {back} ms");
    // A song with stops is learnt, not rolled back at every stop.
    let (moves, over) = stop_time();
    assert!(moves <= 1.5, "moves {moves} dB");
    assert!(over <= 100.0, "{over} ms over target + 3 LU");
}

#[test]
fn the_release_follows_the_phrase_type() {
    // Before, every phrase came back 885 to 895 ms after its end. Now a
    // lone word in 400 ms, two words in 646, a long phrase in 1262; no
    // pumping between words either way.
    let types = phrase_types();
    std::thread::scope(|sc| {
        for (name, s, most, least) in [
            (types[0].0, &types[0].1, 500.0, 0.0),
            (types[1].0, &types[1].1, 800.0, 0.0),
            (types[2].0, &types[2].1, 1_400.0, 1_100.0),
        ] {
            sc.spawn(move || {
                let sim = simulate(&s.x, false, None, duck_only(12.0));
                let t = timing(&sim, s.onset(), s.offset(), 12.0);
                let back = t.back_full.unwrap();
                assert!((least..=most).contains(&back), "{name}: back in {back} ms");
                let (rise, _) = pumping(&sim, s.onset(), s.offset(), 12.0);
                assert!(rise < 0.25, "{name}: pumps {rise} dB");
            });
        }
    });
}

#[test]
fn the_duck_cuts_the_voice_band_more_and_nothing_while_silent() {
    // Before: 12 dB at every tone; music mode dipped 2.2 to 2.9 dB at 1 to
    // 3 kHz all along. Now the lows and highs go down 9.3 / 9.8 dB, the
    // voice band 14 to 20 dB, and nothing moves while the voice is silent.
    let (silent, talking) = tone_gains(duck_only(12.0));
    assert!(silent.iter().all(|g| g.abs() < 0.05), "{silent:?}");
    let [lo, _, mid, _, hi] = talking;
    assert!(lo > -10.5 && hi > -11.0, "{talking:?}");
    assert!(mid < -18.0, "{talking:?}");
    let (silent, talking) = tone_gains(music(-26.0));
    assert!(silent.iter().all(|g| g.abs() < 0.1), "music: {silent:?}");
    assert!(talking[2] < talking[0] - 3.0, "music: {talking:?}");
}

#[test]
fn the_spectral_duck_is_transparent_and_clean() {
    let (idle, after, thd, zip) = transparency();
    assert!(idle.starts_with("exact"), "idle: {idle}");
    assert!(after.starts_with("exact"), "after: {after}");
    // -132 / -129 and -93 dBc.
    assert!(thd.0 < -100.0 && thd.1 < -100.0, "{thd:?}");
    assert!(zip < -80.0, "spur {zip} dBc");
}

#[test]
fn the_voice_band_gets_more_room_under_the_same_gap() {
    // Medium, voice -26 dBFS: voice-band SNR 16.3 -> 20.2 dB (duck) and
    // 18.6 -> 20.7 (music), the lows 1.9 and 1.4 dB fuller than before,
    // the gap where it was (11.5, 13.6 LU).
    std::thread::scope(|sc| {
        for (music_mode, song_lufs, gap, snr) in
            [(false, -14.0, 12.0, 19.0), (true, -10.0, 14.0, 19.7)]
        {
            sc.spawn(move || {
                let x = song(26.0, song_lufs, false, 15);
                let r = bands_under_voice(&long_talk(-26.0), &x, level_settings(music_mode, 1));
                assert!(
                    (r.gap - gap).abs() <= 1.5,
                    "music {music_mode}: gap {}",
                    r.gap
                );
                assert!(r.snr >= snr, "music {music_mode}: SNR {}", r.snr);
                assert!(
                    r.lows > r.mids + 2.5,
                    "music {music_mode}: lows {} mids {}",
                    r.lows,
                    r.mids
                );
            });
        }
    });
}
