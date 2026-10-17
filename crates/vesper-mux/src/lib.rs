//! Matroska (MKV) muxing of encoded video and Opus audio packets, through
//! FFmpeg's `matroska` muxer.
//!
//! Packet timestamps are `CLOCK_MONOTONIC` instants (see `vesper_core`); the
//! muxer rebases them to the recording start. Clusters are closed every second
//! and written straight to the file, so a recording cut short by a crash or a
//! kill of the process stays playable up to its last complete cluster (no cues,
//! no duration), and [`recover_stale`] repairs it at the next launch. There is
//! no fsync: writeback starts every couple of seconds (see `recovery`), so a
//! power loss or kernel crash loses a few seconds more.
//!
//! Writes go through `write(2)` into the page cache and normally return in
//! microseconds, but they can block under dirty-page writeback pressure (slow
//! disk). Callers that cannot afford that should feed the muxer from its own
//! thread.

use std::ffi::{CString, c_int};
use std::fmt;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::ptr;
use std::time::Duration;

use ffmpeg_next::ffi as ff;
use vesper_core::{Codec, EncodedPacket};

mod live;
mod recovery;
pub use live::{Live, LiveAudio, LiveError, LiveErrorKind, redact};
pub use recovery::{
    NOTIFY_AFTER_TRIES, Outcome, SYNC_PERIOD, recover_stale, set_marker_dir, writing,
};

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
    /// Track name shown by players and editors (Matroska `Name`); empty for none.
    pub name: String,
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

const DELETED: &str = "the file was deleted while it was written";

impl Error {
    /// The file was deleted (or its folder) before `finish`: nothing was saved.
    pub fn is_deleted(&self) -> bool {
        self.0 == DELETED
    }
}

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
    /// Opus pre-skip of each audio track: its first packet starts that
    /// much before the audio it decodes to.
    pre_skip: Vec<Duration>,
    /// Video is dropped until the first keyframe so the file starts decodable.
    got_keyframe: bool,
    /// Last written pts per stream, in its time base.
    last_pts: Vec<i64>,
    /// The output file, opened again to notice it being deleted: writes to
    /// an unlinked file succeed, and `finish` would claim it was saved.
    /// shortcut: on NFS a file deleted while open is "silly-renamed"
    /// (`.nfsXXXX`) and keeps a link, so the deletion goes unnoticed there;
    /// also compare the path's inode at finish if NFS output matters.
    file: Option<File>,
    /// Marker and periodic writeback of the file (see `recovery`).
    guard: Option<recovery::Guard>,
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
        for a in audio {
            if a.sample_rate == 0 || a.channels == 0 || !is_opus_head(&a.extradata) {
                return Err(Error(
                    "audio tracks need a sample rate, channels and an OpusHead".into(),
                ));
            }
        }
        let cpath = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error("path contains a NUL byte".into()))?;
        let mut m = Self {
            ctx: ptr::null_mut(),
            packet: ptr::null_mut(),
            start,
            frame_ns: 1_000_000_000 / video.fps as i64,
            pre_skip: audio
                .iter()
                .map(|a| {
                    let samples = u16::from_le_bytes([a.extradata[10], a.extradata[11]]);
                    Duration::from_nanos(
                        u64::from(samples) * 1_000_000_000 / u64::from(a.sample_rate),
                    )
                })
                .collect(),
            got_keyframe: false,
            last_pts: vec![i64::MIN; 1 + audio.len()],
            file: None,
            guard: None,
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
            // Matroska's Colour element, the same as the bitstream's VUI
            // written by vesper-encode: BT.709 limited range, sRGB transfer
            // (screen content), chroma sited left like the VA-VPP output.
            (*par).color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
            (*par).color_space = ff::AVColorSpace::AVCOL_SPC_BT709;
            (*par).color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
            (*par).color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_IEC61966_2_1;
            (*par).chroma_location = ff::AVChromaLocation::AVCHROMA_LOC_LEFT;
            (*st).avg_frame_rate = ff::AVRational {
                num: video.fps as c_int,
                den: 1,
            };
            set_extradata(par, &video.extradata)?;

            for (i, a) in audio.iter().enumerate() {
                let st = m.new_stream()?;
                if !a.name.is_empty() {
                    let title = CString::new(a.name.replace('\0', "")).unwrap_or_default();
                    ff::av_dict_set(&mut (*st).metadata, c"title".as_ptr(), title.as_ptr(), 0);
                }
                // Players pick the first audio track (the mix, when there is one).
                if i == 0 {
                    (*st).disposition = ff::AV_DISPOSITION_DEFAULT as c_int;
                }
                let par = (*st).codecpar;
                (*par).codec_type = ff::AVMediaType::AVMEDIA_TYPE_AUDIO;
                (*par).codec_id = ff::AVCodecID::AV_CODEC_ID_OPUS;
                (*par).sample_rate = a.sample_rate as c_int;
                ff::av_channel_layout_default(&mut (*par).ch_layout, a.channels as c_int);
                set_extradata(par, &a.extradata)?;
                // Matroska's CodecDelay must equal Opus's pre-skip (OpusHead bytes 10-11).
                (*par).initial_padding =
                    u16::from_le_bytes([a.extradata[10], a.extradata[11]]) as c_int;
            }

            check(
                ff::avio_open(&mut (*m.ctx).pb, cpath.as_ptr(), ff::AVIO_FLAG_WRITE),
                "open output file",
            )?;
            m.file = File::open(path).ok();
            m.guard = m
                .file
                .as_ref()
                .and_then(|f| recovery::Guard::start(path, f));
            // Hand each closed cluster to the OS at once: a kill loses at most ~1 s.
            (*m.ctx).flush_packets = 1;
            // A stalled track must not make libavformat hold more than a
            // cluster of the other tracks in memory (default: 10 s).
            (*m.ctx).max_interleave_delta = 1_000_000;
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
    /// slice given to [`Muxer::create`]). Packets ending their pre-skip before
    /// `start` are dropped; the first Opus packet, `pre_skip` early, is kept.
    pub fn write_audio(&mut self, track: usize, packet: &EncodedPacket) -> Result<(), Error> {
        let Some(&pre_skip) = self.pre_skip.get(track) else {
            return Err(Error(format!("no audio track {track}")));
        };
        if packet.pts + pre_skip < self.start {
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
        // Signed: an Opus packet may start up to its pre-skip before `start`.
        let pts_ns = i64::try_from(packet.pts.as_nanos() as i128 - self.start.as_nanos() as i128)
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
            let pts = ff::av_rescale_q(pts_ns, NANOS, tb);
            // libavformat rejects a timestamp going backwards (e.g. after a
            // capture renegotiation): drop that packet, keep the recording.
            if pts < self.last_pts[stream] {
                return Ok(());
            }
            self.last_pts[stream] = pts;
            p.pts = pts;
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
            // A failed trailer keeps the marker: the next launch repairs the file.
            if let Some(guard) = self.guard.take() {
                guard.end(ret >= 0);
            }
            check(ret, "write MKV trailer")?;
        }
        let file = self.file.take();
        if file
            .and_then(|f| f.metadata().ok())
            .is_some_and(|m| m.nlink() == 0)
        {
            return Err(Error(DELETED.into()));
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

fn is_opus_head(extradata: &[u8]) -> bool {
    extradata.len() >= 19 && extradata.starts_with(b"OpusHead")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opus(name: &str) -> AudioTrack {
        let mut head = b"OpusHead".to_vec();
        head.extend_from_slice(&[1, 2]); // version, channels
        head.extend_from_slice(&312u16.to_le_bytes()); // pre-skip
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&[0, 0, 0]); // gain, mapping family
        AudioTrack {
            name: name.into(),
            sample_rate: 48_000,
            channels: 2,
            extradata: head,
        }
    }

    #[test]
    fn audio_tracks_keep_their_order_and_names() {
        let path =
            std::env::temp_dir().join(format!("vesper-mux-names-{}.mkv", std::process::id()));
        let video = VideoTrack {
            codec: Codec::Av1,
            width: 64,
            height: 64,
            fps: 30,
            extradata: Vec::new(),
        };
        let tracks = [opus("Mix"), opus("Desktop"), opus(""), opus("Firefox")];
        let start = Duration::from_secs(5);
        let mut mux = Muxer::create(&path, start, video, &tracks).unwrap();
        for i in 0..tracks.len() {
            let packet = EncodedPacket {
                data: std::sync::Arc::from(&[(31 << 3) | 0b100][..]),
                pts: start,
                keyframe: true,
            };
            mux.write_audio(i, &packet).unwrap();
        }
        mux.finish().unwrap();
        let input = ffmpeg_next::format::input(&path).unwrap();
        let audio: Vec<_> = input
            .streams()
            .skip(1)
            .map(|s| {
                let title = s.metadata().get("title").map(str::to_owned);
                let default = s
                    .disposition()
                    .contains(ffmpeg_next::format::stream::Disposition::DEFAULT);
                (title, default)
            })
            .collect();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            audio,
            [
                (Some("Mix".into()), true),
                (Some("Desktop".into()), false),
                (None, false),
                (Some("Firefox".into()), false),
            ]
        );
    }
}
