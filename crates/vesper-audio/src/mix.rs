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

use std::collections::VecDeque;
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
    /// Its track stopped: it no longer holds the mix back.
    closed: bool,
}

struct State {
    inputs: Vec<Queue>,
    /// `None` once the mix is finished.
    enc: Option<Encoder<OnPacket>>,
    sum: Vec<f32>,
}

impl State {
    /// Sums what every open input has, or everything once all are closed.
    fn advance(&mut self) {
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
        }
        if let Some(enc) = &mut self.enc {
            enc.push(&self.sum);
        }
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A running mix. Dropping it encodes what its inputs left and flushes.
pub struct Mix {
    state: Arc<Mutex<State>>,
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
            })),
            extradata,
        })
    }

    /// A new input. Create every input before any of them gets samples.
    pub fn input(&self) -> MixInput {
        let mut state = lock(&self.state);
        state.inputs.push(Queue::default());
        MixInput {
            state: self.state.clone(),
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
        if let Some(enc) = &mut lock(&self.state).enc {
            enc.tap = tap;
        }
    }

    /// Off: the mix only feeds its tap; no Opus packet is made. For a mix
    /// kept for a live stream while the files have none.
    pub fn set_encoding(&self, on: bool) {
        if let Some(enc) = &mut lock(&self.state).enc {
            enc.opus = on;
        }
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
    index: usize,
}

impl MixInput {
    /// Adds the track's next samples (interleaved stereo).
    pub(crate) fn push(&self, samples: &[f32]) {
        let mut state = lock(&self.state);
        if state.enc.is_none() {
            return;
        }
        state.inputs[self.index].samples.extend(samples);
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
        a.push(&sine(0.25));
        // b has nothing yet: nothing is mixed.
        assert!(rx.try_recv().is_err());
        b.push(&sine(0.125));
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
        a.push(&sine);
        b.push(&sine);
        // The peak of the last meter window, as sent to the encoder.
        let state = lock(&mix.state);
        let shared = &state.enc.as_ref().unwrap().shared;
        let peak = f32::from_bits(shared.peak.load(std::sync::atomic::Ordering::Relaxed));
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
        a.push(&vec![2.0; FRAME * usize::from(CHANNELS) * 5]);
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
    fn closed_input_adds_silence_past_its_end() {
        let (tx, rx) = mpsc::channel();
        let mix = Mix::start(Duration::ZERO, move |p| tx.send(p).unwrap()).unwrap();
        let (a, b) = (mix.input(), mix.input());
        let block = FRAME * usize::from(CHANNELS);
        a.push(&vec![0.5; block]);
        drop(a);
        b.push(&vec![0.5; block * 3]);
        drop(b);
        drop(mix);
        let packets: Vec<_> = rx.iter().collect();
        // Three blocks of mix (plus the encoder's flush), not one.
        assert!(packets.len() >= 3, "{} packets", packets.len());
    }
}
