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
        keyframe: true,
    }
}
