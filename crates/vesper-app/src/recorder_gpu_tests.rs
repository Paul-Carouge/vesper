//! The whole pipeline (GPU encode, MKV, replay buffer) fed by synthetic game
//! frames, hammered with the sequences a user produces. Skipped without a
//! VAAPI GPU (like vesper-encode's and vesper-mux's tests).

use super::producer::{POOL, Producer};
use super::*;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Instant;

const W: u32 = 640;
const H: u32 = 360;

fn settings(route: &GameFrames) -> Settings {
    Settings {
        codec: Codec::Hevc,
        fps: 30,
        quality: 28,
        max_bits_per_pixel: 0.1,
        tracks: Vec::new(),
        mix: false,
        game: Some(route.clone()),
    }
}

/// Feeds the route ~30 frames per second of the current `size` pattern until
/// `live` is false; `paused` skips frames (the game froze or is loading).
fn feeder(
    route: GameFrames,
    live: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    size: Arc<Mutex<(u32, u32)>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // Producers are not Send: created here, one per size.
        let mut producers: Vec<((u32, u32), Producer)> = Vec::new();
        let mut t = 0u32;
        while live.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(33));
            if paused.load(Ordering::Relaxed) {
                continue;
            }
            let want = *lock(&size);
            if !producers.iter().any(|(s, _)| *s == want) {
                producers.push((want, unsafe { Producer::new(want.0, want.1) }));
            }
            let producer = &producers.iter().find(|(s, _)| *s == want).unwrap().1;
            let frame = unsafe { producer.frame(t as usize % POOL, t, 0, monotonic_now()) };
            route.push(Ok(frame));
            t += 1;
        }
    })
}

struct Rig {
    route: GameFrames,
    live: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    size: Arc<Mutex<(u32, u32)>>,
    feeder: Option<std::thread::JoinHandle<()>>,
}

impl Rig {
    fn new() -> Option<Self> {
        if !Path::new("/dev/dri/renderD128").exists() {
            eprintln!("no GPU, skipping");
            return None;
        }
        let route = GameFrames::default();
        let live = Arc::new(AtomicBool::new(true));
        let paused = Arc::new(AtomicBool::new(false));
        let size = Arc::new(Mutex::new((W, H)));
        let feeder = feeder(route.clone(), live.clone(), paused.clone(), size.clone());
        Some(Self {
            route,
            live,
            paused,
            size,
            feeder: Some(feeder),
        })
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Relaxed);
        if let Some(f) = self.feeder.take() {
            let _ = f.join();
        }
    }
}

/// Next event that is not stats, source or audio news. `None`: no encoder here.
fn event(rx: &Receiver<Event>) -> Option<Event> {
    loop {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Event::Stats { .. } | Event::Source(_) | Event::AudioStarted) => {}
            Ok(Event::Stopped(Some(e))) if e.contains("Could not open the encoder") => {
                eprintln!("no VAAPI encoder ({e}), skipping");
                return None;
            }
            Ok(e) => return Some(e),
            Err(RecvTimeoutError::Timeout) => panic!("no event for 20 s: the pipeline hung"),
            Err(RecvTimeoutError::Disconnected) => panic!("events ended without Stopped"),
        }
    }
}

/// A file a player can open: (video packets, first one is a keyframe,
/// pts of the last video packet in seconds).
fn inspect(path: &Path) -> (usize, bool, f64) {
    let mut input = ffmpeg_next::format::input(path)
        .unwrap_or_else(|e| panic!("{} does not open: {e}", path.display()));
    let (mut n, mut first_key, mut last) = (0, false, 0.0);
    for (stream, packet) in input.packets() {
        if stream.index() != 0 {
            continue;
        }
        if n == 0 {
            first_key = packet.is_key();
        }
        n += 1;
        let tb = stream.time_base();
        last = packet.pts().unwrap_or(0) as f64 * f64::from(tb.numerator())
            / f64::from(tb.denominator());
    }
    (n, first_key, last)
}

fn dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vesper-gpu-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Start, stop after 0 ms .. 1 s, start again: every file is valid, begins
/// on a keyframe, and the pipeline accepts the next request.
#[test]
fn rapid_start_stop_cycles_leave_valid_files() {
    let Some(rig) = Rig::new() else { return };
    let (tx, rx) = mpsc::channel();
    let p = Pipeline::start(
        settings(&rig.route),
        Some(Duration::from_secs(5)),
        None,
        move |e| drop(tx.send(e)),
    )
    .unwrap();
    let out = dir("cycles");
    for (i, hold_ms) in [0u64, 1, 50, 300, 700, 0, 33, 1000].into_iter().enumerate() {
        assert!(
            p.record(out.clone()),
            "cycle {i}: pipeline refused a recording"
        );
        match event(&rx) {
            Some(Event::Started) => {}
            None => return,
            Some(other) => panic!("cycle {i}: {other:?}"),
        }
        std::thread::sleep(Duration::from_millis(hold_ms));
        p.stop_recording();
        match event(&rx) {
            Some(Event::Finished(Ok(path))) => {
                let (n, key, _) = inspect(&path);
                assert!(n >= 1, "cycle {i}: {} has no video", path.display());
                assert!(key, "cycle {i}: starts mid-GOP");
            }
            other => panic!("cycle {i}: {other:?}"),
        }
    }
    p.join();
    assert!(matches!(event(&rx), Some(Event::Stopped(None))));
    std::fs::remove_dir_all(out).unwrap();
}

/// Replay saves while recording, then quit with a save possibly in flight:
/// nothing is cut short and nothing is reported twice.
#[test]
fn saving_replays_while_recording_then_quitting_loses_nothing() {
    let Some(rig) = Rig::new() else { return };
    let out = dir("saves");
    let (tx, rx) = mpsc::channel();
    let p = Pipeline::start(
        settings(&rig.route),
        Some(Duration::from_secs(10)),
        Some(out.clone()),
        move |e| drop(tx.send(e)),
    )
    .unwrap();
    if !matches!(event(&rx), Some(Event::Started)) {
        return;
    }
    std::thread::sleep(Duration::from_secs(2));
    for _ in 0..3 {
        assert!(p.save_replay(out.clone()));
        std::thread::sleep(Duration::from_millis(100));
    }
    // Quit at once: the saves and the recording are finished first.
    p.join();
    // `join` returned: every event is already there, nothing is still being written.
    let (mut finished, mut saved, mut stopped) = (Vec::new(), Vec::new(), 0);
    for e in rx.try_iter() {
        match e {
            Event::Finished(r) => finished.push(r),
            Event::ReplaySaved(r) => saved.push(r),
            Event::Stopped(e) => {
                assert_eq!(e, None);
                stopped += 1;
            }
            _ => {}
        }
    }
    assert_eq!(stopped, 1);
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(saved.len(), 3, "{saved:?}");
    for path in finished.into_iter().chain(saved).map(|r| r.unwrap()) {
        let (n, key, _) = inspect(&path);
        assert!(n >= 10 && key, "{}: {n} packets, key {key}", path.display());
    }
    std::fs::remove_dir_all(out).unwrap();
}

/// The game freezes for 2 s (loading screen), changes size, then resumes:
/// the file keeps its size and its clock, with no hole in the timeline.
#[test]
fn a_game_pause_and_resize_keep_the_recording_continuous() {
    let Some(rig) = Rig::new() else { return };
    let out = dir("pause");
    let (tx, rx) = mpsc::channel();
    let p = Pipeline::start(settings(&rig.route), None, Some(out.clone()), move |e| {
        drop(tx.send(e))
    })
    .unwrap();
    if !matches!(event(&rx), Some(Event::Started)) {
        return;
    }
    let started = Instant::now();
    std::thread::sleep(Duration::from_secs(1));
    rig.paused.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(2));
    *lock(&rig.size) = (W * 3 / 4, H * 5 / 6);
    rig.paused.store(false, Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(2));
    p.stop_recording();
    let path = match event(&rx) {
        Some(Event::Finished(Ok(path))) => path,
        other => panic!("{other:?}"),
    };
    let wall = started.elapsed().as_secs_f64();
    let (n, key, last) = inspect(&path);
    assert!(key);
    // One packet per tick, constant frame rate: ~30 per wall second.
    assert!(
        (last - wall).abs() < 0.6,
        "file {last:.2} s, wall {wall:.2} s"
    );
    assert!(n as f64 > wall * 28.0, "{n} packets in {wall:.2} s");
    let input = ffmpeg_next::format::input(&path).unwrap();
    let v = input
        .streams()
        .best(ffmpeg_next::media::Type::Video)
        .unwrap();
    let par = v.parameters();
    // SAFETY: valid codec parameters of an open input.
    let (w, h) = unsafe { ((*par.as_ptr()).width, (*par.as_ptr()).height) };
    assert_eq!(
        (w as u32, h as u32),
        (W, H),
        "the file keeps its first size"
    );
    p.join();
    std::fs::remove_dir_all(out).unwrap();
}
