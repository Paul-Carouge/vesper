//! A live stream, from the pipeline thread's side.

use super::*;

/// The video of a live stream.
pub(super) enum StreamVideo {
    /// The recording's packets.
    Shared,
    /// The stream's own encoder, being opened on a helper thread (opening
    /// takes tens of milliseconds: never on the tick).
    Opening(std::sync::mpsc::Receiver<Result<Encoder, String>>),
    Own(Box<Encoder>),
}

/// A live stream, from the pipeline thread's side.
pub(super) struct Streaming {
    /// `None` until the stream's encoder is open: its header is needed.
    sink: Option<LiveSink>,
    video: StreamVideo,
    /// What it started with, to start again in H.264 (see `h264_retry`).
    request: StreamRequest,
    url: String,
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    /// AAC when the mix runs.
    aac: Option<Aac>,
    on_event: OnEvent,
    /// Stream frame number of the last picture sent.
    last: Option<u64>,
    /// The recording encoder has a new picture since the stream's last one.
    fresh: bool,
    /// A keyframe was asked for and has not come out yet.
    key_pending: bool,
    /// Stream frames in a row whose encode took longer than a frame.
    slow: u32,
}

impl Streaming {
    /// Checks the request, starts opening the stream's encoder (unless it
    /// can share the recording's) and opens its audio.
    pub(super) fn start(
        request: &StreamRequest,
        v: &Video,
        settings: &Settings,
        sources: &AudioSources,
        on_event: &OnEvent,
    ) -> Result<Self, String> {
        let url = request.url()?;
        let s = &request.settings;
        let codec = s.codec();
        let fps = s.fps.clamp(1, settings.fps);
        let (width, height) = stream_size(v.track.width, v.track.height, s.height);
        // Same codec, size and rate, and the recording's cap within the
        // stream's budget: one encode serves both.
        let shared = codec == settings.codec
            && (width, height) == (v.track.width, v.track.height)
            && fps == settings.fps
            && v.bitrate <= s.bitrate;
        let video = if shared {
            StreamVideo::Shared
        } else {
            let mut source = v
                .enc
                .scaled_source()
                .map_err(|e| tr!("Could not open the stream encoder: {}", e))?;
            // The GPU encoder serves both: the recording keeps its quality,
            // the stream takes the faster level when both are a lot.
            let rec = v.enc.vcn_load();
            let load = |s: &vesper_encode::ScaledSource| {
                Some(rec? + s.vcn_load(codec, width, height, fps)?)
            };
            if load(&source).is_some_and(|l| l > VCN_STREAM_FAST) {
                source.prefer_speed();
            }
            let load = load(&source);
            log!("live: GPU encoder load with the recording {load:?}");
            if load.is_some_and(|l| l > VCN_OVERLOAD) {
                on_event(Event::Notice(overload_notice(settings.codec, Some(codec))));
            }
            let (tx, rx) = std::sync::mpsc::channel();
            let rate = RateControl::ConstantBitrate(s.bitrate);
            std::thread::Builder::new()
                .name("vesper-live-open".into())
                .spawn(move || {
                    let opened = source
                        .open(codec, width, height, fps, rate)
                        .map_err(|e| tr!("Could not open the stream encoder: {}", e));
                    let _ = tx.send(opened);
                })
                .map_err(|e| tr!("Could not open the stream encoder: {}", e))?;
            StreamVideo::Opening(rx)
        };
        // The mix runs whenever a track does: without it, a silent stream.
        let aac = if lock(sources).mixer.is_some() {
            Some(Aac::new().map_err(|e| tr!("Could not open the AAC encoder: {}", e))?)
        } else {
            on_event(Event::AudioMissing(
                tr!("Streaming without sound: no audio track runs").into(),
            ));
            None
        };
        log!(
            "live: {width}x{height} {fps} fps {codec:?}, {} kbit/s, {}",
            s.bitrate / 1000,
            if shared {
                "sharing the recording's encode"
            } else {
                "own encode"
            }
        );
        let mut streaming = Self {
            sink: None,
            video,
            request: request.clone(),
            url,
            codec,
            width,
            height,
            fps,
            aac,
            on_event: on_event.clone(),
            last: None,
            fresh: true,
            key_pending: false,
            slow: 0,
        };
        if shared {
            streaming.connect(v.track.clone(), sources)?;
        }
        Ok(streaming)
    }

    /// Starts the live thread and the audio, once the video header is known.
    fn connect(&mut self, track: VideoTrack, sources: &AudioSources) -> Result<(), String> {
        let audio = self.aac.as_ref().map(live_audio);
        let sink = LiveSink::start(
            std::mem::take(&mut self.url),
            track,
            audio,
            self.on_event.clone(),
        )
        .map_err(|e| tr!("Could not start the stream: {}", e))?;
        if let Some(aac) = self.aac.take() {
            lock(sources).set_stream_tap(Some(AudioTap::new(aac, sink.audio_in())));
        }
        self.sink = Some(sink);
        Ok(())
    }

    /// The live thread ended on its own (the server refused the stream).
    pub(super) fn ended(&self) -> bool {
        self.sink.as_ref().is_some_and(LiveSink::ended)
    }

    /// The same stream in H.264, when the server rejected this one's codec
    /// (a server without Enhanced RTMP), with what to tell the user.
    pub(super) fn h264_retry(&self) -> Option<(StreamRequest, String)> {
        if !self.sink.as_ref().is_some_and(LiveSink::codec_rejected) {
            return None;
        }
        let mut request = self.request.clone();
        request.settings.codec = Codec::H264;
        let name = if self.codec == Codec::Av1 {
            "AV1"
        } else {
            "HEVC"
        };
        let notice = tr!("The server refused {}: trying H.264", name);
        Some((request, notice))
    }

    /// Sends this tick's picture. `packets`: what the recording encoder
    /// made of it; `new_picture`: it encoded a new picture (not a repeat).
    pub(super) fn tick(
        &mut self,
        v: &mut Video,
        k: u64,
        fps: u32,
        packets: &[EncodedPacket],
        new_picture: bool,
        sources: &AudioSources,
    ) -> Result<(), String> {
        self.fresh |= new_picture;
        if let StreamVideo::Opening(rx) = &self.video {
            let enc = match rx.try_recv() {
                Ok(opened) => opened?,
                Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(()),
                Err(_) => return Err(tr!("Could not open the stream encoder").into()),
            };
            let track = VideoTrack {
                codec: self.codec,
                width: self.width,
                height: self.height,
                fps: self.fps,
                extradata: enc.extradata().to_vec(),
            };
            self.video = StreamVideo::Own(Box::new(enc));
            self.connect(track, sources)?;
        }
        let Some(sink) = &self.sink else {
            return Ok(());
        };
        let enc = match &mut self.video {
            StreamVideo::Own(enc) => enc,
            _ => {
                // A keyframe in the recording only for a new connection;
                // after a congestion, the next one comes within a GOP (2 s).
                if sink.starting() && !self.key_pending {
                    v.enc.force_keyframe();
                    self.key_pending = true;
                }
                for p in packets {
                    self.key_pending &= !p.keyframe;
                    sink.push_video(p.clone());
                }
                return Ok(());
            }
        };
        // Every tick at the recording's rate, but only one per stream frame.
        let n = k * u64::from(self.fps) / u64::from(fps);
        if self.last == Some(n) {
            return Ok(());
        }
        self.last = Some(n);
        if sink.wants_keyframe() && !self.key_pending {
            enc.force_keyframe();
            self.key_pending = true;
        }
        let pts = v.start + frame_offset(n, self.fps);
        let t = std::time::Instant::now();
        let out = if std::mem::take(&mut self.fresh) {
            enc.encode_scaled(&v.enc, pts)
        } else {
            enc.repeat(pts)
        };
        // The GPU cannot keep up with both encodes: the recording comes first.
        if t.elapsed() > frame_offset(1, fps) {
            self.slow += 1;
            if self.slow >= self.fps {
                return Err("The GPU encoder is overloaded: the stream ended so the \
                            recording stays smooth"
                    .into());
            }
        } else {
            self.slow = 0;
        }
        for p in out.map_err(|e| tr!("Stream encoding failed: {}", e))? {
            self.key_pending &= !p.keyframe;
            sink.push_video(p);
        }
        Ok(())
    }

    /// Ends the stream (with `error` if it failed here) and its audio;
    /// returns the live thread to join and its abort flag (see
    /// [`Control::keep_live`]). `abort`: do not wait for a slow network (the
    /// pipeline is ending).
    pub(super) fn stop(
        self,
        sources: &AudioSources,
        error: Option<String>,
        abort: bool,
    ) -> Option<(JoinHandle<()>, Arc<AtomicBool>)> {
        lock(sources).set_stream_tap(None);
        let Some(sink) = self.sink else {
            // Never connected: the UI still waits for the end.
            (self.on_event)(Event::Stream(StreamState::Ended(error)));
            return None;
        };
        let flag = sink.abort_flag();
        let thread = match error {
            _ if abort => sink.abort(),
            Some(e) => sink.fail(e),
            None => sink.stop(),
        };
        thread.map(|t| (t, flag))
    }
}
