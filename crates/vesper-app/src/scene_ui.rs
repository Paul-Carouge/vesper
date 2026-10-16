//! The scene chips and the scene editor (`ui/scenes.slint`). The scenes are
//! UI state: `SceneStore.scenes` holds them (read back by `Prefs::from_ui`),
//! every change goes through [`show`] or an in-place row update, and the
//! active scene goes to the running pipeline at once (also while recording
//! or streaming). The canvas size restarts the replay buffer: it is locked
//! while recording or streaming.
//!
//! The editor's preview draws the layers itself: images and text are
//! rasterized on a helper thread and cached; the capture and webcams are
//! stand-ins. While the editor shows the recorded scene, the window is
//! visible and a compositor runs, it shows the composed picture instead
//! ([`crate::recorder::PREVIEW`], about 10 fps at 480 x 270).

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use slint::{
    Color, ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel,
};

use crate::overlay::{self, Key};
use crate::prefs::Prefs;
use crate::scenes::{
    Align, Canvas, Drag, Fit, Handle, Layer, LayerSource, MAX_LAYERS, Scene, Scenes, TextStyle,
};
use crate::{
    AppWindow, CameraChoice, Games, LayerKind, LayerRow, LiveState, RecState, SceneRow, SceneStore,
    Shared, flash_notice,
};

/// Rasters kept for the preview (images and text seen lately).
const RASTER_CACHE: usize = 48;
/// Longest side of an image raster for the preview, pixels.
const PREVIEW_SIDE: u32 = 960;
/// Without a composed picture this long, the preview draws the layers.
const LIVE_STALE: Duration = Duration::from_millis(600);
/// Font families listed at most (the search narrows them).
const MAX_FONTS: usize = 200;

thread_local! {
    /// Rasters of images and text for the preview, newest last.
    static RASTERS: RefCell<Vec<(Key, Image)>> = const { RefCell::new(Vec::new()) };
    /// Rasters asked for and not back yet.
    static PENDING: RefCell<Vec<Key>> = const { RefCell::new(Vec::new()) };
    static RASTERIZER: RefCell<Option<Sender<Key>>> = const { RefCell::new(None) };
    /// The layer being dragged on the preview: scene, layer, drag.
    static DRAG: RefCell<Option<(usize, usize, Drag)>> = const { RefCell::new(None) };
    /// Webcams found, for their names.
    static CAMERAS: RefCell<Vec<CameraChoice>> = const { RefCell::new(Vec::new()) };
    /// Every font family, once listed.
    static FONTS: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
    static FONT_QUERY: RefCell<String> = const { RefCell::new(String::new()) };
    /// When the last composed picture came.
    static LIVE_AT: Cell<Option<Instant>> = const { Cell::new(None) };
    static PIPELINE: RefCell<Option<Shared>> = const { RefCell::new(None) };
    static TICK: slint::Timer = slint::Timer::default();
    static SAVE: slint::Timer = slint::Timer::default();
}

fn fit_index(fit: Fit) -> i32 {
    match fit {
        Fit::Contain => 0,
        Fit::Cover => 1,
        Fit::Stretch => 2,
    }
}

fn color(rgba: u32) -> Color {
    let [r, g, b, a] = rgba.to_be_bytes();
    Color::from_argb_u8(a, r, g, b)
}

fn rgba(c: Color) -> u32 {
    u32::from_be_bytes([c.red(), c.green(), c.blue(), c.alpha()])
}

/// What the layer list calls a layer.
fn label(source: &LayerSource) -> String {
    match source {
        LayerSource::Main => "Capture".into(),
        LayerSource::Camera { node } => CAMERAS.with_borrow(|c| {
            c.iter()
                .find(|c| c.node == node.as_str())
                .map_or_else(|| node.clone(), |c| c.label.to_string())
        }),
        LayerSource::Image { path } => path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into(),
        ),
        LayerSource::Text(style) => {
            let line = style.text.lines().map(str::trim).find(|l| !l.is_empty());
            let line: String = line.unwrap_or("Text").chars().take(48).collect();
            line
        }
    }
}

/// The preview's raster key of a layer.
fn key(source: &LayerSource) -> Option<Key> {
    match source {
        LayerSource::Image { path } => Some(Key::Image(path.clone())),
        LayerSource::Text(style) => Some(Key::Text(style.clone())),
        _ => None,
    }
}

fn row(l: &Layer) -> LayerRow {
    let style = match &l.source {
        LayerSource::Text(s) => s.clone(),
        _ => TextStyle::default(),
    };
    let (kind, value) = match &l.source {
        LayerSource::Main => (LayerKind::Main, String::new()),
        LayerSource::Camera { node } => (LayerKind::Camera, node.clone()),
        LayerSource::Image { path } => (LayerKind::Image, path.display().to_string()),
        LayerSource::Text(s) => (LayerKind::Text, s.text.clone()),
    };
    let (outline, width) = style.outline.unwrap_or((0x0000_00c0, 3.0));
    LayerRow {
        kind,
        label: label(&l.source).into(),
        visible: l.visible,
        opacity: l.opacity,
        x: l.rect[0],
        y: l.rect[1],
        w: l.rect[2],
        h: l.rect[3],
        crop_left: l.crop[0],
        crop_top: l.crop[1],
        crop_right: l.crop[2],
        crop_bottom: l.crop[3],
        fit: fit_index(l.fit),
        value: value.into(),
        font: style.font.into(),
        size: style.size,
        color: color(style.color),
        outline: style.outline.is_some(),
        outline_color: color(outline),
        outline_width: width,
        shadow: style.shadow.is_some(),
        shadow_color: color(style.shadow.unwrap_or(0x0000_00a0)),
        align: match style.align {
            Align::Left => 0,
            Align::Center => 1,
            Align::Right => 2,
        },
        raster: key(&l.source).and_then(cached).unwrap_or_default(),
    }
}

/// The layer a row shows, with its values made valid; `None` for an image
/// without an absolute path or a camera without a node.
fn layer(r: &LayerRow) -> Option<Layer> {
    let source = match r.kind {
        LayerKind::Main => LayerSource::Main,
        LayerKind::Camera => {
            let node = r.value.to_string();
            let ok = (1..=256).contains(&node.len()) && !node.chars().any(char::is_control);
            ok.then_some(LayerSource::Camera { node })?
        }
        LayerKind::Image => {
            let path = PathBuf::from(r.value.as_str());
            path.is_absolute().then_some(LayerSource::Image { path })?
        }
        LayerKind::Text => {
            let text: String = r
                .value
                .chars()
                .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
                .take(1024)
                .collect();
            let font: String = r.font.chars().take(64).collect();
            let bad_font = font
                .chars()
                .any(|c| c.is_control() || ",|=\"'<>&".contains(c));
            LayerSource::Text(TextStyle {
                text,
                font: if bad_font { String::new() } else { font },
                size: r.size.clamp(4.0, 512.0),
                color: rgba(r.color),
                outline: r
                    .outline
                    .then(|| (rgba(r.outline_color), r.outline_width.clamp(0.0, 64.0))),
                shadow: r.shadow.then(|| rgba(r.shadow_color)),
                align: match r.align {
                    0 => Align::Left,
                    2 => Align::Right,
                    _ => Align::Center,
                },
            })
        }
    };
    let crop = [r.crop_left, r.crop_top, r.crop_right, r.crop_bottom].map(|c| c.clamp(0.0, 0.45));
    Some(Layer {
        source,
        visible: r.visible,
        opacity: r.opacity.clamp(0.0, 1.0),
        rect: [
            r.x.clamp(-1.0, 1.0),
            r.y.clamp(-1.0, 1.0),
            r.w.clamp(0.001, 2.0),
            r.h.clamp(0.001, 2.0),
        ],
        crop,
        fit: match r.fit {
            1 => Fit::Cover,
            2 => Fit::Stretch,
            _ => Fit::Contain,
        },
    })
}

/// The scenes the UI shows.
pub fn from_ui(ui: &AppWindow) -> Scenes {
    let store = ui.global::<SceneStore>();
    let list: Vec<Scene> = store
        .get_scenes()
        .iter()
        .map(|s| Scene {
            name: s.name.to_string(),
            layers: s.layers.iter().filter_map(|r| layer(&r)).collect(),
        })
        .collect();
    let active = usize::try_from(store.get_active())
        .ok()
        .and_then(|i| list.get(i))
        .map(|s| s.name.clone())
        .unwrap_or_default();
    let canvas = if store.get_canvas_fixed() {
        let side = |v: i32| (v.clamp(128, 8192) as u32) & !1;
        Canvas::Fixed(
            side(store.get_canvas_width()),
            side(store.get_canvas_height()),
        )
    } else {
        Canvas::Auto
    };
    Scenes {
        canvas,
        active,
        list,
    }
}

/// Shows `scenes`, keeping the edited scene and the selection when they
/// still exist.
pub fn show(ui: &AppWindow, scenes: &Scenes) {
    let store = ui.global::<SceneStore>();
    let rows: Vec<SceneRow> = scenes
        .list
        .iter()
        .map(|s| SceneRow {
            name: s.name.as_str().into(),
            layers: ModelRc::new(VecModel::from(s.layers.iter().map(row).collect::<Vec<_>>())),
        })
        .collect();
    let n = rows.len() as i32;
    store.set_scenes(ModelRc::new(VecModel::from(rows)));
    store.set_active(scenes.active_index().map_or(-1, |i| i as i32));
    let editing = store.get_editing().clamp(0, (n - 1).max(0));
    store.set_editing(editing);
    let layers = scenes
        .list
        .get(editing as usize)
        .map_or(0, |s| s.layers.len()) as i32;
    if store.get_selected() >= layers {
        store.set_selected(layers - 1);
    }
    match scenes.canvas {
        Canvas::Auto => store.set_canvas_fixed(false),
        Canvas::Fixed(w, h) => {
            store.set_canvas_fixed(true);
            store.set_canvas_width(w as i32);
            store.set_canvas_height(h as i32);
        }
    }
    if store.get_open() {
        request_rasters(ui);
    }
    ui.invoke_tray_state_changed();
}

/// The layers model of scene `scene`, to change rows in place.
fn layers_model(ui: &AppWindow, scene: usize) -> Option<ModelRc<LayerRow>> {
    Some(
        ui.global::<SceneStore>()
            .get_scenes()
            .row_data(scene)?
            .layers,
    )
}

fn cached(key: Key) -> Option<Image> {
    RASTERS.with_borrow(|r| r.iter().find(|(k, _)| *k == key).map(|(_, i)| i.clone()))
}

/// Asks for the rasters the edited scene's preview lacks.
fn request_rasters(ui: &AppWindow) {
    let store = ui.global::<SceneStore>();
    let Some(model) = layers_model(ui, store.get_editing().max(0) as usize) else {
        return;
    };
    for r in model.iter() {
        let Some(key) = layer(&r).and_then(|l| key(&l.source)) else {
            continue;
        };
        if cached(key.clone()).is_some() || PENDING.with_borrow(|p| p.contains(&key)) {
            continue;
        }
        PENDING.with_borrow_mut(|p| p.push(key.clone()));
        let sent = RASTERIZER.with_borrow_mut(|tx| {
            let tx = tx.get_or_insert_with(|| spawn_rasterizer(ui.as_weak()));
            tx.send(key.clone()).is_ok()
        });
        if !sent {
            log!("scene preview: the raster thread is gone");
        }
    }
}

/// The thread rasterizing images and text for the preview; the results go
/// to the UI thread, which puts them in the rows showing them.
fn spawn_rasterizer(weak: slint::Weak<AppWindow>) -> Sender<Key> {
    let (tx, rx) = std::sync::mpsc::channel::<Key>();
    let run = move |rx: Receiver<Key>| {
        for key in rx {
            let raster = match &key {
                Key::Image(path) => overlay::image(path).map(|r| shrink(r, PREVIEW_SIDE)),
                Key::Text(style) => overlay::text(style),
            };
            let pixels = raster
                .map_err(|e| log!("scene preview: {e}"))
                .ok()
                .map(|r| {
                    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(r.width, r.height);
                    buffer.make_mut_bytes().copy_from_slice(&r.rgba);
                    buffer
                });
            let _ = weak.upgrade_in_event_loop(move |ui| rastered(&ui, key, pixels));
        }
    };
    if let Err(e) = std::thread::Builder::new()
        .name("vesper-raster".into())
        .spawn(move || run(rx))
    {
        log!("scene preview: no raster thread: {e}");
    }
    tx
}

/// A premultiplied raster no larger than `side` on its longest side.
fn shrink(r: overlay::Raster, side: u32) -> overlay::Raster {
    if r.width.max(r.height) <= side {
        return r;
    }
    let scale = side as f32 / r.width.max(r.height) as f32;
    let (w, h) = (
        ((r.width as f32 * scale).round() as u32).max(1),
        ((r.height as f32 * scale).round() as u32).max(1),
    );
    let Some(img) = image::RgbaImage::from_raw(r.width, r.height, r.rgba) else {
        return overlay::Raster {
            width: 0,
            height: 0,
            rgba: Vec::new(),
        };
    };
    let small = image::imageops::thumbnail(&img, w, h);
    overlay::Raster {
        width: w,
        height: h,
        rgba: small.into_raw(),
    }
}

fn rastered(ui: &AppWindow, key: Key, pixels: Option<SharedPixelBuffer<Rgba8Pixel>>) {
    PENDING.with_borrow_mut(|p| p.retain(|k| *k != key));
    // A failed raster leaves the stand-in.
    let Some(pixels) = pixels.filter(|p| p.width() > 0) else {
        return;
    };
    let image = Image::from_rgba8_premultiplied(pixels);
    RASTERS.with_borrow_mut(|r| {
        r.retain(|(k, _)| *k != key);
        r.push((key.clone(), image.clone()));
        let over = r.len().saturating_sub(RASTER_CACHE);
        r.drain(..over);
    });
    let scenes = ui.global::<SceneStore>().get_scenes();
    for s in scenes.iter() {
        for (i, mut r) in s.layers.iter().enumerate() {
            if layer(&r).and_then(|l| self::key(&l.source)).as_ref() == Some(&key) {
                r.raster = image.clone();
                s.layers.set_row_data(i, r);
            }
        }
    }
}

/// Saves the profile soon: typing or a slider does not write on each step.
fn save_soon(ui: &AppWindow) {
    let weak = ui.as_weak();
    SAVE.with(|t| {
        t.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(400),
            move || {
                if let Some(ui) = weak.upgrade() {
                    Prefs::from_ui(&ui).save();
                }
            },
        );
    });
}

/// Sends the recorded scene to the running pipeline.
fn apply(ui: &AppWindow, rec: &Shared) {
    if let Some(p) = rec.borrow().as_ref() {
        p.set_scene(from_ui(ui).active_scene());
    }
}

/// A change to `scenes`: shown, recorded at once if it is the active one,
/// saved.
fn changed(ui: &AppWindow, rec: &Shared, scenes: &Scenes) {
    show(ui, scenes);
    apply(ui, rec);
    save_soon(ui);
}

/// The editor's scene, as `usize`.
fn editing(ui: &AppWindow) -> usize {
    ui.global::<SceneStore>().get_editing().max(0) as usize
}

/// Records scene `i` from the next frame.
pub fn switch(ui: &AppWindow, rec: &Shared, i: usize) {
    let mut scenes = from_ui(ui);
    let Some(scene) = scenes.list.get(i) else {
        return;
    };
    if scenes.active == scene.name {
        return;
    }
    log!("scene: {}", scene.name);
    scenes.active.clone_from(&scene.name);
    changed(ui, rec, &scenes);
    update_preview(ui);
}

/// The next scene, wrapping; for the global shortcut.
pub fn next(ui: &AppWindow) -> Option<usize> {
    let scenes = from_ui(ui);
    let n = scenes.list.len();
    (n > 1).then(|| scenes.active_index().map_or(0, |i| (i + 1) % n))
}

/// Whether the pipeline should send composed pictures for the preview.
fn update_preview(ui: &AppWindow) {
    let store = ui.global::<SceneStore>();
    let want = store.get_open()
        && ui.window().is_visible()
        && store.get_active() >= 0
        && store.get_editing() == store.get_active();
    crate::recorder::PREVIEW.store(want, Ordering::Relaxed);
    if !want {
        LIVE_AT.set(None);
        store.set_live(false);
    }
}

/// A composed picture from the pipeline.
pub fn on_preview(ui: &AppWindow, pixels: SharedPixelBuffer<Rgba8Pixel>) {
    let store = ui.global::<SceneStore>();
    if !crate::recorder::PREVIEW.load(Ordering::Relaxed) || !store.get_open() {
        return;
    }
    store.set_live_image(Image::from_rgba8(pixels));
    store.set_live(true);
    LIVE_AT.set(Some(Instant::now()));
}

/// Shown, hidden, or showing another scene: what the preview needs.
fn shown(ui: &AppWindow) {
    let store = ui.global::<SceneStore>();
    update_preview(ui);
    if !store.get_open() {
        TICK.with(slint::Timer::stop);
        DRAG.set(None);
        return;
    }
    request_rasters(ui);
    // While open: the window may be hidden (tray), the pictures may stop.
    let weak = ui.as_weak();
    TICK.with(|t| {
        t.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(250),
            move || {
                let Some(ui) = weak.upgrade() else { return };
                update_preview(&ui);
                if LIVE_AT.get().is_none_or(|t| t.elapsed() > LIVE_STALE) {
                    ui.global::<SceneStore>().set_live(false);
                }
            },
        );
    });
}

/// A new layer of `kind` on top of the edited scene, selected.
fn add_layer(ui: &AppWindow, rec: &Shared, source: LayerSource) {
    let mut scenes = from_ui(ui);
    let Some(scene) = scenes.list.get_mut(editing(ui)) else {
        return;
    };
    if scene.layers.len() >= MAX_LAYERS {
        flash_notice(ui, "A scene has at most 16 layers");
        return;
    }
    let (rect, fit) = match &source {
        LayerSource::Main => ([0.0, 0.0, 1.0, 1.0], Fit::Contain),
        LayerSource::Camera { .. } => ([0.74, 0.7, 0.24, 0.27], Fit::Cover),
        LayerSource::Image { .. } => ([0.35, 0.35, 0.3, 0.3], Fit::Contain),
        LayerSource::Text(_) => ([0.1, 0.8, 0.8, 0.12], Fit::Contain),
    };
    scene.layers.push(Layer {
        source,
        rect,
        fit,
        ..Layer::main()
    });
    let top = scene.layers.len() as i32 - 1;
    changed(ui, rec, &scenes);
    ui.global::<SceneStore>().set_selected(top);
}

/// The canvas of the next pipeline: the replay buffer restarts on it.
fn set_canvas(ui: &AppWindow, rec: &Shared, games: &Games, canvas: Canvas) {
    if ui.get_rec_state() != RecState::Idle || ui.get_live_state() != LiveState::Off {
        flash_notice(ui, "The canvas is locked while recording or streaming");
        return show(ui, &from_ui(ui));
    }
    let mut scenes = from_ui(ui);
    if scenes.canvas == canvas {
        return;
    }
    log!("canvas: {canvas:?}");
    scenes.canvas = canvas;
    show(ui, &scenes);
    Prefs::from_ui(ui).save();
    if let Some(p) = rec.borrow_mut().take() {
        p.set_replay(None);
    }
    crate::start_replay(ui, rec, games);
}

pub fn wire(ui: &AppWindow, rec: &Shared, games: &Games) {
    let store = ui.global::<SceneStore>();

    let (weak, r) = (ui.as_weak(), rec.clone());
    store.on_switch(move |i| {
        if let (Some(ui), Ok(i)) = (weak.upgrade(), usize::try_from(i)) {
            switch(&ui, &r, i);
        }
    });

    let weak = ui.as_weak();
    store.on_shown(move || {
        if let Some(ui) = weak.upgrade() {
            shown(&ui);
        }
    });

    let (weak, r) = (ui.as_weak(), rec.clone());
    store.on_add_scene(move || {
        let Some(ui) = weak.upgrade() else { return };
        let mut scenes = from_ui(&ui);
        if let Some(i) = scenes.add() {
            changed(&ui, &r, &scenes);
            let store = ui.global::<SceneStore>();
            store.set_editing(i as i32);
            store.set_selected(-1);
            shown(&ui);
        }
    });

    let (weak, r) = (ui.as_weak(), rec.clone());
    store.on_duplicate_scene(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        let mut scenes = from_ui(&ui);
        if let Some(i) = scenes.duplicate(i.max(0) as usize) {
            changed(&ui, &r, &scenes);
            ui.global::<SceneStore>().set_editing(i as i32);
            shown(&ui);
        }
    });

    let (weak, r) = (ui.as_weak(), rec.clone());
    store.on_rename_scene(move |i, name| {
        let Some(ui) = weak.upgrade() else {
            return false;
        };
        let mut scenes = from_ui(&ui);
        let store = ui.global::<SceneStore>();
        match scenes.rename(i.max(0) as usize, &name) {
            Ok(()) => {
                store.set_name_error("".into());
                changed(&ui, &r, &scenes);
                true
            }
            Err(e) => {
                store.set_name_error(e.into());
                false
            }
        }
    });

    let (weak, r) = (ui.as_weak(), rec.clone());
    store.on_delete_scene(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        let mut scenes = from_ui(&ui);
        scenes.remove(i.max(0) as usize);
        ui.global::<SceneStore>().set_selected(-1);
        changed(&ui, &r, &scenes);
        shown(&ui);
    });

    let (weak, r) = (ui.as_weak(), rec.clone());
    store.on_add_layer(move |kind, value| {
        let Some(ui) = weak.upgrade() else { return };
        let source = match kind {
            LayerKind::Main => LayerSource::Main,
            LayerKind::Camera if !value.is_empty() => LayerSource::Camera { node: value.into() },
            LayerKind::Text => LayerSource::Text(TextStyle {
                text: "Text".into(),
                size: 64.0,
                outline: Some((0x0000_00c0, 3.0)),
                ..TextStyle::default()
            }),
            _ => return,
        };
        add_layer(&ui, &r, source);
    });

    let (weak, r) = (ui.as_weak(), rec.clone());
    store.on_remove_layer(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        let mut scenes = from_ui(&ui);
        if let Some(scene) = scenes.list.get_mut(editing(&ui))
            && let Ok(i) = usize::try_from(i)
            && i < scene.layers.len()
        {
            scene.layers.remove(i);
            ui.global::<SceneStore>().set_selected(-1);
            changed(&ui, &r, &scenes);
        }
    });

    let (weak, r) = (ui.as_weak(), rec.clone());
    store.on_move_layer(move |i, up| {
        let Some(ui) = weak.upgrade() else { return };
        let mut scenes = from_ui(&ui);
        if let Some(to) = scenes.move_layer(editing(&ui), i.max(0) as usize, up) {
            changed(&ui, &r, &scenes);
            ui.global::<SceneStore>().set_selected(to as i32);
        }
    });

    // One layer's settings: changed in place (the editor keeps its state).
    let (weak, r) = (ui.as_weak(), rec.clone());
    store.on_layer_changed(move |i, new| {
        let Some(ui) = weak.upgrade() else { return };
        let (Some(model), Ok(i)) = (layers_model(&ui, editing(&ui)), usize::try_from(i)) else {
            return;
        };
        let Some(l) = layer(&new) else { return };
        if i >= model.row_count() {
            return;
        }
        model.set_row_data(i, row(&l));
        request_rasters(&ui);
        apply(&ui, &r);
        save_soon(&ui);
    });

    // The picker answers on another thread: the pipeline is found again
    // on the UI thread.
    PIPELINE.set(Some(rec.clone()));
    let weak = ui.as_weak();
    store.on_pick_image(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        let scene = editing(&ui);
        let name = from_ui(&ui).list.get(scene).map(|s| s.name.clone());
        let current = usize::try_from(i).ok().and_then(|i| {
            let row = layers_model(&ui, scene)?.row_data(i)?;
            Some(PathBuf::from(row.value.as_str()))
        });
        let weak = weak.clone();
        crate::desktop::pick_image(current, move |result| {
            let _ = weak.upgrade_in_event_loop(move |ui| match result {
                Ok(Some(path)) => {
                    if let Some(r) = PIPELINE.with_borrow(Clone::clone) {
                        image_chosen(&ui, &r, scene, name, i, path);
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    log!("image picker: {e}");
                    flash_notice(&ui, "Could not open the file picker");
                }
            });
        });
    });

    let weak = ui.as_weak();
    store.on_drag_start(move |i, handle, tx, ty| {
        let Some(ui) = weak.upgrade() else { return };
        let scene = editing(&ui);
        let (Some(model), Ok(i), Some(handle)) = (
            layers_model(&ui, scene),
            usize::try_from(i),
            Handle::from_index(handle),
        ) else {
            return;
        };
        let rects: Vec<_> = model
            .iter()
            .map(|r| ([r.x, r.y, r.w, r.h], r.visible))
            .collect();
        let Some(&(start, _)) = rects.get(i) else {
            return;
        };
        let others = rects
            .iter()
            .enumerate()
            .filter(|&(j, &(_, visible))| j != i && visible)
            .map(|(_, &(rect, _))| rect);
        DRAG.set(Some((scene, i, Drag::new(handle, start, (tx, ty), others))));
    });

    let (weak, r) = (ui.as_weak(), rec.clone());
    store.on_drag(move |dx, dy, shift, snap| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(snapped) = DRAG.with_borrow(|d| {
            d.as_ref()
                .map(|(scene, i, drag)| (*scene, *i, drag.to(dx, dy, shift, snap)))
        }) else {
            return;
        };
        let (scene, i, s) = snapped;
        let Some(model) = layers_model(&ui, scene) else {
            return;
        };
        let Some(mut row) = model.row_data(i) else {
            return;
        };
        [row.x, row.y, row.w, row.h] = s.rect;
        model.set_row_data(i, row);
        let store = ui.global::<SceneStore>();
        store.set_guide_x(s.guide_x.unwrap_or(-1.0));
        store.set_guide_y(s.guide_y.unwrap_or(-1.0));
        apply(&ui, &r);
    });

    let weak = ui.as_weak();
    store.on_drag_end(move || {
        let Some(ui) = weak.upgrade() else { return };
        let store = ui.global::<SceneStore>();
        store.set_guide_x(-1.0);
        store.set_guide_y(-1.0);
        if DRAG.take().is_some() {
            save_soon(&ui);
        }
    });

    let weak = ui.as_weak();
    store.on_search_fonts(move |query| {
        let Some(ui) = weak.upgrade() else { return };
        FONT_QUERY.set(query.to_lowercase());
        if FONTS.with_borrow(Option::is_some) {
            return show_fonts(&ui);
        }
        let store = ui.global::<SceneStore>();
        if store.get_fonts_loading() {
            return;
        }
        store.set_fonts_loading(true);
        let weak = weak.clone();
        let spawned = std::thread::Builder::new()
            .name("vesper-fonts".into())
            .spawn(move || {
                let db = overlay::fonts();
                let mut families: Vec<String> = db
                    .faces()
                    .flat_map(|f| f.families.iter().map(|(name, _)| name.clone()))
                    .collect();
                families.sort_by_key(|f| f.to_lowercase());
                families.dedup();
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    FONTS.set(Some(families));
                    ui.global::<SceneStore>().set_fonts_loading(false);
                    show_fonts(&ui);
                });
            });
        if let Err(e) = spawned {
            log!("fonts: no thread: {e}");
            store.set_fonts_loading(false);
        }
    });

    let weak = ui.as_weak();
    store.on_list_cameras(move || {
        let Some(ui) = weak.upgrade() else { return };
        let store = ui.global::<SceneStore>();
        if store.get_cameras_state() == 1 {
            return;
        }
        store.set_cameras_state(1);
        let weak = weak.clone();
        let spawned = std::thread::Builder::new()
            .name("vesper-cameras".into())
            .spawn(move || {
                let found = vesper_capture::camera::cameras().unwrap_or_else(|e| {
                    log!("cameras: {e}");
                    Vec::new()
                });
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    let list: Vec<CameraChoice> = found
                        .into_iter()
                        .map(|c| CameraChoice {
                            node: c.node_name.into(),
                            label: c.description.into(),
                        })
                        .collect();
                    CAMERAS.set(list.clone());
                    let store = ui.global::<SceneStore>();
                    store.set_cameras(ModelRc::new(VecModel::from(list)));
                    store.set_cameras_state(2);
                    // Their names in the layer list.
                    show(&ui, &from_ui(&ui));
                });
            });
        if let Err(e) = spawned {
            log!("cameras: no thread: {e}");
            store.set_cameras_state(2);
        }
    });

    let (weak, r, g) = (ui.as_weak(), rec.clone(), games.clone());
    store.on_canvas_changed(move |fixed, w, h| {
        let Some(ui) = weak.upgrade() else { return };
        let canvas = if fixed {
            let side = |v: i32| (v.clamp(128, 8192) as u32) & !1;
            Canvas::Fixed(side(w), side(h))
        } else {
            Canvas::Auto
        };
        set_canvas(&ui, &r, &g, canvas);
    });
}

fn show_fonts(ui: &AppWindow) {
    let query = FONT_QUERY.with_borrow(Clone::clone);
    let list: Vec<slint::SharedString> = FONTS.with_borrow(|f| {
        f.iter()
            .flatten()
            .filter(|f| f.to_lowercase().contains(&query))
            .take(MAX_FONTS)
            .map(|f| f.as_str().into())
            .collect()
    });
    ui.global::<SceneStore>()
        .set_fonts(ModelRc::new(VecModel::from(list)));
}

/// An image picked for layer `i` (-1: a new layer) of scene `scene`, if
/// that scene is still there under `name`.
fn image_chosen(
    ui: &AppWindow,
    rec: &Shared,
    scene: usize,
    name: Option<String>,
    i: i32,
    path: PathBuf,
) {
    let scenes = from_ui(ui);
    if scenes.list.get(scene).map(|s| &s.name) != name.as_ref() {
        return;
    }
    if !Path::new(&path).is_absolute() {
        return;
    }
    log!("scene image: {}", path.display());
    let Ok(i) = usize::try_from(i) else {
        return add_layer(ui, rec, LayerSource::Image { path });
    };
    let Some(model) = layers_model(ui, scene) else {
        return;
    };
    let Some(mut r) = model.row_data(i) else {
        return;
    };
    r.value = path.display().to_string().into();
    let Some(l) = layer(&r) else { return };
    model.set_row_data(i, row(&l));
    request_rasters(ui);
    apply(ui, rec);
    save_soon(ui);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_round_trip_layers() {
        let layers = [
            Layer::main(),
            Layer {
                source: LayerSource::Camera {
                    node: "v4l2_input.usb".into(),
                },
                visible: false,
                opacity: 0.5,
                rect: [0.74, 0.7, 0.24, 0.27],
                crop: [0.1, 0.0, 0.05, 0.2],
                fit: Fit::Cover,
            },
            Layer {
                source: LayerSource::Image {
                    path: "/a/logo.png".into(),
                },
                fit: Fit::Stretch,
                ..Layer::main()
            },
            Layer {
                source: LayerSource::Text(TextStyle {
                    text: "Back\nsoon".into(),
                    font: "Noto Sans".into(),
                    size: 72.0,
                    color: 0x1122_3344,
                    outline: Some((0x0000_00c0, 2.5)),
                    shadow: Some(0x0000_0080),
                    align: Align::Right,
                }),
                ..Layer::main()
            },
        ];
        for l in layers {
            assert_eq!(layer(&row(&l)), Some(l));
        }
    }

    #[test]
    fn rows_are_made_valid() {
        let mut r = row(&Layer::main());
        r.opacity = 3.0;
        r.crop_left = 0.9;
        r.w = 0.0;
        let l = layer(&r).unwrap();
        assert_eq!((l.opacity, l.crop[0], l.rect[2]), (1.0, 0.45, 0.001));
        r.kind = LayerKind::Image;
        r.value = "relative.png".into();
        assert_eq!(layer(&r), None);
        r.kind = LayerKind::Camera;
        r.value = "".into();
        assert_eq!(layer(&r), None);
        r.kind = LayerKind::Text;
        r.value = "a\u{0}b\nc".into();
        r.font = "Evil\"<font>".into();
        match layer(&r).unwrap().source {
            LayerSource::Text(s) => assert_eq!((s.text.as_str(), s.font.as_str()), ("ab\nc", "")),
            other => panic!("{other:?}"),
        }
        assert_eq!(label(&LayerSource::Text(TextStyle::default())), "Text");
        let text = TextStyle {
            text: "\n  Hello  \nworld".into(),
            ..TextStyle::default()
        };
        assert_eq!(label(&LayerSource::Text(text)), "Hello");
    }

    #[test]
    fn previews_shrink_to_their_side() {
        let r = overlay::Raster {
            width: 4000,
            height: 1000,
            rgba: vec![255; 4000 * 1000 * 4],
        };
        let s = shrink(r, 960);
        assert_eq!((s.width, s.height, s.rgba.len()), (960, 240, 960 * 240 * 4));
    }
}
