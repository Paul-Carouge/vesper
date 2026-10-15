//! Live streaming to Twitch, YouTube or a custom RTMP / SRT server, next to
//! the recording and the replay buffer.
//!
//! The pipeline thread hands encoded video to a [`LiveSink`], the mix's tap
//! hands it AAC (encoded on that audio thread from the mix's samples), and a
//! `vesper-live` thread sends both. The network never blocks capture,
//! encoding or the recording: pushing only queues, under a short lock. When
//! the queue holds more than [`MAX_QUEUE`] of video (the uplink is too slow),
//! the queued video is dropped and so is new video up to the next keyframe,
//! which the stream encoder then makes at once. A lost connection is retried
//! with a backoff (1, 2, 4... 30 s) while the recording goes on.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use vesper_audio::{Aac, CHANNELS, SAMPLE_RATE};
use vesper_core::{Codec, EncodedPacket};
use vesper_mux::{Live, LiveAudio, LiveErrorKind, VideoTrack};

use crate::recorder::{Event, lock};

/// Video queued beyond this is dropped: the uplink is too slow.
pub const MAX_QUEUE: Duration = Duration::from_secs(1);

/// Audio kept while video waits for a keyframe or a connection.
const MAX_AUDIO_QUEUE: Duration = Duration::from_secs(3);

/// A packet alone in its queue waits this long for one of the other kind
/// before it is sent anyway (no audio, or a stalled one).
const INTERLEAVE_WAIT: Duration = Duration::from_millis(300);

/// Connecting gives up after this long.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A connection the server closes within this time may have been refused.
const EARLY_CLOSE: Duration = Duration::from_secs(5);

/// That many in a row end the stream.
const MAX_EARLY_CLOSES: u32 = 3;

/// Longest wait between two reconnection attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How often `Event::StreamStats` is sent.
const STATS_PERIOD: Duration = Duration::from_secs(1);

/// Where the stream goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Service {
    #[default]
    Twitch,
    YouTube,
    Custom,
}

/// Twitch's automatic ingest (its global anycast endpoint), as listed by
/// <https://ingest.twitch.tv/ingests>, over TLS.
const TWITCH: &str = "rtmps://ingest.global-contribute.live-video.net/app";
/// YouTube's primary ingest over TLS (`rtmp://a.rtmp.youtube.com/live2` in clear).
const YOUTUBE: &str = "rtmps://a.rtmps.youtube.com:443/live2";

/// The stream settings of the UI. The key is kept apart (see `secret`).
#[derive(Clone, PartialEq)]
pub struct StreamSettings {
    pub service: Service,
    /// Server URL of [`Service::Custom`]: `rtmp://`, `rtmps://` or `srt://`.
    pub custom_url: String,
    /// Fit the picture in a 16:9 box of this height; `None`: native size.
    pub height: Option<u32>,
    pub fps: u32,
    /// Bits per second.
    pub bitrate: u64,
    pub codec: Codec,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            service: Service::Twitch,
            custom_url: String::new(),
            height: Some(1080),
            fps: 60,
            bitrate: 6_000_000,
            codec: Codec::H264,
        }
    }
}

// A custom URL may hold a key (pasted with it): only its host is printed.
impl fmt::Debug for StreamSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamSettings")
            .field("service", &self.service)
            .field("custom_url", &vesper_mux::redact(&self.custom_url))
            .field("height", &self.height)
            .field("fps", &self.fps)
            .field("bitrate", &self.bitrate)
            .field("codec", &self.codec)
            .finish()
    }
}

impl StreamSettings {
    /// The codec actually sent: Twitch's ingest takes H.264 only (its
    /// HEVC/AV1 "Enhanced Broadcasting" is limited to OBS with NVIDIA GPUs).
    pub fn codec(&self) -> Codec {
        match self.service {
            Service::Twitch => Codec::H264,
            _ => self.codec,
        }
    }
}

/// A stream key. Never printed: `Debug` masks it.
#[derive(Clone, PartialEq, Eq)]
pub struct StreamKey(String);

impl StreamKey {
    /// `None` for an empty key. Spaces around it (a careless paste) go.
    pub fn new(key: &str) -> Option<Self> {
        let key = key.trim();
        (!key.is_empty()).then(|| Self(key.to_owned()))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for StreamKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StreamKey(****)")
    }
}

/// What a stream starts with.
#[derive(Debug, Clone)]
pub struct StreamRequest {
    pub settings: StreamSettings,
    pub key: Option<StreamKey>,
}

impl StreamRequest {
    /// The ingest URL, key included: never log it (see `vesper_mux::redact`).
    pub fn url(&self) -> Result<String, String> {
        let key = self.key.as_ref().map(StreamKey::expose);
        let need_key = || key.ok_or_else(|| "Set a stream key in the settings first".to_owned());
        match self.settings.service {
            Service::Twitch => Ok(format!("{TWITCH}/{}", need_key()?)),
            Service::YouTube => Ok(format!("{YOUTUBE}/{}", need_key()?)),
            Service::Custom => custom_url(self.settings.custom_url.trim(), key),
        }
    }
}

/// `url` with `key` added the way its protocol takes it: a last path
/// segment for RTMP, a `streamid` for SRT. A key may already be in the URL.
fn custom_url(url: &str, key: Option<&str>) -> Result<String, String> {
    let scheme = url.split_once("://").map(|(s, _)| s);
    let host = url.split_once("://").map_or("", |(_, rest)| rest);
    if host.is_empty() || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("Enter the server URL in the settings first".into());
    }
    match (scheme, key) {
        (Some("rtmp" | "rtmps"), None) => Ok(url.to_owned()),
        (Some("rtmp" | "rtmps"), Some(key)) => Ok(format!("{}/{key}", url.trim_end_matches('/'))),
        (Some("srt"), None) => Ok(url.to_owned()),
        (Some("srt"), Some(key)) => {
            let sep = if url.contains('?') { '&' } else { '?' };
            Ok(format!("{url}{sep}streamid={key}"))
        }
        _ => Err("The server URL must start with rtmp://, rtmps:// or srt://".into()),
    }
}

/// Size of the stream for a `width`x`height` picture: fit in a 16:9 box of
/// `height` (never larger than the picture), even dimensions.
pub fn stream_size(width: u32, height: u32, target: Option<u32>) -> (u32, u32) {
    let Some(target) = target else {
        return (width & !1, height & !1);
    };
    let box_w = f64::from(target) * 16.0 / 9.0;
    let scale = (box_w / f64::from(width))
        .min(f64::from(target) / f64::from(height))
        .min(1.0);
    let even = |v: u32| ((f64::from(v) * scale).round() as u32).max(2) & !1;
    (even(width), even(height))
}

/// State of a stream, as the UI shows it.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamState {
    Connecting,
    /// Sending since this instant (on air since the first connection).
    Live,
    /// The connection was lost; the next attempt is in `retry_in`.
    Reconnecting {
        error: String,
        retry_in: Duration,
    },
    /// The stream ended, with the error that ended it if any.
    Ended(Option<String>),
}

/// A short sentence for a connection error.
fn describe(kind: LiveErrorKind, host: &str, live_before: bool) -> String {
    match kind {
        LiveErrorKind::Unreachable => format!("Cannot reach {host}: check the network"),
        LiveErrorKind::Refused if live_before => format!("{host} refused the connection"),
        LiveErrorKind::Refused => format!("{host} refused the connection: check the server URL"),
        LiveErrorKind::Rejected if !live_before => {
            "The server rejected the stream: check the stream key".into()
        }
        LiveErrorKind::Rejected | LiveErrorKind::Other => "Connection lost".into(),
        LiveErrorKind::Insecure => {
            format!("The secure connection to {host} failed: its certificate is not trusted")
        }
        LiveErrorKind::Aborted => "Stopped".into(),
    }
}

/// Packets waiting for the network, shared by the pushers and the writer.
#[derive(Default)]
struct Queue {
    video: VecDeque<(EncodedPacket, Instant)>,
    audio: VecDeque<(EncodedPacket, Instant)>,
    /// Connected: packets are kept. Before, they are not (the stream
    /// starts on a keyframe once connected).
    connected: bool,
    /// Video is dropped until the next keyframe.
    want_key: bool,
    /// ... because the network was too slow (not a new connection): count it.
    congested: bool,
    /// Video packets dropped while connected.
    dropped: u64,
    stop: bool,
    /// Why the pipeline stopped the stream, if it failed on its side.
    error: Option<String>,
    /// The writer thread is done.
    ended: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
    /// Interrupts a blocking connect or write at once.
    abort: Arc<AtomicBool>,
}

/// A running stream. Dropping it does not wait for its thread.
pub struct LiveSink {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl LiveSink {
    /// Starts the `vesper-live` thread, which connects to `url` and sends
    /// what is pushed from then on. `on_event` gets the state and the stats.
    pub fn start(
        url: String,
        video: VideoTrack,
        audio: Option<LiveAudio>,
        on_event: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            wake: Condvar::new(),
            abort: Arc::new(AtomicBool::new(false)),
        });
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name("vesper-live".into())
            .spawn(move || {
                let error = run(&s, &url, &video, audio.as_ref(), &on_event)
                    .or_else(|| lock(&s.queue).error.take());
                lock(&s.queue).ended = true;
                on_event(Event::Stream(StreamState::Ended(error)));
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Queues an encoded video packet. Never blocks on the network.
    pub fn push_video(&self, p: EncodedPacket) {
        let mut q = lock(&self.shared.queue);
        if !q.connected {
            return;
        }
        if q.want_key && !p.keyframe {
            q.dropped += u64::from(q.congested);
            return;
        }
        (q.want_key, q.congested) = (false, false);
        q.video.push_back((p, Instant::now()));
        let span = q.video.back().unwrap().0.pts - q.video.front().unwrap().0.pts;
        if span > MAX_QUEUE {
            // Congested: drop what waits rather than fall further behind,
            // and resume on the next keyframe.
            q.dropped += q.video.len() as u64;
            q.video.clear();
            (q.want_key, q.congested) = (true, true);
        }
        drop(q);
        self.shared.wake.notify_one();
    }

    /// Where the stream's audio goes, for its [`AudioTap`].
    pub fn audio_in(&self) -> AudioIn {
        AudioIn(self.shared.clone())
    }

    /// Whether video waits for a keyframe: the stream encoder makes one.
    pub fn wants_keyframe(&self) -> bool {
        let q = lock(&self.shared.queue);
        q.connected && q.want_key
    }

    /// The thread ended on its own (the server refused the stream).
    pub fn ended(&self) -> bool {
        lock(&self.shared.queue).ended
    }

    /// Ends the stream; `Ended` follows once it is closed (within seconds).
    /// Returns its thread, to join before the process exits.
    pub fn stop(self) -> Option<JoinHandle<()>> {
        self.end(None)
    }

    /// Ends the stream at once, without waiting for a slow network (the
    /// pipeline is ending: quitting must not hang).
    pub fn abort(self) -> Option<JoinHandle<()>> {
        self.shared.abort.store(true, Ordering::Relaxed);
        self.end(None)
    }

    /// Ends the stream because of `error` on the pipeline's side (its
    /// encoder failed); `Ended` reports it.
    pub fn fail(self, error: String) -> Option<JoinHandle<()>> {
        self.end(Some(error))
    }

    fn end(mut self, error: Option<String>) -> Option<JoinHandle<()>> {
        let mut q = lock(&self.shared.queue);
        q.stop = true;
        q.error = error;
        // Not on air (connecting, or waiting to retry): nothing to close
        // cleanly, so do not wait out a connection timeout.
        if !q.connected {
            self.shared.abort.store(true, Ordering::Relaxed);
        }
        drop(q);
        self.shared.wake.notify_one();
        self.thread.take()
    }
}

impl Drop for LiveSink {
    fn drop(&mut self) {
        // Not stopped through `stop` (the pipeline crashed): end at once.
        if self.thread.is_some() {
            lock(&self.shared.queue).stop = true;
            self.shared.abort.store(true, Ordering::Relaxed);
            self.shared.wake.notify_one();
        }
    }
}

/// The next packet to send, in pts order across both queues: `(is_video,
/// packet)`. A packet alone waits [`INTERLEAVE_WAIT`] for the other kind.
fn next_packet(q: &mut Queue, has_audio: bool, now: Instant) -> Option<(bool, EncodedPacket)> {
    let video = q.video.front().map(|(p, t)| (p.pts, *t));
    let audio = q.audio.front().map(|(p, t)| (p.pts, *t));
    let take_video = match (video, audio) {
        (None, None) => return None,
        (Some((v, _)), Some((a, _))) => v <= a,
        (Some((_, t)), None) => !has_audio || now - t >= INTERLEAVE_WAIT,
        (None, Some((_, t))) if now - t >= INTERLEAVE_WAIT => false,
        (None, Some(_)) => return None,
    };
    if take_video && video.is_some() {
        q.video.pop_front().map(|(p, _)| (true, p))
    } else if !take_video {
        q.audio.pop_front().map(|(p, _)| (false, p))
    } else {
        None
    }
}

/// Connects, sends, reconnects; returns the error that ended the stream.
fn run(
    s: &Shared,
    url: &str,
    video: &VideoTrack,
    audio: Option<&LiveAudio>,
    on_event: &Arc<dyn Fn(Event) + Send + Sync>,
) -> Option<String> {
    let host = vesper_mux::redact(url);
    let host = host.split_once("://").map_or(host.as_str(), |(_, h)| h);
    let mut failures = 0u32;
    let mut early_closes = 0u32;
    // The stream was on air at least once, and since when.
    let mut on_air: Option<Instant> = None;
    on_event(Event::Stream(StreamState::Connecting));
    loop {
        if lock(&s.queue).stop {
            return None;
        }
        log!("live: connecting to {}", vesper_mux::redact(url));
        let error = match Live::connect(url, video, audio, s.abort.clone(), CONNECT_TIMEOUT) {
            Ok(live) => {
                let since = Instant::now();
                on_air.get_or_insert(since);
                log!("live: connected");
                on_event(Event::Stream(StreamState::Live));
                // `None`: stopped.
                let e = send(s, live, audio.is_some(), on_air.unwrap(), on_event)?;
                log!("live: connection lost: {e}");
                // Some services take a stream with a wrong key, then close
                // it: a few connections in a row that end at once are that.
                if since.elapsed() < EARLY_CLOSE {
                    early_closes += 1;
                } else {
                    (early_closes, failures) = (0, 0);
                }
                const CHECK_KEY: &str = "The server closed the stream: check the stream key";
                match early_closes {
                    0 => "Connection lost".to_owned(),
                    MAX_EARLY_CLOSES.. => return Some(CHECK_KEY.into()),
                    _ => CHECK_KEY.into(),
                }
            }
            Err(e) if e.kind() == LiveErrorKind::Aborted => return None,
            Err(e) => {
                log!("live: {e}");
                let text = describe(e.kind(), host, on_air.is_some());
                // A wrong key or URL will not fix itself: only retry what may.
                let retry = on_air.is_some() || e.kind() == LiveErrorKind::Unreachable;
                if !retry {
                    return Some(text);
                }
                text
            }
        };
        let retry_in = Duration::from_secs(1 << failures.min(5)).min(MAX_BACKOFF);
        failures += 1;
        on_event(Event::Stream(StreamState::Reconnecting { error, retry_in }));
        let q = lock(&s.queue);
        let (q, _) = s
            .wake
            .wait_timeout_while(q, retry_in, |q| !q.stop)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if q.stop {
            return None;
        }
    }
}

/// Sends queued packets until the connection fails (`Some(error)`) or a
/// stop request (`None`).
fn send(
    s: &Shared,
    mut live: Live,
    has_audio: bool,
    on_air: Instant,
    on_event: &Arc<dyn Fn(Event) + Send + Sync>,
) -> Option<String> {
    {
        let mut q = lock(&s.queue);
        q.connected = true;
        q.want_key = true;
        q.video.clear();
        q.audio.clear();
    }
    let mut window = (Instant::now(), live.written());
    let result = loop {
        let mut q = lock(&s.queue);
        if q.stop {
            break None;
        }
        let now = Instant::now();
        let Some((is_video, packet)) = next_packet(&mut q, has_audio, now) else {
            let _ = s
                .wake
                .wait_timeout(q, Duration::from_millis(50))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            continue;
        };
        let dropped = q.dropped;
        drop(q);
        let written = if is_video {
            live.write_video(&packet)
        } else {
            live.write_audio(&packet)
        };
        if let Err(e) = written {
            break Some(e.to_string());
        }
        let span = now - window.0;
        if span >= STATS_PERIOD {
            let bytes = live.written() - window.1;
            on_event(Event::StreamStats {
                elapsed: on_air.elapsed(),
                bitrate: (bytes as f64 * 8.0 / span.as_secs_f64()) as u64,
                dropped,
            });
            window = (now, live.written());
        }
    };
    {
        let mut q = lock(&s.queue);
        q.connected = false;
        q.video.clear();
        q.audio.clear();
    }
    if result.is_none() {
        live.finish();
    }
    result
}

/// The audio side of a [`LiveSink`].
pub struct AudioIn(Arc<Shared>);

impl AudioIn {
    /// Queues an AAC packet. Never blocks on the network.
    pub fn push(&self, p: EncodedPacket) {
        let mut q = lock(&self.0.queue);
        if !q.connected {
            return;
        }
        q.audio.push_back((p, Instant::now()));
        while q
            .audio
            .front()
            .is_some_and(|(f, _)| q.audio.back().unwrap().0.pts - f.pts > MAX_AUDIO_QUEUE)
        {
            q.audio.pop_front();
        }
        drop(q);
        self.0.wake.notify_one();
    }
}

/// A live stream's audio: the mix's samples to AAC, then to the sink.
pub struct AudioTap {
    aac: Mutex<Aac>,
    sink: AudioIn,
}

impl AudioTap {
    pub fn new(aac: Aac, sink: AudioIn) -> Arc<Self> {
        Arc::new(Self {
            aac: Mutex::new(aac),
            sink,
        })
    }

    /// The mix's tap feeding this stream.
    pub fn tap(self: &Arc<Self>) -> vesper_audio::Tap {
        let t = self.clone();
        Box::new(move |pts, samples| {
            lock(&t.aac).push(pts, samples, &mut |p| t.sink.push(p));
        })
    }
}

/// AAC stream description for an encoder's header.
pub fn live_audio(aac: &Aac) -> LiveAudio {
    LiveAudio {
        sample_rate: SAMPLE_RATE,
        channels: u32::from(CHANNELS),
        extradata: aac.extradata().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(service: Service, url: &str, key: &str) -> StreamRequest {
        StreamRequest {
            settings: StreamSettings {
                service,
                custom_url: url.into(),
                ..StreamSettings::default()
            },
            key: StreamKey::new(key),
        }
    }

    #[test]
    fn urls_put_the_key_where_each_service_wants_it() {
        assert_eq!(
            request(Service::Twitch, "", " live_1_abc ").url().unwrap(),
            "rtmps://ingest.global-contribute.live-video.net/app/live_1_abc"
        );
        assert_eq!(
            request(Service::YouTube, "", "a-b-c").url().unwrap(),
            "rtmps://a.rtmps.youtube.com:443/live2/a-b-c"
        );
        assert_eq!(
            request(Service::Custom, "rtmp://127.0.0.1/live/", "k")
                .url()
                .unwrap(),
            "rtmp://127.0.0.1/live/k"
        );
        assert_eq!(
            request(Service::Custom, "srt://h:9000?latency=200000", "k")
                .url()
                .unwrap(),
            "srt://h:9000?latency=200000&streamid=k"
        );
        assert_eq!(
            request(Service::Custom, "srt://h:9000", "").url().unwrap(),
            "srt://h:9000"
        );
        assert!(request(Service::Twitch, "", "  ").url().is_err());
        assert!(request(Service::Custom, "http://h/x", "k").url().is_err());
        assert!(request(Service::Custom, "rtmp://", "k").url().is_err());
    }

    #[test]
    fn keys_are_never_printed() {
        let r = request(Service::Twitch, "", "live_secret");
        assert!(!format!("{r:?}").contains("secret"));
        let r = request(Service::Custom, "rtmp://h.example/app/live_secret", "");
        assert!(!format!("{r:?}").contains("secret"), "{r:?}");
    }

    #[test]
    fn twitch_always_gets_h264() {
        let mut s = StreamSettings {
            codec: Codec::Av1,
            ..StreamSettings::default()
        };
        assert_eq!(s.codec(), Codec::H264);
        s.service = Service::YouTube;
        assert_eq!(s.codec(), Codec::Av1);
    }

    #[test]
    fn stream_sizes_fit_a_16_9_box_and_never_upscale() {
        assert_eq!(stream_size(2560, 1440, Some(1080)), (1920, 1080));
        assert_eq!(stream_size(3840, 2160, Some(720)), (1280, 720));
        assert_eq!(stream_size(3440, 1440, Some(1080)), (1920, 804));
        assert_eq!(stream_size(1280, 1024, Some(1080)), (1280, 1024));
        assert_eq!(stream_size(1920, 1200, Some(1080)), (1728, 1080));
        assert_eq!(stream_size(1280, 720, Some(1080)), (1280, 720));
        assert_eq!(stream_size(1281, 721, None), (1280, 720));
    }

    fn packet(ms: u64, keyframe: bool) -> EncodedPacket {
        EncodedPacket {
            data: Arc::from(&[0u8; 4][..]),
            pts: Duration::from_millis(ms),
            keyframe,
        }
    }

    fn sink() -> LiveSink {
        LiveSink {
            shared: Arc::new(Shared {
                queue: Mutex::new(Queue::default()),
                wake: Condvar::new(),
                abort: Arc::new(AtomicBool::new(false)),
            }),
            thread: None,
        }
    }

    #[test]
    fn congestion_drops_queued_video_until_the_next_keyframe() {
        let s = sink();
        // Not connected: nothing is kept.
        s.push_video(packet(0, true));
        assert!(lock(&s.shared.queue).video.is_empty());
        {
            let mut q = lock(&s.shared.queue);
            q.connected = true;
            q.want_key = true;
        }
        // Starts on a keyframe.
        s.push_video(packet(10, false));
        assert!(s.wants_keyframe());
        s.push_video(packet(20, true));
        assert!(!s.wants_keyframe());
        // Nothing is sent: past a second of video, the queue is dropped.
        for ms in (36..=1036).step_by(16) {
            s.push_video(packet(ms, false));
        }
        let q = lock(&s.shared.queue);
        assert!(q.video.is_empty(), "{}", q.video.len());
        assert!(q.want_key);
        assert_eq!(
            q.dropped, 64,
            "the wait for the first keyframe is not a drop"
        );
        drop(q);
        s.push_video(packet(1052, false));
        s.push_video(packet(1068, true));
        let q = lock(&s.shared.queue);
        assert_eq!(q.dropped, 65);
        assert_eq!(q.video.len(), 1);
    }

    #[test]
    fn packets_leave_in_pts_order_and_a_lone_kind_waits_briefly() {
        let mut q = Queue::default();
        let t0 = Instant::now();
        q.video.push_back((packet(100, true), t0));
        q.video.push_back((packet(116, false), t0));
        q.audio.push_back((packet(90, true), t0));
        q.audio.push_back((packet(111, true), t0));
        let order: Vec<_> = std::iter::from_fn(|| next_packet(&mut q, true, t0))
            .map(|(v, p)| (v, p.pts.as_millis()))
            .collect();
        assert_eq!(order, [(false, 90), (true, 100), (false, 111)]);
        // Video alone waits for audio, then goes.
        assert!(next_packet(&mut q, true, t0).is_none());
        assert!(next_packet(&mut q, true, t0 + INTERLEAVE_WAIT).is_some());
        // Without audio, video never waits.
        q.video.push_back((packet(132, false), t0));
        assert!(next_packet(&mut q, false, t0).is_some());
    }

    #[test]
    fn errors_read_as_what_to_check() {
        assert_eq!(
            describe(LiveErrorKind::Rejected, "h", false),
            "The server rejected the stream: check the stream key"
        );
        assert_eq!(
            describe(LiveErrorKind::Refused, "h:1935", false),
            "h:1935 refused the connection: check the server URL"
        );
        assert_eq!(
            describe(LiveErrorKind::Unreachable, "h", true),
            "Cannot reach h: check the network"
        );
        assert_eq!(
            describe(LiveErrorKind::Refused, "h", true),
            "h refused the connection"
        );
    }
}
