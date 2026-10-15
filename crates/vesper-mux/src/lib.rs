//! Matroska (MKV) muxing of encoded video and Opus audio packets, through
//! FFmpeg's `matroska` muxer.
//!
//! Packet timestamps are `CLOCK_MONOTONIC` instants (see `vesper_core`); the
//! muxer rebases them to the recording start. Clusters are closed every second
//! and written straight to the file, so a recording cut short by a crash or a
//! kill stays playable up to its last complete cluster (no cues, no duration).
//!
//! Writes go through `write(2)` into the page cache and normally return in
//! microseconds, but they can block under dirty-page writeback pressure (slow
//! disk). Callers that cannot afford that should feed the muxer from its own
//! thread.

use std::ffi::{CString, c_int};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use std::time::Duration;

use ffmpeg_next::ffi as ff;
use vesper_core::{Codec, EncodedPacket};

const NANOS: ff::AVRational = ff::AVRational {
    num: 1,
    den: 1_000_000_000,
};

/// Video stream description.
#[derive(Debug, Clone)]
pub struct VideoTrack {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    /// Nominal frame rate, also used as each frame's display duration.
    pub fps: u32,
    /// Codec header from the encoder (`vesper_encode::Encoder::extradata`).
    pub extradata: Vec<u8>,
}

/// Opus audio stream description.
#[derive(Debug, Clone)]
pub struct AudioTrack {
    pub sample_rate: u32,
    pub channels: u32,
    /// `OpusHead` identification header.
    pub extradata: Vec<u8>,
}

#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn check(ret: c_int, what: &str) -> Result<c_int, Error> {
    if ret < 0 {
        Err(Error(format!("{what}: {}", ffmpeg_next::Error::from(ret))))
    } else {
        Ok(ret)
    }
}

/// Copies `data` into a new FFmpeg-owned, padded buffer for `codecpar.extradata`.
unsafe fn set_extradata(par: *mut ff::AVCodecParameters, data: &[u8]) -> Result<(), Error> {
    if data.is_empty() {
        return Ok(());
    }
    unsafe {
        let buf = ff::av_mallocz(data.len() + ff::AV_INPUT_BUFFER_PADDING_SIZE as usize) as *mut u8;
        if buf.is_null() {
            return Err(Error("extradata: allocation failed".into()));
        }
        ptr::copy_nonoverlapping(data.as_ptr(), buf, data.len());
        (*par).extradata = buf;
        (*par).extradata_size = data.len() as c_int;
    }
    Ok(())
}

/// Writes one MKV file: stream 0 is video, streams 1.. are audio tracks.
pub struct Muxer {
    ctx: *mut ff::AVFormatContext,
    packet: *mut ff::AVPacket,
    start: Duration,
    frame_ns: i64,
    audio_tracks: usize,
    /// Video is dropped until the first keyframe so the file starts decodable.
    got_keyframe: bool,
}

// SAFETY: the FFmpeg objects are owned by this struct and only used through
// `&mut self`, so moving the muxer to another thread is fine.
unsafe impl Send for Muxer {}

impl Muxer {
    /// Creates `path` (truncating it) and writes the file header.
    /// `start` is the `CLOCK_MONOTONIC` instant of the recording start:
    /// it becomes timestamp 0 and earlier packets are dropped.
    pub fn create(
        path: &Path,
        start: Duration,
        video: VideoTrack,
        audio: &[AudioTrack],
    ) -> Result<Self, Error> {
        if video.width == 0 || video.height == 0 || video.fps == 0 {
            return Err(Error("width, height and fps must be non-zero".into()));
        }
        let cpath = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error("path contains a NUL byte".into()))?;
        let mut m = Self {
            ctx: ptr::null_mut(),
            packet: ptr::null_mut(),
            start,
            frame_ns: 1_000_000_000 / video.fps as i64,
            audio_tracks: audio.len(),
            got_keyframe: false,
        };
        // On error, `m` is dropped and frees whatever was created so far.
        unsafe {
            m.packet = ff::av_packet_alloc();
            if m.packet.is_null() {
                return Err(Error("av_packet_alloc: allocation failed".into()));
            }
            check(
                ff::avformat_alloc_output_context2(
                    &mut m.ctx,
                    ptr::null(),
                    c"matroska".as_ptr(),
                    cpath.as_ptr(),
                ),
                "alloc matroska muxer",
            )?;

            let st = m.new_stream()?;
            let par = (*st).codecpar;
            (*par).codec_type = ff::AVMediaType::AVMEDIA_TYPE_VIDEO;
            (*par).codec_id = match video.codec {
                Codec::H264 => ff::AVCodecID::AV_CODEC_ID_H264,
                Codec::Hevc => ff::AVCodecID::AV_CODEC_ID_HEVC,
                Codec::Av1 => ff::AVCodecID::AV_CODEC_ID_AV1,
            };
            (*par).width = video.width as c_int;
            (*par).height = video.height as c_int;
            (*st).avg_frame_rate = ff::AVRational {
                num: video.fps as c_int,
                den: 1,
            };
            set_extradata(par, &video.extradata)?;

            for a in audio {
                let st = m.new_stream()?;
                let par = (*st).codecpar;
                (*par).codec_type = ff::AVMediaType::AVMEDIA_TYPE_AUDIO;
                (*par).codec_id = ff::AVCodecID::AV_CODEC_ID_OPUS;
                (*par).sample_rate = a.sample_rate as c_int;
                ff::av_channel_layout_default(&mut (*par).ch_layout, a.channels as c_int);
                set_extradata(par, &a.extradata)?;
            }

            check(
                ff::avio_open(&mut (*m.ctx).pb, cpath.as_ptr(), ff::AVIO_FLAG_WRITE),
                "open output file",
            )?;
            // Hand each closed cluster to the OS at once: a kill loses at most ~1 s.
            (*m.ctx).flush_packets = 1;
            let mut opts = ptr::null_mut();
            ff::av_dict_set(
                &mut opts,
                c"cluster_time_limit".as_ptr(),
                c"1000".as_ptr(),
                0,
            );
            let ret = ff::avformat_write_header(m.ctx, &mut opts);
            ff::av_dict_free(&mut opts);
            if ret < 0 {
                // No header, so `close` must not write a trailer.
                ff::avio_closep(&mut (*m.ctx).pb);
            }
            check(ret, "write MKV header")?;
        }
        Ok(m)
    }

    unsafe fn new_stream(&mut self) -> Result<*mut ff::AVStream, Error> {
        let st = unsafe { ff::avformat_new_stream(self.ctx, ptr::null()) };
        if st.is_null() {
            return Err(Error("avformat_new_stream: allocation failed".into()));
        }
        // Matroska timestamps are in milliseconds.
        unsafe { (*st).time_base = ff::AVRational { num: 1, den: 1000 } };
        Ok(st)
    }

    /// Writes one video packet. Packets before `start`, and before the first
    /// keyframe at or after it, are dropped.
    pub fn write_video(&mut self, packet: &EncodedPacket) -> Result<(), Error> {
        if packet.pts < self.start || !(self.got_keyframe || packet.keyframe) {
            return Ok(());
        }
        self.got_keyframe = true;
        self.write(0, packet, self.frame_ns)
    }

    /// Writes one Opus packet to audio track `track` (index into the `audio`
    /// slice given to [`Muxer::create`]). Packets before `start` are dropped.
    pub fn write_audio(&mut self, track: usize, packet: &EncodedPacket) -> Result<(), Error> {
        if track >= self.audio_tracks {
            return Err(Error(format!("no audio track {track}")));
        }
        if packet.pts < self.start {
            return Ok(());
        }
        self.write(1 + track, packet, 0)
    }

    fn write(
        &mut self,
        stream: usize,
        packet: &EncodedPacket,
        duration_ns: i64,
    ) -> Result<(), Error> {
        let pts_ns = i64::try_from((packet.pts - self.start).as_nanos())
            .map_err(|_| Error("pts overflow".into()))?;
        let size =
            c_int::try_from(packet.data.len()).map_err(|_| Error("packet too large".into()))?;
        unsafe {
            let tb = (**(*self.ctx).streams.add(stream)).time_base;
            let p = &mut *self.packet;
            // Not reference-counted: libavformat copies the data it keeps.
            p.data = packet.data.as_ptr() as *mut u8;
            p.size = size;
            p.stream_index = stream as c_int;
            p.pts = ff::av_rescale_q(pts_ns, NANOS, tb);
            p.dts = p.pts; // no B-frames (vesper-core contract)
            p.duration = ff::av_rescale_q(duration_ns, NANOS, tb);
            p.flags = if packet.keyframe {
                ff::AV_PKT_FLAG_KEY
            } else {
                0
            };
            // Takes the packet's content and resets it.
            check(
                ff::av_interleaved_write_frame(self.ctx, self.packet),
                "write packet",
            )?;
        }
        Ok(())
    }

    /// Flushes queued packets, writes cues and duration, and closes the file.
    pub fn finish(mut self) -> Result<(), Error> {
        self.close()
    }

    fn close(&mut self) -> Result<(), Error> {
        if self.ctx.is_null() {
            return Ok(());
        }
        unsafe {
            let mut ret = 0;
            if !(*self.ctx).pb.is_null() {
                ret = ff::av_write_trailer(self.ctx);
                ff::avio_closep(&mut (*self.ctx).pb);
            }
            ff::avformat_free_context(self.ctx);
            self.ctx = ptr::null_mut();
            check(ret, "write MKV trailer")?;
        }
        Ok(())
    }
}

impl Drop for Muxer {
    fn drop(&mut self) {
        let _ = self.close();
        unsafe { ff::av_packet_free(&mut self.packet) };
    }
}
