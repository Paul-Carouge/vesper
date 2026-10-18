//! Helpers shared by the integration test and `mux_bench`: the GPU test
//! pattern producer from vesper-encode, and a stand-in Opus track.

#![allow(dead_code)]

#[path = "../../../vesper-encode/tests/common/mod.rs"]
pub mod producer;

use std::sync::Arc;
use std::time::Duration;

use vesper_core::EncodedPacket;
use vesper_mux::AudioTrack;

/// Duration of one Opus packet.
pub const OPUS_FRAME: Duration = Duration::from_millis(20);

/// 48 kHz stereo Opus track with a valid `OpusHead`.
pub fn opus_track() -> AudioTrack {
    let mut head = b"OpusHead".to_vec();
    head.push(1); // version
    head.push(2); // channels
    head.extend_from_slice(&312u16.to_le_bytes()); // pre-skip
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // channel mapping family
    AudioTrack {
        name: "Desktop".into(),
        sample_rate: 48_000,
        channels: 2,
        extradata: head,
    }
}

/// A valid 20 ms Opus packet: CELT fullband stereo TOC with an empty frame
/// (decoded as silence / concealment).
pub fn opus_packet(pts: Duration) -> EncodedPacket {
    EncodedPacket {
        data: Arc::from(&[(31 << 3) | 0b100][..]),
        pts,
        dts: pts,
        keyframe: true,
    }
}

/// `frames` pictures of 640x360 at 30 fps encoded in H.264 by the GPU,
/// first at `start`: B-frames, packets in decode order, a keyframe every
/// 60. Returns the codec header and the packets. GPU only.
pub fn h264_b_frames(start: Duration, frames: u32) -> (Vec<u8>, Vec<EncodedPacket>) {
    use producer::{POOL, Producer};
    use vesper_core::Codec;
    use vesper_encode::{Encoder, RateControl};
    let producer = unsafe { Producer::new(640, 360) };
    let mut enc = Encoder::new(Codec::H264, 640, 360, 30, RateControl::ConstantQp(30)).unwrap();
    let mut packets = Vec::new();
    for t in 0..frames {
        let pts = start + Duration::from_nanos(u64::from(t) * 1_000_000_000 / 30);
        let frame = unsafe { producer.frame(t as usize % POOL, t, 0, pts) };
        packets.extend(enc.encode(frame).unwrap());
    }
    packets.extend(enc.flush().unwrap());
    assert!(
        packets.windows(2).any(|w| w[1].pts < w[0].pts),
        "the encoder made B-frames"
    );
    (enc.extradata().to_vec(), packets)
}
