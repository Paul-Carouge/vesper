//! The whole pipeline (GPU encode, MKV, replay buffer) fed by synthetic game
//! frames, hammered with the sequences a user produces. Skipped without a
//! VAAPI GPU (like vesper-encode's and vesper-mux's tests).

use super::producer::{POOL, Producer};
use super::*;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Instant;
use vesper_core::ReleaseGuard;

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
        scene: Scene::main(),
        canvas: Canvas::Auto,
    }
}

/// Feeds the route ~30 frames per second of the current `size` pattern until
/// `live` is false; `paused` skips frames (the game froze or is loading).
fn feeder(
    route: GameFrames,
    live: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    size: Arc<Mutex<(u32, u32)>>,
    counts: Arc<(AtomicUsize, AtomicUsize)>,
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
            let mut frame = unsafe { producer.frame(t as usize % POOL, t, 0, monotonic_now()) };
            let c = counts.clone();
            c.0.fetch_add(1, Ordering::SeqCst);
            let release = std::mem::replace(&mut frame.release, ReleaseGuard::new(|| {}));
            frame.release = ReleaseGuard::new(move || {
                drop(release);
                c.1.fetch_add(1, Ordering::SeqCst);
            });
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
    /// Frames sent, frames released by the pipeline.
    counts: Arc<(AtomicUsize, AtomicUsize)>,
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
        let counts = Arc::new((AtomicUsize::new(0), AtomicUsize::new(0)));
        let feeder = feeder(
            route.clone(),
            live.clone(),
            paused.clone(),
            size.clone(),
            counts.clone(),
        );
        Some(Self {
            route,
            live,
            paused,
            size,
            counts,
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
        None,
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
    let p = Pipeline::start(
        settings(&rig.route),
        None,
        Some(out.clone()),
        None,
        move |e| drop(tx.send(e)),
    )
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

/// The game goes away while only the replay buffer runs: no frozen frames
/// fill it, the seconds before stay; back after longer than the span, the
/// replay holds only the new frames, from a keyframe, with no hole.
#[test]
fn a_gone_game_neither_freezes_nor_ages_the_replay() {
    let Some(rig) = Rig::new() else { return };
    let out = dir("gone");
    let (tx, rx) = mpsc::channel();
    let span = Duration::from_secs(2);
    let p = Pipeline::start(settings(&rig.route), Some(span), None, None, move |e| {
        drop(tx.send(e))
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    rig.paused.store(true, Ordering::Relaxed);
    rig.route.0.gone.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(3));
    assert!(p.save_replay(out.clone()));
    let path = match event(&rx) {
        Some(Event::ReplaySaved(Ok(path))) => path,
        None => return,
        other => panic!("{other:?}"),
    };
    let (n, key, last) = inspect(&path);
    // At most the ~1.5 s of game before it went, no 3 s of repeats after.
    assert!(key && n <= 50, "{n} packets, key {key}");
    assert!(
        last < 1.8,
        "ends at {last:.2} s: repeats after the game left"
    );

    rig.route.0.gone.store(false, Ordering::Relaxed);
    rig.paused.store(false, Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(1));
    assert!(p.save_replay(out.clone()));
    let path = match event(&rx) {
        Some(Event::ReplaySaved(Ok(path))) => path,
        other => panic!("{other:?}"),
    };
    let (n, key, last) = inspect(&path);
    // Only the second since the return: ~30 packets over ~1 s, no hole.
    assert!(key, "starts mid-GOP");
    assert!((20..=40).contains(&n), "{n} packets");
    assert!(last < 1.3, "spans {last:.2} s: old packets across the gap");
    p.join();
    std::fs::remove_dir_all(out).unwrap();
}

/// A recording keeps its constant frame rate while the game is gone, until
/// it is stopped (its stop needs a packet).
#[test]
fn a_recording_keeps_repeating_while_the_game_is_gone() {
    let Some(rig) = Rig::new() else { return };
    let out = dir("gonerec");
    let (tx, rx) = mpsc::channel();
    let p = Pipeline::start(
        settings(&rig.route),
        None,
        Some(out.clone()),
        None,
        move |e| drop(tx.send(e)),
    )
    .unwrap();
    if !matches!(event(&rx), Some(Event::Started)) {
        return;
    }
    let started = Instant::now();
    std::thread::sleep(Duration::from_secs(1));
    rig.paused.store(true, Ordering::Relaxed);
    rig.route.0.gone.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(2));
    p.stop_recording();
    let path = match event(&rx) {
        Some(Event::Finished(Ok(path))) => path,
        other => panic!("{other:?}"),
    };
    let wall = started.elapsed().as_secs_f64();
    let (n, _, last) = inspect(&path);
    assert!(
        (last - wall).abs() < 0.6,
        "file {last:.2} s, wall {wall:.2} s"
    );
    assert!(n as f64 > wall * 28.0, "{n} packets in {wall:.2} s");
    p.join();
    std::fs::remove_dir_all(out).unwrap();
}

// --- scenes ---

use crate::scenes::{Fit, Layer, LayerSource, TextStyle};

/// The size of a file's video.
fn video_size(path: &Path) -> (u32, u32) {
    let input = ffmpeg_next::format::input(path).unwrap();
    let v = input
        .streams()
        .best(ffmpeg_next::media::Type::Video)
        .unwrap();
    let par = v.parameters();
    // SAFETY: valid codec parameters of an open input.
    let (w, h) = unsafe { ((*par.as_ptr()).width, (*par.as_ptr()).height) };
    (w as u32, h as u32)
}

/// 8-bit 4:2:0 planes of a decoded frame.
struct Yuv {
    w: usize,
    planes: [Vec<u8>; 3],
}

impl Yuv {
    fn at(&self, x: usize, y: usize) -> [u8; 3] {
        let cw = self.w / 2;
        [
            self.planes[0][y * self.w + x],
            self.planes[1][(y / 2) * cw + x / 2],
            self.planes[2][(y / 2) * cw + x / 2],
        ]
    }
}

/// Every frame of a file's video, decoded in software.
fn decode_all(path: &Path) -> Vec<Yuv> {
    use ffmpeg_next::{codec, format::Pixel, frame};
    let mut input = ffmpeg_next::format::input(path).unwrap();
    let stream = input
        .streams()
        .best(ffmpeg_next::media::Type::Video)
        .unwrap();
    let index = stream.index();
    let ctx = codec::context::Context::from_parameters(stream.parameters()).unwrap();
    let mut dec = ctx.decoder().video().unwrap();
    let mut out = Vec::new();
    let take = |dec: &mut ffmpeg_next::decoder::Video, out: &mut Vec<Yuv>| {
        let mut f = frame::Video::empty();
        while dec.receive_frame(&mut f).is_ok() {
            assert_eq!(f.format(), Pixel::YUV420P);
            let (w, h) = (f.width() as usize, f.height() as usize);
            let plane = |i: usize, pw: usize, ph: usize| {
                let (data, stride) = (f.data(i), f.stride(i));
                (0..ph)
                    .flat_map(|r| &data[r * stride..r * stride + pw])
                    .copied()
                    .collect::<Vec<u8>>()
            };
            out.push(Yuv {
                w,
                planes: [
                    plane(0, w, h),
                    plane(1, w / 2, h / 2),
                    plane(2, w / 2, h / 2),
                ],
            });
        }
    };
    for (s, packet) in input.packets() {
        if s.index() == index {
            dec.send_packet(&packet).unwrap();
            take(&mut dec, &mut out);
        }
    }
    dec.send_eof().unwrap();
    take(&mut dec, &mut out);
    out
}

/// Pure red in BT.709 limited range, as the encoder converts.
const RED: [f64; 3] = [63.0, 102.0, 240.0];

fn is_red(yuv: [u8; 3]) -> bool {
    (0..3).all(|k| (f64::from(yuv[k]) - RED[k]).abs() <= 16.0)
}

/// A 32x32 opaque red PNG in `dir`.
fn red_png(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("red.png");
    image::RgbaImage::from_pixel(32, 32, image::Rgba([255, 0, 0, 255]))
        .save(&path)
        .unwrap();
    path
}

/// The capture with a red image at x 0.5..0.75, y 0.5..0.75.
fn logo_scene(png: &Path) -> Scene {
    Scene {
        name: "Logo".into(),
        layers: vec![
            Layer::main(),
            Layer {
                source: LayerSource::Image { path: png.into() },
                rect: [0.5, 0.5, 0.25, 0.25],
                fit: Fit::Stretch,
                ..Layer::main()
            },
        ],
    }
}

/// The red rect is in `f`, and not around it.
fn assert_logo(f: &Yuv, what: &str) {
    let (w, h) = (W as usize, H as usize);
    let (x0, y0, x1, y1) = (w / 2, h / 2, w * 3 / 4, h * 3 / 4);
    for (x, y) in [
        (x0 + 16, y0 + 16),
        (x1 - 16, y0 + 16),
        ((x0 + x1) / 2, (y0 + y1) / 2),
        (x0 + 16, y1 - 16),
        (x1 - 16, y1 - 16),
    ] {
        assert!(is_red(f.at(x, y)), "{what}: ({x}, {y}) is {:?}", f.at(x, y));
    }
    // The test pattern may be red by chance at one point, not at all four.
    let outside = [
        (x0 - 8, y0 + 20),
        (x1 + 8, y0 + 20),
        (x0 + 20, y0 - 8),
        (x0 + 20, y1 + 8),
    ];
    let red = outside.iter().filter(|&&(x, y)| is_red(f.at(x, y))).count();
    assert!(red <= 1, "{what}: red outside the rect");
}

/// Starts a pipeline recording into `out`; `None` without an encoder.
fn record_with(settings: Settings, out: &Path) -> Option<(Pipeline, Receiver<Event>)> {
    let (tx, rx) = mpsc::channel();
    let p = Pipeline::start(settings, None, Some(out.into()), None, move |e| {
        drop(tx.send(e))
    })
    .unwrap();
    match event(&rx) {
        Some(Event::Started) => Some((p, rx)),
        None => None,
        Some(other) => panic!("{other:?}"),
    }
}

/// Stops the recording, returns its file.
fn finish(p: &Pipeline, rx: &Receiver<Event>) -> PathBuf {
    p.stop_recording();
    match event(rx) {
        Some(Event::Finished(Ok(path))) => path,
        other => panic!("{other:?}"),
    }
}

/// An image layer is drawn where its rect says, in the file's pictures.
#[test]
fn an_image_overlay_is_recorded_at_its_place() {
    let Some(rig) = Rig::new() else { return };
    let out = dir("logo");
    let png = red_png(&out);
    let scene = logo_scene(&png);
    let settings = Settings {
        scene,
        ..settings(&rig.route)
    };
    let Some((p, rx)) = record_with(settings, &out) else {
        return;
    };
    std::thread::sleep(Duration::from_millis(1500));
    let path = finish(&p, &rx);
    p.join();
    let frames = decode_all(&path);
    assert!(frames.len() > 30, "{} frames", frames.len());
    assert_logo(frames.last().unwrap(), "last frame");
    assert_eq!(video_size(&path), (W, H));
    assert!(rig.route.0.probe.compositors.load(Ordering::Relaxed) >= 1);
    std::fs::remove_dir_all(out).unwrap();
}

/// Scenes switched while recording (capture alone, a logo, a text card
/// without the capture, back): one file, its size and frame rate kept.
#[test]
fn switching_scenes_keeps_one_constant_file() {
    let Some(rig) = Rig::new() else { return };
    let out = dir("switch");
    let png = red_png(&out);
    let Some((p, rx)) = record_with(settings(&rig.route), &out) else {
        return;
    };
    let started = Instant::now();
    let brb = Scene {
        name: "BRB".into(),
        layers: vec![Layer {
            source: LayerSource::Text(TextStyle {
                text: "Be right back".into(),
                ..TextStyle::default()
            }),
            rect: [0.1, 0.4, 0.8, 0.2],
            ..Layer::main()
        }],
    };
    for scene in [logo_scene(&png), brb, Scene::main(), logo_scene(&png)] {
        std::thread::sleep(Duration::from_millis(700));
        assert!(p.set_scene(scene));
    }
    std::thread::sleep(Duration::from_millis(700));
    let path = finish(&p, &rx);
    let wall = started.elapsed().as_secs_f64();
    p.join();
    assert!(matches!(event(&rx), Some(Event::Stopped(None))));
    assert_eq!(video_size(&path), (W, H));
    let (n, key, last) = inspect(&path);
    assert!(key);
    assert!(
        (last - wall).abs() < 0.6,
        "file {last:.2} s, wall {wall:.2} s"
    );
    assert!(n as f64 > wall * 28.0, "{n} packets in {wall:.2} s");
    assert_logo(decode_all(&path).last().unwrap(), "after the switches");
    std::fs::remove_dir_all(out).unwrap();
}

/// A scene that shows the capture alone (hidden or webcam layers aside)
/// records as without scenes: no compositor, no Vulkan device.
#[test]
fn a_capture_only_scene_creates_no_compositor() {
    let Some(rig) = Rig::new() else { return };
    let mut scene = Scene::main();
    scene.layers.push(Layer {
        source: LayerSource::Image {
            path: "/nonexistent.png".into(),
        },
        visible: false,
        ..Layer::main()
    });
    scene.layers.push(Layer {
        source: LayerSource::Camera {
            node: "some-camera".into(),
        },
        ..Layer::main()
    });
    let out = dir("plain");
    let settings = Settings {
        scene,
        ..settings(&rig.route)
    };
    let Some((p, rx)) = record_with(settings, &out) else {
        return;
    };
    std::thread::sleep(Duration::from_millis(800));
    let path = finish(&p, &rx);
    p.join();
    assert_eq!(rig.route.0.probe.compositors.load(Ordering::Relaxed), 0);
    assert!(inspect(&path).0 > 15);
    std::fs::remove_dir_all(out).unwrap();
}

/// The compositor fails mid-recording: the capture alone goes on, the user
/// is told once, the compositor comes back, the recording never stops.
#[test]
fn a_compositor_failure_falls_back_without_losing_the_recording() {
    let Some(rig) = Rig::new() else { return };
    let out = dir("fault");
    let png = red_png(&out);
    let settings = Settings {
        scene: logo_scene(&png),
        ..settings(&rig.route)
    };
    let Some((p, rx)) = record_with(settings, &out) else {
        return;
    };
    let started = Instant::now();
    std::thread::sleep(Duration::from_secs(1));
    rig.route.0.probe.fail.store(true, Ordering::Relaxed);
    match event(&rx) {
        Some(Event::Notice(_)) => {}
        other => panic!("{other:?}"),
    }
    // Retried after 0.3 s (tests), then drawn again.
    std::thread::sleep(Duration::from_millis(1500));
    let path = finish(&p, &rx);
    let wall = started.elapsed().as_secs_f64();
    p.join();
    assert!(matches!(event(&rx), Some(Event::Stopped(None))));
    assert!(rig.route.0.probe.compositors.load(Ordering::Relaxed) >= 2);
    let (n, _, last) = inspect(&path);
    assert!(
        (last - wall).abs() < 0.6,
        "file {last:.2} s, wall {wall:.2} s"
    );
    assert!(n as f64 > wall * 28.0, "{n} packets in {wall:.2} s");
    assert_logo(decode_all(&path).last().unwrap(), "after the retry");
    std::fs::remove_dir_all(out).unwrap();
}

/// CPU time (user + system) of this process's threads named `name`.
fn thread_cpu(name: &str) -> Duration {
    // SAFETY: plain sysconf query.
    let tick = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
    let mut ticks = 0;
    for task in std::fs::read_dir("/proc/self/task").unwrap().flatten() {
        let comm = std::fs::read_to_string(task.path().join("comm")).unwrap_or_default();
        if comm.trim() != name {
            continue;
        }
        let stat = std::fs::read_to_string(task.path().join("stat")).unwrap_or_default();
        // After `(comm)`: state is field 3, utime 14, stime 15.
        let rest = stat.rsplit_once(')').map_or("", |(_, r)| r);
        let f: Vec<u64> = rest
            .split_whitespace()
            .map(|v| v.parse().unwrap_or(0))
            .collect();
        ticks += f.get(11).unwrap_or(&0) + f.get(12).unwrap_or(&0);
    }
    Duration::from_millis(ticks * 1000 / tick)
}

/// Cost of a scene (image + text over the capture) against the capture
/// alone, at 1440p 30 fps. Measures, asserts nothing:
/// `cargo test -p vesper-app --bin vesper scene_cost -- --ignored --nocapture`
#[test]
#[ignore]
fn scene_cost_at_1440p() {
    let Some(rig) = Rig::new() else { return };
    *lock(&rig.size) = (2560, 1440);
    std::thread::sleep(Duration::from_millis(300));
    let out = dir("cost");
    std::fs::create_dir_all(&out).unwrap();
    let png = out.join("logo.png");
    image::RgbaImage::from_fn(1024, 512, |x, y| {
        image::Rgba([(x / 4) as u8, (y / 2) as u8, 200, 220])
    })
    .save(&png)
    .unwrap();
    let scene = Scene {
        name: "Cost".into(),
        layers: vec![
            Layer::main(),
            Layer {
                source: LayerSource::Image { path: png },
                rect: [0.02, 0.02, 0.2, 0.1],
                opacity: 0.9,
                ..Layer::main()
            },
            Layer {
                source: LayerSource::Text(TextStyle {
                    text: "Vesper scene cost".into(),
                    outline: Some((0x000000c0, 3.0)),
                    shadow: Some(0x00000080),
                    ..TextStyle::default()
                }),
                rect: [0.02, 0.88, 0.5, 0.08],
                ..Layer::main()
            },
        ],
    };
    let probe = &rig.route.0.probe;
    for (what, scene) in [("passthrough", Scene::main()), ("image+text", scene)] {
        let settings = Settings {
            scene,
            ..settings(&rig.route)
        };
        let Some((p, rx)) = record_with(settings, &out) else {
            return;
        };
        std::thread::sleep(Duration::from_secs(1));
        let (cpu0, wall0) = (thread_cpu("vesper-recorder"), Instant::now());
        let (n0, g0) = (
            probe.composed.load(Ordering::Relaxed),
            probe.gpu_ns.load(Ordering::Relaxed),
        );
        std::thread::sleep(Duration::from_secs(4));
        let cpu = thread_cpu("vesper-recorder") - cpu0;
        let wall = wall0.elapsed();
        let n = probe.composed.load(Ordering::Relaxed) - n0;
        let g = probe.gpu_ns.load(Ordering::Relaxed) - g0;
        finish(&p, &rx);
        p.join();
        eprintln!(
            "{what}: pipeline thread {:.1}% CPU, {n} compositions, {:.3} ms GPU each",
            cpu.as_secs_f64() / wall.as_secs_f64() * 100.0,
            if n > 0 {
                g as f64 / n as f64 / 1e6
            } else {
                0.0
            }
        );
    }
    std::fs::remove_dir_all(out).unwrap();
}

/// A fixed canvas with a scene that does not show the capture records
/// without any capture frame, at the canvas size.
#[test]
fn a_fixed_canvas_scene_without_the_capture_needs_no_frame() {
    let Some(rig) = Rig::new() else { return };
    rig.paused.store(true, Ordering::Relaxed);
    let out = dir("brb");
    let brb = Scene {
        name: "BRB".into(),
        layers: vec![Layer {
            source: LayerSource::Text(TextStyle {
                text: "Be right back".into(),
                ..TextStyle::default()
            }),
            rect: [0.1, 0.4, 0.8, 0.2],
            ..Layer::main()
        }],
    };
    let settings = Settings {
        scene: brb,
        canvas: Canvas::Fixed(480, 270),
        ..settings(&rig.route)
    };
    let Some((p, rx)) = record_with(settings, &out) else {
        return;
    };
    std::thread::sleep(Duration::from_millis(1500));
    let path = finish(&p, &rx);
    p.join();
    assert_eq!(video_size(&path), (480, 270));
    let (n, key, _) = inspect(&path);
    assert!(key && n > 20, "{n} packets");
    std::fs::remove_dir_all(out).unwrap();
}

/// With an overlay scene, every capture frame goes back to its producer
/// within a tick or so: none is kept while the source sends nothing (the
/// game's ring has 4 buffers, KWin's pool a few).
#[test]
fn composed_scenes_release_every_capture_frame_promptly() {
    for damage_driven in [false, true] {
        let Some(rig) = Rig::new() else { return };
        rig.route
            .0
            .probe
            .base
            .store(damage_driven, Ordering::Relaxed);
        let out = dir(&format!("release-{damage_driven}"));
        let png = red_png(&out);
        let settings = Settings {
            scene: logo_scene(&png),
            ..settings(&rig.route)
        };
        let Some((p, rx)) = record_with(settings, &out) else {
            return;
        };
        let (sent, released) = (&rig.counts.0, &rig.counts.1);
        // While frames flow: at most the parked one and the one being drawn.
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(37));
            let held = sent.load(Ordering::SeqCst) - released.load(Ordering::SeqCst);
            assert!(held <= 2, "{held} frames held");
        }
        rig.paused.store(true, Ordering::Relaxed);
        // Two ticks after the last frame, all are back.
        std::thread::sleep(Duration::from_millis(33 + 2 * 34));
        assert_eq!(
            released.load(Ordering::SeqCst),
            sent.load(Ordering::SeqCst),
            "a frame is kept (damage-driven {damage_driven})"
        );
        // A scene switch without a new frame: drawn from the copy of a
        // damage-driven source, else at the next frame.
        assert!(p.set_scene(Scene::main()));
        std::thread::sleep(Duration::from_millis(200));
        assert!(p.set_scene(logo_scene(&png)));
        std::thread::sleep(Duration::from_millis(300));
        let path = finish(&p, &rx);
        p.join();
        let frames = decode_all(&path);
        let last = frames.last().unwrap();
        if damage_driven {
            assert_logo(last, "redrawn from the copy");
        }
        assert_eq!(released.load(Ordering::SeqCst), sent.load(Ordering::SeqCst));
        drop(rig);
        std::fs::remove_dir_all(out).unwrap();
    }
}

/// Toggling between a composed scene and the capture alone keeps one warm
/// compositor instead of making a device per switch.
#[test]
fn rapid_scene_toggles_reuse_one_compositor() {
    let Some(rig) = Rig::new() else { return };
    let out = dir("toggle");
    let png = red_png(&out);
    let Some((p, rx)) = record_with(settings(&rig.route), &out) else {
        return;
    };
    for i in 0..12 {
        let scene = if i % 2 == 0 {
            logo_scene(&png)
        } else {
            Scene::main()
        };
        assert!(p.set_scene(scene));
        std::thread::sleep(Duration::from_millis(120));
    }
    let path = finish(&p, &rx);
    p.join();
    assert_eq!(rig.route.0.probe.compositors.load(Ordering::Relaxed), 1);
    assert!(inspect(&path).0 > 30);
    std::fs::remove_dir_all(out).unwrap();
}

/// The compositor often busy (texture copies being queued): frames wait
/// one at a time, a newer one replaces an older, which goes back at once;
/// the picture never goes back in time.
#[test]
fn frames_waiting_for_a_busy_compositor_never_go_back_in_time() {
    let Some(rig) = Rig::new() else { return };
    let out = dir("busy");
    let png = red_png(&out);
    let settings = Settings {
        scene: logo_scene(&png),
        ..settings(&rig.route)
    };
    let Some((p, rx)) = record_with(settings, &out) else {
        return;
    };
    let probe = &rig.route.0.probe;
    // Busy while frames come (one waits), free when a newer one comes, then
    // ticks without frames: the waiting older one must not be drawn then.
    for _ in 0..15 {
        probe.busy.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(70));
        probe.busy.store(false, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(40));
        rig.paused.store(true, Ordering::Relaxed);
        // An overlay change while no frame comes: the scene is redrawn.
        assert!(p.set_scene(logo_scene(&png)));
        std::thread::sleep(Duration::from_millis(80));
        rig.paused.store(false, Ordering::Relaxed);
    }
    rig.paused.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(150));
    let (sent, released) = (&rig.counts.0, &rig.counts.1);
    assert_eq!(released.load(Ordering::SeqCst), sent.load(Ordering::SeqCst));
    assert!(
        !probe.went_back.load(Ordering::Relaxed),
        "an older frame composed after a newer"
    );
    let path = finish(&p, &rx);
    p.join();
    assert!(inspect(&path).0 > 40);
    std::fs::remove_dir_all(out).unwrap();
}

/// A composition still in flight (the tick did not wait for it) when the
/// scene becomes the capture alone: its game frame goes back right away,
/// not when the idle compositor is retired.
#[test]
fn leaving_a_scene_releases_a_composition_still_in_flight() {
    let Some(rig) = Rig::new() else { return };
    let out = dir("inflight");
    let png = red_png(&out);
    let settings = Settings {
        scene: logo_scene(&png),
        ..settings(&rig.route)
    };
    let Some((p, rx)) = record_with(settings, &out) else {
        return;
    };
    let probe = &rig.route.0.probe;
    std::thread::sleep(Duration::from_millis(500));
    probe.no_wait.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(300));
    assert!(p.set_scene(Scene::main()));
    rig.paused.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(200));
    let (sent, released) = (&rig.counts.0, &rig.counts.1);
    assert_eq!(released.load(Ordering::SeqCst), sent.load(Ordering::SeqCst));
    let path = finish(&p, &rx);
    p.join();
    assert!(inspect(&path).0 > 20);
    std::fs::remove_dir_all(out).unwrap();
}
