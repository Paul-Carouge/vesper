//! Ducking bench: scenarios fed through [`VoiceDetector`] and [`Ducker`]
//! as `vesper-audio` wires them, and the metrics that matter to a listener.
//!
//! The voice track and the ducked track run 20 ms blocks side by side. As in
//! `vesper-audio`'s `encode_block`, the voice key holds "speaking until"
//! (end of the last voiced window plus [`VOICE_HOLD_FRAMES`]) and a ducked
//! block asks it at its own end; here both are on one sample clock, the
//! voice's block first (the best case of the app's two threads).
//!
//! The duck's timing is read off a probe: a [`Ducker`] (duck only, the same
//! depth) on a constant 1.0, so its output is its gain, sample by sample.
//!
//! Full table: `cargo test --release -p vesper-dsp -- --ignored --nocapture
//! ducking_report`. The other tests are quick checks of what the current
//! behaviour already does well, so it does not regress.

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
    // shortcut: linear 16 to 48 kHz upsampling (images above 8 kHz, ~-30 dB):
    // fine for energy detection and loudness; a proper resampler if a
    // spectral detector is benched on these.
    let mut x = vec![0.0; lead_ms * MS];
    for w in x16.windows(2) {
        for k in 0..3 {
            x.push(w[0] + (w[1] - w[0]) * k as f32 / 3.0);
        }
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
}

/// Runs the voice `mic` (mono, through the voice chain first if `chain`)
/// against `music` (mono; silence if None) with `settings`.
fn simulate(mic: &[f32], chain: bool, music: Option<&[f32]>, settings: DuckSettings) -> Sim {
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
    let mut duck = Ducker::new(CH);
    let probe_set = DuckSettings {
        duck_db: depth,
        music_lufs: None,
    };
    let blocks = mic.len() / BLK;
    let mut sim = Sim {
        speaking: Vec::with_capacity(blocks),
        gain: Vec::with_capacity(blocks * BLK),
        out: Vec::with_capacity(blocks * BLK),
        level: Vec::with_capacity(blocks),
        comp: Vec::with_capacity(blocks),
    };
    let (mut v, mut m, mut p) = (
        vec![0.0; BLK * CH],
        vec![0.0; BLK * CH],
        vec![1.0; BLK * CH],
    );
    let mut until = 0;
    for b in 0..blocks {
        let at = b * BLK;
        for (i, f) in v.as_chunks_mut::<CH>().0.iter_mut().enumerate() {
            f.fill(mic[at + i]);
        }
        if let Some(c) = &mut chain {
            c.process(&mut v);
        }
        let end = at + BLK;
        if let Some(last) = det.process(&v) {
            until = until.max(at + last + VOICE_HOLD_FRAMES);
        }
        let speaking = end < until;
        sim.speaking.push(speaking);
        p.fill(1.0);
        probe.process(&mut p, probe_set, speaking);
        sim.gain
            .extend(p.as_chunks::<CH>().0.iter().map(|f| lin_to_db(f[0])));
        for (i, f) in m.as_chunks_mut::<CH>().0.iter_mut().enumerate() {
            f.fill(music.map_or(0.0, |x| x[at + i]));
        }
        duck.process(&mut m, settings, speaking);
        sim.out.extend(m.as_chunks::<CH>().0.iter().map(|f| f[0]));
        sim.level.push(duck.level);
        sim.comp.push(duck.comp);
    }
    sim
}

/// Duck timing of one phrase, ms: onset to -3 dB, to full depth; offset
/// to back over -3 dB, to over -0.5 dB. Err: "n/a" when already there at
/// the onset / offset (ducked early, released early), or "never".
struct Timing {
    to_3db: Result<f32, &'static str>,
    to_full: Result<f32, &'static str>,
    back_3db: Result<f32, &'static str>,
    back_full: Result<f32, &'static str>,
}

fn timing(sim: &Sim, onset: usize, offset: usize, depth: f32) -> Timing {
    let g = &sim.gain;
    let after = |from: usize, f: &dyn Fn(f32) -> bool| match g[from.min(g.len())..]
        .iter()
        .position(|&x| f(x))
    {
        Some(0) => Err("n/a"),
        Some(i) => Ok(ms(i)),
        None => Err("never"),
    };
    Timing {
        to_3db: after(onset, &|x| x <= -3.0),
        to_full: after(onset, &|x| x <= -(depth - 0.5)),
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
        music_lufs: None,
    }
}

fn music(lufs: f32) -> DuckSettings {
    DuckSettings {
        duck_db: 0.0,
        music_lufs: Some(lufs),
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

/// Level of `hz` in mono `x` (Goertzel), dB.
fn tone_db(x: &[f32], hz: f32) -> f32 {
    let w = std::f64::consts::TAU * f64::from(hz) / f64::from(FS);
    let c = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &v in x {
        let s0 = f64::from(v) + c * s1 - s2;
        (s2, s1) = (s1, s0);
    }
    let p = s1 * s1 + s2 * s2 - c * s1 * s2;
    10.0 * (p.max(1e-30)).log10() as f32
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
    }

    println!("\n### Voice over music: LU gap (voice minus music, while the voice speaks)\n");
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

    println!("\n### Presence dip (music mode)\n");
    {
        // 200 Hz and 2 kHz tones, equal level; the dip is their difference.
        let n = 6 * 48_000;
        let x: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / FS;
                0.05 * ((std::f32::consts::TAU * 200.0 * t).sin()
                    + (std::f32::consts::TAU * 2_000.0 * t).sin())
            })
            .collect();
        let quiet = vec![0.0; n];
        let mut talking = vec![0.0; n];
        add(
            &mut talking,
            &synth_speech(6_000, &[(500, 5_000)], 130.0, -26.0, 2).x,
            0,
        );
        for (name, mic) in [("voice silent", &quiet), ("voice speaking", &talking)] {
            let sim = simulate(mic, false, Some(&x), music(-26.0));
            let tail = &sim.out[4 * 48_000..];
            let src = &x[4 * 48_000..];
            let rel = |hz| tone_db(tail, hz) - tone_db(src, hz);
            println!(
                "- {name}: 2 kHz {:.1} dB vs 200 Hz {:.1} dB: dip {:.1} dB",
                rel(2_000.0),
                rel(200.0),
                rel(2_000.0) - rel(200.0)
            );
        }
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
    let idle = time(&mut |b, _| d.process(b, duck_only(12.0), false));
    let mut d = Ducker::new(CH);
    let ducking = time(&mut |b, i| d.process(b, duck_only(12.0), i % 100 < 50));
    let mut d = Ducker::new(CH);
    let bg = time(&mut |b, i| d.process(b, music(-26.0), i % 100 < 50));
    println!("| Stage | ns per block |\n|---|---|");
    println!("| VoiceDetector | {det:.0} |");
    println!("| Ducker, idle | {idle:.0} |");
    println!("| Ducker, ducking 12 dB | {ducking:.0} |");
    println!("| Ducker, background music | {bg:.0} |");
}

// Quick checks of what works today, so later changes do not regress it.

#[test]
fn a_spoken_phrase_ducks_fast_and_holds_through_its_gaps() {
    let s = speech_with_floor(synth_speech(5_000, &phrase_words(), 130.0, -26.0, 1), -65.0);
    let sim = simulate(&s.x, false, None, duck_only(12.0));
    let t = timing(&sim, s.onset(), s.offset(), 12.0);
    assert!(t.to_3db.is_ok_and(|v| v <= 60.0), "{:?}", t.to_3db);
    assert!(t.to_full.is_ok_and(|v| v <= 100.0), "{:?}", t.to_full);
    assert!(t.back_full.is_ok_and(|v| v <= 1_200.0), "{:?}", t.back_full);
    let (rise, _) = pumping(&sim, s.onset(), s.offset(), 12.0);
    // 0.0 today; a gap that lets the duck up would show as tenths of a dB.
    assert!(rise < 0.25, "pumps {rise} dB between words");
}

#[test]
fn real_speech_is_ducked_throughout() {
    let s = speech_with_floor(clip("1272-128104-0001", -26.0, 1_000, 2_000), -65.0);
    let sim = simulate(&s.x, false, None, duck_only(12.0));
    assert!(recall(&sim, &s) >= 95.0, "{}", recall(&sim, &s));
    let t = timing(&sim, s.onset(), s.offset(), 12.0);
    assert!(t.to_3db.is_ok_and(|v| v <= 60.0), "{:?}", t.to_3db);
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
