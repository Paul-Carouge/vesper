//! A track that sums other tracks, for players that only play the first one.
//!
//! Every input track shares the mix's anchor and has no gaps (see the
//! timeline), so sample `n` of each input is the same instant: the mix sums
//! queues position by position. It advances as far as the slowest open
//! input; a stalled source still advances, filled with silence by its own
//! encode thread, so the mix lags by at most about that stall timeout.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use vesper_core::EncodedPacket;

use crate::{Encoder, Result, Shared, open_encoder, opus_head};

type OnPacket = Box<dyn FnMut(EncodedPacket) + Send>;

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
        let enc = Encoder::new(encoder, anchor, Arc::new(Shared::default()), on_packet);
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
    use crate::{CHANNELS, FRAME};
    use std::sync::mpsc;

    /// Decodes the mix's packets back to interleaved samples.
    fn decode(head: &[u8], packets: &[EncodedPacket]) -> Vec<f32> {
        use ffmpeg_next as ffmpeg;
        let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::OPUS).unwrap();
        let mut ctx = ffmpeg::codec::Context::new_with_codec(codec);
        unsafe {
            let raw = ctx.as_mut_ptr();
            let buf = ffmpeg::ffi::av_mallocz(head.len() + 64) as *mut u8;
            std::ptr::copy_nonoverlapping(head.as_ptr(), buf, head.len());
            (*raw).extradata = buf;
            (*raw).extradata_size = head.len() as i32;
        }
        let mut dec = ctx.decoder().audio().unwrap();
        let mut out = Vec::new();
        let mut frame = ffmpeg::frame::Audio::empty();
        for p in packets {
            dec.send_packet(&ffmpeg::Packet::copy(&p.data)).unwrap();
            while dec.receive_frame(&mut frame).is_ok() {
                // libopus decodes to planar or packed float: read channel 0.
                let plane: &[f32] = frame.plane(0);
                out.extend_from_slice(&plane[..frame.samples()]);
            }
        }
        out
    }

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
