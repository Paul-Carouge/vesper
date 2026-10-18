//! The pipeline thread: capture, tick, encode.

use super::*;

pub(super) fn run(
    settings: &Settings,
    capture: Option<Starter>,
    slot: &Arc<Mailbox>,
    control: &Mutex<Control>,
    started: &AtomicBool,
    sources: &AudioSources,
    on_event: &OnEvent,
) -> Result<(), String> {
    let mut portal = match &settings.game {
        Some(game) => {
            game.attach(slot, settings.fps);
            None
        }
        None => {
            // Twice the recording rate, not once: a compositor pacing frames
            // at exactly `fps` with a phase close to our ticks' would leave
            // some ticks without a new frame (a repeat, then a skipped one).
            let start =
                capture.unwrap_or_else(|| reconnect::portal(settings.fps.saturating_mul(2)));
            let portal = Portal::start(start, slot.clone())
                .map_err(|e| tr!("Could not start the capture: {}", e))?;
            if let Some(source) = portal.source() {
                on_event(Event::Source(crate::outputs::describe(source)));
            }
            Some(portal)
        }
    };
    let sinks = SharedSinks::default();
    started.store(true, Ordering::Relaxed);
    let result = pump(
        settings,
        slot,
        portal.as_mut(),
        control,
        &sinks,
        sources,
        on_event,
    );
    if let Some(game) = &settings.game {
        game.detach(slot);
    }
    // Stopping the audio flushes its last packets into the sinks, so do it
    // before closing, without holding the sinks lock the audio threads need.
    stop_audio(sources);
    let rec = lock(&sinks).rec.take();
    if let Some(r) = rec {
        on_event(Event::Finished(r.finish(result.clone().err())));
    }
    // Last: closing the portal session may hang, the file is safe by now.
    drop(portal);
    result
}

/// The encoder, opened on the first frame.
pub(super) struct Video {
    pub(super) enc: Encoder,
    pub(super) track: VideoTrack,
    /// Bitrate cap of the encoder, bits per second.
    pub(super) bitrate: u64,
    /// Pts of the first frame: the tick grid starts there.
    pub(super) start: Duration,
}

impl Video {
    fn open(settings: &Settings, width: u32, height: u32) -> Result<Self, String> {
        let fps = settings.fps;
        let bitrate =
            (f64::from(width) * f64::from(height) * f64::from(fps) * settings.max_bits_per_pixel)
                as u64;
        let enc = Encoder::new(
            settings.codec,
            width,
            height,
            fps,
            RateControl::Quality {
                quality: settings.quality,
                max_bitrate: bitrate,
            },
        )
        // Kept in English: matched by `friendly()`.
        .map_err(|e| format!("Could not open the encoder: {e}"))?;
        let track = VideoTrack {
            codec: settings.codec,
            width,
            height,
            fps,
            extradata: enc.extradata().to_vec(),
        };
        log!(
            "encoding {width}x{height} {:?} at quality {}{}, max {bitrate} bit/s, GPU encoder load {:?}",
            settings.codec,
            settings.quality,
            if settings.codec == Codec::H264 {
                " (one step finer: B-frames)"
            } else {
                ""
            },
            enc.vcn_load()
        );
        // The grid starts now, not at the frame's own pts: opening the encoder
        // takes a while, and a producer clock off by seconds must not stall
        // or burst the ticks. The first frame gets this pts. Rounded up to
        // the tick grid the game layer paces its copies to.
        Ok(Self {
            enc,
            track,
            bitrate,
            start: next_tick(monotonic_now(), fps),
        })
    }
}

impl Video {
    /// The encode takes more of the GPU encoder than it can give reliably.
    fn overloaded(&self) -> bool {
        self.enc.vcn_load().is_some_and(|l| l > VCN_OVERLOAD)
    }
}

/// Ticks until neither a recording nor the replay buffer needs the pipeline.
pub(super) fn pump(
    settings: &Settings,
    slot: &Mailbox,
    mut portal: Option<&mut Portal>,
    control: &Mutex<Control>,
    sinks: &SharedSinks,
    sources: &AudioSources,
    on_event: &OnEvent,
) -> Result<(), String> {
    let fps = settings.fps;
    let mut video: Option<Video> = None;
    let mut k = 0u64;
    let mut skipped = 0;
    // `skipped` when the recording started, and whether Stats were sent since.
    let mut rec_skipped = 0;
    let mut stats_sent = false;
    // Last capture size, to log size changes once.
    let mut last_size = None;
    // A source size other than the encoder's, and since when.
    let mut new_size: Option<((u32, u32), Duration)> = None;
    // Since when a recording request waits for a first frame.
    let mut waiting_since: Option<Duration> = None;
    // Set by every encoded tick, read once `video` exists.
    let mut deadline = Duration::ZERO;
    // Reads the requests before the first wait: a recording request must
    // arm its first-frame timeout even if no frame ever comes.
    slot.poke();
    let mut window = (monotonic_now(), 0u32);
    let mut error = None;
    // Ticks were skipped because the game was gone: its return starts a GOP.
    let mut was_gone = false;
    let mut streaming: Option<Streaming> = None;
    let mut stopping: Option<Streaming> = None;
    let mut stage = Stage::new(settings, on_event);

    loop {
        match &video {
            Some(_) => sleep_until(deadline),
            // No frame yet (e.g. no game): sleep until one, or a request.
            None => slot.wait(
                waiting_since.map(|t| (t + FIRST_FRAME_TIMEOUT).saturating_sub(monotonic_now())),
            ),
        }
        let (frame, ended) = slot.take();
        // A lost portal capture comes back in the background; the last
        // picture repeats meanwhile.
        let changes = match (portal.as_mut(), &ended) {
            (Some(p), _) => p.tick(ended.is_some()).map_err(|e| e.to_string()),
            (None, Some(e)) => Err(e.clone()),
            (None, None) => Ok([None, None]),
        };
        let changes = match changes {
            Ok(c) => c,
            Err(e) => {
                error = Some(tr!("Capture ended: {}", e));
                break;
            }
        };
        let mut stopped = false;
        for change in changes.into_iter().flatten() {
            match change {
                Change::Found(source) => {
                    on_event(Event::Notice(tr!("Source back").into()));
                    on_event(Event::Source(crate::outputs::describe(&source)));
                }
                Change::Lost => {
                    log!("capture lost: {}", ended.as_deref().unwrap_or_default());
                    on_event(Event::Notice(tr!("Source lost: reconnecting…").into()));
                }
                // The file is kept: a clean stop, not a failure.
                Change::Refused => {
                    on_event(Event::Notice(
                        tr!("The source was not picked again: capture stopped").into(),
                    ));
                    stopped = true;
                }
                Change::Failed(e) => {
                    error = Some(tr!("Capture ended: {}", e));
                    stopped = true;
                }
            }
        }
        if stopped {
            break;
        }
        // The canvas: a fixed one, else the first frame's size. A fixed
        // canvas with a scene that does not show the capture (e.g. "Be
        // right back") needs no frame at all.
        let size = match settings.canvas {
            Canvas::Fixed(w, h) => Some((w, h)),
            Canvas::Auto => frame.as_ref().map(|f| (f.width, f.height)),
        };
        let mut first = video.is_none() && (frame.is_some() || !stage.shows_main);
        if let (true, Some((width, height))) = (first, size) {
            // Audio starts with the first frame: before it, the ring and a
            // recording would drop its packets anyway. Before the encoder
            // opens, so the tracks cover the first video tick.
            start_audio_and_tell(sources, sinks, on_event);
            match Video::open(settings, width, height) {
                Ok(v) => {
                    if v.overloaded() {
                        on_event(Event::Notice(overload_notice(settings.codec, None)));
                    }
                    stage.set_canvas(width, height);
                    video = Some(v);
                }
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }
        first &= video.is_some();
        // Requests are read under the control lock, so none lands on an
        // ended pipeline unseen.
        // The live thread ended on its own (the server refused the stream).
        reap_ended_stream(&mut streaming, control, sources, on_event);
        let (record, stop, replay, save, go_live, scene) = {
            // Only this thread sets `rec`; never take `sinks` under
            // `control`: the UI would wait on muxer writes.
            let recording = lock(sinks).rec.is_some();
            let mut c = lock(control);
            let live = streaming.is_some() || c.stream.is_some();
            if c.quit || !(recording || c.record.is_some() || c.replay.is_some() || live) {
                c.ended = true;
                break;
            }
            // Stopped after the control lock: it takes the mix's lock,
            // which an audio thread may hold while it waits on a muxer write.
            if std::mem::take(&mut c.stop_stream) {
                match streaming.take() {
                    Some(s) => stopping = Some(s),
                    // Still waiting for a first frame: nothing to close.
                    None => on_event(Event::Stream(StreamState::Ended(None))),
                }
            }
            // A stream waits for the first frame; one at a time.
            let go_live = if video.is_some() && streaming.is_none() {
                c.stream.take()
            } else {
                if streaming.is_some() {
                    c.stream = None;
                }
                None
            };
            let stop = std::mem::take(&mut c.stop_recording);
            if stop {
                c.record = None;
            }
            // A recording waits for the first frame, and for the previous
            // one to be finished.
            let record = if video.is_some() && !recording {
                c.record.take()
            } else {
                None
            };
            // No frame at all (the game quit or stopped presenting): give up
            // on the recording and the stream; the replay buffer keeps waiting.
            // The game window's picker restarts the wait: the user is picking.
            if settings.game.as_ref().is_some_and(GameFrames::picking) {
                waiting_since = None;
            }
            waiting_since = (video.is_none() && (c.record.is_some() || c.stream.is_some()))
                .then(|| waiting_since.unwrap_or_else(monotonic_now));
            if waiting_since.is_some_and(|t| monotonic_now() - t >= FIRST_FRAME_TIMEOUT) {
                waiting_since = None;
                let what = if settings.game.is_some() {
                    tr!("No frame from the game")
                } else {
                    tr!("No frame from the source")
                };
                if c.record.take().is_some() {
                    on_event(Event::Finished(Err(what.into())));
                }
                if c.stream.take().is_some() {
                    on_event(Event::Stream(StreamState::Ended(Some(what.into()))));
                }
                // Without a replay buffer the pipeline ends: check again at
                // once instead of waiting for a frame.
                slot.poke();
            }
            (
                record,
                stop,
                c.replay,
                c.save.take(),
                go_live,
                c.scene.take(),
            )
        };
        if let Some(scene) = scene {
            stage.set_scene(scene);
            // Without a frame yet, a scene without the capture may start now.
            if video.is_none() {
                slot.poke();
            }
        }
        if let Some(s) = stopping.take() {
            let handle = s.stop(sources, None, false);
            lock(control).keep_live(handle);
        }
        // New tracks: restart the audio, and its part of the replay buffer
        // (the old tracks' packets do not match the new files' streams).
        // Before the first frame, the audio has not started: it reads the
        // new tracks when it does.
        let changed = lock(sources).changed;
        if changed && video.is_some() && lock(sinks).rec.is_none() {
            log!("audio tracks changed: restarting the audio");
            stop_audio(sources);
            start_audio_and_tell(sources, sinks, on_event);
        }
        let streams = lock(sources).streams();
        {
            let mut s = lock(sinks);
            match replay {
                None => s.ring = None,
                Some(span) => s.ring.get_or_insert_with(|| Ring::new(span, streams)).span = span,
            }
        }
        if let Some(folder) = save {
            save_replay(folder, video.as_ref(), sinks, control, sources, on_event);
        }
        // While nothing records, follow a lasting size change with a new
        // encoder (and an empty replay buffer: its packets need the old one).
        // A fixed canvas never changes: the compositor or the encoder
        // scales the capture into it.
        let auto = settings.canvas == Canvas::Auto;
        if let (Some(v), Some(f), true) = (&video, &frame, auto) {
            let size = (f.width, f.height);
            if size == (v.track.width, v.track.height) {
                new_size = None;
            } else if new_size.is_none_or(|(s, _)| s != size) {
                new_size = Some((size, monotonic_now()));
            }
        }
        if let (Some((_, since)), Some(f)) = (new_size, &frame)
            && monotonic_now() - since >= RESIZE_AFTER
            && lock(sinks).rec.is_none()
            && streaming.is_none()
        {
            new_size = None;
            let was_overloaded = video.as_ref().is_some_and(Video::overloaded);
            match Video::open(settings, f.width, f.height) {
                Ok(v) => {
                    // Told once, not on every resize of a game.
                    if v.overloaded() && !was_overloaded {
                        on_event(Event::Notice(overload_notice(settings.codec, None)));
                    }
                    stage.set_canvas(f.width, f.height);
                    video = Some(v);
                }
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
            (k, first, last_size) = (0, true, None);
            let streams = lock(sources).streams();
            if let Some(ring) = lock(sinks).ring.as_mut() {
                *ring = Ring::new(ring.span, streams);
            }
        }
        let Some(v) = &mut video else {
            continue;
        };
        if !first {
            k += 1;
        }
        let pts = v.start + frame_offset(k, fps);
        if let Some(folder) = record
            && start_recording(&folder, v, pts, sinks, sources, on_event)
        {
            (rec_skipped, stats_sent) = (skipped, false);
        }
        if stop && let Some(r) = lock(sinks).rec.as_ref() {
            r.stop_at(pts);
            // Only a video packet ends a recording: with no picture yet (a
            // fixed canvas waiting for a compositor that never came), end it
            // here.
            if !stage.pictured {
                r.close(Some(tr!("No picture was recorded").into()));
            }
        }
        if let Some(request) = go_live {
            match Streaming::start(&request, v, settings, sources, on_event) {
                Ok(s) => streaming = Some(s),
                Err(e) => {
                    log!("live: {e}");
                    on_event(Event::Stream(StreamState::Ended(Some(e))));
                }
            }
        }

        // A resized source (window, game) is scaled to the file's size.
        if let Some(f) = &frame
            && last_size != Some((f.width, f.height))
        {
            if last_size.is_some() {
                log!(
                    "source is now {}x{}: scaled to {}x{}",
                    f.width,
                    f.height,
                    v.track.width,
                    v.track.height
                );
            }
            last_size = Some((f.width, f.height));
        }
        // The game quit or crashed: no frozen picture, so the replay
        // buffer keeps the seconds before it. A recording or a stream keeps
        // its frame rate until the UI stops it (its stop needs a packet).
        let recording = lock(sinks).rec.is_some() || streaming.is_some();
        let gone =
            frame.is_none() && !recording && settings.game.as_ref().is_some_and(GameFrames::gone);
        let packets = if gone {
            was_gone = true;
            Ok(Vec::new())
        } else {
            if frame.is_some() && std::mem::take(&mut was_gone) {
                v.enc.force_keyframe();
            }
            stage.picture(&mut v.enc, frame, pts)
        };
        let packets = match packets {
            Ok(p) => p,
            Err(e) => {
                error = Some(tr!("Encoding failed: {}", e));
                break;
            }
        };
        let new_picture = stage.fresh();
        // Before the sinks take the packets. A failing stream ends alone.
        // A stream with its own encoder needs a picture to scale.
        if !gone
            && stage.pictured
            && let Some(s) = &mut streaming
            && let Err(e) = s.tick(v, k, fps, &packets, new_picture, sources)
        {
            log!("live: {e}");
            let handle = streaming
                .take()
                .and_then(|s| s.stop(sources, Some(e), false));
            lock(control).keep_live(handle);
        }
        lock(sinks).write_video(packets);
        // The writer ended the file (stopped, failed, disk too slow).
        let ended = lock(sinks).rec.take_if(|r| r.ended());
        if let Some(r) = ended {
            on_event(Event::Finished(r.finish(None)));
        }

        deadline = v.start + frame_offset(k + 1, fps);
        let now = monotonic_now();
        if now >= deadline + frame_offset(1, fps) {
            // More than a frame behind (GPU too busy): skip the missed ticks,
            // so video keeps its place against the audio, and count them.
            let behind = (now - v.start).as_nanos() * u128::from(fps) / 1_000_000_000;
            let behind = (behind as u64).saturating_sub(k + 1);
            k += behind;
            skipped += behind;
            deadline = v.start + frame_offset(k + 1, fps);
        }

        window.1 += 1;
        let span = now.saturating_sub(window.0);
        // Only between `Started` and the stop: the UI counts from `Started`.
        let rec_start = lock(sinks)
            .rec
            .as_ref()
            .and_then(|r| r.running().then_some(r.start));
        if let Some(start) = rec_start
            && (!stats_sent || span >= STATS_PERIOD)
        {
            on_event(Event::Stats {
                width: v.track.width,
                height: v.track.height,
                elapsed: pts.saturating_sub(start),
                fps: if stats_sent {
                    window.1 as f32 / span.as_secs_f32()
                } else {
                    fps as f32
                },
                dropped: skipped - rec_skipped,
            });
            stats_sent = true;
        }
        if span >= STATS_PERIOD {
            window = (now, 0);
        }
    }

    wind_down(
        &mut video,
        error,
        &mut streaming,
        control,
        sinks,
        sources,
        on_event,
    )
}

/// Starts the audio (see [`start_audio`]) and tells the UI.
fn start_audio_and_tell(sources: &AudioSources, sinks: &SharedSinks, on_event: &OnEvent) {
    start_audio(sources, sinks, on_event);
    reset_ring_audio(sources, sinks);
    let except = lock(sources).exclusions.as_ref().map(|e| e.apps());
    on_event(Event::DesktopExcept(except.unwrap_or_default()));
    on_event(Event::AudioStarted);
}

/// Stops a stream whose live thread ended on its own, and queues its H.264
/// retry when the server rejected the codec.
fn reap_ended_stream(
    streaming: &mut Option<Streaming>,
    control: &Mutex<Control>,
    sources: &AudioSources,
    on_event: &OnEvent,
) {
    if let Some(s) = streaming.take_if(|s| s.ended()) {
        let retry = s.h264_retry();
        let handle = s.stop(sources, None, false);
        let mut c = lock(control);
        c.keep_live(handle);
        if let Some((request, notice)) = retry {
            log!("live: codec rejected, trying H.264");
            queue_retry(&mut c, request, notice, on_event);
        }
    }
}

/// Opens a recording in `folder` starting at `pts`, with the ring's audio
/// from there on. Returns whether its writer started (`Event::Started`
/// follows once the file is created).
fn start_recording(
    folder: &Path,
    v: &mut Video,
    pts: Duration,
    sinks: &SharedSinks,
    sources: &AudioSources,
    on_event: &OnEvent,
) -> bool {
    let audio = lock(sources).file_tracks();
    let mut s = lock(sinks);
    // Desktop audio is stamped ahead of real time (playback delay): its
    // first packets already wait in the ring.
    let lead = s
        .ring
        .iter()
        .flat_map(|r| r.audio.iter().enumerate())
        .flat_map(|(i, track)| {
            let after = track.iter().filter(|p| p.pts + AUDIO_MARGIN >= pts);
            after.map(move |p| (i, p.clone()))
        })
        .collect();
    let track = v.track.clone();
    match Recording::start(folder.into(), pts, track, audio, lead, on_event.clone()) {
        Ok(r) => {
            s.rec = Some(r);
            drop(s);
            v.enc.force_keyframe();
            true
        }
        Err(e) => {
            drop(s);
            on_event(Event::Finished(Err(e)));
            false
        }
    }
}

/// After the last tick: ends the stream, flushes the encoder into the sinks,
/// and gives the pipeline's result.
fn wind_down(
    video: &mut Option<Video>,
    mut error: Option<String>,
    streaming: &mut Option<Streaming>,
    control: &Mutex<Control>,
    sinks: &SharedSinks,
    sources: &AudioSources,
    on_event: &OnEvent,
) -> Result<(), String> {
    // Error breaks too: later requests must start a new pipeline.
    lock(control).ended = true;
    if let Some(s) = streaming.take() {
        // Quitting or failed: do not wait for a slow network.
        let handle = s.stop(sources, None, true);
        lock(control).keep_live(handle);
    } else if lock(control).stream.take().is_some() {
        // Requested, never started: the UI waits for its end.
        on_event(Event::Stream(StreamState::Ended(None)));
    }
    if let Some(v) = video {
        match v.enc.flush() {
            // The recording is finished by `run`.
            Ok(packets) => lock(sinks).write_video(packets),
            Err(e) => {
                error.get_or_insert(e.to_string());
            }
        }
    }
    match error {
        Some(e) => Err(e),
        None if video.is_none() && lock(control).record.is_some() => {
            Err(tr!("No frame was captured").into())
        }
        None => Ok(()),
    }
}
