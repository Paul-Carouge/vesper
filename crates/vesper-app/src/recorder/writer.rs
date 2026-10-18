//! The recording file's own thread. The pipeline and the audio threads only
//! queue packets (shared data, no copy): a slow disk never stalls the
//! capture, the encode, the live stream or the audio.

use super::*;

/// Most a recording queues for its writer, in bytes and in pts span: a
/// disk this far behind is not catching up, the recording stops there.
const QUEUE_BYTES: usize = 256 << 20;
const QUEUE_SPAN: Duration = Duration::from_secs(10);

/// Kept in English: matched by `friendly()`.
const TOO_SLOW: &str = "Disk too slow: recording stopped";

/// Packets waiting for the writer, stream 0 being the video.
struct Queue {
    state: Mutex<State>,
    wake: Condvar,
    max_bytes: usize,
    max_span: Duration,
}

#[derive(Default)]
struct State {
    packets: VecDeque<(usize, EncodedPacket)>,
    bytes: usize,
    /// The first video packet from this pts on closes the queue.
    stop_at: Option<Duration>,
    /// No more packets: the writer drains the queue, then finishes.
    closed: bool,
    /// Why the recording ends early (disk too slow, the pipeline's error).
    error: Option<String>,
    /// The file is created and `Event::Started` sent.
    started: bool,
}

impl Queue {
    fn new(max_bytes: usize, max_span: Duration) -> Self {
        Self {
            state: Mutex::default(),
            wake: Condvar::new(),
            max_bytes,
            max_span,
        }
    }

    /// Never waits on the disk. Past the limits, closes the queue: the
    /// writer finishes the file with what it has.
    fn push(&self, stream: usize, p: EncodedPacket) {
        let mut s = lock(&self.state);
        if s.closed {
            return;
        }
        // The stop: nothing from this packet on is written, and nothing more
        // queued, so a slow disk draining the queue cannot overflow it.
        if stream == 0 && s.stop_at.is_some_and(|t| p.pts >= t) {
            s.closed = true;
            drop(s);
            self.wake.notify_one();
            return;
        }
        let span = s.packets.front().map(|(_, f)| p.pts.saturating_sub(f.pts));
        if s.bytes + p.data.len() > self.max_bytes || span.is_some_and(|d| d > self.max_span) {
            log!(
                "recording: disk too slow ({} MB, {:?} queued), stopping",
                s.bytes >> 20,
                span.unwrap_or_default()
            );
            s.closed = true;
            s.error.get_or_insert_with(|| TOO_SLOW.into());
        } else {
            s.bytes += p.data.len();
            s.packets.push_back((stream, p));
            // Audio waits for the next video packet (one wakeup per frame),
            // or a few: a recording without a picture yet has no video.
            if stream != 0 && s.packets.len() < 64 {
                return;
            }
        }
        drop(s);
        self.wake.notify_one();
    }

    /// The next packet to write; `None` once closed and drained.
    fn pop(&self) -> Option<(usize, EncodedPacket)> {
        let mut s = lock(&self.state);
        loop {
            if let Some((stream, p)) = s.packets.pop_front() {
                s.bytes -= p.data.len();
                return Some((stream, p));
            }
            if s.closed {
                return None;
            }
            s = self.wake.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn close(&self, error: Option<String>) {
        let mut s = lock(&self.state);
        s.closed = true;
        if s.error.is_none() {
            s.error = error;
        }
        drop(s);
        self.wake.notify_one();
    }

    /// The writer is done: later packets are dropped. Gives the early end.
    fn end(&self) -> Option<String> {
        let mut s = lock(&self.state);
        s.closed = true;
        s.bytes = 0;
        // Released outside the lock.
        let left = std::mem::take(&mut s.packets);
        let error = s.error.take();
        drop(s);
        drop(left);
        error
    }
}

/// An open recording: its queue and its writer thread.
pub(super) struct Recording {
    queue: Arc<Queue>,
    writer: Option<JoinHandle<Result<PathBuf, String>>>,
    /// Pts of its first (forced key) frame.
    pub(super) start: Duration,
}

impl Recording {
    /// Starts the writer: it creates `<folder>/Vesper <time>.mkv`, writes
    /// `lead` (`(audio stream, packet)` already in the ring), sends
    /// `Event::Started`, then the queued packets.
    pub(super) fn start(
        folder: PathBuf,
        start: Duration,
        video: VideoTrack,
        audio: Vec<AudioTrack>,
        lead: Vec<(usize, EncodedPacket)>,
        on_event: OnEvent,
    ) -> Result<Self, String> {
        let queue = Arc::new(Queue::new(QUEUE_BYTES, QUEUE_SPAN));
        let q = queue.clone();
        let writer = std::thread::Builder::new()
            .name("vesper-mux-writer".into())
            .spawn(move || write(&q, &folder, start, &video, &audio, &lead, &on_event))
            .map_err(|e| tr!("Could not start: {}", e))?;
        Ok(Self {
            queue,
            writer: Some(writer),
            start,
        })
    }

    /// Queues a packet of file stream `stream` (0 the video, then audio).
    pub(super) fn push(&self, stream: usize, p: EncodedPacket) {
        self.queue.push(stream, p);
    }

    /// Ends the file before the first video packet from `pts` on.
    pub(super) fn stop_at(&self, pts: Duration) {
        lock(&self.queue.state).stop_at.get_or_insert(pts);
    }

    /// Ends the file once the queue is written, with `error` if any.
    pub(super) fn close(&self, error: Option<String>) {
        self.queue.close(error);
    }

    /// Recording, as the UI was told: the file is created, no stop pending.
    pub(super) fn running(&self) -> bool {
        let s = lock(&self.queue.state);
        s.started && s.stop_at.is_none() && !s.closed
    }

    /// The writer finished (stopped, failed, or the disk was too slow).
    pub(super) fn ended(&self) -> bool {
        self.writer.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Closes the queue and waits until the writer has written it.
    pub(super) fn finish(mut self, error: Option<String>) -> Result<PathBuf, String> {
        self.close(error);
        let writer = self.writer.take().expect("joined once");
        writer
            .join()
            .unwrap_or_else(|_| Err(tr!("The recording stopped on an internal error").into()))
    }
}

impl Drop for Recording {
    /// Not finished: the writer still finalizes the file, once the audio
    /// threads let go of the sinks too.
    fn drop(&mut self) {
        self.queue.close(None);
    }
}

/// The writer thread.
fn write(
    q: &Queue,
    folder: &Path,
    start: Duration,
    video: &VideoTrack,
    audio: &[AudioTrack],
    lead: &[(usize, EncodedPacket)],
    on_event: &OnEvent,
) -> Result<PathBuf, String> {
    let (mut mux, path) = match create_file(folder, "Vesper", start, video, audio) {
        Ok(created) => created,
        Err(e) => {
            q.end();
            return Err(e);
        }
    };
    log!("recording to {}", path.display());
    on_event(Event::Started);
    // After `Started`: the pipeline's `Stats` follow it.
    lock(&q.state).started = true;
    for (i, p) in lead {
        if let Err(e) = mux.write_audio(*i, p) {
            log!("audio: {e}");
        }
    }
    let mut error = None;
    while let Some((stream, p)) = q.pop() {
        match stream {
            0 => {
                if let Err(e) = mux.write_video(&p) {
                    error = Some(e.to_string());
                    break;
                }
            }
            n => {
                if let Err(e) = mux.write_audio(n - 1, &p) {
                    log!("audio track {}: {e}", n - 1);
                }
            }
        }
    }
    let early = q.end();
    let finished = mux.finish();
    if finished.as_ref().is_err_and(vesper_mux::Error::is_deleted) {
        return Err(tr!("The recording file was deleted while recording").into());
    }
    match error.or(early).or(finished.err().map(|e| e.to_string())) {
        None => Ok(path),
        // Kept in English: matched by `friendly()`.
        Some(e) => Err(format!("{e} (saved up to there: {})", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(ms: u64, len: usize) -> EncodedPacket {
        EncodedPacket {
            data: Arc::from(vec![0u8; len]),
            pts: Duration::from_millis(ms),
            keyframe: true,
        }
    }

    fn drain(q: &Queue) -> Vec<(usize, u64)> {
        std::iter::from_fn(|| q.pop())
            .map(|(s, p)| (s, p.pts.as_millis() as u64))
            .collect()
    }

    #[test]
    fn packets_come_out_in_order_and_bytes_add_up() {
        let q = Queue::new(1000, QUEUE_SPAN);
        q.push(0, packet(0, 100));
        q.push(1, packet(5, 10));
        q.push(2, packet(3, 10));
        q.push(0, packet(16, 100));
        assert_eq!(lock(&q.state).bytes, 220);
        assert_eq!(q.pop().map(|(s, _)| s), Some(0));
        assert_eq!(lock(&q.state).bytes, 120);
        q.close(None);
        assert_eq!(drain(&q), [(1, 5), (2, 3), (0, 16)]);
        assert_eq!(lock(&q.state).bytes, 0);
        assert_eq!(q.end(), None);
    }

    #[test]
    fn too_many_bytes_close_the_queue_with_the_slow_disk_error() {
        let q = Queue::new(250, QUEUE_SPAN);
        q.push(0, packet(0, 100));
        q.push(0, packet(16, 100));
        q.push(0, packet(33, 100));
        // Closed: later packets are dropped, the queued ones still written.
        q.push(0, packet(50, 1));
        assert_eq!(drain(&q), [(0, 0), (0, 16)]);
        assert_eq!(q.end().as_deref(), Some(TOO_SLOW));
    }

    #[test]
    fn too_long_a_span_closes_the_queue() {
        let q = Queue::new(usize::MAX, Duration::from_secs(1));
        q.push(0, packet(0, 1));
        q.push(1, packet(1000, 1));
        q.push(0, packet(1001, 1));
        assert_eq!(drain(&q), [(0, 0), (1, 1000)]);
        assert_eq!(q.end().as_deref(), Some(TOO_SLOW));
    }

    #[test]
    fn the_first_video_packet_from_the_stop_ends_the_file() {
        let q = Queue::new(1000, QUEUE_SPAN);
        q.push(0, packet(0, 1));
        lock(&q.state).stop_at = Some(Duration::from_millis(16));
        q.push(1, packet(1, 1));
        q.push(0, packet(16, 1));
        // Nothing more is queued: a slow disk cannot overflow after a stop.
        q.push(1, packet(17, 1));
        q.push(0, packet(33, 1));
        assert_eq!(drain(&q), [(0, 0), (1, 1)]);
        assert_eq!(q.end(), None);
    }

    #[test]
    fn a_waiting_writer_wakes_for_video_and_for_the_close() {
        let q = Arc::new(Queue::new(1000, QUEUE_SPAN));
        let reader = {
            let q = q.clone();
            std::thread::spawn(move || drain(&q))
        };
        q.push(1, packet(0, 1));
        q.push(0, packet(1, 1));
        q.push(1, packet(2, 1));
        q.close(Some("pipeline error".into()));
        assert_eq!(reader.join().unwrap(), [(1, 0), (0, 1), (1, 2)]);
        assert_eq!(q.end().as_deref(), Some("pipeline error"));
    }

    fn av1() -> VideoTrack {
        VideoTrack {
            codec: Codec::Av1,
            width: 64,
            height: 64,
            fps: 30,
            extradata: Vec::new(),
        }
    }

    fn opus() -> AudioTrack {
        let mut head = b"OpusHead".to_vec();
        head.extend_from_slice(&[1, 2]); // version, channels
        head.extend_from_slice(&312u16.to_le_bytes()); // pre-skip
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&[0, 0, 0]); // gain, mapping family
        AudioTrack {
            name: "Mix".into(),
            sample_rate: 48_000,
            channels: 2,
            extradata: head,
        }
    }

    #[test]
    fn the_writer_writes_the_lead_then_the_queue_and_finishes_the_file() {
        let folder = std::env::temp_dir().join(format!("vesper-writer-{}", std::process::id()));
        let (tx, rx) = std::sync::mpsc::channel();
        let on_event: OnEvent = Arc::new(move |e| drop(tx.send(format!("{e:?}"))));
        // One Opus frame per packet (a CELT 20 ms TOC byte).
        let opus_packet = |ms| EncodedPacket {
            data: Arc::from(&[(31 << 3) | 0b100][..]),
            pts: Duration::from_millis(ms),
            keyframe: true,
        };
        let lead = vec![(0, opus_packet(1000)), (0, opus_packet(1020))];
        let start = Duration::from_secs(1);
        let rec = Recording::start(folder.clone(), start, av1(), vec![opus()], lead, on_event);
        let rec = rec.unwrap();
        for k in 2..50 {
            rec.push(1, opus_packet(1000 + k * 20));
        }
        let path = rec.finish(None).unwrap();
        assert_eq!(rx.recv().unwrap(), "Started");
        let mut input = ffmpeg_next::format::input(&path).unwrap();
        let pts: Vec<_> = input.packets().filter_map(|(_, p)| p.pts()).collect();
        std::fs::remove_dir_all(&folder).unwrap();
        assert_eq!(pts.len(), 50);
        assert!(pts.is_sorted(), "{pts:?}");
    }

    #[test]
    fn a_failed_create_ends_the_writer_with_the_error() {
        let file = std::env::temp_dir().join(format!("vesper-writer-file-{}", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        let on_event: OnEvent = Arc::new(|e| panic!("{e:?}"));
        let rec = Recording::start(
            file.clone(),
            Duration::ZERO,
            av1(),
            Vec::new(),
            Vec::new(),
            on_event,
        )
        .unwrap();
        while !rec.ended() {
            std::thread::sleep(Duration::from_millis(1));
        }
        rec.push(0, packet(0, 1));
        let err = rec.finish(Some("later".into())).unwrap_err();
        std::fs::remove_file(&file).unwrap();
        assert!(err.contains("Could not create"), "{err}");
    }
}
