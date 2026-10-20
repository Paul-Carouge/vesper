//! Voice ducking and background music.
//!
//! A voice track (a microphone) runs a [`VoiceDetector`] on what it records.
//! Other tracks run a [`Ducker`]: it lowers them while the voice speaks, and
//! in background music mode also levels them well under the voice (a slow
//! short-term loudness leveler), tames their peaks (a gentle compressor) and
//! dips the voice presence band (a broad bell around 2 kHz), so speech stays
//! clear over them.
//!
//! The detector and the ducker meet through the caller (another thread, a
//! few ms apart): they only exchange "the voice speaks until then", which
//! a gain envelope tolerates. Every gain of the ducker (leveler, compressor,
//! duck) is summed in dB once per [`CHUNK`] frames, and the linear gain is
//! interpolated across the chunk: no zipper noise, one `exp` per chunk.
//! Nothing allocates, locks or makes a syscall after construction.

use crate::biquad::{CHUNK, Coefs, State};
use nnnoiseless::DenoiseState;

use crate::{FS, Glide, coef, db_to_lin, lin_to_db};

/// Voice detection window: 10 ms, an RNNoise frame.
const WINDOW: usize = DenoiseState::FRAME_SIZE;
/// RNNoise works on 16-bit sample values.
const SCALE: f32 = 32_768.0;
/// A window is voiced when RNNoise's voice probability is over `P_VOICE`
/// and its level `MARGIN_DB` over the noise floor. RNNoise alone is not
/// enough: on steady noise its probability drifts up to 0.9 and more
/// within seconds, and a noise burst then reads as voice.
const P_VOICE: f32 = 0.9;
const MARGIN_DB: f32 = 6.0;
/// ...and RNNoise keeps it: its output (the window before, RNNoise runs a
/// window late) is at most this far under that window's input. Speech
/// comes out nearly whole (about -1 dB at 9 dB SNR), a band-limited cough
/// or noise burst 4 to 10 dB down even when the probability says voice.
/// shortcut: full-band white noise bursts, after a few seconds of them,
/// get both a high probability and about -1 dB kept: they read as voice
/// (the bench's band-limited coughs, breaths and clicks do not). A
/// spectral flatness test would catch them if real mics show this.
const KEPT_DB: f32 = -3.0;
/// Within the hold after voice, a window this far over the floor is voiced
/// whatever RNNoise says: the unvoiced start of the next word (a fricative,
/// a plosive) keeps the phrase going instead of letting the duck up.
const BRIDGE_DB: f32 = 12.0;
const HOLD_WINDOWS: u8 = (VOICE_HOLD_FRAMES / WINDOW) as u8;
/// Windows under this (RMS dBFS) are digital silence (a stalled source
/// filled in, a gate): they leave the detector as it was.
const SILENCE_DB: f32 = -80.0;
/// Voice starts when 2 of the last 3 windows are voiced: a click is not
/// a word.
const VOTES: u8 = 0b111;
const OPEN_VOTES: u32 = 2;
/// Noise floor: the quietest window of the last `SPANS` spans of
/// `SPAN` windows (minimum statistics over 2 s), followed down at once
/// and up at 10 dB/s at most, speaking or not: a steady new noise (a fan)
/// is the floor about 2 s after it appears plus 1 s per 10 dB.
const SPAN: usize = 25;
const SPANS: usize = 8;
const FLOOR_RISE: f32 = 0.1;
const FLOOR_MAX: f32 = -25.0;

/// How long the voice counts as speaking after its last voiced window
/// (300 ms): the gaps between words do not pump the ducked tracks.
pub const VOICE_HOLD_FRAMES: usize = 14_400;

/// Time for a duck to reach its depth, and to come back from it, ms (for a
/// depth of at least [`MIN_RATE_DEPTH`]; less deep ducks are faster).
const ATTACK_MS: f32 = 50.0;
const RELEASE_MS: f32 = 600.0;
const MIN_RATE_DEPTH: f32 = 6.0;
/// Duck depth of background music when ducking is not set on its own.
pub const MUSIC_DUCK_DB: f32 = 6.0;

/// Presence dip of background music.
const DIP_HZ: f32 = 2_000.0;
/// About two octaves (1 to 4 kHz).
const DIP_Q: f64 = 0.7;
const DIP_DB: f32 = -4.0;
const DIP_GLIDE_MS: f32 = 100.0;

/// Leveler: loudness blocks of 100 ms, 30 of them (3 s, short-term loudness).
const BLOCK: usize = 4_800;
const BLOCKS: usize = 30;
/// Blocks quieter than this (LUFS) are left out: a pause is not leveled up.
const GATE_LUFS: f32 = -50.0;
/// Most the leveler lowers and raises, dB.
const LEVEL_MIN_DB: f32 = -30.0;
const LEVEL_MAX_DB: f32 = 6.0;
/// Leveler speed, dB/s: down fast (a loud song starts), up slowly.
const LEVEL_DOWN: f32 = 10.0;
/// Down speed once music mode turned on, after [`CATCH_UP_BLOCKS`] blocks
/// measured, until the leveler reached its first target: a loud song is
/// not left loud for seconds.
const LEVEL_CATCH_UP: f32 = 100.0;
const CATCH_UP_BLOCKS: usize = 3;
const LEVEL_UP: f32 = 2.0;

/// Compressor of background music: peaks this far over the target
/// loudness (peak dBFS against LUFS, a usual crest factor) are compressed.
const COMP_ABOVE: f32 = 10.0;
const COMP_RATIO: f32 = 2.5;
const COMP_KNEE: f32 = 6.0;
const COMP_ATTACK_MS: f32 = 10.0;
const COMP_RELEASE_MS: f32 = 300.0;

/// Sum of squares, eight lanes so it vectorizes.
fn sum_sq(x: &[f32]) -> f32 {
    let (chunks, rest) = x.as_chunks::<8>();
    let mut acc = [0f32; 8];
    for c in chunks {
        for (a, s) in acc.iter_mut().zip(c) {
            *a += s * s;
        }
    }
    acc.iter().sum::<f32>() + rest.iter().map(|s| s * s).sum::<f32>()
}

/// Tells when a voice track speaks, from its raw input (before its filters,
/// volume and mute): RNNoise's voice probability per 10 ms window (its own
/// state, not the track's noise suppression), backed by the window's level
/// over a noise floor tracked by minimum statistics. Not calling it (a
/// muted microphone) and digital silence leave its state as it was.
pub struct VoiceDetector {
    ch: usize,
    vad: Box<DenoiseState<'static>>,
    /// The current window, mixed to mono, and its denoised output (unused).
    input: Box<[f32; WINDOW]>,
    output: Box<[f32; WINDOW]>,
    /// Sum of squares of the current window, and its frames.
    sum: f32,
    frames: usize,
    floor: f32,
    /// Quietest window of each span (`FLOOR_MAX` when none yet), the
    /// current span and its windows.
    mins: [f32; SPANS],
    span: usize,
    in_span: usize,
    /// Last windows voiced, one bit each (newest lowest).
    votes: u8,
    speaking: bool,
    /// Windows since the last one spoken, up to `HOLD_WINDOWS`.
    quiet: u8,
    /// Level of the last window RNNoise heard, dBFS.
    last: f32,
}

impl VoiceDetector {
    /// Allocates, and sets up RNNoise's tables and the calling thread's FFT
    /// caches (see [`VoiceDetector::warm_up`]).
    pub fn new(ch: usize) -> Self {
        assert!(ch > 0, "no channel");
        let mut d = Self {
            ch,
            vad: DenoiseState::new(),
            input: Box::new([0.0; WINDOW]),
            output: Box::new([0.0; WINDOW]),
            sum: 0.0,
            frames: 0,
            floor: FLOOR_MAX,
            mins: [FLOOR_MAX; SPANS],
            span: 0,
            in_span: 0,
            votes: 0,
            speaking: false,
            quiet: HOLD_WINDOWS,
            last: SILENCE_DB,
        };
        d.warm_up();
        d
    }

    /// Prepares the calling thread: RNNoise's FFT keeps per-thread caches it
    /// allocates on a thread's first frame. Call it on the thread that will
    /// run [`VoiceDetector::process`]. Silence leaves RNNoise nearly as new.
    pub fn warm_up(&mut self) {
        self.vad.process_frame(&mut self.output[..], &[0.0; WINDOW]);
    }

    /// Listens afresh (a track that just became a voice): forgets the floor
    /// and the votes. RNNoise's state is kept: it would allocate, and it
    /// follows the input within a few windows anyway.
    pub fn reset(&mut self) {
        (self.sum, self.frames) = (0.0, 0);
        self.floor = FLOOR_MAX;
        self.mins = [FLOOR_MAX; SPANS];
        (self.span, self.in_span) = (0, 0);
        (self.votes, self.speaking, self.quiet) = (0, false, HOLD_WINDOWS);
    }

    /// Ends any phrase (digital silence, or not listening: a muted mic);
    /// the floor and RNNoise's state stay as they were.
    pub fn pause(&mut self) {
        (self.votes, self.speaking, self.quiet) = (0, false, HOLD_WINDOWS);
    }

    /// The noise floor it tracks, dBFS RMS.
    pub fn floor_db(&self) -> f32 {
        self.floor
    }

    /// Feeds `buf` (interleaved, whole frames). Returns where (frames from
    /// its start) the last window found voiced in it ends, if any.
    pub fn process(&mut self, buf: &[f32]) -> Option<usize> {
        let mut last = None;
        let mut at = 0;
        let ch = self.ch;
        let frames = buf.len() / ch;
        while at < frames {
            let take = (WINDOW - self.frames).min(frames - at);
            let part = &buf[at * ch..(at + take) * ch];
            self.sum += sum_sq(part);
            let mono = &mut self.input[self.frames..self.frames + take];
            for (m, f) in mono.iter_mut().zip(part.chunks_exact(ch)) {
                *m = f.iter().sum::<f32>() * (SCALE / ch as f32);
            }
            (self.frames, at) = (self.frames + take, at + take);
            if self.frames == WINDOW && self.window() {
                last = Some(at);
            }
        }
        last
    }

    /// Ends a window: whether it is voiced.
    fn window(&mut self) -> bool {
        let mean = self.sum / (WINDOW * self.ch) as f32;
        (self.sum, self.frames) = (0.0, 0);
        let level = 10.0 * mean.max(1e-12).log10();
        if level < SILENCE_DB {
            self.pause();
            return false;
        }
        let p = self
            .vad
            .process_frame(&mut self.output[..], &self.input[..]);
        // Both on the mono mix RNNoise hears: a stereo mic is not biased.
        let db = |x: &[f32]| {
            10.0 * (sum_sq(x) / (WINDOW as f32 * SCALE * SCALE))
                .max(1e-12)
                .log10()
        };
        let kept = db(&self.output[..]) - self.last;
        self.last = db(&self.input[..]);
        let over = level - self.floor;
        let strict = p > P_VOICE && over > MARGIN_DB && kept > KEPT_DB;
        let voiced = strict || (self.quiet < HOLD_WINDOWS && over > BRIDGE_DB);
        self.votes = (self.votes << 1 | u8::from(voiced)) & VOTES;
        self.speaking = self.votes.count_ones() >= OPEN_VOTES || (self.speaking && voiced);
        // Only RNNoise's voice restarts the bridge: a bridged window does
        // not, or a noise right after speech would hold the voice on.
        self.quiet = if self.speaking && strict {
            0
        } else {
            (self.quiet + 1).min(HOLD_WINDOWS)
        };
        // Minimum statistics: the floor never learns from silence (above),
        // and a steady new noise lifts it at a bounded rate.
        self.mins[self.span] = self.mins[self.span].min(level);
        let min = self.mins.iter().copied().fold(FLOOR_MAX, f32::min);
        self.floor = (self.floor + FLOOR_RISE).min(min);
        self.in_span += 1;
        if self.in_span == SPAN {
            self.span = (self.span + 1) % SPANS;
            (self.mins[self.span], self.in_span) = (FLOOR_MAX, 0);
        }
        self.speaking
    }
}

/// What a [`Ducker`] does.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DuckSettings {
    /// Lowered by this much (dB) while the voice speaks; 0: not lowered.
    pub duck_db: f32,
    /// Background music, leveled to this short-term loudness (LUFS);
    /// `None`: off. It also ducks, by [`MUSIC_DUCK_DB`] unless `duck_db`
    /// is set.
    pub music_lufs: Option<f32>,
}

/// Moves `x` towards `target` by at most `down` (when lower) or `up`.
fn slew(x: f32, target: f32, down: f32, up: f32) -> f32 {
    if target < x {
        (x - down).max(target)
    } else {
        (x + up).min(target)
    }
}

/// Compressor gain change (dB, <= 0) at peak level `x` dB.
fn curve(x: f32, threshold: f32) -> f32 {
    let over = x - threshold;
    let slope = 1.0 / COMP_RATIO - 1.0;
    if 2.0 * over <= -COMP_KNEE {
        0.0
    } else if 2.0 * over < COMP_KNEE {
        slope * (over + COMP_KNEE / 2.0).powi(2) / (2.0 * COMP_KNEE)
    } else {
        slope * over
    }
}

/// Ducking and background music of a non-voice track (see the module docs).
pub struct Ducker {
    ch: usize,
    /// Presence dip gain, dB: 0 when off.
    dip: Glide,
    dip_c: Coefs,
    dip_z: Vec<State>,
    kw: [Coefs; 2],
    kw_z: Vec<[State; 2]>,
    /// K-weighted power of the current 100 ms block (summed over the
    /// channels), and its frames.
    power: f64,
    frames: usize,
    /// Mean power of the last [`BLOCKS`] blocks.
    blocks: [f32; BLOCKS],
    next: usize,
    /// Music mode in the last block: turned on, the leveler starts over.
    music: bool,
    /// Blocks measured since music mode turned on, while it catches up.
    catch_up: Option<usize>,
    /// Gain the leveler goes to, dB.
    level_target: f32,
    /// Gains, dB: leveler, compressor, duck.
    level: f32,
    comp: f32,
    duck: f32,
    /// Linear gain at the end of the last chunk.
    gain: f32,
}

impl Ducker {
    pub fn new(ch: usize) -> Self {
        assert!(ch > 0, "no channel");
        Self {
            ch,
            dip: Glide::new(0.0, DIP_GLIDE_MS, CHUNK),
            dip_c: Coefs::peaking(DIP_HZ, DIP_Q, 0.0),
            dip_z: vec![State::default(); ch],
            kw: Coefs::k_weighting(),
            kw_z: vec![[State::default(); 2]; ch],
            power: 0.0,
            frames: 0,
            blocks: [0.0; BLOCKS],
            next: 0,
            music: false,
            catch_up: None,
            level_target: 0.0,
            level: 0.0,
            comp: 0.0,
            duck: 0.0,
            gain: 1.0,
        }
    }

    /// How much the duck lowers the track now, dB (>= 0). For a meter.
    pub fn ducking_db(&self) -> f32 {
        0.0 - self.duck
    }

    /// Nothing to do: every gain at 0 dB, no dip.
    fn idle(&self) -> bool {
        self.gain == 1.0
            && self.level == 0.0
            && self.comp == 0.0
            && self.duck == 0.0
            && self.dip.settled()
            && self.dip.cur == 0.0
    }

    /// Processes `buf` (interleaved, whole frames) with `settings`; the
    /// voice speaks during it or not. Real-time safe.
    pub fn process(&mut self, buf: &mut [f32], settings: DuckSettings, speaking: bool) {
        let music = settings.music_lufs;
        let depth = if settings.duck_db > 0.0 {
            settings.duck_db
        } else if music.is_some() {
            MUSIC_DUCK_DB
        } else {
            0.0
        };
        if music.is_none() && !(speaking && depth > 0.0) && self.idle() {
            self.music = false;
            return;
        }
        if music.is_some() && !self.music {
            // A new measurement: the old one may be of other music.
            (self.blocks, self.power, self.frames) = ([0.0; BLOCKS], 0.0, 0);
            self.catch_up = Some(0);
        }
        self.music = music.is_some();
        if music.is_none() {
            (self.level_target, self.catch_up) = (0.0, None);
        }
        self.dip.target = if music.is_some() { DIP_DB } else { 0.0 };
        let per_chunk = CHUNK as f32 / FS;
        let rate = depth.max(MIN_RATE_DEPTH);
        let (attack, release) = (
            rate / ATTACK_MS * 1e3 * per_chunk,
            rate / RELEASE_MS * 1e3 * per_chunk,
        );
        let (comp_attack, comp_release) = (
            coef(COMP_ATTACK_MS / CHUNK as f32),
            coef(COMP_RELEASE_MS / CHUNK as f32),
        );
        let duck_target = if speaking { -depth } else { 0.0 };
        let ch = self.ch;
        for chunk in buf.chunks_mut(CHUNK * ch) {
            if !self.dip.settled() {
                self.dip_c = Coefs::peaking(DIP_HZ, DIP_Q, self.dip.step());
            }
            let mut peak = 0f32;
            // Measured before the dip: the leveler hears the music as it is.
            if let Some(lufs) = music {
                self.measure(chunk, lufs);
                peak = chunk.iter().fold(0f32, |m, s| m.max(s.abs()));
            }
            if self.dip.cur != 0.0 {
                for frame in chunk.chunks_exact_mut(ch) {
                    for (s, z) in frame.iter_mut().zip(&mut self.dip_z) {
                        *s = z.run(&self.dip_c, f64::from(*s)) as f32;
                    }
                }
            }
            // Out of music mode the leveler goes back fast.
            let up = if music.is_some() {
                LEVEL_UP
            } else {
                LEVEL_DOWN
            };
            let down = match self.catch_up {
                Some(n) if n >= CATCH_UP_BLOCKS => LEVEL_CATCH_UP,
                _ => LEVEL_DOWN,
            };
            self.level = slew(
                self.level,
                self.level_target,
                down * per_chunk,
                up * per_chunk,
            );
            if self.catch_up.is_some_and(|n| n >= CATCH_UP_BLOCKS)
                && self.level <= self.level_target
            {
                self.catch_up = None;
            }
            let gc = match music {
                Some(lufs) => curve(lin_to_db(peak) + self.level, lufs + COMP_ABOVE),
                None => 0.0,
            };
            let a = if gc < self.comp {
                comp_attack
            } else {
                comp_release
            };
            self.comp = gc + a * (self.comp - gc);
            if self.comp > -1e-4 {
                self.comp = 0.0;
            }
            self.duck = slew(self.duck, duck_target, attack, release);
            let to = db_to_lin(self.level + self.comp + self.duck);
            self.ramp(chunk, to);
        }
        self.dip_z.iter_mut().for_each(State::flush);
        for z in &mut self.kw_z {
            z.iter_mut().for_each(State::flush);
        }
    }

    /// Ramps the gain of `chunk` from the last chunk's to `to`.
    fn ramp(&mut self, chunk: &mut [f32], to: f32) {
        let from = self.gain;
        self.gain = to;
        if from == to {
            if to != 1.0 {
                chunk.iter_mut().for_each(|s| *s *= to);
            }
            return;
        }
        let frames = chunk.len() / self.ch;
        let step = (to - from) / frames as f32;
        for (i, frame) in chunk.chunks_exact_mut(self.ch).enumerate() {
            let g = from + step * (i + 1) as f32;
            frame.iter_mut().for_each(|s| *s *= g);
        }
    }

    /// Adds `chunk` to the loudness measurement; sets the leveler's target
    /// at the end of each 100 ms block.
    fn measure(&mut self, chunk: &[f32], lufs: f32) {
        let [c0, c1] = self.kw;
        for frame in chunk.chunks_exact(self.ch) {
            for (&s, z) in frame.iter().zip(&mut self.kw_z) {
                let x = z[0].run(&c0, f64::from(s));
                let y = z[1].run(&c1, x);
                self.power += y * y;
            }
        }
        self.frames += chunk.len() / self.ch;
        if self.frames < BLOCK {
            return;
        }
        self.blocks[self.next] = (self.power / self.frames as f64) as f32;
        self.next = (self.next + 1) % BLOCKS;
        (self.power, self.frames) = (0.0, 0);
        if let Some(n) = &mut self.catch_up {
            *n += 1;
        }
        let gate = db_to_lin(GATE_LUFS + 0.691).powi(2);
        let (sum, n) = self
            .blocks
            .iter()
            .filter(|&&p| p > gate)
            .fold((0.0, 0), |(s, n), p| (s + p, n + 1));
        if n > 0 {
            let loudness = -0.691 + 10.0 * (sum / n as f32).log10();
            self.level_target = (lufs - loudness).clamp(LEVEL_MIN_DB, LEVEL_MAX_DB);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;

    const CH: usize = 2;
    /// 20 ms blocks, as the tracks run them.
    const BLK: usize = 960;

    /// Speech-like: noise bursts of `on` ms every `period` ms,
    /// at RMS `db`, over a noise floor at `floor_db`, `ms` long.
    fn speech(ms: usize, on: usize, period: usize, db: f32, floor_db: f32) -> Vec<f32> {
        let n = ms * 48;
        let floor = noise(n * CH, db_to_lin(floor_db) * 3f32.sqrt(), 3);
        let voice = noise(n * CH, db_to_lin(db) * 3f32.sqrt(), 11);
        (0..n * CH)
            .map(|i| {
                let t = i / CH / 48;
                floor[i] + if t % period < on { voice[i] } else { 0.0 }
            })
            .collect()
    }

    /// Voiced or not, per 20 ms block.
    fn detect(buf: &[f32]) -> Vec<bool> {
        let mut d = VoiceDetector::new(CH);
        buf.chunks(BLK * CH)
            .map(|b| d.process(b).is_some())
            .collect()
    }

    #[test]
    fn sum_sq_matches_the_plain_sum() {
        let x = noise(1_001, 0.7, 5);
        let plain: f32 = x.iter().map(|s| s * s).sum();
        assert!((sum_sq(&x) - plain).abs() < 1e-3 * plain);
    }

    #[test]
    fn noise_alone_is_not_voice() {
        // Steady noise at -40 dBFS (above the absolute threshold) and at
        // -70: once the floor is found (~100 ms), never voice.
        for floor in [-40.0, -70.0] {
            let voiced = detect(&speech(10_000, 0, 1_000, -20.0, floor));
            assert!(voiced[10..].iter().all(|v| !v), "floor {floor}");
        }
        // Digital silence.
        let voiced = detect(&vec![0.0; 48_000 * CH]);
        assert!(voiced.iter().all(|v| !v));
    }

    #[test]
    fn digital_silence_leaves_the_detector_as_it_was() {
        // A muted or stalled microphone filled with zeros: the floor is not
        // dragged down, so the room is not voice when it comes back.
        let mut d = VoiceDetector::new(CH);
        let room = speech(3_000, 0, 1_000, -20.0, -45.0);
        room.chunks(BLK * CH).for_each(|b| _ = d.process(b));
        let (floor, mins) = (d.floor, d.mins);
        assert!((-48.0..-44.0).contains(&floor), "{floor}");
        for _ in 0..150 {
            assert_eq!(d.process(&[0.0; BLK * CH]), None);
        }
        assert_eq!((d.floor, d.mins), (floor, mins));
        assert!(room.chunks(BLK * CH).all(|b| d.process(b).is_none()));
    }

    #[test]
    fn the_floor_follows_a_new_noise_at_a_bounded_rate() {
        // -70 dB room, then a -30 dB noise from 3 s.
        let mut x = speech(3_000, 0, 1_000, -20.0, -70.0);
        x.extend(speech(9_000, 0, 1_000, -20.0, -30.0));
        let mut d = VoiceDetector::new(CH);
        let floors: Vec<f32> = x
            .chunks(BLK * CH)
            .map(|b| {
                d.process(b);
                d.floor
            })
            .collect();
        // The 2 s minimum holds it, then it rises 10 dB/s at most.
        assert!(floors[150 + 80] < -69.0, "{}", floors[230]);
        for w in floors[150..].windows(2) {
            assert!(w[1] - w[0] <= 2.0 * FLOOR_RISE + 1e-4, "{w:?}");
        }
        assert!(floors[150 + 300] > -33.0, "{}", floors[450]);
    }

    #[test]
    fn reset_forgets_the_floor() {
        let mut d = VoiceDetector::new(CH);
        speech(1_000, 0, 1_000, -20.0, -60.0)
            .chunks(BLK * CH)
            .for_each(|b| _ = d.process(b));
        assert!(d.floor < -55.0);
        d.reset();
        assert_eq!(d.floor, FLOOR_MAX);
    }

    #[test]
    fn a_click_is_not_voice() {
        // 5 ms bursts: under the two windows a word needs.
        let voiced = detect(&speech(3_000, 5, 300, -20.0, -60.0));
        assert!(voiced.iter().all(|v| !v));
    }

    /// Runs a ducker over `buf` in 20 ms blocks; `speaking(block)`.
    fn duck(buf: &mut [f32], settings: DuckSettings, speaking: impl Fn(usize) -> bool) -> Ducker {
        let mut d = Ducker::new(CH);
        for (i, b) in buf.chunks_mut(BLK * CH).enumerate() {
            d.process(b, settings, speaking(i));
        }
        d
    }

    fn duck_only(db: f32) -> DuckSettings {
        DuckSettings {
            duck_db: db,
            music_lufs: None,
        }
    }

    #[test]
    fn duck_reaches_its_depth_in_the_attack_time_and_comes_back() {
        // Constant 1.0: the output is the gain. Voice from 1 s to 2 s.
        let mut buf = vec![1.0; 4 * 48_000 * CH];
        duck(&mut buf, duck_only(12.0), |b| (50..100).contains(&b));
        let db = |ms: usize| lin_to_db(buf[ms * 48 * CH]);
        assert_eq!(db(990), 0.0);
        // Halfway down after half the attack, all the way after it.
        assert!((db(1_025) + 6.0).abs() < 0.3, "{}", db(1_025));
        assert!((db(1_051) + 12.0).abs() < 0.05, "{}", db(1_051));
        assert!((db(1_900) + 12.0).abs() < 0.05);
        // Back over the release.
        assert!((db(2_300) + 6.0).abs() < 0.3, "{}", db(2_300));
        assert!(db(2_601).abs() < 0.05, "{}", db(2_601));
        // No zipper: the gain moves by a tiny step per sample.
        let step = buf
            .chunks(CH)
            .map(|f| f[0])
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!(step < 2e-3, "{step}");
    }

    #[test]
    fn idle_ducker_leaves_the_audio_untouched() {
        let input = noise(48_000 * CH, 0.5, 9);
        let mut buf = input.clone();
        let d = duck(&mut buf, duck_only(12.0), |_| false);
        assert_eq!(buf, input);
        assert!(d.idle());
        // Turned off while ducked: it comes back, then idles.
        let mut buf = vec![1.0; 2 * 48_000 * CH];
        let mut d = Ducker::new(CH);
        for (i, b) in buf.chunks_mut(BLK * CH).enumerate() {
            let s = if i < 20 {
                duck_only(12.0)
            } else {
                duck_only(0.0)
            };
            d.process(b, s, true);
        }
        assert!(d.idle());
        assert_eq!(*buf.last().unwrap(), 1.0);
    }

    fn music(lufs: f32) -> DuckSettings {
        DuckSettings {
            duck_db: 0.0,
            music_lufs: Some(lufs),
        }
    }

    /// Loudness of `buf` (stereo), LUFS, ungated.
    fn loudness(buf: &[f32]) -> f32 {
        let mut z = [[State::default(); 2]; CH];
        let [c0, c1] = Coefs::k_weighting();
        let mut p = 0.0;
        for frame in buf.chunks(CH) {
            for (&s, z) in frame.iter().zip(&mut z) {
                let x = z[0].run(&c0, f64::from(s));
                let y = z[1].run(&c1, x);
                p += y * y;
            }
        }
        -0.691 + 10.0 * (p / (buf.len() / CH) as f64).log10() as f32
    }

    #[test]
    fn k_weighting_reads_a_1khz_sine_as_its_rms() {
        // BS.1770: a 0 dBFS 1 kHz sine on both channels is -3.01 + 3 LUFS.
        let s = sine(1_000.0, 1.0, 48_000, CH);
        assert!(
            (loudness(&s[9_600..]) - 0.0).abs() < 0.1,
            "{}",
            loudness(&s)
        );
    }

    #[test]
    fn leveler_brings_loud_music_to_the_target_and_raises_quiet_music_a_little() {
        // White noise music at about -6 LUFS, 12 s: leveled to the target
        // (measured before the dip, so the gain is target minus input).
        let input = noise(12 * 48_000 * CH, 0.5, 4);
        let mut buf = input.clone();
        let d = duck(&mut buf, music(-26.0), |_| false);
        let want = -26.0 - loudness(&input[9 * 48_000 * CH..]);
        assert!((d.level - want).abs() < 0.5, "{} dB, want {want}", d.level);
        assert_eq!(d.ducking_db(), 0.0);
        // About -39 LUFS: raised by the most the leveler does.
        let mut buf = noise(12 * 48_000 * CH, 0.01, 4);
        let d = duck(&mut buf, music(-26.0), |_| false);
        assert!((d.level - LEVEL_MAX_DB).abs() < 0.01, "{}", d.level);
    }

    #[test]
    fn leveler_catches_a_loud_song_up_at_once() {
        // -6 LUFS music, turned on: within 0.6 s near its target, not
        // ~20 dB over it for seconds.
        let input = noise(48_000 * CH, 0.5, 4);
        let want = -26.0 - loudness(&input);
        let mut d = Ducker::new(CH);
        let mut buf = input.clone();
        for b in buf[..30 * BLK * CH].chunks_mut(BLK * CH) {
            d.process(b, music(-26.0), false);
        }
        assert!((d.level - want).abs() < 2.0, "{} dB, want {want}", d.level);
        assert!(d.catch_up.is_none());
    }

    #[test]
    fn music_peaks_are_compressed() {
        // A quiet bed with a loud 100 ms hit every 3 s: the hits come out
        // lower against the bed than they went in.
        let input: Vec<f32> = noise(12 * 48_000 * CH, 1.0, 8)
            .into_iter()
            .enumerate()
            .map(|(i, s)| {
                s * if (i / CH) % 144_000 < 4_800 {
                    1.0
                } else {
                    0.02
                }
            })
            .collect();
        let mut buf = input.clone();
        duck(&mut buf, music(-26.0), |_| false);
        let ratio = |b: &[f32]| {
            let hit = &b[(9 * 48_000 + 960) * CH..(9 * 48_000 + 4_800) * CH];
            let bed = &b[(10 * 48_000) * CH..(11 * 48_000) * CH];
            rms_db(hit) - rms_db(bed)
        };
        let (before, after) = (ratio(&input), ratio(&buf));
        assert!(after < before - 1.5, "{before} dB to {after} dB");
    }

    /// Steady-state gain of the presence dip at `freq`, dB.
    fn dip_response(freq: f32) -> f32 {
        let input = sine(freq, 0.5, 48_000, 1);
        let c = Coefs::peaking(DIP_HZ, DIP_Q, DIP_DB);
        let mut z = State::default();
        let out: Vec<f32> = input
            .iter()
            .map(|&s| z.run(&c, f64::from(s)) as f32)
            .collect();
        rms_db(&out[24_000..]) - rms_db(&input[24_000..])
    }

    #[test]
    fn presence_dip_is_a_broad_bell_around_2khz() {
        let at = dip_response;
        assert!((at(2_000.0) - DIP_DB).abs() < 0.1, "{}", at(2_000.0));
        // About half the depth an octave away, nothing far away.
        assert!(at(1_000.0) < -1.5 && at(4_000.0) < -1.5);
        assert!(at(100.0).abs() < 0.1, "{}", at(100.0));
        assert!(at(16_000.0).abs() < 0.3, "{}", at(16_000.0));
    }

    #[test]
    fn music_mode_glides_the_dip_in() {
        let mut d = Ducker::new(CH);
        let mut buf = vec![0.0; BLK * CH];
        d.process(&mut buf, music(-26.0), false);
        assert!(d.dip.cur < 0.0 && d.dip.cur > DIP_DB);
        for _ in 0..100 {
            d.process(&mut buf, music(-26.0), false);
        }
        assert_eq!(d.dip.cur, DIP_DB);
    }

    #[test]
    fn music_ducks_softly_by_default_and_by_the_set_depth() {
        for (settings, want) in [
            (music(-26.0), MUSIC_DUCK_DB),
            (
                DuckSettings {
                    duck_db: 18.0,
                    music_lufs: Some(-26.0),
                },
                18.0,
            ),
        ] {
            let mut buf = vec![0.0; 2 * 48_000 * CH];
            let d = duck(&mut buf, settings, |b| b > 20);
            assert!((d.ducking_db() - want).abs() < 1e-3, "{}", d.ducking_db());
        }
    }

    #[test]
    fn no_nan_or_denormals_after_silence() {
        let mut buf = noise(48_000 * CH, 0.5, 1);
        buf.extend(vec![0.0; 10 * 48_000 * CH]);
        let d = duck(&mut buf, music(-26.0), |b| b % 7 < 3);
        assert!(buf.iter().all(|s| s.is_finite()));
        let tiny = |x: f64| x != 0.0 && x.abs() < 1e-30;
        for z in d.dip_z.iter().chain(d.kw_z.iter().flatten()) {
            let (a, b) = z.raw();
            assert!(!tiny(a) && !tiny(b));
        }
    }

    /// CPU per track: `cargo test --release -p vesper-dsp -- --ignored
    /// --nocapture cpu_per_track`. 60 s of 48 kHz stereo in 20 ms blocks.
    #[test]
    #[ignore = "benchmark"]
    fn cpu_per_track() {
        let audio = noise(60 * 48_000 * CH, 0.3, 2);
        let time = |what: &str, f: &mut dyn FnMut(&mut [f32], usize)| {
            let mut buf = audio.clone();
            let t = std::time::Instant::now();
            for (i, b) in buf.chunks_mut(BLK * CH).enumerate() {
                f(b, i);
            }
            let per_block = t.elapsed().as_secs_f64() / (buf.len() / (BLK * CH)) as f64;
            println!(
                "{what}: {:.2} us per 20 ms block, {:.3} % of a core",
                per_block * 1e6,
                per_block / 0.02 * 100.0
            );
            std::hint::black_box(buf);
        };
        let mut v = VoiceDetector::new(CH);
        time("voice detector", &mut |b, _| {
            std::hint::black_box(v.process(b));
        });
        let mut d = Ducker::new(CH);
        time("ducker, idle", &mut |b, _| {
            d.process(b, duck_only(12.0), false)
        });
        let mut d = Ducker::new(CH);
        time("ducker, ducking", &mut |b, i| {
            d.process(b, duck_only(12.0), i % 100 < 50)
        });
        let mut d = Ducker::new(CH);
        time("background music", &mut |b, i| {
            d.process(b, music(-26.0), i % 100 < 50)
        });
    }
}

#[cfg(test)]
#[path = "duck_bench.rs"]
mod bench;
