//! Voice ducking and background music.
//!
//! A voice track (a microphone) runs a [`VoiceDetector`] on its raw input
//! and a [`VoiceMeter`] on what it records (its loudness while it speaks).
//! Other tracks run a [`Ducker`]: it lowers them while the voice speaks (as
//! deep as it takes to leave them a set gap under the voice, once its
//! loudness is known), and in background music mode also levels them under
//! the voice (a slow loudness leveler that keeps a song's dynamics and
//! catches up with a song change), tames their peaks (a gentle compressor)
//! and dips the voice presence band (a broad bell around 2 kHz), so speech
//! stays clear over them.
//!
//! The detector and the ducker meet through a [`VoiceKey`] (other threads):
//! the voice marks its voiced 10 ms windows on the shared timeline, and a
//! ducked track, its audio held back by [`LOOKAHEAD_FRAMES`], asks about
//! its block and the lookahead after it; the voice also tells its loudness
//! there. Every gain of the ducker (leveler, compressor,
//! duck) is summed in dB once per [`CHUNK`] frames, and the linear gain is
//! interpolated across the chunk: no zipper noise, one `exp` per chunk.
//! The ducker measures its track as it comes in ([`Ducker::look_ahead`]),
//! before the lookahead holds it back: a much louder song is held down
//! from its first sample, a louder section ducked as it starts.
//! Nothing allocates, locks or makes a syscall after construction.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

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
/// comes out nearly whole (about -1 dB at 9 dB SNR); the bench's coughs
/// (noise low-passed at 3 kHz) 4 to 10 dB down even when the probability
/// says voice. Not every band-limited burst is: see `ZCR_MAX`.
const KEPT_DB: f32 = -3.0;
/// ...and it is not broadband noise: its zero-crossing rate (crossings per
/// sample) is under this. Voiced speech reads 0.0 to 0.3, white noise
/// about 0.5. After a few seconds of broadband bursts (static, hiss),
/// RNNoise gives them a high probability and keeps them whole.
/// shortcut: band-limited noise bursts (low-passed at 4 kHz, and low
/// rumble such as mic bumps or wind) have a low rate and can still read as
/// voice; a spectral check (flatness, or harmonicity) would catch them.
const ZCR_MAX: f32 = 0.35;
/// Within the hold after voice, a window this far over the floor is voiced
/// whatever RNNoise says: the unvoiced start of the next word (a fricative,
/// a plosive) keeps the phrase going instead of letting the duck up.
const BRIDGE_DB: f32 = 12.0;
const HOLD_WINDOWS: u8 = (VOICE_HOLD_FRAMES / WINDOW) as u8;
/// Windows under this (RMS dBFS) are digital silence (a stalled source
/// filled in, a gate): they end any phrase but leave the floor and
/// RNNoise's state as they were.
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

/// How far ahead of its audio a ducked track reads the voice, frames
/// (120 ms; a multiple of the tracks' 20 ms block). Its audio waits that
/// long (a processing latency, taken off its timeline like a plugin's), so
/// the duck is down by the first syllable although the detector needs
/// some voiced sound before it opens (about 30 ms, plus the 50 ms attack).
/// 80 ms would do with the voice told in time; the rest covers a mic that
/// tells 40 ms after a playback track got the same instant (capture and
/// playback delays). See `ducking_report`'s lookahead table.
pub const LOOKAHEAD_FRAMES: usize = 5_760;

/// Windows per word of a [`VoiceKey`]; its top 32 bits tag the word. The
/// timeline counts from boot: a 32-bit tag of 320 ms words wraps after 43
/// years, so any two tags compare (wrapping, as `i32`) the right way.
const KEY_BITS: u64 = 32;
/// The low 32 bits: a word's window bits, or a word index's tag.
const LOW32: u64 = u32::MAX as u64;
/// Words of a [`VoiceKey`]: 32 of 320 ms, 10 s of history (the hold, the
/// lookahead and a voice seconds late fit well within).
const KEY_WORDS: usize = 32;

/// When the voices speak, on the timeline the tracks share: a timeline
/// frame is a track's anchor in frames plus its sample position, so tracks
/// with the same anchor agree on it sample for sample. Voice tracks mark
/// their voiced 10 ms windows and how far they have told; ducked tracks ask
/// about any span. Lock-free, wait-free for readers, no allocation:
/// several voices merge (a window is voiced if any voice says so).
#[derive(Debug, Default)]
pub struct VoiceKey {
    /// One bit per window: word `k` holds windows `32k..32k + 32` in its
    /// low bits and `k` (32 bits) on top. A word tagged otherwise holds
    /// older windows, none of them asked about any more.
    words: [AtomicU64; KEY_WORDS],
    /// Timeline frame up to which a voice has told (the furthest one).
    heard: AtomicU64,
    /// Voice tracks listening.
    voices: AtomicU32,
    /// `f32` bits of the voice's loudness as recorded (LUFS, see
    /// [`VoiceMeter`]), 0 while none is measured.
    loudness: AtomicU32,
}

impl VoiceKey {
    /// A voice track starts listening.
    pub fn add_voice(&self) {
        self.voices.fetch_add(1, Ordering::Relaxed);
    }

    /// A voice track stops listening (one that called [`Self::add_voice`]).
    /// The last one takes its loudness away.
    pub fn remove_voice(&self) {
        if self.voices.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.loudness.store(0, Ordering::Relaxed);
        }
    }

    /// A voice measured its loudness, LUFS (see [`VoiceMeter`]). With
    /// several voices the last one to tell wins: a voice tells only
    /// after 200 ms more of its own speech, so it is the one speaking now,
    /// the one the ducked tracks make room for. The loudest instead would
    /// bury a quiet guest under music set for a loud host.
    pub fn set_loudness(&self, lufs: f32) {
        self.loudness.store(lufs.to_bits(), Ordering::Relaxed);
    }

    /// The voice's loudness, LUFS, once measured.
    pub fn loudness(&self) -> Option<f32> {
        let lufs = f32::from_bits(self.loudness.load(Ordering::Relaxed));
        (lufs < 0.0).then_some(lufs)
    }

    /// Whether a voice track listens: tracks only duck under one then.
    pub fn has_voice(&self) -> bool {
        self.voices.load(Ordering::Relaxed) > 0
    }

    /// The detector's window ending at timeline frame `end` is voiced.
    /// Kept in the window holding its middle: exact when the track's
    /// anchor is a whole number of windows, 5 ms off at worst.
    pub fn voiced(&self, end: u64) {
        let w = end.saturating_sub(WINDOW as u64 / 2) / WINDOW as u64;
        let k = w / KEY_BITS;
        let (tag, bit) = (k & LOW32, 1 << (w % KEY_BITS));
        let word = &self.words[k as usize % KEY_WORDS];
        let mut cur = word.load(Ordering::Relaxed);
        loop {
            let old = cur >> KEY_BITS;
            // A word with no bit set was never written (every write sets
            // one): any tag takes it, whatever its starting tag 0 says.
            let new = if old == tag {
                cur | bit
            } else if cur & LOW32 == 0 || tag.wrapping_sub(old) as u32 as i32 > 0 {
                tag << KEY_BITS | bit
            } else {
                // Older than the history: no one asks about it.
                return;
            };
            if new == cur {
                return;
            }
            match word.compare_exchange_weak(cur, new, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return,
                Err(now) => cur = now,
            }
        }
    }

    /// A voice has told everything up to timeline frame `to`.
    pub fn heard(&self, to: u64) {
        self.heard.fetch_max(to, Ordering::Release);
    }

    /// Whether a voice speaks in timeline frames `from..to`, as far as the
    /// voices have told: past that nothing is known, so nothing is voiced.
    pub fn speaking(&self, from: u64, to: u64) -> bool {
        let to = to.min(self.heard.load(Ordering::Acquire));
        if to <= from {
            return false;
        }
        let w = WINDOW as u64;
        let last = (to - 1) / w;
        // Only the history: a word wider than it would be read twice.
        let first =
            (from / w).max((last / KEY_BITS).saturating_sub(KEY_WORDS as u64 - 1) * KEY_BITS);
        (first / KEY_BITS..=last / KEY_BITS).any(|k| {
            let cur = self.words[k as usize % KEY_WORDS].load(Ordering::Relaxed);
            if cur >> KEY_BITS != k & LOW32 {
                return false;
            }
            let base = k * KEY_BITS;
            let (lo, hi) = (first.max(base) - base, last.min(base + KEY_BITS - 1) - base);
            cur & (u64::MAX >> (63 - hi)) & (u64::MAX << lo) != 0
        })
    }

    /// Whether a track ducks its block of `frames` at timeline frame `at`,
    /// reading the voice `lookahead` frames past it: a voice spoke within
    /// [`VOICE_HOLD_FRAMES`] before the block's end, or speaks up to
    /// `lookahead` after it. Where the voices have not told that far, only
    /// what they told counts; a track ahead of them in real time (its
    /// output delay, a Bluetooth headset) past the block's end ducks as
    /// they speak at their last word, with its hold: the duck comes as late
    /// as they are, less the lookahead. A voice that stalls goes on telling
    /// (its silence fill).
    pub fn ducks(&self, at: u64, frames: usize, lookahead: usize) -> bool {
        let heard = self.heard.load(Ordering::Acquire);
        let end = at + frames as u64;
        let from = end.min(heard).saturating_sub(VOICE_HOLD_FRAMES as u64);
        self.speaking(from, end + lookahead as u64)
    }
}

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

/// Most an adaptive duck lowers, dB.
const MAX_DEPTH_DB: f32 = 36.0;
/// How fast an adaptive duck's depth follows the music and the voice,
/// dB/s: up fast enough for a louder section to be ducked within about
/// 200 ms, down slowly; both slower than the attack, so the duck never
/// jumps.
const DEPTH_UP: f32 = 40.0;
const DEPTH_RATE: f32 = 6.0;
/// The depth follows the track's momentary loudness (400 ms) when it is
/// this far over its short-term one (a louder section), its short-term
/// loudness otherwise: a beat does not move it.
const MOMENTARY_BLOCKS: usize = 4;
const MOMENTARY_SLACK_LU: f32 = 1.0;

/// Loudness blocks of 100 ms. The track's short-term loudness (the music
/// under the voice, for an adaptive duck) is over the last 3 s of them,
/// the leveler's over the last 20 s (the sections of a song average out:
/// it keeps the song's dynamics), gated at -10 LU under their mean as
/// BS.1770 does (a pause or a quiet bridge does not pull it down).
const BLOCK: usize = 4_800;
const SHORT_BLOCKS: usize = 30;
const BLOCKS: usize = 200;
const RELATIVE_GATE: f32 = 0.1;
/// Blocks quieter than this (LUFS) are left out: a pause is not leveled up.
const GATE_LUFS: f32 = -50.0;
/// Each of the last 300 ms this far over the leveler's loudness before
/// them (the loud blocks would pull it up, through its gate): a louder
/// song started, the leveler catches up at once. A song's own sections
/// stay well within (8 LU apart, a chorus is about 2.5 over the mean), and
/// a hit (a drum, a shot) is shorter.
const LOUD_BLOCKS: usize = 3;
const SONG_UP_LU: f32 = 6.0;
/// Until then, what is coming (per piece looked ahead, 20 ms) is held
/// at most this far over the target: brought down at `GUARD_ATTACK` dB/s,
/// a piece early, let go at `GUARD_RELEASE` dB/s. A beat is well under.
const GUARD_LU: f32 = 8.0;
const GUARD_ATTACK: f32 = 1_000.0;
const GUARD_RELEASE: f32 = 20.0;
/// Pieces looked ahead and not processed yet, at most (the lookahead is
/// 6 blocks of 20 ms).
const AHEAD: usize = 16;
/// Short-term loudness this far under the leveler's: a quieter song
/// started, the leveler measures it alone and rises fast.
const SONG_DOWN_LU: f32 = 10.0;
/// Most the leveler lowers and raises, dB.
const LEVEL_MIN_DB: f32 = -30.0;
const LEVEL_MAX_DB: f32 = 15.0;
/// Leveler speed, dB/s: down fast (a loud song starts), up slowly.
const LEVEL_DOWN: f32 = 10.0;
/// Down speed once music mode turned on or a louder song started, after
/// [`CATCH_UP_BLOCKS`] blocks measured, until the leveler reached its
/// target: a loud song is not left loud for seconds.
const LEVEL_CATCH_UP: f32 = 300.0;
const CATCH_UP_BLOCKS: usize = 3;
const LEVEL_UP: f32 = 2.0;
/// Up speed once a quieter song started, until the leveler reached it.
const LEVEL_RISE: f32 = 10.0;

/// The voice's loudness: K-weighted power of its voiced 10 ms windows
/// (above `VOICE_GATE_LUFS`) in blocks of 200 ms of speech, the last 50
/// (10 s of speech), gated at -10 LU under their mean. Told after 1.6 s
/// of speech.
const VOICE_BLOCK: usize = 20;
const VOICE_BLOCKS: usize = 50;
const VOICE_MIN_BLOCKS: usize = 8;
const VOICE_GATE_LUFS: f32 = -70.0;

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

/// K-weighted mean power (summed over the channels) at `lufs`.
fn lufs_power(lufs: f32) -> f32 {
    db_to_lin(lufs + 0.691).powi(2)
}

/// Loudness (LUFS) of mean powers `blocks` over the `gate` power, and if
/// `relative` also within 10 LU of their mean (BS.1770's two gates);
/// `None` when none is left.
fn gated(blocks: impl Iterator<Item = f32> + Clone, gate: f32, relative: bool) -> Option<f32> {
    let mean = |gate: f32| {
        let (sum, n) = blocks
            .clone()
            .filter(|&p| p > gate)
            .fold((0.0, 0), |(s, n), p| (s + p, n + 1));
        (n > 0).then(|| sum / n as f32)
    };
    let mut m = mean(gate)?;
    if relative {
        m = mean(gate.max(m * RELATIVE_GATE))?;
    }
    Some(-0.691 + 10.0 * m.log10())
}

/// BS.1770 K-weighting, per channel.
struct KWeight {
    c: [Coefs; 2],
    z: Vec<[State; 2]>,
}

impl KWeight {
    fn new(ch: usize) -> Self {
        Self {
            c: Coefs::k_weighting(),
            z: vec![[State::default(); 2]; ch],
        }
    }

    /// K-weighted energy of `buf` (interleaved, whole frames): the sum of
    /// its squares over frames and channels.
    fn energy(&mut self, buf: &[f32]) -> f64 {
        let [c0, c1] = self.c;
        let mut e = 0.0;
        for frame in buf.chunks_exact(self.z.len()) {
            for (&s, z) in frame.iter().zip(&mut self.z) {
                let x = z[0].run(&c0, f64::from(s));
                let y = z[1].run(&c1, x);
                e += y * y;
            }
        }
        e
    }

    /// Keeps the filters away from denormals (after silence).
    fn flush(&mut self) {
        self.z.iter_mut().flatten().for_each(State::flush);
    }
}

/// Measures a voice as recorded (after its filters and volume): its
/// loudness over its voiced windows (see `VOICE_BLOCK`), the reference an
/// adaptive duck and background music go under. Lock-free and
/// allocation-free after construction; the voice track runs it on each
/// block it records and tells the [`VoiceKey`].
pub struct VoiceMeter {
    kw: KWeight,
    /// Mean powers of voiced windows in the current block, and how many.
    power: f32,
    windows: usize,
    /// Mean power of the last [`VOICE_BLOCKS`] blocks, the next one, and
    /// how many there are.
    blocks: [f32; VOICE_BLOCKS],
    next: usize,
    count: usize,
}

impl VoiceMeter {
    pub fn new(ch: usize) -> Self {
        assert!(ch > 0, "no channel");
        Self {
            kw: KWeight::new(ch),
            power: 0.0,
            windows: 0,
            blocks: [0.0; VOICE_BLOCKS],
            next: 0,
            count: 0,
        }
    }

    /// Forgets the voice heard so far (a track that just became a voice).
    pub fn reset(&mut self) {
        (self.power, self.windows, self.next, self.count) = (0.0, 0, 0, 0);
    }

    /// Feeds `buf` (interleaved, whole frames), in windows of 10 ms from
    /// its start; `voiced(from, to)` tells whether the voice spoke in
    /// frames `from..to` of it. Returns the voice's loudness (LUFS) when
    /// it measured more of it.
    pub fn process(
        &mut self,
        buf: &[f32],
        mut voiced: impl FnMut(usize, usize) -> bool,
    ) -> Option<f32> {
        let ch = self.kw.z.len();
        let mut told = None;
        for (i, w) in buf.chunks(WINDOW * ch).enumerate() {
            let frames = w.len() / ch;
            let p = (self.kw.energy(w) / frames as f64) as f32;
            let from = i * WINDOW;
            if p <= lufs_power(VOICE_GATE_LUFS) || !voiced(from, from + frames) {
                continue;
            }
            self.power += p;
            self.windows += 1;
            if self.windows < VOICE_BLOCK {
                continue;
            }
            self.blocks[self.next] = self.power / self.windows as f32;
            self.next = (self.next + 1) % VOICE_BLOCKS;
            self.count = (self.count + 1).min(VOICE_BLOCKS);
            (self.power, self.windows) = (0.0, 0);
            if self.count >= VOICE_MIN_BLOCKS {
                let blocks = self.blocks[..self.count].iter().copied();
                told = gated(blocks, 0.0, true).or(told);
            }
        }
        self.kw.flush();
        told
    }
}

/// Tells when a voice track speaks, from its raw input (before its filters,
/// volume and mute): RNNoise's voice probability per 10 ms window (its own
/// state, not the track's noise suppression), backed by the window's level
/// over a noise floor tracked by minimum statistics. Digital silence, and
/// a muted microphone (not calling it, then [`VoiceDetector::pause`]), end
/// any phrase and keep the floor and RNNoise's state.
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
        self.process_windows(buf, |end, voiced| {
            if voiced {
                last = Some(end);
            }
        });
        last
    }

    /// Feeds `buf` (interleaved, whole frames); `on_window` gets each
    /// window that ends in it: where it ends (frames from `buf`'s start),
    /// and whether the voice speaks there.
    pub fn process_windows(&mut self, buf: &[f32], mut on_window: impl FnMut(usize, bool)) {
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
            if self.frames == WINDOW {
                let voiced = self.window();
                on_window(at, voiced);
            }
        }
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
        let crossings = self
            .input
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0));
        let zcr = crossings.count() as f32 / WINDOW as f32;
        let strict = p > P_VOICE && over > MARGIN_DB && kept > KEPT_DB && zcr < ZCR_MAX;
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
    /// With `gap_lu`, only until the voice's loudness is known.
    pub duck_db: f32,
    /// While the voice speaks, lowered so it sits this far (LU) under the
    /// voice (as loud as both are recorded), once the voice's loudness is
    /// known; 0: by `duck_db` (or [`MUSIC_DUCK_DB`]) whatever the levels.
    pub gap_lu: f32,
    /// Background music, leveled to this short-term loudness (LUFS) until
    /// the voice's loudness is known; `None`: off. It also ducks, by
    /// [`MUSIC_DUCK_DB`] unless `duck_db` is set.
    pub music_lufs: Option<f32>,
    /// Background music, leveled this far (LU) under the voice once its
    /// loudness is known; 0: to `music_lufs` whatever the voice.
    pub music_under_lu: f32,
}

impl DuckSettings {
    /// A track lowered at level `duck` while the voice speaks, played as
    /// background music at level `music`, or both (levels 0, 1, 2: low,
    /// medium, high). The fixed depths and absolute loudness are the
    /// fallbacks until the voice is measured; with the voice at -18 LUFS
    /// (a mic at about -20 dBFS RMS) the music sits where they put it.
    pub fn at_levels(duck: Option<usize>, music: Option<usize>) -> Self {
        let under = music.map(|l| [12.0, 8.0, 4.0][l]);
        Self {
            duck_db: duck.map_or(0.0, |l| [6.0, 12.0, 20.0][l]),
            // Music only: as far under as its level, plus its soft duck.
            gap_lu: match (duck, under) {
                (Some(l), _) => [8.0, 12.0, 16.0][l],
                (None, Some(u)) => u + MUSIC_DUCK_DB,
                (None, None) => 0.0,
            },
            music_lufs: music.map(|l| [-30.0, -26.0, -22.0][l]),
            music_under_lu: under.unwrap_or(0.0),
        }
    }
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

/// What a [`Ducker`] makes of its settings and the voice.
struct Plan {
    /// Fixed depth, dB (0: no duck).
    fixed: f32,
    /// The loudness the track goes under while the voice speaks (adaptive).
    under: Option<f32>,
    /// Music mode: the loudness it is leveled to.
    target: Option<f32>,
}

impl Plan {
    fn new(settings: DuckSettings, voice: Option<f32>) -> Self {
        let fixed = if settings.duck_db > 0.0 {
            settings.duck_db
        } else if settings.music_lufs.is_some() {
            MUSIC_DUCK_DB
        } else {
            0.0
        };
        Self {
            fixed,
            under: voice
                .filter(|_| fixed > 0.0 && settings.gap_lu > 0.0)
                .map(|v| v - settings.gap_lu),
            target: settings.music_lufs.map(|lufs| match voice {
                Some(v) if settings.music_under_lu > 0.0 => v - settings.music_under_lu,
                _ => lufs,
            }),
        }
    }

    fn measuring(&self) -> bool {
        self.target.is_some() || self.under.is_some()
    }
}

/// Ducking and background music of a non-voice track (see the module docs).
pub struct Ducker {
    ch: usize,
    /// Presence dip gain, dB: 0 when off.
    dip: Glide,
    dip_c: Coefs,
    dip_z: Vec<State>,
    kw: KWeight,
    /// K-weighted power of the current 100 ms block (summed over the
    /// channels), and its frames.
    power: f64,
    frames: usize,
    /// Mean power of the last [`BLOCKS`] blocks, the next one, and how
    /// many of them the leveler reads (since it started over).
    blocks: [f32; BLOCKS],
    next: usize,
    count: usize,
    /// Measuring in the last block (music mode, or an adaptive duck):
    /// turned on, the measurement starts over.
    measuring: bool,
    /// Momentary (400 ms), short-term (3 s) loudness of the track, and the
    /// leveler's (20 s, gated), LUFS.
    momentary: Option<f32>,
    short: Option<f32>,
    long: Option<f32>,
    /// Pieces measured by [`Ducker::look_ahead`] and not processed yet,
    /// oldest first from `ahead_at`: their frames and loudness (LUFS).
    ahead: [(usize, f32); AHEAD],
    ahead_at: usize,
    ahead_len: usize,
    /// Blocks measured since music mode turned on or a louder song
    /// started, while it catches up.
    catch_up: Option<usize>,
    /// A quieter song started: the leveler rises fast until it is there.
    rising: bool,
    /// Gain the leveler goes to, dB.
    level_target: f32,
    /// Gains, dB: leveler, compressor, duck.
    level: f32,
    comp: f32,
    duck: f32,
    /// Most the leveler's gain may be now, dB: a much louder piece coming
    /// is held [`GUARD_LU`] over the target until the leveler catches up.
    cap: f32,
    /// Depth of the duck, dB (it glides when adaptive).
    depth: f32,
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
            kw: KWeight::new(ch),
            power: 0.0,
            frames: 0,
            blocks: [0.0; BLOCKS],
            next: 0,
            count: 0,
            measuring: false,
            momentary: None,
            short: None,
            long: None,
            ahead: [(0, 0.0); AHEAD],
            ahead_at: 0,
            ahead_len: 0,
            catch_up: None,
            rising: false,
            level_target: 0.0,
            level: 0.0,
            comp: 0.0,
            duck: 0.0,
            cap: LEVEL_MAX_DB,
            depth: 0.0,
            gain: 1.0,
        }
    }

    /// How much the duck lowers the track now, dB (>= 0). For a meter.
    pub fn ducking_db(&self) -> f32 {
        0.0 - self.duck
    }

    /// The leveler's gain now, dB (its cap included).
    fn leveled(&self) -> f32 {
        self.level.min(self.cap)
    }

    /// Nothing to do: every gain at 0 dB, no dip.
    fn idle(&self) -> bool {
        self.gain == 1.0
            && self.leveled() == 0.0
            && self.comp == 0.0
            && self.duck == 0.0
            && self.dip.settled()
            && self.dip.cur == 0.0
    }

    /// Measures `buf` (interleaved, whole frames) before it is processed:
    /// a track whose audio waits [`LOOKAHEAD_FRAMES`] (or any delay) gives
    /// each block here as it comes in, then to [`Ducker::process`] when it
    /// comes out, so the leveler and the duck see a louder song or section
    /// before they play it. Without it, `process` measures each block just
    /// before it. Real-time safe.
    pub fn look_ahead(&mut self, buf: &[f32], settings: DuckSettings, voice: Option<f32>) {
        let plan = Plan::new(settings, voice);
        let measuring = plan.measuring();
        if measuring && !self.measuring {
            // A new measurement: the old one may be of other music.
            (self.power, self.frames, self.count) = (0.0, 0, 0);
            (self.momentary, self.short, self.long) = (None, None, None);
            self.catch_up = plan.target.map(|_| 0);
            self.ahead_len = 0;
        }
        self.measuring = measuring;
        if !measuring {
            return;
        }
        let ch = self.ch;
        let energy: f64 = buf
            .chunks(CHUNK * ch)
            .map(|chunk| self.measure(chunk, plan.target))
            .sum();
        self.kw.flush();
        let frames = buf.len() / ch;
        let lufs = -0.691 + 10.0 * ((energy / frames.max(1) as f64) as f32).log10();
        if self.ahead_len == AHEAD {
            // Out of step (a delay that changed): the oldest is dropped.
            self.ahead_at = (self.ahead_at + 1) % AHEAD;
            self.ahead_len -= 1;
        }
        self.ahead[(self.ahead_at + self.ahead_len) % AHEAD] = (frames, lufs);
        self.ahead_len += 1;
    }

    /// The loudest of the pieces measured for the next `frames` and the
    /// one after them (the guard comes down a piece early), taken off.
    fn take_ahead(&mut self, frames: usize) -> f32 {
        let mut loudest = f32::NEG_INFINITY;
        let mut need = frames;
        while need > 0 && self.ahead_len > 0 {
            let (n, lufs) = self.ahead[self.ahead_at];
            loudest = loudest.max(lufs);
            need = need.saturating_sub(n);
            self.ahead_at = (self.ahead_at + 1) % AHEAD;
            self.ahead_len -= 1;
        }
        if self.ahead_len > 0 {
            loudest = loudest.max(self.ahead[self.ahead_at].1);
        }
        loudest
    }

    /// Processes `buf` (interleaved, whole frames) with `settings`; the
    /// voice speaks during it or not, and its loudness is `voice` (LUFS,
    /// see [`VoiceKey::loudness`]) once known. Real-time safe.
    pub fn process(
        &mut self,
        buf: &mut [f32],
        settings: DuckSettings,
        speaking: bool,
        voice: Option<f32>,
    ) {
        let plan = Plan::new(settings, voice);
        let measuring = plan.measuring();
        if !measuring && !(speaking && plan.fixed > 0.0) && self.idle() {
            self.measuring = false;
            return;
        }
        if !measuring || self.ahead_len == 0 {
            // Not looked ahead: measured now.
            self.look_ahead(buf, settings, voice);
        }
        let target = plan.target;
        if target.is_none() {
            (self.level_target, self.catch_up, self.rising) = (0.0, None, false);
        }
        let ch = self.ch;
        let loudest = if measuring {
            self.take_ahead(buf.len() / ch)
        } else {
            f32::NEG_INFINITY
        };
        // Music mode: what is coming is held under the target plus
        // `GUARD_LU` (a much louder song, before the leveler catches up).
        let cap = target.map_or(LEVEL_MAX_DB, |t| (t + GUARD_LU - loudest).min(LEVEL_MAX_DB));
        self.dip.target = if target.is_some() { DIP_DB } else { 0.0 };
        let per_chunk = CHUNK as f32 / FS;
        let (comp_attack, comp_release) = (
            coef(COMP_ATTACK_MS / CHUNK as f32),
            coef(COMP_RELEASE_MS / CHUNK as f32),
        );
        for chunk in buf.chunks_mut(CHUNK * ch) {
            if !self.dip.settled() {
                self.dip_c = Coefs::peaking(DIP_HZ, DIP_Q, self.dip.step());
            }
            let peak = if measuring {
                chunk.iter().fold(0f32, |m, s| m.max(s.abs()))
            } else {
                0.0
            };
            if self.dip.cur != 0.0 {
                for frame in chunk.chunks_exact_mut(ch) {
                    for (s, z) in frame.iter_mut().zip(&mut self.dip_z) {
                        *s = z.run(&self.dip_c, f64::from(*s)) as f32;
                    }
                }
            }
            // Out of music mode the leveler goes back fast.
            let up = match target {
                None => LEVEL_DOWN,
                Some(_) if self.rising => LEVEL_RISE,
                Some(_) => LEVEL_UP,
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
            if self.rising && self.level >= self.level_target {
                self.rising = false;
            }
            self.cap = slew(
                self.cap,
                cap,
                GUARD_ATTACK * per_chunk,
                GUARD_RELEASE * per_chunk,
            );
            let level = self.leveled();
            let gc = match target {
                Some(lufs) => curve(lin_to_db(peak) + level, lufs + COMP_ABOVE),
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
            // Adaptive: as deep as it takes for the music (as leveled) to
            // sit `gap_lu` under the voice: deeper at once for a louder
            // section (its momentary loudness), back up slowly; until both
            // are measured, and otherwise, the fixed depth.
            match (plan.under, self.short) {
                (Some(under), Some(short)) => {
                    let now = self
                        .momentary
                        .map_or(short, |m| short.max(m - MOMENTARY_SLACK_LU));
                    let want = (now + level - under).clamp(0.0, MAX_DEPTH_DB);
                    self.depth = slew(
                        self.depth,
                        want,
                        DEPTH_RATE * per_chunk,
                        DEPTH_UP * per_chunk,
                    );
                }
                (Some(_), None) => {
                    let step = DEPTH_RATE * per_chunk;
                    self.depth = slew(self.depth, plan.fixed, step, step);
                }
                (None, _) => self.depth = plan.fixed,
            }
            let rate = self.depth.max(MIN_RATE_DEPTH) * 1e3 * per_chunk;
            let duck_target = if speaking { -self.depth } else { 0.0 };
            self.duck = slew(self.duck, duck_target, rate / ATTACK_MS, rate / RELEASE_MS);
            let to = db_to_lin(level + self.comp + self.duck);
            self.ramp(chunk, to);
        }
        self.dip_z.iter_mut().for_each(State::flush);
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

    /// The last `n` blocks' mean powers (of those measured).
    fn last(&self, n: usize) -> impl Iterator<Item = f32> + Clone {
        let n = n.min(self.count);
        (1..=n).map(move |i| self.blocks[(self.next + BLOCKS - i) % BLOCKS])
    }

    /// Adds `chunk` to the loudness measurement, and returns its K-weighted
    /// energy; at the end of each 100 ms block, updates the loudness and,
    /// in music mode (to `target` LUFS), the leveler's target.
    fn measure(&mut self, chunk: &[f32], target: Option<f32>) -> f64 {
        let energy = self.kw.energy(chunk);
        self.power += energy;
        self.frames += chunk.len() / self.ch;
        if self.frames < BLOCK {
            return energy;
        }
        self.blocks[self.next] = (self.power / self.frames as f64) as f32;
        self.next = (self.next + 1) % BLOCKS;
        self.count = (self.count + 1).min(BLOCKS);
        (self.power, self.frames) = (0.0, 0);
        if let Some(n) = &mut self.catch_up {
            *n += 1;
        }
        let gate = lufs_power(GATE_LUFS);
        self.momentary = gated(self.last(MOMENTARY_BLOCKS), gate, false);
        self.short = gated(self.last(SHORT_BLOCKS), gate, false);
        if target.is_some()
            && let Some(long) = self.long
        {
            let before = gated(self.last(BLOCKS).skip(LOUD_BLOCKS), gate, true);
            let loud = before.map(|b| lufs_power(b + SONG_UP_LU));
            if let Some(loud) = loud
                && self.last(LOUD_BLOCKS).all(|p| p > loud)
            {
                // A louder song: measured alone, caught up with at once.
                self.count = LOUD_BLOCKS;
                self.catch_up = Some(CATCH_UP_BLOCKS);
                self.rising = false;
            } else if self.count >= SHORT_BLOCKS
                && self.short.is_some_and(|s| s < long - SONG_DOWN_LU)
            {
                // A quieter one, for 3 s: measured alone (its blocks
                // since the louder one), risen to fast.
                let quiet = lufs_power(long - SONG_DOWN_LU);
                self.count = self.last(SHORT_BLOCKS).take_while(|&p| p < quiet).count();
                self.rising = true;
            }
        }
        self.long = gated(self.last(BLOCKS), gate, true);
        if let (Some(lufs), Some(long)) = (target, self.long) {
            self.level_target = (lufs - long).clamp(LEVEL_MIN_DB, LEVEL_MAX_DB);
        }
        energy
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
    fn noise_bursts_are_not_voice() {
        // 400 ms of white noise every 600 ms, 25 dB over a -55 dB floor,
        // the same on both channels (an upmixed mono mic): loud, but not a
        // voice (RNNoise alone takes them after a few seconds).
        let mono: Vec<f32> = speech(6_000, 400, 600, -30.0, -55.0)
            .chunks(CH)
            .flat_map(|f| [f[0]; CH])
            .collect();
        let voiced = detect(&mono);
        let s: String = voiced.iter().map(|v| if *v { '1' } else { '.' }).collect();
        assert!(voiced.iter().all(|v| !v), "{s}");
    }

    #[test]
    fn digital_silence_ends_the_phrase() {
        // A phrase in progress over a -55 dB room (the bridge armed), 200 ms
        // of digital zeros, then a noise 13 dB over the floor: the bridge
        // must not take the noise for voice.
        let mut d = VoiceDetector::new(CH);
        let room = speech(3_000, 0, 1_000, -20.0, -55.0);
        room.chunks(BLK * CH).for_each(|b| _ = d.process(b));
        (d.speaking, d.quiet) = (true, 0);
        for _ in 0..10 {
            d.process(&[0.0; BLK * CH]);
        }
        let noise = speech(1_000, 0, 1_000, -20.0, -42.0);
        assert!(noise.chunks(BLK * CH).all(|b| d.process(b).is_none()));
        assert!(d.floor < -54.0, "{}", d.floor);
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
            d.process(b, settings, speaking(i), None);
        }
        d
    }

    fn duck_only(db: f32) -> DuckSettings {
        DuckSettings {
            duck_db: db,
            ..DuckSettings::default()
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
            d.process(b, s, true, None);
        }
        assert!(d.idle());
        assert_eq!(*buf.last().unwrap(), 1.0);
    }

    fn music(lufs: f32) -> DuckSettings {
        DuckSettings {
            duck_db: 0.0,
            music_lufs: Some(lufs),
            ..DuckSettings::default()
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
    fn leveler_brings_loud_and_quiet_music_to_the_target() {
        // White noise music at about -6 LUFS, 12 s: leveled to the target
        // (measured before the dip, so the gain is target minus input).
        let input = noise(12 * 48_000 * CH, 0.5, 4);
        let mut buf = input.clone();
        let d = duck(&mut buf, music(-26.0), |_| false);
        let want = -26.0 - loudness(&input[9 * 48_000 * CH..]);
        assert!((d.level - want).abs() < 0.5, "{} dB, want {want}", d.level);
        assert_eq!(d.ducking_db(), 0.0);
        // About -39 LUFS: raised to it too (2 dB/s).
        let input = noise(12 * 48_000 * CH, 0.01, 4);
        let mut buf = input.clone();
        let d = duck(&mut buf, music(-26.0), |_| false);
        let want = -26.0 - loudness(&input[9 * 48_000 * CH..]);
        assert!((d.level - want).abs() < 0.5, "{} dB, want {want}", d.level);
        // About -46 LUFS: raised by the most the leveler does.
        let mut buf = noise(20 * 48_000 * CH, 0.004, 4);
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
            d.process(b, music(-26.0), false, None);
        }
        assert!((d.level - want).abs() < 2.0, "{} dB, want {want}", d.level);
        assert!(d.catch_up.is_none());
    }

    #[test]
    fn music_peaks_are_compressed() {
        // A bed with a hit 26 dB louder, 100 ms every 6 s (the bed within
        // the leveler's relative gate): the hits come out lower against
        // the bed than they went in.
        let input: Vec<f32> = noise(14 * 48_000 * CH, 1.0, 8)
            .into_iter()
            .enumerate()
            .map(|(i, s)| {
                s * if (i / CH) % 288_000 < 4_800 {
                    1.0
                } else {
                    0.05
                }
            })
            .collect();
        let mut buf = input.clone();
        duck(&mut buf, music(-26.0), |_| false);
        let ratio = |b: &[f32]| {
            let hit = &b[(12 * 48_000 + 960) * CH..(12 * 48_000 + 4_800) * CH];
            let bed = &b[(13 * 48_000) * CH..(14 * 48_000) * CH];
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
        d.process(&mut buf, music(-26.0), false, None);
        assert!(d.dip.cur < 0.0 && d.dip.cur > DIP_DB);
        for _ in 0..100 {
            d.process(&mut buf, music(-26.0), false, None);
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
                    ..DuckSettings::default()
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
    fn the_voice_meter_measures_only_the_voiced_windows() {
        // 1 s on, 1 s off: voiced noise at about -20 LUFS, louder
        // unvoiced noise between, digital silence in a voiced window.
        let voiced_part = noise(48_000 * CH, 0.1, 6);
        let want = loudness(&voiced_part);
        let mut m = VoiceMeter::new(CH);
        let loud = noise(48_000 * CH, 0.5, 7);
        let mut told = Vec::new();
        for sec in 0..8 {
            let (buf, voiced) = if sec % 2 == 0 {
                (&voiced_part, true)
            } else {
                (&loud, false)
            };
            for b in buf.chunks(BLK * CH) {
                told.extend(m.process(b, |_, _| voiced));
            }
            // Voiced silence: under the gate, not counted.
            assert_eq!(m.process(&[0.0; BLK * CH], |_, _| true), None);
        }
        // 1.6 s of voice before it tells, then every 200 ms of it.
        assert_eq!(told.len(), 4 * 5 - (VOICE_MIN_BLOCKS - 1));
        let last = *told.last().unwrap();
        assert!((last - want).abs() < 0.3, "{last} LUFS, want {want}");
        m.reset();
        assert_eq!(m.process(&voiced_part[..BLK * CH], |_, _| true), None);
    }

    #[test]
    fn the_key_holds_the_last_voice_loudness_while_a_voice_listens() {
        let key = VoiceKey::default();
        assert_eq!(key.loudness(), None);
        key.add_voice();
        key.add_voice();
        key.set_loudness(-20.0);
        key.set_loudness(-30.0);
        assert_eq!(key.loudness(), Some(-30.0));
        key.remove_voice();
        assert_eq!(key.loudness(), Some(-30.0));
        key.remove_voice();
        assert_eq!(key.loudness(), None);
    }

    /// Runs a ducker over `buf` in 20 ms blocks, the voice speaking from
    /// 4 s on at `voice` LUFS.
    fn duck_under(buf: &mut [f32], settings: DuckSettings, voice: Option<f32>) -> Ducker {
        let mut d = Ducker::new(CH);
        for (i, b) in buf.chunks_mut(BLK * CH).enumerate() {
            d.process(b, settings, i >= 200, voice);
        }
        d
    }

    #[test]
    fn an_adaptive_duck_puts_the_track_its_gap_under_the_voice() {
        // A -18.6 LUFS track: under a -20 LUFS voice by 12 LU, 13.4 dB
        // down; under a -30 LUFS one, 23.4 dB down; the fixed 12 dB until the
        // voice is measured.
        let input = noise(8 * 48_000 * CH, 0.1, 4);
        let track = loudness(&input);
        let set = DuckSettings::at_levels(Some(1), None);
        for (voice, want) in [
            (Some(-20.0), track + 20.0 + 12.0),
            (Some(-30.0), track + 30.0 + 12.0),
            (None, 12.0),
        ] {
            let mut buf = input.clone();
            let d = duck_under(&mut buf, set, voice);
            let got = d.ducking_db();
            assert!((got - want).abs() < 0.2, "{voice:?}: {got} dB, want {want}");
        }
        // As deep as it goes, no deeper.
        let mut buf = input.clone();
        let d = duck_under(&mut buf, set, Some(-60.0));
        assert_eq!(d.ducking_db(), MAX_DEPTH_DB);
        // It glides there: no step larger than the fixed duck's.
        let mut buf = vec![1.0; 8 * 48_000 * CH];
        let mut d = Ducker::new(CH);
        for (i, b) in buf.chunks_mut(BLK * CH).enumerate() {
            // Speaking all along; the voice is measured at 2 s.
            let voice = (i >= 100).then_some(-40.0);
            d.process(b, set, true, voice);
        }
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
    fn background_music_is_leveled_under_the_voice() {
        // Medium: 8 LU under the voice, absolute -26 LUFS until measured.
        let input = noise(12 * 48_000 * CH, 0.1, 4);
        let track = loudness(&input[9 * 48_000 * CH..]);
        let set = DuckSettings::at_levels(None, Some(1));
        for (voice, want) in [(Some(-16.0), -24.0), (Some(-30.0), -38.0), (None, -26.0)] {
            let mut buf = input.clone();
            let mut d = Ducker::new(CH);
            for b in buf.chunks_mut(BLK * CH) {
                d.process(b, set, false, voice);
            }
            let got = track + d.level;
            assert!(
                (got - want).abs() < 0.5,
                "{voice:?}: {got} LUFS, want {want}"
            );
        }
    }

    #[test]
    fn no_nan_or_denormals_after_silence() {
        let mut buf = noise(48_000 * CH, 0.5, 1);
        buf.extend(vec![0.0; 10 * 48_000 * CH]);
        let d = duck(&mut buf, music(-26.0), |b| b % 7 < 3);
        assert!(buf.iter().all(|s| s.is_finite()));
        let tiny = |x: f64| x != 0.0 && x.abs() < 1e-30;
        for z in d.dip_z.iter().chain(d.kw.z.iter().flatten()) {
            let (a, b) = z.raw();
            assert!(!tiny(a) && !tiny(b));
        }
    }

    /// A key with windows 100..110 voiced (frames 48 000..52 800).
    fn key_with_a_word() -> VoiceKey {
        let key = VoiceKey::default();
        for w in 100..110 {
            key.voiced((w + 1) * WINDOW as u64);
        }
        key
    }

    #[test]
    fn the_key_tells_voiced_windows_on_the_timeline() {
        let key = key_with_a_word();
        assert!(!key.speaking(48_000, 48_001), "not told yet");
        key.heard(60_000);
        assert!(key.speaking(48_000, 48_001));
        assert!(key.speaking(52_799, 52_800));
        assert!(!key.speaking(52_800, 60_000));
        assert!(!key.speaking(0, 48_000));
        // A second voice: windows merge.
        key.voiced(56_160);
        assert!(key.speaking(55_680, 56_160));
        assert!(key.speaking(0, 60_000));
    }

    #[test]
    fn a_ducked_block_reads_the_hold_before_and_the_lookahead_after() {
        let key = key_with_a_word();
        key.heard(200_000);
        let ducks = |end: u64| key.ducks(end - 960, 960, LOOKAHEAD_FRAMES);
        let look = LOOKAHEAD_FRAMES as u64;
        assert!(!ducks(48_000 - look));
        assert!(ducks(48_000 - look + 1));
        let hold = VOICE_HOLD_FRAMES as u64;
        assert!(ducks(52_800 + hold - 1));
        assert!(!ducks(52_800 + hold));
        // Without the lookahead: from the word's first frame.
        assert!(!key.ducks(48_000 - 960, 960, 0));
        assert!(key.ducks(48_000 - 959, 960, 0));
    }

    #[test]
    fn ahead_of_the_voices_their_last_word_holds() {
        let key = key_with_a_word();
        key.heard(52_800);
        // Far ahead: as the voice speaks at its last word.
        assert!(key.ducks(100_000, 960, LOOKAHEAD_FRAMES));
        // Its hold runs out as it tells more silence.
        key.heard(52_800 + VOICE_HOLD_FRAMES as u64);
        assert!(!key.ducks(100_000, 960, LOOKAHEAD_FRAMES));
    }

    #[test]
    fn the_key_forgets_words_older_than_its_history() {
        let key = key_with_a_word();
        // The same word slot, 15.36 s later.
        let later = (KEY_WORDS as u64 * KEY_BITS + 100) * WINDOW as u64;
        key.heard(later + 10 * WINDOW as u64);
        assert!(
            !key.speaking(later, later + 480),
            "an old word is not read as new"
        );
        key.voiced(later + WINDOW as u64);
        assert!(key.speaking(later, later + 480));
        assert!(!key.speaking(48_000, 52_800), "forgotten");
        // A voice too late for the history changes nothing.
        key.voiced(48_480);
        assert!(!key.speaking(48_000, 48_480));
        assert!(key.speaking(later, later + 480));
    }

    #[test]
    fn the_key_works_at_any_uptime() {
        // Timeline frames count from boot: 5 h up, and word counts around
        // where 16- and 32-bit tags would flip sign.
        let w = WINDOW as u64;
        let five_hours = 5 * 3_600 * 48_000 / (w * KEY_BITS);
        for words in [
            five_hours,
            (1 << 15) - 1,
            1 << 15,
            (1 << 15) + 7,
            (1 << 16) + 3,
            (1 << 31) - 1,
            1 << 31,
            (1 << 31) + 5,
        ] {
            let key = VoiceKey::default();
            let base = words * KEY_BITS * w;
            for i in 100..110 {
                key.voiced(base + (i + 1) * w);
            }
            key.heard(base + 200 * w);
            assert!(key.speaking(base + 100 * w, base + 101 * w), "{words}");
            assert!(!key.speaking(base + 110 * w, base + 200 * w), "{words}");
            assert!(key.ducks(base + 100 * w, 960, LOOKAHEAD_FRAMES), "{words}");
            // The same slot, idle for 1000 turns of the ring, then written.
            let later = base + (KEY_WORDS as u64 * KEY_BITS * 1_000 + 100) * w;
            key.heard(later + 10 * w);
            assert!(!key.speaking(later, later + w), "{words}: stale word");
            key.voiced(later + w);
            assert!(key.speaking(later, later + w), "{words}");
            assert!(!key.speaking(later + w, later + 10 * w), "{words}");
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
            d.process(b, duck_only(12.0), false, None)
        });
        let mut d = Ducker::new(CH);
        time("ducker, ducking", &mut |b, i| {
            d.process(b, duck_only(12.0), i % 100 < 50, None)
        });
        let mut d = Ducker::new(CH);
        time("background music", &mut |b, i| {
            d.process(b, music(-26.0), i % 100 < 50, None)
        });
    }
}

#[cfg(test)]
#[path = "duck_bench.rs"]
mod bench;
