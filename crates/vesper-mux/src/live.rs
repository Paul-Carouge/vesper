//! Live output through libavformat: FLV over RTMP or RTMPS (`rtmp://`,
//! `rtmps://`, Enhanced RTMP for HEVC and AV1), or MPEG-TS over SRT
//! (`srt://`). Any other URL is written as FLV (a file, for tests).
//!
//! Every network call can block: run a [`Live`] on its own thread. Each call
//! gives up after a timeout, or at once when the `abort` flag passed to
//! [`Live::connect`] is set.
//!
//! `rtmps://` always checks the server's certificate and host name
//! (`tls_verify=1`); a libavformat that cannot is refused. Only the network
//! protocols a stream needs are allowed (no `file:`, no HTTP redirection).
//! Never raise FFmpeg's log level above `AV_LOG_VERBOSE` without a callback
//! that filters it: the RTMP debug logs print the play path, the stream key.

use std::ffi::{CString, c_int, c_void};
use std::fmt;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ffmpeg_next::ffi as ff;
use vesper_core::{Codec, EncodedPacket};

use crate::{NANOS, VideoTrack, set_extradata};

/// A write blocked this long means the connection is dead.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Name resolution gives up after this long (it cannot be interrupted).
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

/// The protocols a stream may open, nested ones included.
const PROTOCOLS: &std::ffi::CStr = c"rtmp,rtmps,tls,tcp,srt";

/// libavformat 62 (FFmpeg 8) and later check the TLS host name with every
/// backend; older OpenSSL builds checked the chain only.
const MIN_TLS_AVFORMAT: u32 = 62;

/// Test only: a CA file the TLS layer trusts, for a local test server.
#[cfg(test)]
static TEST_CA: std::sync::Mutex<Option<CString>> = std::sync::Mutex::new(None);

/// Options of every `avio_open2`: the protocol whitelist, certificate
/// checks for TLS, and no Nagle delay over TCP (RTMP, RTMPS). Freed by the
/// caller.
fn open_options(tls: bool, tcp: bool) -> *mut ff::AVDictionary {
    let mut opts = ptr::null_mut();
    unsafe {
        ff::av_dict_set(
            &mut opts,
            c"protocol_whitelist".as_ptr(),
            PROTOCOLS.as_ptr(),
            0,
        );
        if tcp {
            // Small FLV packets otherwise wait for ACKs: tens of ms of jitter.
            ff::av_dict_set(&mut opts, c"tcp_nodelay".as_ptr(), c"1".as_ptr(), 0);
        }
        if tls {
            ff::av_dict_set(&mut opts, c"tls_verify".as_ptr(), c"1".as_ptr(), 0);
            #[cfg(test)]
            if let Some(ca) = TEST_CA.lock().unwrap().as_ref() {
                ff::av_dict_set(&mut opts, c"ca_file".as_ptr(), ca.as_ptr(), 0);
            }
        }
    }
    opts
}

/// Whether `key` is still in `opts`: no protocol took it.
fn unconsumed(opts: *mut ff::AVDictionary, key: &std::ffi::CStr) -> bool {
    unsafe { !ff::av_dict_get(opts, key.as_ptr(), ptr::null(), 0).is_null() }
}

/// Resolves `host` on a throwaway thread: `getaddrinfo` cannot be
/// interrupted, so a stop does not wait for it. `None`: no address, or no
/// answer within [`RESOLVE_TIMEOUT`] or before `abort`.
fn resolve(host: &str, port: u16, abort: &AtomicBool) -> Option<bool> {
    use std::net::ToSocketAddrs;
    let (tx, rx) = std::sync::mpsc::channel();
    let host = host.to_owned();
    std::thread::Builder::new()
        .name("vesper-resolve".into())
        .spawn(move || {
            let found = (host.as_str(), port)
                .to_socket_addrs()
                .is_ok_and(|mut a| a.next().is_some());
            let _ = tx.send(found);
        })
        .ok()?;
    let end = Instant::now() + RESOLVE_TIMEOUT;
    while Instant::now() < end && !abort.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(found) => return Some(found),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => return None,
        }
    }
    None
}

/// AAC audio stream description.
#[derive(Debug, Clone)]
pub struct LiveAudio {
    pub sample_rate: u32,
    pub channels: u32,
    /// AudioSpecificConfig.
    pub extradata: Vec<u8>,
}

/// Why a connection or a write failed, for a message the user can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveErrorKind {
    /// No network, or no answer in time.
    Unreachable,
    /// The host name does not exist (or no network to look it up).
    Unresolved,
    /// Nothing listens there: a wrong server address or port.
    Refused,
    /// The server answered, then refused or closed the stream while it
    /// started: most often a wrong stream key.
    Rejected,
    /// The TLS handshake of an `rtmps://` server failed: its certificate is
    /// not trusted (an interception, a captive portal).
    Insecure,
    /// An `rtmps://` server does not speak TLS (most often a plain RTMP one).
    NotTls,
    /// `abort` was set.
    Aborted,
    /// Anything else (a write that failed once live, a bad URL).
    Other,
}

#[derive(Debug, Clone)]
pub struct LiveError {
    kind: LiveErrorKind,
    what: String,
}

impl LiveError {
    fn new(kind: LiveErrorKind, what: impl Into<String>) -> Self {
        Self {
            kind,
            what: what.into(),
        }
    }

    pub fn kind(&self) -> LiveErrorKind {
        self.kind
    }
}

impl fmt::Display for LiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.what)
    }
}

impl std::error::Error for LiveError {}

/// What libavformat's blocking calls poll: give up on abort or past the deadline.
struct Interrupt {
    abort: Arc<AtomicBool>,
    epoch: Instant,
    /// Nanoseconds since `epoch`.
    deadline: AtomicU64,
    /// The deadline fired (not the abort).
    timed_out: AtomicBool,
}

impl Interrupt {
    fn arm(&self, timeout: Duration) {
        let at = (self.epoch.elapsed() + timeout).as_nanos() as u64;
        self.deadline.store(at, Ordering::Relaxed);
        self.timed_out.store(false, Ordering::Relaxed);
    }
}

unsafe extern "C" fn interrupted(opaque: *mut c_void) -> c_int {
    // SAFETY: `opaque` is the `Interrupt` boxed in the `Live` that owns the context.
    let i = unsafe { &*(opaque as *const Interrupt) };
    if i.abort.load(Ordering::Relaxed) {
        return 1;
    }
    let late = i.epoch.elapsed().as_nanos() as u64 > i.deadline.load(Ordering::Relaxed);
    if late {
        i.timed_out.store(true, Ordering::Relaxed);
    }
    c_int::from(late)
}

/// One connection to a live ingest: stream 0 is video, stream 1 audio (if any).
pub struct Live {
    ctx: *mut ff::AVFormatContext,
    packet: *mut ff::AVPacket,
    /// Boxed: libavformat keeps a pointer to it.
    interrupt: Box<Interrupt>,
    /// Dts of the first video packet (a keyframe): time 0 of the stream.
    start: Option<Duration>,
    frame_ns: i64,
    /// Last written dts per stream, in its time base.
    last_dts: [i64; 2],
    has_audio: bool,
    /// Bytes handed to the network so far.
    written: u64,
}

// SAFETY: the FFmpeg objects are owned by this struct and only used through
// `&mut self`; the interrupt state is atomics.
unsafe impl Send for Live {}

/// `scheme://host...` without the path, query and credentials, for logs.
pub fn redact(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let host = rest.split(['/', '?']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host);
    format!("{scheme}://{host}")
}

/// `host:port` of a network URL, port defaulted per scheme.
fn endpoint(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let default = match scheme {
        "rtmp" => 1935,
        "rtmps" => 443,
        "srt" => return None, // UDP: libsrt resolves and reports itself
        _ => return None,
    };
    let host = rest.split(['/', '?']).next()?;
    let host = host.rsplit('@').next()?;
    if let Some(v6) = host.strip_prefix('[') {
        let (addr, port) = v6.split_once(']')?;
        let port = port
            .strip_prefix(':')
            .map_or(Some(default), |p| p.parse().ok())?;
        return Some((addr.to_owned(), port));
    }
    match host.rsplit_once(':') {
        Some((h, p)) => Some((h.to_owned(), p.parse().ok()?)),
        None => Some((host.to_owned(), default)),
    }
}

/// The error for a failed TLS handshake with `host`: an untrusted
/// certificate if a handshake without the check succeeds, else no TLS server.
fn handshake_failure(host: &str, speaks_tls: bool) -> LiveError {
    if speaks_tls {
        LiveError::new(
            LiveErrorKind::Insecure,
            format!("TLS handshake with {host} failed: certificate not trusted"),
        )
    } else {
        LiveError::new(
            LiveErrorKind::NotTls,
            format!("TLS handshake with {host} failed: the server does not speak TLS"),
        )
    }
}

impl Live {
    /// Connects to `url` (it holds the stream key: never log it) and writes
    /// the stream header. Gives up after `timeout`, or when `abort` is set.
    /// A failed `rtmps://` handshake is probed again within that same
    /// `timeout` to say why it failed.
    pub fn connect(
        url: &str,
        video: &VideoTrack,
        audio: Option<&LiveAudio>,
        abort: Arc<AtomicBool>,
        timeout: Duration,
    ) -> Result<Self, LiveError> {
        use LiveErrorKind::*;
        if video.width == 0 || video.height == 0 || video.fps == 0 {
            return Err(LiveError::new(
                Other,
                "width, height and fps must be non-zero",
            ));
        }
        // FFmpeg's MPEG-TS muxer writes AV1 as private data no player reads.
        if url.starts_with("srt://") && video.codec == Codec::Av1 {
            return Err(LiveError::new(Other, "MPEG-TS (SRT) cannot carry AV1"));
        }
        let curl =
            CString::new(url).map_err(|_| LiveError::new(Other, "the URL contains a NUL byte"))?;
        let format = if url.starts_with("srt://") {
            c"mpegts"
        } else {
            c"flv"
        };
        let tls = url.starts_with("rtmps://");
        let srt = url.starts_with("srt://");
        // SAFETY: a plain getter.
        if tls && unsafe { ff::avformat_version() } >> 16 < MIN_TLS_AVFORMAT {
            return Err(LiveError::new(
                Insecure,
                "this FFmpeg cannot check TLS host names",
            ));
        }
        // libavformat's resolver error is a plain EIO, as is a refused
        // stream: resolve first to tell them apart.
        let endpoint = endpoint(url);
        if let Some((host, port)) = &endpoint {
            match resolve(host, *port, &abort) {
                Some(true) => {}
                _ if abort.load(Ordering::Relaxed) => {
                    return Err(LiveError::new(Aborted, "stopped"));
                }
                Some(false) => {
                    return Err(LiveError::new(Unresolved, format!("cannot resolve {host}")));
                }
                None => {
                    return Err(LiveError::new(
                        Unreachable,
                        format!("no answer resolving {host}"),
                    ));
                }
            }
        }
        let mut live = Self {
            ctx: ptr::null_mut(),
            packet: ptr::null_mut(),
            interrupt: Box::new(Interrupt {
                abort,
                epoch: Instant::now(),
                deadline: AtomicU64::new(0),
                timed_out: AtomicBool::new(false),
            }),
            start: None,
            frame_ns: 1_000_000_000 / i64::from(video.fps),
            last_dts: [i64::MIN; 2],
            has_audio: audio.is_some(),
            written: 0,
        };
        live.interrupt.arm(timeout);
        // On error, `live` is dropped and frees whatever was created so far.
        unsafe {
            live.packet = ff::av_packet_alloc();
            if live.packet.is_null() {
                return Err(LiveError::new(Other, "av_packet_alloc: allocation failed"));
            }
            let ret = ff::avformat_alloc_output_context2(
                &mut live.ctx,
                ptr::null(),
                format.as_ptr(),
                curl.as_ptr(),
            );
            live.check(ret, "alloc muxer", Other)?;
            (*live.ctx).interrupt_callback = ff::AVIOInterruptCB {
                callback: Some(interrupted),
                opaque: (&*live.interrupt as *const Interrupt).cast_mut().cast(),
            };
            // Each packet goes out at once: no added latency.
            (*live.ctx).flush_packets = 1;

            let st = live.new_stream()?;
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
            set_extradata(par, &video.extradata).map_err(|e| LiveError::new(Other, e.0))?;
            if let Some(a) = audio {
                let st = live.new_stream()?;
                let par = (*st).codecpar;
                (*par).codec_type = ff::AVMediaType::AVMEDIA_TYPE_AUDIO;
                (*par).codec_id = ff::AVCodecID::AV_CODEC_ID_AAC;
                (*par).sample_rate = a.sample_rate as c_int;
                (*par).frame_size = 1024;
                ff::av_channel_layout_default(&mut (*par).ch_layout, a.channels as c_int);
                set_extradata(par, &a.extradata).map_err(|e| LiveError::new(Other, e.0))?;
            }

            let mut opts = open_options(tls, !srt);
            let ret = ff::avio_open2(
                &mut (*live.ctx).pb,
                curl.as_ptr(),
                ff::AVIO_FLAG_WRITE,
                &(*live.ctx).interrupt_callback,
                &mut opts,
            );
            // Fail closed: a TLS layer that did not take `tls_verify` did
            // not check the certificate.
            let unchecked = tls && ret >= 0 && unconsumed(opts, c"tls_verify");
            ff::av_dict_free(&mut opts);
            if unchecked {
                return Err(LiveError::new(
                    Insecure,
                    "the TLS layer does not check certificates",
                ));
            }
            // An SRT caller with no listener fails with a generic error:
            // never blame the key for it. RTMP gets refused instead.
            let fallback = if srt { Unreachable } else { Rejected };
            let connected = live.check(ret, "connect", fallback);
            // A failed TLS handshake is a plain EIO too: try one alone.
            if connected.as_ref().is_err_and(|e| e.kind == Rejected)
                && tls
                && let Some((host, port)) = &endpoint
                && !live.tls_handshake(host, *port, true)
            {
                // A probe cut short by a stop or the deadline says nothing.
                live.interrupted()?;
                // Without the certificate check: is it TLS at all?
                let speaks_tls = live.tls_handshake(host, *port, false);
                live.interrupted()?;
                return Err(handshake_failure(host, speaks_tls));
            }
            connected?;
            // No seek back to patch the header at the end: it is a stream.
            ff::av_dict_set(
                &mut opts,
                c"flvflags".as_ptr(),
                c"no_duration_filesize".as_ptr(),
                0,
            );
            let ret = ff::avformat_write_header(live.ctx, &mut opts);
            ff::av_dict_free(&mut opts);
            if ret < 0 {
                // No header, so `close` must not write a trailer.
                ff::avio_closep(&mut (*live.ctx).pb);
            }
            live.check(ret, "start the stream", Rejected)?;
        }
        Ok(live)
    }

    /// An error for `ret`, classified; `fallback` for errors that are not
    /// about reaching the host.
    fn check(&self, ret: c_int, what: &str, fallback: LiveErrorKind) -> Result<(), LiveError> {
        if ret >= 0 {
            return Ok(());
        }
        let i = &self.interrupt;
        let kind = if i.abort.load(Ordering::Relaxed) {
            LiveErrorKind::Aborted
        } else if i.timed_out.load(Ordering::Relaxed)
            || [
                libc_errno::ETIMEDOUT,
                libc_errno::EHOSTUNREACH,
                libc_errno::ENETUNREACH,
            ]
            .iter()
            .any(|&e| ret == ff::AVERROR(e))
        {
            LiveErrorKind::Unreachable
        } else if ret == ff::AVERROR(libc_errno::ECONNREFUSED) {
            LiveErrorKind::Refused
        } else {
            fallback
        };
        let why = if kind == LiveErrorKind::Unreachable && i.timed_out.load(Ordering::Relaxed) {
            "timed out".to_owned()
        } else {
            ffmpeg_next::Error::from(ret).to_string()
        };
        Err(LiveError::new(kind, format!("{what}: {why}")))
    }

    /// `Aborted` or `Unreachable` if the last blocking call was cut short.
    fn interrupted(&self) -> Result<(), LiveError> {
        let i = &self.interrupt;
        if i.abort.load(Ordering::Relaxed) {
            Err(LiveError::new(LiveErrorKind::Aborted, "stopped"))
        } else if i.timed_out.load(Ordering::Relaxed) {
            Err(LiveError::new(LiveErrorKind::Unreachable, "timed out"))
        } else {
            Ok(())
        }
    }

    /// Whether a TLS connection to `host:port` succeeds, the certificate
    /// checked when `verify`. Nothing is sent over it.
    fn tls_handshake(&self, host: &str, port: u16, verify: bool) -> bool {
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_owned()
        };
        let Ok(url) = CString::new(format!("tls://{host}:{port}")) else {
            return false;
        };
        // No new deadline: the probe stays within the one of `connect`.
        let mut pb = ptr::null_mut();
        unsafe {
            let mut opts = open_options(true, true);
            if !verify {
                ff::av_dict_set(&mut opts, c"tls_verify".as_ptr(), c"0".as_ptr(), 0);
            }
            let ret = ff::avio_open2(
                &mut pb,
                url.as_ptr(),
                ff::AVIO_FLAG_WRITE,
                &(*self.ctx).interrupt_callback,
                &mut opts,
            );
            let unchecked = verify && unconsumed(opts, c"tls_verify");
            ff::av_dict_free(&mut opts);
            ff::avio_closep(&mut pb);
            ret >= 0 && !unchecked
        }
    }

    unsafe fn new_stream(&mut self) -> Result<*mut ff::AVStream, LiveError> {
        let st = unsafe { ff::avformat_new_stream(self.ctx, ptr::null()) };
        if st.is_null() {
            return Err(LiveError::new(
                LiveErrorKind::Other,
                "avformat_new_stream: allocation failed",
            ));
        }
        Ok(st)
    }

    /// Bytes sent so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Writes a video packet, in decode order. The first one must be a
    /// keyframe: its dts is the stream's time 0 (no timestamp goes below
    /// it, B-frames or not). Packets whose dts goes back are dropped.
    pub fn write_video(&mut self, packet: &EncodedPacket) -> Result<(), LiveError> {
        if self.start.is_none() {
            if !packet.keyframe {
                return Ok(());
            }
            self.start = Some(packet.dts);
        }
        self.write(0, packet, self.frame_ns)
    }

    /// Writes an AAC packet; dropped before the first video packet.
    pub fn write_audio(&mut self, packet: &EncodedPacket) -> Result<(), LiveError> {
        match self.start {
            Some(start) if self.has_audio && packet.pts >= start => {
                self.write(1, packet, 1024 * 1_000_000_000 / 48_000)
            }
            _ => Ok(()),
        }
    }

    fn write(
        &mut self,
        stream: usize,
        packet: &EncodedPacket,
        duration_ns: i64,
    ) -> Result<(), LiveError> {
        let start = self.start.unwrap_or_default();
        let rebase = |t: Duration| {
            i64::try_from(t.saturating_sub(start).as_nanos())
                .map_err(|_| LiveError::new(LiveErrorKind::Other, "timestamp overflow"))
        };
        let (pts_ns, dts_ns) = (rebase(packet.pts)?, rebase(packet.dts)?);
        let size = c_int::try_from(packet.data.len())
            .map_err(|_| LiveError::new(LiveErrorKind::Other, "packet too large"))?;
        unsafe {
            let tb = (**(*self.ctx).streams.add(stream)).time_base;
            let dts = ff::av_rescale_q(dts_ns, NANOS, tb);
            if dts <= self.last_dts[stream] {
                return Ok(());
            }
            self.last_dts[stream] = dts;
            let p = &mut *self.packet;
            p.data = packet.data.as_ptr() as *mut u8;
            p.size = size;
            p.stream_index = stream as c_int;
            p.pts = ff::av_rescale_q(pts_ns, NANOS, tb);
            p.dts = dts;
            p.duration = ff::av_rescale_q(duration_ns, NANOS, tb);
            p.flags = if packet.keyframe {
                ff::AV_PKT_FLAG_KEY
            } else {
                0
            };
            self.interrupt.arm(WRITE_TIMEOUT);
            // Not interleaved: the caller already orders the packets, and
            // waiting for a stalled stream would delay the other.
            let ret = ff::av_write_frame(self.ctx, self.packet);
            // The packet was not reference counted: nothing to free.
            p.data = ptr::null_mut();
            p.size = 0;
            self.check(ret, "send", LiveErrorKind::Other)?;
        }
        self.written += packet.data.len() as u64;
        Ok(())
    }

    /// Ends the stream cleanly (a short wait at most) and disconnects.
    pub fn finish(mut self) {
        self.close();
    }

    fn close(&mut self) {
        if self.ctx.is_null() {
            return;
        }
        unsafe {
            if !(*self.ctx).pb.is_null() {
                self.interrupt.arm(Duration::from_secs(2));
                ff::av_write_trailer(self.ctx);
                ff::avio_closep(&mut (*self.ctx).pb);
            }
            ff::avformat_free_context(self.ctx);
            self.ctx = ptr::null_mut();
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.close();
        unsafe { ff::av_packet_free(&mut self.packet) };
    }
}

/// The errno values libavformat reports, without a libc dependency.
mod libc_errno {
    pub const ETIMEDOUT: i32 = 110;
    pub const ECONNREFUSED: i32 = 111;
    pub const EHOSTUNREACH: i32 = 113;
    pub const ENETUNREACH: i32 = 101;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video() -> VideoTrack {
        VideoTrack {
            codec: Codec::H264,
            width: 64,
            height: 64,
            fps: 30,
            extradata: Vec::new(),
        }
    }

    #[test]
    fn redact_keeps_only_scheme_and_host() {
        assert_eq!(
            redact("rtmps://ingest.example.net/app/live_123_secret"),
            "rtmps://ingest.example.net"
        );
        assert_eq!(
            redact("srt://h.example:9000?streamid=secret&passphrase=x"),
            "srt://h.example:9000"
        );
        assert_eq!(redact("rtmp://user:pw@h:1935/a/k"), "rtmp://h:1935");
    }

    #[test]
    fn endpoints_get_their_scheme_port() {
        assert_eq!(
            endpoint("rtmp://live.example/app/k"),
            Some(("live.example".into(), 1935))
        );
        assert_eq!(
            endpoint("rtmps://h.example/app/k"),
            Some(("h.example".into(), 443))
        );
        assert_eq!(
            endpoint("rtmp://127.0.0.1:19350/live/k"),
            Some(("127.0.0.1".into(), 19350))
        );
        assert_eq!(endpoint("rtmp://[::1]:1936/a"), Some(("::1".into(), 1936)));
        assert_eq!(endpoint("srt://h:9000"), None);
    }

    #[test]
    fn errors_say_what_to_check() {
        let abort = Arc::new(AtomicBool::new(false));
        let t = Duration::from_secs(5);
        // Nothing listens on port 1.
        let e = Live::connect(
            "rtmp://127.0.0.1:1/app/key",
            &video(),
            None,
            abort.clone(),
            t,
        )
        .err()
        .unwrap();
        assert_eq!(e.kind(), LiveErrorKind::Refused, "{e}");
        let e = Live::connect(
            "rtmp://no-such-host.invalid/app/key",
            &video(),
            None,
            abort,
            t,
        )
        .err()
        .unwrap();
        assert_eq!(e.kind(), LiveErrorKind::Unresolved, "{e}");
        assert!(!e.to_string().contains("key"), "{e}");
    }

    /// Runs `openssl` in `dir`; false when it fails or is missing.
    fn openssl(dir: &std::path::Path, args: &[&str]) -> bool {
        std::process::Command::new("openssl")
            .args(args)
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// An `openssl s_server` on 127.0.0.1 with `cert`/`key` from `dir`,
    /// killed on drop.
    struct TlsServer(std::process::Child, u16);

    impl TlsServer {
        fn start(dir: &std::path::Path, cert: &str, key: &str) -> Option<Self> {
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .ok()?
                .local_addr()
                .ok()?
                .port();
            let accept = format!("127.0.0.1:{port}");
            let child = std::process::Command::new("openssl")
                .args([
                    "s_server", "-quiet", "-accept", &accept, "-cert", cert, "-key", key,
                ])
                .current_dir(dir)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok()?;
            let server = Self(child, port);
            for _ in 0..50 {
                if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    return Some(server);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            None
        }
    }

    impl Drop for TlsServer {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn connect_kind(port: u16) -> LiveErrorKind {
        let url = format!("rtmps://localhost:{port}/app/key");
        let abort = Arc::new(AtomicBool::new(false));
        Live::connect(&url, &video(), None, abort, Duration::from_secs(3))
            .err()
            .expect("an openssl server does not speak RTMP")
            .kind()
    }

    /// Certificates are checked: a self-signed one and one for another host
    /// are refused as insecure (not blamed on the key); one from a trusted
    /// CA for the right host passes TLS (then fails at RTMP).
    #[test]
    fn rtmps_checks_the_certificate_and_the_host_name() {
        let dir = std::env::temp_dir().join(format!("vesper-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let san = |host: &str| format!("subjectAltName=DNS:{host}");
        let made = openssl(
            &dir,
            &[
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-keyout",
                "self.key",
                "-out",
                "self.pem",
                "-subj",
                "/CN=localhost",
                "-addext",
                &san("localhost"),
            ],
        ) && openssl(
            &dir,
            &[
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-keyout",
                "ca.key",
                "-out",
                "ca.pem",
                "-subj",
                "/CN=Vesper test CA",
            ],
        ) && ["localhost", "other.example"].iter().all(|host| {
            openssl(
                &dir,
                &[
                    "req",
                    "-newkey",
                    "rsa:2048",
                    "-nodes",
                    "-keyout",
                    &format!("{host}.key"),
                    "-out",
                    &format!("{host}.csr"),
                    "-subj",
                    &format!("/CN={host}"),
                    "-addext",
                    &san(host),
                ],
            ) && openssl(
                &dir,
                &[
                    "x509",
                    "-req",
                    "-days",
                    "1",
                    "-in",
                    &format!("{host}.csr"),
                    "-CA",
                    "ca.pem",
                    "-CAkey",
                    "ca.key",
                    "-CAcreateserial",
                    "-copy_extensions",
                    "copy",
                    "-out",
                    &format!("{host}.pem"),
                ],
            )
        });
        if !made {
            eprintln!("no openssl, skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let ca = CString::new(dir.join("ca.pem").to_str().unwrap()).unwrap();

        let server = TlsServer::start(&dir, "self.pem", "self.key").unwrap();
        assert_eq!(
            connect_kind(server.1),
            LiveErrorKind::Insecure,
            "self-signed"
        );
        drop(server);

        *TEST_CA.lock().unwrap() = Some(ca);
        let server = TlsServer::start(&dir, "other.example.pem", "other.example.key").unwrap();
        let wrong_host = connect_kind(server.1);
        drop(server);
        let server = TlsServer::start(&dir, "localhost.pem", "localhost.key").unwrap();
        let trusted = connect_kind(server.1);
        drop(server);
        *TEST_CA.lock().unwrap() = None;
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            wrong_host,
            LiveErrorKind::Insecure,
            "certificate for another host"
        );
        assert_ne!(trusted, LiveErrorKind::Insecure, "trusted CA, right host");
    }

    #[test]
    fn handshake_failures_tell_certificate_from_no_tls() {
        let e = handshake_failure("h", true);
        assert_eq!(e.kind(), LiveErrorKind::Insecure);
        assert!(e.to_string().contains("certificate"), "{e}");
        let e = handshake_failure("h", false);
        assert_eq!(e.kind(), LiveErrorKind::NotTls);
        assert!(e.to_string().contains("does not speak TLS"), "{e}");
    }

    /// `rtmps://` to a plain RTMP server: no TLS, not an untrusted certificate.
    #[test]
    fn rtmps_to_a_plain_server_says_it_is_no_tls() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            // Answers every connection like an RTMP server: S0 (version 3), S1.
            for mut s in listener.incoming().flatten() {
                use std::io::Write;
                let mut reply = vec![0u8; 1537];
                reply[0] = 3;
                let _ = s.write_all(&reply);
            }
        });
        assert_eq!(connect_kind(port), LiveErrorKind::NotTls);
    }

    /// A listener that answers its first connection like an RTMP server (no
    /// TLS) and holds later ones open, silent; `on_hold` runs at each of them.
    fn plain_then_silent(on_hold: impl Fn() + Send + 'static) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for (n, s) in listener.incoming().flatten().enumerate() {
                let mut s = s;
                if n == 0 {
                    use std::io::Write;
                    let mut reply = vec![0u8; 1537];
                    reply[0] = 3;
                    let _ = s.write_all(&reply);
                } else {
                    on_hold();
                    held.push(s);
                }
            }
        });
        port
    }

    /// A stop or the deadline during the TLS probe is not blamed on TLS.
    #[test]
    fn an_interrupted_tls_probe_says_why() {
        let abort = Arc::new(AtomicBool::new(false));
        let a = abort.clone();
        let port = plain_then_silent(move || a.store(true, Ordering::Relaxed));
        let url = format!("rtmps://localhost:{port}/app/key");
        let e = Live::connect(&url, &video(), None, abort, Duration::from_secs(10))
            .err()
            .unwrap();
        assert_eq!(e.kind(), LiveErrorKind::Aborted, "{e}");

        let port = plain_then_silent(|| {});
        let url = format!("rtmps://localhost:{port}/app/key");
        let abort = Arc::new(AtomicBool::new(false));
        let t = Instant::now();
        let e = Live::connect(&url, &video(), None, abort, Duration::from_secs(1))
            .err()
            .unwrap();
        assert_eq!(e.kind(), LiveErrorKind::Unreachable, "{e}");
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    }

    /// Plain TCP takes `tcp_nodelay` (RTMP passes its options down to it).
    #[test]
    fn tcp_takes_nodelay() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = CString::new(format!("tcp://{}", listener.local_addr().unwrap())).unwrap();
        unsafe {
            let mut opts = open_options(false, true);
            let mut pb = ptr::null_mut();
            let ret = ff::avio_open2(
                &mut pb,
                url.as_ptr(),
                ff::AVIO_FLAG_WRITE,
                ptr::null(),
                &mut opts,
            );
            let left = unconsumed(opts, c"tcp_nodelay");
            ff::av_dict_free(&mut opts);
            ff::avio_closep(&mut pb);
            assert!(ret >= 0 && !left, "{ret} {left}");
        }
    }

    #[test]
    fn srt_without_a_listener_is_not_blamed_on_the_key() {
        let abort = Arc::new(AtomicBool::new(false));
        let e = Live::connect(
            "srt://127.0.0.1:9?streamid=key",
            &video(),
            None,
            abort,
            Duration::from_secs(5),
        )
        .err()
        .unwrap();
        assert_ne!(e.kind(), LiveErrorKind::Rejected, "{e}");
    }

    #[test]
    fn abort_interrupts_a_hanging_connect() {
        // A listener that never answers the RTMP handshake.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("rtmp://{}/app/key", listener.local_addr().unwrap());
        let abort = Arc::new(AtomicBool::new(false));
        let a = abort.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            a.store(true, Ordering::Relaxed);
        });
        let t = Instant::now();
        let e = Live::connect(&url, &video(), None, abort, Duration::from_secs(30))
            .err()
            .unwrap();
        assert_eq!(e.kind(), LiveErrorKind::Aborted, "{e}");
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        drop(listener);
    }
}
