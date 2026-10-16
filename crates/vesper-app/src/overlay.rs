//! Overlay sources of a scene, made ready for the compositor on a helper
//! thread ("vesper-overlay"), never on the pipeline's tick: images decoded
//! (PNG, JPEG, at most 8192 x 8192), text rasterized (an SVG `<text>`
//! through resvg), both premultiplied RGBA, then staged (mips, staging
//! copy) without the compositor's lock. Under the lock only the GPU copy is
//! queued. The thread also creates the compositor (opening a Vulkan device
//! takes a while). The pipeline thread composes under the same lock with
//! `try_lock`: it never waits for this thread.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use resvg::{tiny_skia, usvg};
use vesper_compose::Compositor;

use crate::recorder::lock;
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
    while let Ok(mut job) = rx.recv() {
        while let Ok(newer) = rx.try_recv() {
            job = newer;
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
