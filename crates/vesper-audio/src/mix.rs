//! A track that sums other tracks, for players that only play the first one.
//!
//! Every input track shares the mix's anchor and has no gaps (see the
//! timeline), so sample `n` of each input is the same instant: the mix sums
//! queues position by position. It advances as far as the slowest open
//! input; a stalled source still advances, filled with silence by its own
//! encode thread, so the mix lags by at most about that stall timeout.
//!
//! A true-peak limiter (ceiling [`CEILING_DB`] dBTP) keeps a loud sum from
//! clipping; the encoder removes its lookahead delay from the timeline.
//!
//! A mix nobody uses (no mix track in the files, no live stream) is idle:
//! its inputs drop their samples without taking its lock. Woken up, it
//! lines its inputs up again on their frame index on the shared timeline.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use vesper_core::EncodedPacket;

use vesper_dsp::{Chain, FilterSpec};

use crate::{CHANNELS, Encoder, Result, Shared, Tap, open_encoder, opus_head};

type OnPacket = Box<dyn FnMut(EncodedPacket) + Send>;

/// Ceiling of the mix's limiter, dBTP.
pub const CEILING_DB: f32 = -1.0;

/// The mix's limiter, as an encoder [`crate::Process`].
fn limiter() -> crate::Process {
    let spec = FilterSpec::Limiter {
        ceiling_db: CEILING_DB,
        release_ms: 60.0,
        lookahead_ms: 2.0,
    };
    let (mut chain, _) = Chain::new(&[spec], usize::from(CHANNELS));
    let mut warm = false;
    Box::new(move |buf| {
        if !warm {
            warm = true;
            chain.warm_up();
        }
        chain.process(buf);
        chain.latency()
    })
}

#[derive(Default)]
struct Queue {
    samples: VecDeque<f32>,
    /// Frame index on the timeline of `samples[0]`, once it got any.
    start: Option<u64>,
    /// Its track stopped: it no longer holds the mix back.
    closed: bool,
}

impl Queue {
    /// Frame index of the end of `samples`.
    fn end(&self) -> Option<u64> {
        let ch = usize::from(CHANNELS);
        self.start.map(|s| s + (self.samples.len() / ch) as u64)
    }

    /// Drops the frames before `at`.
    fn trim_to(&mut self, at: u64) {
        let ch = usize::from(CHANNELS);
        if let Some(start) = self.start.filter(|&s| s < at) {
            let drop = (((at - start) as usize) * ch).min(self.samples.len());
            self.samples.drain(..drop);
            self.start = Some(at);
        }
    }
}

struct State {
    inputs: Vec<Queue>,
    /// `None` once the mix is finished.
    enc: Option<Encoder<OnPacket>>,
    sum: Vec<f32>,
    /// Frame index of the next mixed frame; `None` until the inputs are
    /// lined up (after the start or a wake-up).
    pos: Option<u64>,
}

impl State {
    /// Sums what every open input has, or everything once all are closed.
    fn advance(&mut self) {
        if self.pos.is_none() {
            // Line up on the latest first frame of the inputs that have one.
            let open = self.inputs.iter().filter(|q| !q.closed);
            if open.clone().any(|q| q.start.is_none()) {
                return;
            }
            let Some(at) = self.inputs.iter().filter_map(|q| q.start).max() else {
                return;
            };
            if self.inputs.iter().any(|q| !q.closed && q.end() < Some(at)) {
                return;
            }
            for q in &mut self.inputs {
                q.trim_to(at);
            }
            self.pos = Some(at);
            if let Some(enc) = &mut self.enc {
                enc.restart_at(at);
            }
        }
        let open = self.inputs.iter().filter(|q| !q.closed);
        let n = match open.map(|q| q.samples.len()).min() {
            Some(n) => n,
            None => self
                .inputs
                .iter()
                .map(|q| q.samples.len())
                .max()
                .unwrap_or(0),
        };
        if n == 0 {
            return;
        }
        self.sum.clear();
        self.sum.resize(n, 0.0);
        for q in &mut self.inputs {
            // A closed input may hold less: it adds silence past its end.
            let take = n.min(q.samples.len());
            for (s, x) in self.sum.iter_mut().zip(q.samples.drain(..take)) {
                *s += x;
            }
            q.start = q.start.map(|s| s + (take / usize::from(CHANNELS)) as u64);
        }
        self.pos = self.pos.map(|p| p + (n / usize::from(CHANNELS)) as u64);
        if let Some(enc) = &mut self.enc {
            enc.push(&self.sum);
        }
    }

    /// Idle (nothing encoded or tapped) or not, from the encoder's settings.
    /// A change starts the inputs over.
    fn update_active(&mut self, active: &AtomicBool) {
        let on = self.enc.as_ref().is_some_and(|e| e.opus || e.tap.is_some());
        if active.swap(on, Ordering::Relaxed) != on {
            for q in &mut self.inputs {
                q.samples.clear();
                q.start = None;
            }
            self.pos = None;
        }
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A running mix. Dropping it encodes what its inputs left and flushes.
pub struct Mix {
    state: Arc<Mutex<State>>,
    /// Off: the inputs drop their samples (see the module docs).
    active: Arc<AtomicBool>,
    extradata: Vec<u8>,
}

impl Mix {
    /// A mix starting at `anchor` (the anchor of its inputs). `on_packet`
    /// runs on whichever input thread completes a 20 ms block.
    pub fn start(
        anchor: Duration,
        on_packet: impl FnMut(EncodedPacket) + Send + 'static,
    ) -> Result<Self> {
        let encoder = open_encoder()?;
        let extradata = opus_head(&encoder)?;
        let on_packet: OnPacket = Box::new(on_packet);
        let mut enc = Encoder::new(encoder, anchor, Arc::new(Shared::default()), on_packet);
        enc.process = Some(limiter());
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                inputs: Vec::new(),
                enc: Some(enc),
                sum: Vec::new(),
                pos: None,
            })),
            active: Arc::new(AtomicBool::new(true)),
            extradata,
        })
    }

    /// A new input. Create every input before any of them gets samples.
    pub fn input(&self) -> MixInput {
        let mut state = lock(&self.state);
        state.inputs.push(Queue::default());
        MixInput {
            state: self.state.clone(),
            active: self.active.clone(),
            index: state.inputs.len() - 1,
        }
    }

    /// `OpusHead` for the container's codec private data.
    pub fn extradata(&self) -> &[u8] {
        &self.extradata
    }

    /// Hands every mixed and limited block to `tap` from now on (`None`
    /// stops it), e.g. a live stream's own audio encode.
    pub fn set_tap(&self, tap: Option<Tap>) {
        let mut state = lock(&self.state);
        if let Some(enc) = &mut state.enc {
            enc.tap = tap;
        }
        state.update_active(&self.active);
    }

    /// Off: the mix only feeds its tap; no Opus packet is made. For a mix
    /// kept for a live stream while the files have none.
    pub fn set_encoding(&self, on: bool) {
        let mut state = lock(&self.state);
        if let Some(enc) = &mut state.enc {
            enc.opus = on;
        }
        state.update_active(&self.active);
    }

    /// Whether the mix works (it is encoded or tapped).
    pub fn active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }
}

impl Drop for Mix {
    fn drop(&mut self) {
        let mut state = lock(&self.state);
        for q in &mut state.inputs {
            q.closed = true;
        }
        state.advance();
        if let Some(enc) = state.enc.take() {
            enc.finish();
        }
    }
}

/// One track's way into a [`Mix`]. Dropping it closes the input.
pub struct MixInput {
    state: Arc<Mutex<State>>,
    active: Arc<AtomicBool>,
    index: usize,
}

impl MixInput {
    /// Adds the track's next samples (interleaved stereo), whose first frame
    /// is frame `at` of the shared timeline.
    pub(crate) fn push(&self, samples: &[f32], at: u64) {
        // Idle: no lock, no work.
        if !self.active.load(Ordering::Relaxed) {
            return;
        }
        let mut state = lock(&self.state);
        if state.enc.is_none() {
            return;
        }
        let q = &mut state.inputs[self.index];
        // First samples, or a hole: the queue starts over there.
        if q.end() != Some(at) {
            q.samples.clear();
            q.start = Some(at);
        }
        q.samples.extend(samples);
        state.advance();
    }
}

impl Drop for MixInput {
    fn drop(&mut self) {
        let mut state = lock(&self.state);
        state.inputs[self.index].closed = true;
        state.advance();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FRAME;
    use crate::tests::decode;
    use std::sync::mpsc;

    #[test]
    fn mix_sums_aligned_inputs_and_waits_for_the_slowest() {
        let (tx, rx) = mpsc::channel();
        let mix = Mix::start(Duration::from_secs(1), move |p| tx.send(p).unwrap()).unwrap();
        let head = mix.extradata().to_vec();
        let (a, b, failed) = (mix.input(), mix.input(), mix.input());
        // A track that never started does not hold the mix back.
        drop(failed);
        // In-phase 480 Hz sines (whole periods per block): the sum's peak adds up.
        let sine = |amp: f32| -> Vec<f32> {
            (0..FRAME * 10)
                .flat_map(|i| {
                    let v = amp * (i as f32 * 480.0 / 48_000.0 * std::f32::consts::TAU).sin();
                    [v; CHANNELS as usize]
                })
                .collect()
        };
        a.push(&sine(0.25), 0);
        // b has nothing yet: nothing is mixed.
        assert!(rx.try_recv().is_err());
        b.push(&sine(0.125), 0);
        drop((a, b));
        drop(mix);
        let packets: Vec<_> = rx.iter().collect();
        assert!(packets.len() >= 10, "{} packets", packets.len());
        for w in packets.windows(2) {
            assert_eq!(w[1].pts - w[0].pts, Duration::from_millis(20));
        }
        // Past the codec's start-up, the decoded mix peaks at 0.25 + 0.125.
        let decoded = decode(&head, &packets);
        let mid = &decoded[decoded.len() / 3..decoded.len() * 2 / 3];
        let peak = mid.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!((peak - 0.375).abs() < 0.03, "{peak}");
    }

    #[test]
    fn mix_is_limited_to_the_ceiling() {
        let mix = Mix::start(Duration::ZERO, |_| {}).unwrap();
        let (a, b) = (mix.input(), mix.input());
        // Two in-phase full-scale 1 kHz sines: the sum peaks at +6 dBFS.
        let sine: Vec<f32> = (0..48_000)
            .flat_map(|i| [(i as f32 * 1_000.0 / 48_000.0 * std::f32::consts::TAU).sin(); 2])
            .collect();
        a.push(&sine, 0);
        b.push(&sine, 0);
        // The loudest block sent to the encoder.
        let state = lock(&mix.state);
        let shared = &state.enc.as_ref().unwrap().shared;
        let peak = shared.take_peaks().left;
        let ceiling = 10f32.powf(CEILING_DB / 20.0);
        assert!(peak <= ceiling + 1e-6, "{peak}");
        assert!(peak > ceiling * 0.95, "{peak}");
    }

    #[test]
    fn tap_gets_the_limited_mix_with_its_pts_even_without_opus() {
        let (tx, rx) = mpsc::channel();
        let anchor = Duration::from_secs(3);
        let mix = Mix::start(anchor, move |p| tx.send(p).unwrap()).unwrap();
        mix.set_encoding(false);
        let (btx, brx) = mpsc::channel();
        mix.set_tap(Some(Box::new(move |pts, block: &[f32]| {
            btx.send((pts, block.to_vec())).unwrap()
        })));
        let a = mix.input();
        a.push(&vec![2.0; FRAME * usize::from(CHANNELS) * 5], 0);
        drop(a);
        drop(mix);
        assert!(rx.try_recv().is_err(), "no Opus packet");
        let blocks: Vec<_> = brx.try_iter().collect();
        assert!(blocks.len() >= 5, "{}", blocks.len());
        for (k, (pts, block)) in blocks.iter().enumerate() {
            assert_eq!(*pts, anchor + Duration::from_millis(20 * k as u64));
            assert_eq!(block.len(), FRAME * usize::from(CHANNELS));
        }
        // Limited: the +6 dBFS input is held under the ceiling.
        let ceiling = 10f32.powf(CEILING_DB / 20.0);
        let peak = blocks[2].1.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak <= ceiling + 1e-6, "{peak}");
    }

    #[test]
    fn an_idle_mix_does_nothing_then_lines_its_inputs_up_again() {
        let anchor = Duration::from_secs(2);
        let mix = Mix::start(anchor, |_| {}).unwrap();
        mix.set_encoding(false);
        assert!(!mix.active());
        let (a, b) = (mix.input(), mix.input());
        let block = FRAME * usize::from(CHANNELS);
        // Idle: nothing is kept.
        a.push(&vec![0.1; block], 0);
        b.push(&vec![0.1; block], 0);
        assert!(lock(&mix.state).inputs.iter().all(|q| q.samples.is_empty()));
        // A stream starts: the tap gets blocks from the later input's first
        // frame on, both inputs summed position by position.
        let (tx, rx) = mpsc::channel();
        mix.set_tap(Some(Box::new(move |pts, s: &[f32]| {
            tx.send((pts, s[0])).unwrap()
        })));
        assert!(mix.active());
        a.push(&vec![0.1; block * 4], 10 * FRAME as u64);
        b.push(&vec![0.2; block * 3], 11 * FRAME as u64);
        let got: Vec<_> = rx.try_iter().collect();
        assert!(!got.is_empty());
        let frame = |n: u64| anchor + Duration::from_nanos(n * 1_000_000_000 / 48_000);
        assert_eq!(got[0].0, frame(11 * FRAME as u64));
        assert!((got[1].1 - 0.3).abs() < 1e-4, "{}", got[1].1);
        // Back to idle: its queues empty.
        mix.set_tap(None);
        assert!(!mix.active());
        assert!(lock(&mix.state).inputs.iter().all(|q| q.samples.is_empty()));
    }

    #[test]
    fn closed_input_adds_silence_past_its_end() {
        let (tx, rx) = mpsc::channel();
        let mix = Mix::start(Duration::ZERO, move |p| tx.send(p).unwrap()).unwrap();
        let (a, b) = (mix.input(), mix.input());
        let block = FRAME * usize::from(CHANNELS);
        a.push(&vec![0.5; block], 0);
        drop(a);
        b.push(&vec![0.5; block * 3], 0);
        drop(b);
        drop(mix);
        let packets: Vec<_> = rx.iter().collect();
        // Three blocks of mix (plus the encoder's flush), not one.
        assert!(packets.len() >= 3, "{} packets", packets.len());
    }
}
