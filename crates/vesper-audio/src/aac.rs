//! AAC-LC encoding (FFmpeg's native `aac` encoder) of samples a track
//! already produced, for live streams: RTMP (FLV) carries AAC, not Opus.

use std::sync::Arc;
use std::time::Duration;

use ffmpeg::format::Sample;
use ffmpeg::format::sample::Type;
use ffmpeg::{ChannelLayout, codec, encoder, frame};
use ffmpeg_next as ffmpeg;
use tracing::error;
use vesper_core::EncodedPacket;

use crate::{CHANNELS, Result, SAMPLE_RATE};

/// What streaming services recommend for stereo.
const BIT_RATE: usize = 160_000;

/// Incoming samples this far from where the previous ones ended start a new
/// timeline (the audio restarted with another anchor).
const JUMP_NS: i128 = 10_000_000;

fn samples_ns(n: i64) -> i128 {
    i128::from(n) * 1_000_000_000 / i128::from(SAMPLE_RATE)
}

/// Encodes interleaved stereo F32 samples at [`SAMPLE_RATE`] to AAC-LC.
pub struct Aac {
    encoder: encoder::Audio,
    frame: frame::Audio,
    /// Frames per AAC packet (1024).
    size: usize,
    /// Interleaved samples waiting for a whole packet.
    pending: Vec<f32>,
    /// `CLOCK_MONOTONIC` nanoseconds of the counter's sample 0.
    base: Option<i128>,
    /// Frames given to the encoder.
    sent: i64,
    extradata: Vec<u8>,
}

// SAFETY: the codec context and frame are owned by this struct and only used
// through `&mut self`.
unsafe impl Send for Aac {}

impl Aac {
    pub fn new() -> Result<Self> {
        let codec = encoder::find_by_name("aac").ok_or("FFmpeg has no aac encoder")?;
        let mut ctx = codec::Context::new_with_codec(codec).encoder().audio()?;
        ctx.set_rate(SAMPLE_RATE as i32);
        ctx.set_channel_layout(ChannelLayout::STEREO);
        ctx.set_format(Sample::F32(Type::Planar));
        ctx.set_bit_rate(BIT_RATE);
        ctx.set_time_base((1, SAMPLE_RATE as i32));
        // AudioSpecificConfig in `extradata`, for the FLV sequence header.
        ctx.set_flags(codec::Flags::GLOBAL_HEADER);
        let encoder = ctx.open_as(codec)?;
        let size = encoder.frame_size() as usize;
        if size == 0 {
            return Err("the aac encoder has no frame size".into());
        }
        // SAFETY: the opened encoder owns valid extradata of that size.
        let extradata = unsafe {
            let ctx = encoder.as_ptr();
            if (*ctx).extradata.is_null() {
                return Err("the aac encoder produced no AudioSpecificConfig".into());
            }
            std::slice::from_raw_parts((*ctx).extradata, (*ctx).extradata_size as usize).to_vec()
        };
        let mut frame = frame::Audio::new(Sample::F32(Type::Planar), size, ChannelLayout::STEREO);
        frame.set_rate(SAMPLE_RATE);
        Ok(Self {
            encoder,
            frame,
            size,
            pending: Vec::with_capacity(2 * size * usize::from(CHANNELS)),
            base: None,
            sent: 0,
            extradata,
        })
    }

    /// AudioSpecificConfig, the codec header a container needs.
    pub fn extradata(&self) -> &[u8] {
        &self.extradata
    }

    /// Encodes `samples` (interleaved stereo) whose first sample plays at
    /// `pts`; finished packets go to `out`, stamped so they decode in sync
    /// (the first one starts the encoder's priming delay early).
    pub fn push(&mut self, pts: Duration, samples: &[f32], out: &mut impl FnMut(EncodedPacket)) {
        let ch = usize::from(CHANNELS);
        let pts = pts.as_nanos() as i128;
        let queued = self.sent + (self.pending.len() / ch) as i64;
        match self.base {
            None => self.base = Some(pts),
            Some(base) if (pts - (base + samples_ns(queued))).abs() > JUMP_NS => {
                // A partial packet of the old timeline is dropped. The
                // encoder's lookahead still holds up to 1024 old frames: they
                // come out once, ~21 ms of overlap at an audio restart. Fine.
                self.pending.clear();
                self.base = Some(pts - samples_ns(self.sent));
            }
            Some(_) => {}
        }
        self.pending.extend_from_slice(samples);
        let len = self.size * ch;
        while self.pending.len() >= len {
            for c in 0..ch {
                let plane = self.frame.plane_mut::<f32>(c);
                for (dst, src) in plane
                    .iter_mut()
                    .zip(self.pending[c..len].iter().step_by(ch))
                {
                    *dst = *src;
                }
            }
            self.pending.drain(..len);
            self.frame.set_pts(Some(self.sent));
            self.sent += self.size as i64;
            if let Err(e) = self.encoder.send_frame(&self.frame) {
                error!("aac encode failed: {e}");
            }
            self.drain(out);
        }
    }

    fn drain(&mut self, out: &mut impl FnMut(EncodedPacket)) {
        let base = self.base.unwrap_or(0);
        let mut packet = ffmpeg::Packet::empty();
        while self.encoder.receive_packet(&mut packet).is_ok() {
            let pts = base + samples_ns(packet.pts().unwrap_or(0));
            out(EncodedPacket::new(
                Arc::from(packet.data().unwrap_or_default()),
                Duration::from_nanos(pts.max(0) as u64),
                true,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| [0.5 * (i as f32 * 440.0 / 48_000.0 * std::f32::consts::TAU).sin(); 2])
            .collect()
    }

    #[test]
    fn packets_are_aligned_on_the_input_pts_and_follow_a_restart() {
        let mut aac = Aac::new().unwrap();
        // AudioSpecificConfig: AAC-LC, 48 kHz, stereo.
        assert!(aac.extradata().starts_with(&[0x11, 0x90]));
        let start = Duration::from_secs(10);
        let mut packets = Vec::new();
        // One second in 20 ms blocks, as the mix hands them over.
        let block = sine(960);
        for k in 0..50 {
            aac.push(start + Duration::from_millis(20 * k), &block, &mut |p| {
                packets.push(p)
            });
        }
        // 46 whole packets in, minus the encoder's lookahead.
        assert!((40..=46).contains(&packets.len()), "{}", packets.len());
        // The first packet holds the priming: it starts 1024 samples early.
        let priming = Duration::from_nanos(1024 * 1_000_000_000 / 48_000);
        assert_eq!(packets[0].pts, start - priming);
        for w in packets.windows(2) {
            let step = (w[1].pts - w[0].pts).as_nanos();
            assert!(step.abs_diff(1024 * 1_000_000_000 / 48_000) <= 1, "{step}");
        }
        // The audio restarted 5 s later on a new anchor: packets follow it.
        let restart = start + Duration::from_secs(6);
        let before = packets.len();
        for k in 0..50 {
            aac.push(restart + Duration::from_millis(20 * k), &block, &mut |p| {
                packets.push(p)
            });
        }
        let after: Vec<_> = packets[before..].iter().map(|p| p.pts).collect();
        assert!(after.len() > 40);
        let first_new = *after.iter().find(|&&t| t >= restart - priming).unwrap();
        assert!(
            first_new <= restart + Duration::from_millis(50),
            "{first_new:?}"
        );
        assert!(after.windows(2).all(|w| w[0] < w[1]));
    }
}
