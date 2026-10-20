//! Real-time audio filters for Vesper's tracks: noise suppression (RNNoise),
//! expander, compressor, de-esser, limiter, high-pass and gain; and voice
//! ducking with a background music mode ([`VoiceDetector`], [`Ducker`]).
//!
//! Every filter works on interleaved `f32` blocks at [`SAMPLE_RATE`], with a
//! channel count fixed at construction. [`Filter::process`] never allocates,
//! locks or makes a syscall: buffers are allocated by the constructors, and
//! [`Chain::warm_up`] sets up the per-thread FFT caches of RNNoise once on
//! the audio thread.
//!
//! A track's filters are described by [`FilterSpec`]s (plain data, with a
//! `name:key=value,...` text form for the settings file) and run by a
//! [`Chain`]. Parameter changes go through the [`ChainControl`] returned with
//! the chain: it publishes the new values in atomics, the audio thread picks
//! them up at the next block and each filter glides to them (no zipper noise).
//! A change of structure (filters added, removed, reordered, or a limiter
//! lookahead change, which changes the latency) needs a new [`Chain`]: build
//! it off the audio thread, hand it over, and drop the old one off the audio
//! thread too: [`live`] does both, with a lock-free handover.
//!
//! [`FilterSpec::Vst3`] filters are built by another crate (they talk to a
//! plugin host process): it registers its builder with [`set_external`].

mod biquad;
mod denoise;
mod duck;
mod dynamics;
mod limiter;
mod live;
mod spec;

use std::cell::Cell;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

pub use duck::{
    DuckSettings, Ducker, LOOKAHEAD_FRAMES, MUSIC_DUCK_DB, VOICE_HOLD_FRAMES, VoiceDetector,
    VoiceKey,
};
pub use live::{LiveChain, LiveControl, live};
pub use spec::{FilterSpec, MAX_PARAMS, ParseError};

/// Sample rate of every filter (RNNoise only runs at 48 kHz).
pub const SAMPLE_RATE: u32 = 48_000;
const FS: f32 = SAMPLE_RATE as f32;

/// An audio filter: a box with parameters that processes blocks.
pub trait Filter {
    /// Filters `buf` in place: interleaved samples, a whole number of frames
    /// of the channel count given at construction. Real-time safe.
    fn process(&mut self, buf: &mut [f32]);
    /// Clears the state (delay lines, envelopes) between two streams. Not
    /// meant for the audio callback: it may allocate.
    fn reset(&mut self);
    /// Delay added to the signal, in frames. Built-in filters keep it for
    /// their lifetime; a hosted plugin may change it between two blocks, so
    /// it is read again after every block.
    fn latency(&self) -> usize;
    /// Sets parameter `index` (in [`FilterSpec::param_names`] order). The
    /// filter glides to it over a few ms. Real-time safe; parameters that
    /// would change the latency are ignored.
    fn set_param(&mut self, index: usize, value: f32);
    /// Gain reduction at the end of the last block, dB (0 for filters
    /// that do not reduce dynamics). For meters.
    fn reduction_db(&self) -> f32 {
        0.0
    }
}

/// Builds the filter of a [`FilterSpec::Vst3`]: its id and channel count.
/// None: a pass-through instead.
pub type ExternalBuilder = fn(u32, usize) -> Option<Box<dyn Filter + Send>>;

static EXTERNAL: OnceLock<ExternalBuilder> = OnceLock::new();

/// Sets the builder of [`FilterSpec::Vst3`] filters, once per process.
/// Returns false if one was already set. Without one, they pass the audio
/// through unchanged.
pub fn set_external(build: ExternalBuilder) -> bool {
    EXTERNAL.set(build).is_ok()
}

fn external(id: u32, channels: usize) -> Box<dyn Filter + Send> {
    EXTERNAL
        .get()
        .and_then(|build| build(id, channels))
        .unwrap_or_else(|| Box::new(PassThrough))
}

thread_local! {
    static BLOCK_START: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// When the [`Chain`] block running on this thread started; None outside
/// [`Chain::process`]. Lets hosted plugins share one time budget per block
/// instead of each taking its own.
pub fn block_start() -> Option<Instant> {
    BLOCK_START.with(Cell::get)
}

/// A filter that changes nothing.
struct PassThrough;

impl Filter for PassThrough {
    fn process(&mut self, _: &mut [f32]) {}
    fn reset(&mut self) {}
    fn latency(&self) -> usize {
        0
    }
    fn set_param(&mut self, _: usize, _: f32) {}
}

/// Parameter values published by a [`ChainControl`] for one filter.
#[derive(Default)]
struct Params {
    /// Bit `i` set: `values[i]` changed since the audio thread last looked.
    dirty: AtomicU32,
    /// `f32` bits.
    values: [AtomicU32; MAX_PARAMS],
    /// `f32` bits of the filter's last [`Filter::reduction_db`].
    reduction: AtomicU32,
}

/// An ordered list of filters, run on the audio thread.
pub struct Chain {
    filters: Vec<Box<dyn Filter + Send>>,
    params: Vec<Arc<Params>>,
    channels: usize,
}

impl Chain {
    /// Builds the filters of `specs` (out-of-range values are clamped) for
    /// `channels` interleaved channels. The [`ChainControl`] changes their
    /// parameters from another thread.
    pub fn new(specs: &[FilterSpec], channels: usize) -> (Self, ChainControl) {
        assert!(channels > 0, "no channel");
        let specs: Vec<_> = specs.iter().map(|s| s.clamped()).collect();
        let params: Vec<Arc<Params>> = specs.iter().map(|_| Arc::default()).collect();
        let chain = Self {
            filters: specs.iter().map(|s| s.build(channels)).collect(),
            params: params.clone(),
            channels,
        };
        (chain, ChainControl { specs, params })
    }

    /// Runs every filter in order on `buf` (see [`Filter::process`]),
    /// applying the parameter changes published since the last block first.
    /// Non-finite input samples become 0: a NaN would stick in the filters'
    /// states until a reset.
    pub fn process(&mut self, buf: &mut [f32]) {
        let outer = BLOCK_START.replace(Some(Instant::now()));
        for s in buf.iter_mut().filter(|s| !s.is_finite()) {
            *s = 0.0;
        }
        for (filter, params) in self.filters.iter_mut().zip(&self.params) {
            let dirty = params.dirty.swap(0, Ordering::Acquire);
            if dirty != 0 {
                for (i, value) in params.values.iter().enumerate() {
                    if dirty & (1 << i) != 0 {
                        filter.set_param(i, f32::from_bits(value.load(Ordering::Relaxed)));
                    }
                }
            }
            filter.process(buf);
            let reduction = filter.reduction_db().max(0.0);
            params
                .reduction
                .store(reduction.to_bits(), Ordering::Relaxed);
        }
        BLOCK_START.set(outer);
    }

    /// Prepares the calling thread to run the chain: call it once on the
    /// audio thread before real-time use. nnnoiseless's FFT (easyfft) keeps
    /// its planners and scratch buffers per thread and allocates them at the
    /// first frame a thread processes. Runs 10 ms of silence through the
    /// chain, then resets it; allocates.
    pub fn warm_up(&mut self) {
        let mut silence = vec![0.0; denoise::FRAME * self.channels];
        self.process(&mut silence);
        self.reset();
    }

    /// Resets every filter (see [`Filter::reset`]).
    pub fn reset(&mut self) {
        self.filters.iter_mut().for_each(|f| f.reset());
    }

    /// Total delay of the chain, in frames.
    pub fn latency(&self) -> usize {
        self.filters.iter().map(|f| f.latency()).sum()
    }
}

/// Changes the parameters of a running [`Chain`] without locking it.
pub struct ChainControl {
    specs: Vec<FilterSpec>,
    params: Vec<Arc<Params>>,
}

impl ChainControl {
    /// Current parameters of the chain's filters.
    pub fn specs(&self) -> &[FilterSpec] {
        &self.specs
    }

    /// Sets the parameters of filter `index` (clamped); the chain glides to
    /// them from its next block. Returns false, changing nothing, when this
    /// needs a new chain: no filter `index`, another kind of filter, or a
    /// new limiter lookahead.
    #[must_use]
    pub fn set(&mut self, index: usize, spec: FilterSpec) -> bool {
        let Some(old) = self.specs.get_mut(index) else {
            return false;
        };
        let new = spec.clamped();
        let (before, after) = (old.values(), new.values());
        let changed = |i: &usize| before[*i] != after[*i];
        // Another plugin instance is another filter.
        let other_plugin = matches!(old, FilterSpec::Vst3 { .. }) && *old != new;
        if other_plugin
            || old.name() != new.name()
            || (0..MAX_PARAMS)
                .filter(changed)
                .any(|i| old.is_structural(i))
        {
            return false;
        }
        let params = &self.params[index];
        let mut dirty = 0;
        for i in (0..MAX_PARAMS).filter(changed) {
            params.values[i].store(after[i].to_bits(), Ordering::Relaxed);
            dirty |= 1 << i;
        }
        // Release: the values are visible once the audio thread sees the bits.
        params.dirty.fetch_or(dirty, Ordering::Release);
        *old = new;
        true
    }

    /// Gain reduction of filter `index` after the chain's last block, dB
    /// (0 for filters without one, or no filter `index`).
    pub fn reduction_db(&self, index: usize) -> f32 {
        self.params
            .get(index)
            .map_or(0.0, |p| f32::from_bits(p.reduction.load(Ordering::Relaxed)))
    }
}

fn db_to_lin(db: f32) -> f32 {
    (db * (std::f32::consts::LN_10 / 20.0)).exp()
}

/// Floored at -200 dB.
fn lin_to_db(x: f32) -> f32 {
    20.0 * x.max(1e-10).log10()
}

/// One-pole coefficient with a time constant of `ms`.
fn coef(ms: f32) -> f32 {
    (-1.0 / (ms.max(1e-3) * 1e-3 * FS)).exp()
}

/// Largest magnitude of an interleaved frame (stereo-linked detection).
fn peak(frame: &[f32]) -> f32 {
    frame.iter().fold(0.0, |m, s| m.max(s.abs()))
}

/// A parameter gliding to its target (one-pole, then snapping to it).
#[derive(Clone, Copy)]
struct Glide {
    cur: f32,
    target: f32,
    k: f32,
}

impl Glide {
    /// Time constant `ms` when stepped once every `every` frames.
    fn new(value: f32, ms: f32, every: usize) -> Self {
        Self {
            cur: value,
            target: value,
            k: 1.0 - coef(ms / every as f32),
        }
    }

    fn settled(&self) -> bool {
        self.cur == self.target
    }

    fn step(&mut self) -> f32 {
        let d = self.target - self.cur;
        // Snapping also keeps the value away from denormals.
        let next = self.cur + d * self.k;
        // Also snap once f32 rounding stops the progress.
        self.cur = if d.abs() < 1e-4 || next == self.cur {
            self.target
        } else {
            next
        };
        self.cur
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::*;

    /// `frames` frames of a sine of peak `amp` on every channel.
    pub fn sine(freq: f32, amp: f32, frames: usize, ch: usize) -> Vec<f32> {
        (0..frames * ch)
            .map(|i| amp * (std::f32::consts::TAU * freq * (i / ch) as f32 / FS).sin())
            .collect()
    }

    /// Uniform white noise in `[-amp, amp]` (xorshift).
    pub fn noise(len: usize, amp: f32, mut seed: u32) -> Vec<f32> {
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                amp * (seed as f32 / u32::MAX as f32 * 2.0 - 1.0)
            })
            .collect()
    }

    pub fn rms_db(buf: &[f32]) -> f32 {
        let power = buf.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / buf.len() as f64;
        10.0 * power.max(1e-30).log10() as f32
    }

    /// Processes `buf` in odd-sized blocks (whole frames).
    pub fn run(filter: &mut dyn Filter, buf: &mut [f32], ch: usize) {
        buf.chunks_mut(333 * ch).for_each(|b| filter.process(b));
    }

    /// Runs `spec` on `input` (stereo), returns the output.
    pub fn apply(spec: FilterSpec, input: &[f32]) -> Vec<f32> {
        let mut f = spec.build(2);
        let mut buf = input.to_vec();
        run(f.as_mut(), &mut buf, 2);
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::*;
    use super::*;

    #[test]
    fn chain_latency_is_the_sum() {
        let specs = FilterSpec::voice_chain();
        let (chain, _) = Chain::new(&specs, 2);
        let sum: usize = specs.iter().map(|s| s.build(2).latency()).sum();
        assert_eq!(chain.latency(), sum);
        // RNNoise 960 (two 480-frame frames) + limiter 2 ms lookahead + 3.
        assert_eq!(chain.latency(), 960 + 96 + 3);
    }

    #[test]
    fn control_glides_to_new_params() {
        let (mut chain, mut control) = Chain::new(&[FilterSpec::Gain { db: 0.0 }], 2);
        assert!(control.set(0, FilterSpec::Gain { db: -6.0 }));
        let mut buf = vec![1.0; 9_600 * 2];
        chain.process(&mut buf);
        // No step: the gain moves a little per sample.
        let jump = buf
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!(jump < 0.01, "jump {jump}");
        assert!((buf[0] - 1.0).abs() < 0.01, "{}", buf[0]);
        assert!((lin_to_db(*buf.last().unwrap()) + 6.0).abs() < 0.01);
        assert_eq!(control.specs()[0], FilterSpec::Gain { db: -6.0 });
    }

    #[test]
    fn glides_settle() {
        // These used to stop short of the target in f32 rounding.
        for (from, to, ms, every) in [
            (0.0, -6.0, 10.0, 1),
            (0.0, 24.0, 10.0, 1),
            (80.0, 900.0, 30.0, 32),
            (6_500.0, 8_000.0, 30.0, 32),
        ] {
            let mut g = Glide::new(from, ms, every);
            g.target = to;
            for _ in 0..100_000 {
                g.step();
            }
            assert!(g.settled(), "{from} to {to}: stuck at {}", g.cur);
        }
    }

    #[test]
    fn chain_zeroes_non_finite_input() {
        let (mut chain, _) = Chain::new(&FilterSpec::voice_chain(), 2);
        let mut buf = vec![0.1; 4_800 * 2];
        buf[100] = f32::NAN;
        buf[200] = f32::INFINITY;
        chain.process(&mut buf);
        let mut more = vec![0.1; 4_800 * 2];
        chain.process(&mut more);
        assert!(buf.iter().chain(&more).all(|s| s.is_finite()));
    }

    #[test]
    fn control_refuses_structural_changes() {
        let limiter = FilterSpec::default_for("limiter").unwrap();
        let (_chain, mut control) = Chain::new(&[limiter], 2);
        assert!(!control.set(0, FilterSpec::Gain { db: 1.0 }));
        assert!(!control.set(1, limiter));
        let FilterSpec::Limiter {
            ceiling_db,
            release_ms,
            ..
        } = limiter
        else {
            unreachable!()
        };
        let longer = FilterSpec::Limiter {
            ceiling_db,
            release_ms,
            lookahead_ms: 5.0,
        };
        assert!(!control.set(0, longer));
        let slower = FilterSpec::Limiter {
            ceiling_db,
            release_ms: 300.0,
            lookahead_ms: 2.0,
        };
        assert!(control.set(0, slower));
    }

    #[test]
    fn plugins_without_a_builder_pass_through() {
        let a = FilterSpec::Vst3 { id: 1 };
        let (mut chain, mut control) = Chain::new(&[a, FilterSpec::Gain { db: 0.0 }], 2);
        let mut buf = vec![0.25; 960 * 2];
        chain.process(&mut buf);
        assert!(buf.iter().all(|&s| s == 0.25));
        assert_eq!(chain.latency(), 0);
        assert!(control.set(0, a));
        assert!(!control.set(0, FilterSpec::Vst3 { id: 2 }));
    }

    #[test]
    fn chain_runs_the_voice_chain() {
        let (mut chain, _) = Chain::new(&FilterSpec::voice_chain(), 2);
        let mut buf = noise(48_000 * 2, 0.5, 7);
        chain.process(&mut buf);
        assert!(
            buf.iter()
                .all(|s| s.is_finite() && s.abs() <= db_to_lin(-1.0) + 1e-6)
        );
    }
}
