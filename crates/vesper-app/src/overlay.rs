//! Overlay sources of a scene, made ready for the compositor on a helper
//! thread ("vesper-overlay"), never on the pipeline's tick: images decoded
//! (PNG, JPEG, at most 8192 x 8192), text rasterized (an SVG `<text>`
//! through resvg), both premultiplied RGBA, then staged (mips, staging
//! copy) without the compositor's lock. Under the lock only the GPU copy is
//! queued. The thread also creates the compositor (opening a Vulkan device
//! takes a while). The pipeline thread composes under the same lock with
//! `try_lock`: it never waits for this thread.
//!
//! Webcams have a thread of their own ([`Cameras`]).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use resvg::{tiny_skia, usvg};
use vesper_capture::camera::{Camera, CameraFrame, CpuFrame, CpuLayout, Matrix, Range};
use vesper_compose::{Compositor, StagedYuv, YuvLayout, YuvMatrix, YuvPicture};
use vesper_core::VideoFrame;

use crate::recorder::{Event, OnEvent, lock};
use crate::scenes::{Align, TextStyle};

/// Largest image side decoded.
const MAX_IMAGE_SIDE: u32 = 8192;

/// What a texture shows.
#[derive(Debug, Clone, PartialEq)]
pub enum Key {
    Image(PathBuf),
    Text(TextStyle),
}

/// Premultiplied RGBA8 pixels.
#[derive(Debug)]
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Between the pipeline thread and the overlay thread.
#[derive(Default)]
pub struct Shared {
    /// The pipeline drops it after a failure; the next job makes another.
    pub comp: Mutex<Option<Compositor>>,
    /// A texture or a compositor is new since the last [`Overlays::changed`].
    changed: AtomicBool,
    /// The compositor could not be created.
    error: Mutex<Option<String>>,
    /// No compositor is created before then (backoff after a failure).
    suspended: Mutex<Option<Instant>>,
    /// Retired: queued jobs are skipped.
    quit: AtomicBool,
    /// Compositors created.
    pub created: AtomicUsize,
}

/// The textures one canvas needs: `(texture id, source)`.
struct Job {
    canvas: (u32, u32),
    want: Vec<(u32, Key)>,
}

/// The overlay thread, ended when this is dropped (see [`Overlays::retire`]).
pub struct Overlays {
    jobs: Sender<Job>,
    thread: JoinHandle<()>,
    pub shared: Arc<Shared>,
}

impl Overlays {
    pub fn start() -> std::io::Result<Self> {
        let (jobs, rx) = std::sync::mpsc::channel();
        let shared = Arc::new(Shared::default());
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name("vesper-overlay".into())
            .spawn(move || run(&rx, &s))?;
        Ok(Self {
            jobs,
            thread,
            shared,
        })
    }

    /// Ends the thread; it frees the compositor on its way out. Join the
    /// handle (or wait for it to finish) before creating another, so two
    /// devices never coexist.
    pub fn retire(self) -> JoinHandle<()> {
        self.shared.quit.store(true, Ordering::Release);
        drop(self.jobs);
        self.thread
    }

    /// Makes the compositor (a `canvas` one) hold exactly the textures of
    /// `want`, in the background; only the latest request counts.
    pub fn request(&self, canvas: (u32, u32), want: Vec<(u32, Key)>) {
        let _ = self.jobs.send(Job { canvas, want });
    }

    /// No compositor is created for `backoff`, whatever the requests.
    pub fn suspend(&self, backoff: Duration) {
        *lock(&self.shared.suspended) = Some(Instant::now() + backoff);
    }

    /// Something new is drawable since the last call.
    pub fn changed(&self) -> bool {
        self.shared.changed.swap(false, Ordering::AcqRel)
    }

    /// Why the compositor could not be created, once.
    pub fn error(&self) -> Option<String> {
        lock(&self.shared.error).take()
    }
}

fn run(rx: &Receiver<Job>, shared: &Shared) {
    // Sources that could not be loaded for this canvas: a fixed file is read
    // again once the canvas or the compositor changes. Loaded ones are not
    // kept: once uploaded, only the GPU copy remains.
    let mut failed: Vec<Key> = Vec::new();
    let mut uploaded: Vec<u32> = Vec::new();
    let mut canvas = None;
    let quit = || shared.quit.load(Ordering::Acquire);
    while let Ok(mut job) = rx.recv() {
        while let Ok(newer) = rx.try_recv() {
            job = newer;
        }
        if quit() {
            break;
        }
        if canvas != Some(job.canvas) {
            failed.clear();
            canvas = Some(job.canvas);
        }
        let (w, h) = job.canvas;
        let missing = lock(&shared.comp)
            .as_ref()
            .is_none_or(|c| (c.width(), c.height()) != job.canvas);
        if missing {
            if lock(&shared.suspended).is_some_and(|t| Instant::now() < t) {
                // The pipeline asks again once the backoff is over.
                continue;
            }
            // The old one first: its VRAM is freed before the new one's.
            // Not under the lock: its teardown waits for the GPU.
            let old = lock(&shared.comp).take();
            drop(old);
            uploaded.clear();
            failed.clear();
            // A compositor still being freed elsewhere goes first (unless
            // stuck in the driver): one device at a time.
            while crate::recorder::retiring()
                .is_some_and(|age| age < crate::recorder::RETIRING_STUCK)
            {
                if quit() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            match Compositor::new(w, h) {
                Ok(c) => {
                    log!("scene compositor: {w}x{h}");
                    *lock(&shared.comp) = Some(c);
                    shared.created.fetch_add(1, Ordering::Relaxed);
                    shared.changed.store(true, Ordering::Release);
                }
                Err(e) => {
                    *lock(&shared.error) = Some(e.to_string());
                    continue;
                }
            }
        }
        let Some(uploader) = lock(&shared.comp).as_ref().map(Compositor::uploader) else {
            continue;
        };
        for (id, key) in &job.want {
            if quit() {
                break;
            }
            if uploaded.contains(id) || failed.contains(key) {
                continue;
            }
            // Decoded and staged without the lock.
            let staged = load(key).and_then(|r| {
                uploader
                    .prepare(r.width, r.height, &r.rgba)
                    .map_err(|e| e.to_string())
            });
            let staged = match staged {
                Ok(s) => s,
                Err(e) => {
                    log!("scene overlay: {e}");
                    failed.push(key.clone());
                    continue;
                }
            };
            let mut comp = lock(&shared.comp);
            let Some(c) = comp.as_mut() else { break };
            match c.set_staged(*id, staged) {
                Ok(()) => {
                    uploaded.push(*id);
                    shared.changed.store(true, Ordering::Release);
                }
                Err(e) => log!("scene overlay: upload failed: {e}"),
            }
        }
        if let Some(c) = lock(&shared.comp).as_mut() {
            uploaded.retain(|id| {
                let keep = job.want.iter().any(|(w, _)| w == id);
                if !keep {
                    c.remove_texture(*id);
                }
                keep
            });
        }
    }
    // The pipeline is done with it: the compositor dies here, never on its
    // tick.
    let old = lock(&shared.comp).take();
    drop(old);
}

/// How long a camera stays open after its scene is left: switching back
/// soon finds it streaming.
#[cfg(not(test))]
pub const CAMERA_WARM: Duration = Duration::from_secs(10);
#[cfg(test)]
pub const CAMERA_WARM: Duration = Duration::from_secs(1);
/// How often open cameras are asked for a picture (`Camera` has no wakeup).
const CAMERA_POLL: Duration = Duration::from_millis(10);
const CAMERA_RETRY_FIRST: Duration = Duration::from_secs(1);
const CAMERA_RETRY_MAX: Duration = Duration::from_secs(10);

/// A camera's news for the pipeline thread.
pub enum Feed {
    /// A picture decoded on the GPU.
    Gpu(VideoFrame),
    /// A YUV picture, staged for the compositor.
    Yuv(StagedYuv),
    /// No picture any more (the camera failed, or left the scene): hide it.
    Gone,
}

#[derive(Default)]
struct Want {
    /// `(camera id, node name)` of the cameras the scene shows.
    list: Vec<(u32, String)>,
    changed: bool,
}

/// Between the pipeline thread and the camera thread.
#[derive(Default)]
struct CameraShared {
    want: Mutex<Want>,
    /// Signalled with `want` changed or `quit` set.
    wake: Condvar,
    /// Where YUV pictures are staged.
    overlays: Mutex<Option<Arc<Shared>>>,
    /// The latest news of each camera.
    mailbox: Mutex<Vec<(u32, Feed)>>,
    news: AtomicBool,
    quit: AtomicBool,
}

impl CameraShared {
    fn publish(&self, id: u32, feed: Feed) {
        let old = {
            let mut m = lock(&self.mailbox);
            match m.iter_mut().find(|(i, _)| *i == id) {
                Some(slot) => Some(std::mem::replace(&mut slot.1, feed)),
                None => {
                    m.push((id, feed));
                    None
                }
            }
        };
        // Outside the lock: a GPU picture's release unrefs a VA surface.
        drop(old);
        self.news.store(true, Ordering::Release);
    }
}

/// The cameras of a pipeline, streamed on their own thread
/// ("vesper-camfeed"): opened while the scene shows them (and
/// [`CAMERA_WARM`] after), reopened with a backoff when they fail, closed
/// when this is dropped. Latest picture wins: older ones go back at once.
/// YUV pictures are staged there, off the pipeline's tick. Sends
/// [`Event::Camera`] when the first opens and the last closes, and one
/// notice per failure streak.
pub struct Cameras {
    shared: Arc<CameraShared>,
    thread: Option<JoinHandle<()>>,
}

impl Cameras {
    pub fn start(on_event: OnEvent) -> std::io::Result<Self> {
        let shared = Arc::new(CameraShared::default());
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name("vesper-camfeed".into())
            .spawn(move || feed(&s, &on_event))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// The cameras the scene shows now.
    pub fn want(&self, list: Vec<(u32, String)>) {
        *lock(&self.shared.want) = Want {
            list,
            changed: true,
        };
        self.shared.wake.notify_all();
    }

    /// The overlay thread whose compositor YUV pictures are staged for.
    pub fn set_overlays(&self, overlays: Option<Arc<Shared>>) {
        let none = overlays.is_none();
        *lock(&self.shared.overlays) = overlays;
        if none {
            // Staged for a compositor on its way out.
            let stale: Vec<_> = {
                let mut m = lock(&self.shared.mailbox);
                let (yuv, keep) = m.drain(..).partition(|(_, f)| matches!(f, Feed::Yuv(_)));
                *m = keep;
                yuv
            };
            drop(stale);
        }
    }

    /// News since the last call.
    pub fn news(&self) -> bool {
        self.shared.news.swap(false, Ordering::AcqRel)
    }

    /// The latest news of each camera.
    pub fn take(&self) -> Vec<(u32, Feed)> {
        std::mem::take(&mut *lock(&self.shared.mailbox))
    }
}

impl Drop for Cameras {
    /// Closes the cameras: `Camera` never waits, so this is quick.
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Release);
        {
            let _want = lock(&self.shared.want);
            self.shared.wake.notify_all();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A camera as the camera thread uses it (a fake one in tests).
trait CameraSource: Send {
    fn take(&self) -> Result<Option<CameraFrame>, String>;
    fn software_decode(&self) -> bool;
}

impl CameraSource for Camera {
    fn take(&self) -> Result<Option<CameraFrame>, String> {
        Camera::take(self).map_err(|e| e.to_string())
    }

    fn software_decode(&self) -> bool {
        Camera::software_decode(self)
    }
}

fn open_camera(name: &str) -> Result<Box<dyn CameraSource>, String> {
    #[cfg(test)]
    if let Some(c) = fake::open(name) {
        return c;
    }
    // Tests never open a real webcam: only the robustness lab's fake node.
    #[cfg(test)]
    if std::env::var("VESPER_LAB_CAMERA").ok().as_deref() != Some(name) {
        return Err(format!("{name:?} is not a test camera"));
    }
    Camera::open(name)
        .map(|c| Box::new(c) as Box<dyn CameraSource>)
        .map_err(|e| e.to_string())
}

struct OpenCamera {
    id: u32,
    cam: Box<dyn CameraSource>,
    /// Out of the scene since then.
    idle_since: Option<Instant>,
    cpu_told: bool,
    /// It sent a picture: it is really streaming (its light is on).
    live: bool,
}

/// A failed camera: its id, next try, current backoff.
type Retry = (u32, Instant, Duration);

/// The camera thread.
fn feed(s: &CameraShared, on_event: &OnEvent) {
    let mut open: Vec<OpenCamera> = Vec::new();
    let mut retry: Vec<Retry> = Vec::new();
    // Cameras the user was told about, until they stream again.
    let mut told: Vec<u32> = Vec::new();
    let mut want: Vec<(u32, String)> = Vec::new();
    let mut on = false;
    while !s.quit.load(Ordering::Acquire) {
        {
            let mut w = lock(&s.want);
            if std::mem::take(&mut w.changed) {
                want.clone_from(&w.list);
            }
        }
        let now = Instant::now();
        let wanted = |id: u32| want.iter().any(|(w, _)| *w == id);
        for o in &mut open {
            if wanted(o.id) {
                o.idle_since = None;
            } else if o.idle_since.is_none() {
                o.idle_since = Some(now);
                s.publish(o.id, Feed::Gone);
            }
        }
        // Closed past the warm delay (dropping a `Camera` never waits).
        open.retain(|o| o.idle_since.is_none_or(|t| now - t < CAMERA_WARM));
        // A camera back in a scene is tried at once, its failure told again.
        retry.retain(|(id, _, _)| wanted(*id));
        told.retain(|id| wanted(*id));
        let fail = |id: u32, why: String, retry: &mut Vec<Retry>, told: &mut Vec<u32>| {
            let backoff = retry
                .iter()
                .find(|r| r.0 == id)
                .map_or(CAMERA_RETRY_FIRST, |r| (r.2 * 2).min(CAMERA_RETRY_MAX));
            retry.retain(|r| r.0 != id);
            retry.push((id, now + backoff, backoff));
            log!("webcam {id}: {why}; retrying in {backoff:?}");
            if !told.contains(&id) {
                told.push(id);
                on_event(Event::Notice(format!(
                    "Webcam hidden: {why}. Vesper keeps trying."
                )));
            }
            s.publish(id, Feed::Gone);
        };
        for (id, name) in &want {
            let due = retry.iter().all(|r| r.0 != *id || r.1 <= now);
            if !due || open.iter().any(|o| o.id == *id) {
                continue;
            }
            match open_camera(name) {
                Ok(cam) => {
                    log!("webcam {id}: opening {name:?}");
                    open.push(OpenCamera {
                        id: *id,
                        cam,
                        idle_since: None,
                        cpu_told: false,
                        live: false,
                    });
                }
                Err(e) => fail(*id, e, &mut retry, &mut told),
            }
        }
        let mut i = 0;
        while i < open.len() {
            let o = &mut open[i];
            let frame = match o.cam.take() {
                Ok(f) => f,
                Err(e) => {
                    let id = o.id;
                    open.remove(i);
                    fail(id, e, &mut retry, &mut told);
                    continue;
                }
            };
            i += 1;
            let Some(frame) = frame else { continue };
            o.live = true;
            if o.idle_since.is_some() {
                continue;
            }
            if let Some(r) = retry.iter().position(|r| r.0 == o.id) {
                retry.remove(r);
            }
            if let Some(t) = told.iter().position(|t| *t == o.id) {
                told.remove(t);
                log!("webcam {}: streaming again", o.id);
            }
            if !o.cpu_told && o.cam.software_decode() {
                o.cpu_told = true;
                on_event(Event::Notice(
                    "Webcam decoded on the CPU: the GPU could not decode it".into(),
                ));
            }
            match frame {
                CameraFrame::Gpu(f) => s.publish(o.id, Feed::Gpu(f)),
                CameraFrame::Cpu(f) => {
                    if let Some(staged) = stage_yuv(s, &f) {
                        s.publish(o.id, Feed::Yuv(staged));
                    }
                }
            }
        }
        // On while a camera streams: a missing one being retried is not.
        let streaming = open.iter().any(|o| o.live);
        if on != streaming {
            on = streaming;
            on_event(Event::Camera(on));
        }
        let w = lock(&s.want);
        if w.changed || s.quit.load(Ordering::Acquire) {
            continue;
        }
        let next_retry = want
            .iter()
            .filter_map(|(id, _)| retry.iter().find(|r| r.0 == *id).map(|r| r.1))
            .min();
        let warm = open
            .iter()
            .filter_map(|o| o.idle_since)
            .map(|t| t + CAMERA_WARM)
            .min();
        let timeout = if open.iter().any(|o| o.idle_since.is_none()) {
            Some(CAMERA_POLL)
        } else {
            // Idle: only a retry, a warm camera to close or the scene matter.
            [next_retry, warm]
                .into_iter()
                .flatten()
                .min()
                .map(|t| t.saturating_duration_since(Instant::now()).max(CAMERA_POLL))
        };
        // A poisoned lock only ends the wait.
        match timeout {
            Some(t) => drop(s.wake.wait_timeout(w, t)),
            None => drop(s.wake.wait(w)),
        }
    }
    drop(open);
    if on {
        on_event(Event::Camera(false));
    }
}

/// Stages a YUV picture for the current compositor; `None` without one (or
/// while it is busy: the next picture comes soon).
fn stage_yuv(s: &CameraShared, f: &CpuFrame) -> Option<StagedYuv> {
    let uploader = {
        let overlays = lock(&s.overlays).clone()?;
        let comp = overlays.comp.try_lock().ok()?;
        comp.as_ref()?.uploader()
    };
    let planes: Vec<(usize, usize)> = f.planes.iter().map(|p| (p.offset, p.stride)).collect();
    let picture = YuvPicture {
        width: f.width,
        height: f.height,
        layout: match f.layout {
            CpuLayout::Nv12 => YuvLayout::Nv12,
            CpuLayout::Yuy2 => YuvLayout::Yuy2,
        },
        planes: &planes,
        data: &f.data,
        matrix: match f.matrix {
            Matrix::Bt601 => YuvMatrix::Bt601,
            Matrix::Bt709 => YuvMatrix::Bt709,
            Matrix::Bt2020 => YuvMatrix::Bt2020,
        },
        full_range: f.range == Range::Full,
    };
    match uploader.prepare_yuv(&picture) {
        Ok(s) => Some(s),
        Err(e) => {
            log!("webcam picture dropped: {e}");
            None
        }
    }
}

/// Fake cameras for the pipeline tests: `fake:` node names.
#[cfg(test)]
pub mod fake {
    use super::*;

    #[derive(Default)]
    pub struct FakeCamera {
        /// What the next `take` returns; an error ends the stream.
        next: Mutex<Option<Result<CameraFrame, String>>>,
        /// Opens fail with this.
        pub refuse: Mutex<Option<String>>,
        pub opens: AtomicUsize,
        /// Open and not dropped.
        pub open_now: AtomicUsize,
    }

    impl FakeCamera {
        /// A new picture; an unread older one goes back.
        pub fn send(&self, frame: CameraFrame) {
            let old = lock(&self.next).replace(Ok(frame));
            drop(old);
        }

        /// Ends the stream, as an unplugged camera does.
        pub fn end(&self, why: &str) {
            let old = lock(&self.next).replace(Err(why.into()));
            drop(old);
        }
    }

    static CAMERAS: Mutex<Vec<(String, Arc<FakeCamera>)>> = Mutex::new(Vec::new());

    /// A fake camera named `fake:<name>`.
    pub fn add(name: &str) -> Arc<FakeCamera> {
        let cam = Arc::new(FakeCamera::default());
        lock(&CAMERAS).push((format!("fake:{name}"), cam.clone()));
        cam
    }

    struct Handle(Arc<FakeCamera>);

    impl Drop for Handle {
        fn drop(&mut self) {
            self.0.open_now.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl CameraSource for Handle {
        fn take(&self) -> Result<Option<CameraFrame>, String> {
            lock(&self.0.next).take().transpose()
        }

        fn software_decode(&self) -> bool {
            false
        }
    }

    pub(super) fn open(name: &str) -> Option<Result<Box<dyn CameraSource>, String>> {
        let cam = lock(&CAMERAS)
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, c)| c.clone())?;
        cam.opens.fetch_add(1, Ordering::SeqCst);
        if let Some(why) = lock(&cam.refuse).clone() {
            return Some(Err(why));
        }
        cam.open_now.fetch_add(1, Ordering::SeqCst);
        Some(Ok(Box::new(Handle(cam))))
    }
}

fn load(key: &Key) -> Result<Raster, String> {
    match key {
        Key::Image(path) => image(path),
        Key::Text(style) => text(style),
    }
}

/// Decodes a PNG or JPEG file, premultiplied.
pub fn image(path: &Path) -> Result<Raster, String> {
    let what = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    // Never a FIFO or a device: opening one may block for ever.
    if !std::fs::metadata(path).map_err(|e| what(&e))?.is_file() {
        return Err(what(&"not a file"));
    }
    let mut reader = image::ImageReader::open(path)
        .and_then(image::ImageReader::with_guessed_format)
        .map_err(|e| what(&e))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE);
    limits.max_image_height = Some(MAX_IMAGE_SIDE);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| what(&e))?.into_rgba8();
    let (width, height) = img.dimensions();
    let mut rgba = img.into_raw();
    for px in rgba.as_chunks_mut::<4>().0 {
        let a = u16::from(px[3]);
        for c in &mut px[..3] {
            *c = ((u16::from(*c) * a + 127) / 255) as u8;
        }
    }
    Ok(Raster {
        width,
        height,
        rgba,
    })
}

/// The system fonts, scanned once (on the first text, on the overlay thread).
pub fn fonts() -> Arc<usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_system_fonts();
            // fontdb's default sans-serif is Arial: take one this system has.
            let has = |name: &str| {
                db.faces()
                    .any(|f| f.families.iter().any(|(n, _)| n == name))
            };
            let sans = ["Noto Sans", "DejaVu Sans", "Liberation Sans", "Cantarell"]
                .into_iter()
                .find(|n| has(n))
                .map(str::to_owned)
                .or_else(|| {
                    db.faces()
                        .next()
                        .and_then(|f| Some(f.families.first()?.0.clone()))
                });
            if let Some(sans) = sans {
                db.set_sans_serif_family(sans);
            }
            log!("fonts: {} faces", db.len());
            Arc::new(db)
        })
        .clone()
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// `fill="#rrggbb" fill-opacity="a"` style attributes of an RGBA colour.
fn paint(attr: &str, rgba: u32) -> String {
    format!(
        "{attr}=\"#{:06x}\" {attr}-opacity=\"{}\"",
        rgba >> 8,
        f64::from(rgba & 0xff) / 255.0
    )
}

/// The SVG of a text layer: one `<tspan>` per line, outline under the fill,
/// optional drop shadow, on a canvas wide enough for any line.
fn text_svg(s: &TextStyle) -> String {
    const WIDTH: f32 = 16384.0;
    let size = s.size;
    let line = size * 1.25;
    let outline = s.outline.map_or(0.0, |(_, w)| w);
    let pad = size * 0.5 + outline;
    // Bounded raster: the pixmap is capped at MAX_IMAGE_SIDE anyway.
    let lines: Vec<&str> = s.text.split('\n').take(64).collect();
    let height = pad * 2.0 + line * lines.len() as f32;
    let (x, anchor) = match s.align {
        Align::Left => (pad, "start"),
        Align::Center => (WIDTH / 2.0, "middle"),
        Align::Right => (WIDTH - pad, "end"),
    };
    let family = if s.font.is_empty() {
        "sans-serif".to_owned()
    } else {
        format!("'{}', sans-serif", xml_escape(&s.font))
    };
    let mut svg =
        format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{WIDTH}\" height=\"{height}\">");
    let mut attrs = format!(
        "font-family=\"{family}\" font-size=\"{size}\" text-anchor=\"{anchor}\" {}",
        paint("fill", s.color)
    );
    if let Some((c, w)) = s.outline.filter(|(_, w)| *w > 0.0) {
        // Twice as wide: the fill drawn over it hides the inner half.
        attrs += &format!(
            " {} stroke-width=\"{}\" stroke-linejoin=\"round\" paint-order=\"stroke\"",
            paint("stroke", c),
            w * 2.0
        );
    }
    if let Some(c) = s.shadow {
        let d = (size * 0.06).max(1.0);
        svg += &format!(
            "<defs><filter id=\"s\" x=\"-20%\" y=\"-20%\" width=\"140%\" height=\"140%\">\
             <feDropShadow dx=\"{d}\" dy=\"{d}\" stdDeviation=\"{d}\" \
             flood-color=\"#{:06x}\" flood-opacity=\"{}\"/></filter></defs>",
            c >> 8,
            f64::from(c & 0xff) / 255.0
        );
        attrs += " filter=\"url(#s)\"";
    }
    svg += &format!("<text xml:space=\"preserve\" {attrs}>");
    for (i, l) in lines.iter().enumerate() {
        let y = pad + size + line * i as f32;
        svg += &format!("<tspan x=\"{x}\" y=\"{y}\">{}</tspan>", xml_escape(l));
    }
    svg + "</text></svg>"
}

/// Rasterizes a text layer, cropped to its ink (outline and shadow
/// included). An error for text with nothing to draw.
pub fn text(style: &TextStyle) -> Result<Raster, String> {
    let opt = usvg::Options {
        fontdb: fonts(),
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_str(&text_svg(style), &opt).map_err(|e| format!("text: {e}"))?;
    let bbox = tree.root().abs_layer_bounding_box();
    if bbox.width() > MAX_IMAGE_SIDE as f32 || bbox.height() > MAX_IMAGE_SIDE as f32 {
        log!(
            "text: {:.0}x{:.0} px, cut to {MAX_IMAGE_SIDE} px",
            bbox.width(),
            bbox.height()
        );
    }
    let side = |v: f32| (v.ceil() as u32).clamp(1, MAX_IMAGE_SIDE);
    let (width, height) = (side(bbox.width()), side(bbox.height()));
    let mut pixmap = tiny_skia::Pixmap::new(width, height).ok_or("text: bad size")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_translate(-bbox.x(), -bbox.y()),
        &mut pixmap.as_mut(),
    );
    let rgba = pixmap.take();
    if rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
        return Err("text: nothing to draw".into());
    }
    Ok(Raster {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A camera stays open (light on) for [`CAMERA_WARM`] after its scene
    /// is left, then closes; dropping [`Cameras`] closes at once.
    #[test]
    fn cameras_stay_warm_after_their_scene_then_close() {
        let cam = fake::add("warm");
        let (tx, rx) = std::sync::mpsc::channel();
        let on_event: OnEvent = Arc::new(move |e| {
            if let Event::Camera(on) = e {
                let _ = tx.send(on);
            }
        });
        let picture = || {
            CameraFrame::Cpu(CpuFrame {
                width: 2,
                height: 2,
                layout: CpuLayout::Yuy2,
                planes: vec![vesper_capture::camera::CpuPlane {
                    offset: 0,
                    stride: 4,
                }],
                data: vec![128; 8],
                matrix: Matrix::Bt601,
                range: Range::Limited,
                pts: Duration::ZERO,
            })
        };
        let wait = Duration::from_secs(2);
        let cams = Cameras::start(on_event).unwrap();
        cams.want(vec![(0, "fake:warm".into())]);
        cam.send(picture());
        assert_eq!(rx.recv_timeout(wait), Ok(true));
        assert_eq!(cam.open_now.load(Ordering::SeqCst), 1);
        cams.want(Vec::new());
        std::thread::sleep(CAMERA_WARM / 2);
        assert_eq!(cam.open_now.load(Ordering::SeqCst), 1, "kept warm");
        assert!(
            matches!(cams.take()[..], [(0, Feed::Gone)]),
            "hidden at once"
        );
        assert_eq!(rx.recv_timeout(wait), Ok(false));
        assert_eq!(cam.open_now.load(Ordering::SeqCst), 0);
        // Back in a scene, then the pipeline ends.
        cams.want(vec![(0, "fake:warm".into())]);
        cam.send(picture());
        assert_eq!(rx.recv_timeout(wait), Ok(true));
        drop(cams);
        assert_eq!(rx.try_recv(), Ok(false));
        assert_eq!(cam.open_now.load(Ordering::SeqCst), 0);
        assert_eq!(cam.opens.load(Ordering::SeqCst), 2);
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vesper-overlay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn images_decode_premultiplied() {
        let path = temp("half.png");
        let img = image::RgbaImage::from_fn(4, 2, |x, _| {
            if x < 2 {
                image::Rgba([200, 100, 50, 255])
            } else {
                image::Rgba([200, 100, 50, 128])
            }
        });
        img.save(&path).unwrap();
        let r = image(&path).unwrap();
        assert_eq!((r.width, r.height), (4, 2));
        assert_eq!(r.rgba[..4], [200, 100, 50, 255]);
        assert_eq!(r.rgba[8..12], [100, 50, 25, 128]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn bad_and_huge_images_are_errors() {
        let path = temp("huge.png");
        image::GrayImage::new(MAX_IMAGE_SIDE + 1, 1)
            .save(&path)
            .unwrap();
        assert!(image(&path).is_err());
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nnot really").unwrap();
        assert!(image(&path).is_err());
        assert!(image(Path::new("/nonexistent/vesper.png")).is_err());
        assert!(image(Path::new("/dev/null")).is_err());
        assert!(image(&std::env::temp_dir()).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn hostile_text_is_escaped() {
        let svg = text_svg(&TextStyle {
            text: "</text><script>&\"'\u{1}".into(),
            ..TextStyle::default()
        });
        assert!(svg.contains("&lt;/text&gt;&lt;script&gt;&amp;&quot;&apos;"));
        assert!(!svg.contains('\u{1}'));
    }

    /// Needs a font on the system (CI images have DejaVu).
    #[test]
    fn text_rasterizes_with_outline_and_shadow() {
        if fonts().is_empty() {
            eprintln!("no font, skipping");
            return;
        }
        let style = TextStyle {
            text: "Hello | <world>\nline two".into(),
            size: 32.0,
            color: 0xff0000ff,
            outline: Some((0x000000ff, 2.0)),
            shadow: Some(0x00000080),
            ..TextStyle::default()
        };
        let r = text(&style).unwrap();
        assert!(r.width > 100 && r.height > 60, "{}x{}", r.width, r.height);
        assert!(r.width < 2000, "cropped to the ink: {}", r.width);
        let px = r.rgba.as_chunks::<4>().0;
        assert!(
            px.iter().any(|p| p[3] == 255 && p[0] > 200 && p[1] < 40),
            "red fill"
        );
        assert!(px.iter().any(|p| p[3] == 255 && p[0] < 40), "black outline");
        assert!(
            px.iter()
                .all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3])
        );
        let empty = TextStyle {
            text: " \n ".into(),
            ..TextStyle::default()
        };
        assert!(text(&empty).is_err());
    }
}
