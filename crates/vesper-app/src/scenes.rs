//! Scenes: what the recording shows. Each scene stacks layers (the main
//! capture, images, text, a webcam) on a canvas, bottom first; one is
//! active. Kept in the profile's file:
//!
//! ```text
//! canvas=auto                      (or 2560x1440)
//! scene_active=Gameplay
//! scenes=
//! scene=Gameplay
//! layer=main|true|1|0,0,1,1|0,0,0,0|contain
//! layer=camera|true|1|0.74,0.7,0.24,0.27|0,0,0,0|cover|<PipeWire node.name>
//! layer=image|true|0.9|0.02,0.02,0.15,0.08|0,0,0,0|contain|/abs/logo.png
//! layer=text|true|1|0.02,0.9,0.5,0.06|0,0,0,0|contain|size=48,color=ffffffff|Hello\nworld
//! ```
//!
//! `layer=kind|visible|opacity|x,y,w,h|left,top,right,bottom|fit[|value]`:
//! the rect is in canvas fractions, the crop in source fractions. The free
//! value (path, node, text) comes last, escaped (`\\`, `\n`, `\r`), so a `|`
//! in it needs nothing. A file without scenes has one implicit scene showing
//! the main capture alone: today's recording, with no compositor at all.
//! At most [`MAX_SCENES`] scenes of [`MAX_LAYERS`] layers; invalid lines are
//! ignored, numbers are clamped.

use std::path::PathBuf;

pub use vesper_compose::Fit;

/// Scenes in a profile.
pub const MAX_SCENES: usize = 16;
/// Layers in a scene, as many as the compositor draws.
pub const MAX_LAYERS: usize = vesper_compose::MAX_LAYERS;
/// Longest text of a text layer, in characters.
const MAX_TEXT: usize = 1024;
/// Smallest and largest fixed canvas side, pixels.
const CANVAS_SIDES: (u32, u32) = (128, 8192);

/// Size of the recorded picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Canvas {
    /// The main capture's first frame size (as without scenes).
    #[default]
    Auto,
    /// This size, whatever the capture's (even sides).
    Fixed(u32, u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    Left,
    #[default]
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextStyle {
    /// May hold line breaks.
    pub text: String,
    /// Family name; empty: the default sans-serif.
    pub font: String,
    /// Raster size of a line, pixels (the rect scales the result).
    pub size: f32,
    /// RGBA, `0xRRGGBBAA`.
    pub color: u32,
    /// Colour and width (pixels) of an outline drawn under the fill.
    pub outline: Option<(u32, f32)>,
    /// Colour of a soft drop shadow.
    pub shadow: Option<u32>,
    pub align: Align,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            text: String::new(),
            font: String::new(),
            size: 48.0,
            color: 0xffff_ffff,
            outline: None,
            shadow: None,
            align: Align::Center,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LayerSource {
    /// The captured screen, window or game.
    Main,
    /// A webcam, by its PipeWire `node.name`.
    Camera {
        node: String,
    },
    /// A PNG or JPEG file (absolute path).
    Image {
        path: PathBuf,
    },
    Text(TextStyle),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    pub source: LayerSource,
    pub visible: bool,
    /// 0 to 1.
    pub opacity: f32,
    /// x, y, width, height as fractions of the canvas.
    pub rect: [f32; 4],
    /// Left, top, right, bottom fractions of the source cut away.
    pub crop: [f32; 4],
    pub fit: Fit,
}

impl Layer {
    /// The main capture over the whole canvas.
    pub fn main() -> Self {
        Self {
            source: LayerSource::Main,
            visible: true,
            opacity: 1.0,
            rect: [0.0, 0.0, 1.0, 1.0],
            crop: [0.0; 4],
            fit: Fit::Contain,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scene {
    pub name: String,
    /// Bottom first.
    pub layers: Vec<Layer>,
}

impl Scene {
    /// The scene of a profile without scenes: the main capture alone.
    pub fn main() -> Self {
        Self {
            name: "Main".into(),
            layers: vec![Layer::main()],
        }
    }

    /// Layers that draw something: visible, not transparent.
    pub fn drawn(&self) -> impl Iterator<Item = &Layer> {
        self.layers.iter().filter(|l| l.visible && l.opacity > 0.0)
    }

    /// Shows the main capture exactly as a recording without scenes does:
    /// frames go straight to the encoder, no compositor.
    pub fn is_passthrough(&self) -> bool {
        let mut drawn = self.drawn();
        drawn.next().is_some_and(|l| *l == Layer::main()) && drawn.next().is_none()
    }

    /// Draws the main capture: it needs its frames.
    pub fn shows_main(&self) -> bool {
        self.drawn().any(|l| l.source == LayerSource::Main)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scenes {
    pub canvas: Canvas,
    /// Name of the active scene; the first one if none has it.
    pub active: String,
    /// Empty: the implicit [`Scene::main`].
    pub list: Vec<Scene>,
}

impl Default for Scenes {
    fn default() -> Self {
        Self::new()
    }
}

impl Scenes {
    pub const fn new() -> Self {
        Self {
            canvas: Canvas::Auto,
            active: String::new(),
            list: Vec::new(),
        }
    }

    /// The scene to record.
    pub fn active_scene(&self) -> Scene {
        self.list
            .iter()
            .find(|s| s.name == self.active)
            .or(self.list.first())
            .cloned()
            .unwrap_or_else(Scene::main)
    }

    /// Index of the scene recorded: the active one, else the first.
    pub fn active_index(&self) -> Option<usize> {
        let found = self.list.iter().position(|s| s.name == self.active);
        found.or((!self.list.is_empty()).then_some(0))
    }

    /// Another scene than `except` has this name (case aside, as the parser).
    fn taken(&self, name: &str, except: Option<usize>) -> bool {
        self.list
            .iter()
            .enumerate()
            .any(|(i, s)| Some(i) != except && s.name.eq_ignore_ascii_case(name))
    }

    /// `base` if free, else `base 2`, `base 3`... (shortened to stay valid).
    pub fn free_name(&self, base: &str) -> String {
        let base = if base.trim().is_empty() {
            "Scene"
        } else {
            base
        };
        (1..)
            .map(|n| {
                let suffix = if n == 1 {
                    String::new()
                } else {
                    format!(" {n}")
                };
                let room = 64 - suffix.chars().count();
                let head: String = base.trim().chars().take(room).collect();
                format!("{}{suffix}", head.trim_end())
            })
            .find(|name| valid_name(name) && !self.taken(name, None))
            .expect("at most MAX_SCENES names are taken")
    }

    /// The name typed for scene `except` (`None`: a new one), trimmed.
    pub fn check_name(&self, name: &str, except: Option<usize>) -> Result<String, &'static str> {
        let name = name.trim();
        if name.is_empty() {
            Err("Type a name")
        } else if !valid_name(name) {
            Err("At most 64 characters, on one line")
        } else if self.taken(name, except) {
            Err("Another scene has this name")
        } else {
            Ok(name.to_owned())
        }
    }

    /// Adds a scene showing the capture; the first one becomes active (it
    /// records what the implicit scene did). Its index, `None` when full.
    pub fn add(&mut self) -> Option<usize> {
        if self.list.len() >= MAX_SCENES {
            return None;
        }
        let name = self.free_name(&format!("Scene {}", self.list.len() + 1));
        if self.list.is_empty() {
            self.active.clone_from(&name);
        }
        self.list.push(Scene {
            name,
            ..Scene::main()
        });
        Some(self.list.len() - 1)
    }

    /// A copy of scene `i` right after it; its index.
    pub fn duplicate(&mut self, i: usize) -> Option<usize> {
        if self.list.len() >= MAX_SCENES {
            return None;
        }
        let mut copy = self.list.get(i)?.clone();
        copy.name = self.free_name(&format!("{} copy", copy.name));
        self.list.insert(i + 1, copy);
        Some(i + 1)
    }

    /// Renames scene `i`; the active scene stays active.
    pub fn rename(&mut self, i: usize, name: &str) -> Result<(), &'static str> {
        let name = self.check_name(name, Some(i))?;
        let scene = self.list.get_mut(i).ok_or("That scene is gone")?;
        if self.active == scene.name {
            self.active.clone_from(&name);
        }
        scene.name = name;
        Ok(())
    }

    /// Removes scene `i`. Without the active one, the first is recorded;
    /// without any, the capture alone.
    pub fn remove(&mut self, i: usize) {
        if i >= self.list.len() {
            return;
        }
        let gone = self.list.remove(i);
        if gone.name == self.active {
            self.active = self
                .list
                .first()
                .map(|s| s.name.clone())
                .unwrap_or_default();
        }
    }

    /// Moves layer `layer` of scene `scene` one step up (towards the top of
    /// the stack) or down; its new index.
    pub fn move_layer(&mut self, scene: usize, layer: usize, up: bool) -> Option<usize> {
        let layers = &mut self.list.get_mut(scene)?.layers;
        let to = if up { layer + 1 } else { layer.checked_sub(1)? };
        if to >= layers.len() {
            return None;
        }
        layers.swap(layer, to);
        Some(to)
    }

    /// The profile lines; only `canvas` for the implicit scene.
    pub fn serialize(&self) -> String {
        let mut text = match self.canvas {
            Canvas::Auto => "canvas=auto\n".to_owned(),
            Canvas::Fixed(w, h) => format!("canvas={w}x{h}\n"),
        };
        let scenes: Vec<&Scene> = self
            .list
            .iter()
            .filter(|s| valid_name(&s.name))
            .take(MAX_SCENES)
            .collect();
        if scenes.is_empty() {
            return text;
        }
        if valid_name(&self.active) {
            text += &format!("scene_active={}\n", self.active);
        }
        text += "scenes=\n";
        for scene in scenes {
            text += &format!("scene={}\n", scene.name);
            for layer in scene.layers.iter().take(MAX_LAYERS) {
                if let Some(line) = serialize_layer(layer) {
                    text += &format!("layer={line}\n");
                }
            }
        }
        text
    }
}

/// Reads the scene lines of a profile, in any order with the others.
#[derive(Debug, Default)]
pub struct Parser {
    canvas: Option<Canvas>,
    active: Option<String>,
    list: Option<Vec<Scene>>,
    /// The scene `layer` lines go to: the last one read, if valid.
    current: Option<usize>,
}

impl Parser {
    /// Takes the line if it is a scene line (valid or not).
    pub fn line(&mut self, key: &str, value: &str) -> bool {
        match key {
            "canvas" => {
                if let Some(c) = parse_canvas(value) {
                    self.canvas = Some(c);
                }
            }
            "scene_active" if valid_name(value) => self.active = Some(value.to_owned()),
            "scene_active" => {}
            "scenes" => {
                self.list.get_or_insert_default();
                self.current = None;
            }
            "scene" => {
                let list = self.list.get_or_insert_default();
                self.current = None;
                let taken = list.iter().any(|s| s.name.eq_ignore_ascii_case(value));
                if valid_name(value) && !taken && list.len() < MAX_SCENES {
                    list.push(Scene {
                        name: value.to_owned(),
                        layers: Vec::new(),
                    });
                    self.current = Some(list.len() - 1);
                }
            }
            "layer" => {
                let scene = self.current.and_then(|i| self.list.as_mut()?.get_mut(i));
                if let Some(scene) = scene
                    && scene.layers.len() < MAX_LAYERS
                    && let Some(layer) = parse_layer(value)
                {
                    scene.layers.push(layer);
                }
            }
            _ => return false,
        }
        true
    }

    /// `base` with the lines read applied: scene lines replace its list.
    pub fn finish(self, base: Scenes) -> Scenes {
        Scenes {
            canvas: self.canvas.unwrap_or(base.canvas),
            active: self.active.unwrap_or(base.active),
            list: self.list.unwrap_or(base.list),
        }
    }
}

/// A scene name: 1 to 64 characters, trimmed, one line.
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.chars().count())
        && name == name.trim()
        && !name.chars().any(char::is_control)
}

fn parse_canvas(value: &str) -> Option<Canvas> {
    let value = value.trim();
    if value == "auto" {
        return Some(Canvas::Auto);
    }
    let (w, h) = value.split_once('x')?;
    let side = |s: &str| {
        let v: u32 = s.trim().parse().ok()?;
        // Even: the encoder's 4:2:0 needs it.
        Some(v.clamp(CANVAS_SIDES.0, CANVAS_SIDES.1) & !1)
    };
    Some(Canvas::Fixed(side(w)?, side(h)?))
}

/// `\` -> `\\`, line feed -> `\n`, carriage return -> `\r`.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

/// Reverses [`escape`]; any other `\` is kept as is.
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn quad(value: &str) -> Option<[f32; 4]> {
    let mut out = [0.0f32; 4];
    let mut parts = value.split(',');
    for v in &mut out {
        *v = parts.next()?.trim().parse().ok()?;
        if !v.is_finite() {
            return None;
        }
    }
    parts.next().is_none().then_some(out)
}

fn fit_name(fit: Fit) -> &'static str {
    match fit {
        Fit::Contain => "contain",
        Fit::Cover => "cover",
        Fit::Stretch => "stretch",
    }
}

fn parse_fit(value: &str) -> Option<Fit> {
    match value.trim() {
        "contain" => Some(Fit::Contain),
        "cover" => Some(Fit::Cover),
        "stretch" => Some(Fit::Stretch),
        _ => None,
    }
}

fn hex(value: &str) -> Option<u32> {
    let value = value.trim();
    (value.len() == 8)
        .then(|| u32::from_str_radix(value, 16).ok())
        .flatten()
}

/// A font family name that fits the style field and an SVG attribute.
fn valid_font(font: &str) -> bool {
    font.chars().count() <= 64
        && !font
            .chars()
            .any(|c| c.is_control() || matches!(c, ',' | '|' | '=' | '"' | '\'' | '<' | '>' | '&'))
}

/// Text a text layer may show: line breaks and tabs, no other control.
fn valid_text(text: &str) -> bool {
    text.chars().count() <= MAX_TEXT
        && !text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
}

fn parse_style(value: &str, text: String) -> Option<TextStyle> {
    let mut style = TextStyle {
        text,
        ..TextStyle::default()
    };
    let mut outline_color = None;
    let mut outline_width = None;
    for pair in value.split(',').filter(|p| !p.trim().is_empty()) {
        let (k, v) = pair.split_once('=')?;
        match k.trim() {
            "size" => {
                let s: f32 = v.trim().parse().ok()?;
                style.size = s.is_finite().then(|| s.clamp(4.0, 512.0))?;
            }
            "color" => style.color = hex(v)?,
            "outline" => outline_color = Some(hex(v)?),
            "outline_width" => {
                let w: f32 = v.trim().parse().ok()?;
                outline_width = Some(w.is_finite().then(|| w.clamp(0.0, 64.0))?);
            }
            "shadow" => style.shadow = Some(hex(v)?),
            "font" if valid_font(v.trim()) => style.font = v.trim().to_owned(),
            "align" => {
                style.align = match v.trim() {
                    "left" => Align::Left,
                    "center" => Align::Center,
                    "right" => Align::Right,
                    _ => return None,
                }
            }
            // A later version's option: skipped, the layer stays.
            _ => {}
        }
    }
    style.outline = outline_color.map(|c| (c, outline_width.unwrap_or(style.size / 16.0)));
    Some(style)
}

fn serialize_style(s: &TextStyle) -> String {
    let mut out = format!("size={},color={:08x}", s.size, s.color);
    if let Some((c, w)) = s.outline {
        out += &format!(",outline={c:08x},outline_width={w}");
    }
    if let Some(c) = s.shadow {
        out += &format!(",shadow={c:08x}");
    }
    if !s.font.is_empty() {
        out += &format!(",font={}", s.font);
    }
    match s.align {
        Align::Center => {}
        Align::Left => out += ",align=left",
        Align::Right => out += ",align=right",
    }
    out
}

fn parse_layer(value: &str) -> Option<Layer> {
    let mut f = value.splitn(7, '|');
    let kind = f.next()?.trim();
    let visible = match f.next()?.trim() {
        "true" => true,
        "false" => false,
        _ => return None,
    };
    let opacity: f32 = f.next()?.trim().parse().ok()?;
    if !opacity.is_finite() {
        return None;
    }
    let [x, y, w, h] = quad(f.next()?)?;
    let crop = quad(f.next()?)?.map(|c| c.clamp(0.0, 1.0));
    // Something of the source must stay.
    if crop[0] + crop[2] > 0.99 || crop[1] + crop[3] > 0.99 {
        return None;
    }
    let fit = parse_fit(f.next()?)?;
    let rest = f.next();
    let source = match (kind, rest) {
        ("main", None) => LayerSource::Main,
        ("camera", Some(node)) => {
            let node = unescape(node);
            let ok = (1..=256).contains(&node.len()) && !node.chars().any(char::is_control);
            ok.then_some(LayerSource::Camera { node })?
        }
        ("image", Some(path)) => {
            let path = PathBuf::from(unescape(path));
            path.is_absolute().then_some(LayerSource::Image { path })?
        }
        ("text", Some(rest)) => {
            let (style, text) = rest.split_once('|')?;
            let text = unescape(text);
            if !valid_text(&text) {
                return None;
            }
            LayerSource::Text(parse_style(style, text)?)
        }
        _ => return None,
    };
    Some(Layer {
        source,
        visible,
        opacity: opacity.clamp(0.0, 1.0),
        rect: [
            x.clamp(-1.0, 1.0),
            y.clamp(-1.0, 1.0),
            w.clamp(0.001, 2.0),
            h.clamp(0.001, 2.0),
        ],
        crop,
        fit,
    })
}

/// `None` for a layer its line could not hold.
fn serialize_layer(l: &Layer) -> Option<String> {
    let q = |v: [f32; 4]| format!("{},{},{},{}", v[0], v[1], v[2], v[3]);
    let head = |kind: &str| {
        format!(
            "{kind}|{}|{}|{}|{}|{}",
            l.visible,
            l.opacity,
            q(l.rect),
            q(l.crop),
            fit_name(l.fit)
        )
    };
    Some(match &l.source {
        LayerSource::Main => head("main"),
        LayerSource::Camera { node } => format!("{}|{}", head("camera"), escape(node)),
        LayerSource::Image { path } => {
            format!("{}|{}", head("image"), escape(path.to_str()?))
        }
        LayerSource::Text(s) => {
            if !valid_font(&s.font) || !valid_text(&s.text) {
                return None;
            }
            format!(
                "{}|{}|{}",
                head("text"),
                serialize_style(s),
                escape(&s.text)
            )
        }
    })
}

/// Smallest layer side while resizing, canvas fraction.
const MIN_SIDE: f32 = 0.02;

/// What a pointer drag on the editor's preview changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    Move,
    N,
    S,
    E,
    W,
    NE,
    NW,
    SE,
    SW,
}

impl Handle {
    /// The UI's numbering: 0 the layer's body, then N, S, E, W, NE, NW, SE, SW.
    pub fn from_index(i: i32) -> Option<Self> {
        use Handle::*;
        [Move, N, S, E, W, NE, NW, SE, SW]
            .get(usize::try_from(i).ok()?)
            .copied()
    }

    /// Whether it moves the left, right, top and bottom edges.
    fn edges(self) -> [bool; 4] {
        use Handle::*;
        [
            matches!(self, W | NW | SW),
            matches!(self, E | NE | SE),
            matches!(self, N | NW | NE),
            matches!(self, S | SW | SE),
        ]
    }
}

/// A layer being moved or resized on the preview, in canvas fractions.
/// Edges snap to the canvas edges and centre and to the other layers'
/// edges and centres, within `tolerance`.
#[derive(Debug, Clone)]
pub struct Drag {
    pub handle: Handle,
    /// The layer's rect when the drag started.
    pub start: [f32; 4],
    /// Snap distance per axis.
    tolerance: (f32, f32),
    /// Vertical lines (x) and horizontal lines (y) to snap to.
    lines: (Vec<f32>, Vec<f32>),
}

/// A drag's result: the rect, and the lines it lies on (to draw).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Snapped {
    pub rect: [f32; 4],
    pub guide_x: Option<f32>,
    pub guide_y: Option<f32>,
}

/// The line nearest to one of `values`, within `tolerance`: (shift, line).
fn nearest(values: &[f32], lines: &[f32], tolerance: f32) -> Option<(f32, f32)> {
    let mut best: Option<(f32, f32)> = None;
    for &v in values {
        for &l in lines {
            let d = l - v;
            if d.abs() <= tolerance && best.is_none_or(|(b, _)| d.abs() < b.abs()) {
                best = Some((d, l));
            }
        }
    }
    best
}

impl Drag {
    pub fn new(
        handle: Handle,
        start: [f32; 4],
        tolerance: (f32, f32),
        others: impl IntoIterator<Item = [f32; 4]>,
    ) -> Self {
        let (mut xs, mut ys) = (vec![0.0, 0.5, 1.0], vec![0.0, 0.5, 1.0]);
        for [x, y, w, h] in others {
            xs.extend([x, x + w / 2.0, x + w]);
            ys.extend([y, y + h / 2.0, y + h]);
        }
        Self {
            handle,
            start,
            tolerance,
            lines: (xs, ys),
        }
    }

    /// The rect once the pointer moved `(dx, dy)` from where the drag
    /// started. `keep_ratio` (Shift) keeps the start's proportions on a
    /// corner; without `snap` it goes exactly where the pointer says.
    pub fn to(&self, dx: f32, dy: f32, keep_ratio: bool, snap: bool) -> Snapped {
        let [x0, y0, w0, h0] = self.start;
        let (tx, ty) = if snap { self.tolerance } else { (-1.0, -1.0) };
        let (xs, ys) = (&self.lines.0, &self.lines.1);
        let rect = if self.handle == Handle::Move {
            let (x, y) = (x0 + dx, y0 + dy);
            let sx = nearest(&[x, x + w0 / 2.0, x + w0], xs, tx).map_or(0.0, |s| s.0);
            let sy = nearest(&[y, y + h0 / 2.0, y + h0], ys, ty).map_or(0.0, |s| s.0);
            [x + sx, y + sy, w0, h0]
        } else {
            let [left, right, top, bottom] = self.handle.edges();
            let snapped = |v: f32, lines: &[f32], t| nearest(&[v], lines, t).map_or(v, |s| s.1);
            let (mut l, mut r, mut t, mut b) = (x0, x0 + w0, y0, y0 + h0);
            if left {
                l = snapped(l + dx, xs, tx).min(r - MIN_SIDE);
            }
            if right {
                r = snapped(r + dx, xs, tx).max(l + MIN_SIDE);
            }
            if top {
                t = snapped(t + dy, ys, ty).min(b - MIN_SIDE);
            }
            if bottom {
                b = snapped(b + dy, ys, ty).max(t + MIN_SIDE);
            }
            if keep_ratio && (left || right) && (top || bottom) {
                // The larger change leads; the opposite corner stays.
                let s = ((r - l) / w0).max((b - t) / h0);
                let (w, h) = (w0 * s, h0 * s);
                if left {
                    l = r - w;
                } else {
                    r = l + w;
                }
                if top {
                    t = b - h;
                } else {
                    b = t + h;
                }
            }
            [l, t, r - l, b - t]
        };
        let [x, y, w, h] = [
            rect[0].clamp(-1.0, 1.0),
            rect[1].clamp(-1.0, 1.0),
            rect[2].clamp(MIN_SIDE, 2.0),
            rect[3].clamp(MIN_SIDE, 2.0),
        ];
        let on = |values: [f32; 3], lines: &[f32]| {
            snap.then(|| nearest(&values, lines, 1e-4).map(|s| s.1))
                .flatten()
        };
        Snapped {
            rect: [x, y, w, h],
            guide_x: on([x, x + w / 2.0, x + w], xs),
            guide_y: on([y, y + h / 2.0, y + h], ys),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Scenes {
        let mut p = Parser::default();
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                p.line(k.trim(), v.trim_end_matches('\r'));
            }
        }
        p.finish(Scenes::default())
    }

    fn sample() -> Scenes {
        Scenes {
            canvas: Canvas::Fixed(2560, 1440),
            active: "BRB".into(),
            list: vec![
                Scene {
                    name: "Gameplay".into(),
                    layers: vec![
                        Layer::main(),
                        Layer {
                            source: LayerSource::Camera {
                                node: "v4l2_input.pci-0000_0e_00.3-usb-0_1_1.0".into(),
                            },
                            rect: [0.74, 0.7, 0.24, 0.27],
                            fit: Fit::Cover,
                            ..Layer::main()
                        },
                        Layer {
                            source: LayerSource::Image {
                                path: "/home/u/logo|1 \\ 2\n.png".into(),
                            },
                            opacity: 0.9,
                            rect: [0.02, 0.02, 0.15, 0.08],
                            crop: [0.1, 0.0, 0.2, 0.05],
                            ..Layer::main()
                        },
                    ],
                },
                Scene {
                    name: "BRB".into(),
                    layers: vec![Layer {
                        source: LayerSource::Text(TextStyle {
                            text: "Back | soon\nwith \\n literal\nand\ttab".into(),
                            font: "Noto Sans".into(),
                            size: 72.5,
                            color: 0x11223344,
                            outline: Some((0x000000c0, 3.0)),
                            shadow: Some(0x00000080),
                            align: Align::Left,
                        }),
                        visible: false,
                        rect: [-0.5, 0.9, 0.5, 0.06],
                        fit: Fit::Stretch,
                        ..Layer::main()
                    }],
                },
                Scene {
                    name: "Empty".into(),
                    layers: Vec::new(),
                },
            ],
        }
    }

    #[test]
    fn round_trip() {
        let s = sample();
        let text = s.serialize();
        assert_eq!(parse(&text), s, "{text}");
        // Stable: serializing again gives the same text.
        assert_eq!(parse(&text).serialize(), text);
    }

    #[test]
    fn without_scenes_the_main_capture_is_shown_alone() {
        let s = parse("codec=hevc\ncanvas=auto\n");
        assert_eq!(s, Scenes::default());
        assert_eq!(s.active_scene(), Scene::main());
        assert!(s.active_scene().is_passthrough());
        assert_eq!(s.serialize(), "canvas=auto\n");
    }

    #[test]
    fn active_scene_falls_back_to_the_first() {
        let mut s = sample();
        assert_eq!(s.active_scene().name, "BRB");
        s.active = "Gone".into();
        assert_eq!(s.active_scene().name, "Gameplay");
    }

    #[test]
    fn passthrough_needs_one_plain_main_layer() {
        let mut scene = Scene::main();
        assert!(scene.is_passthrough() && scene.shows_main());
        // Hidden and transparent layers draw nothing.
        let hidden = Layer {
            source: LayerSource::Image {
                path: "/a.png".into(),
            },
            visible: false,
            ..Layer::main()
        };
        scene.layers.push(hidden.clone());
        scene.layers.push(Layer {
            opacity: 0.0,
            ..hidden.clone()
        });
        assert!(scene.is_passthrough());
        let mut webcam = scene.clone();
        webcam.layers.push(Layer {
            source: LayerSource::Camera { node: "c".into() },
            ..Layer::main()
        });
        assert!(!webcam.is_passthrough() && webcam.shows_main());
        scene.layers.push(Layer {
            visible: true,
            ..hidden
        });
        assert!(!scene.is_passthrough());
        let cropped = Scene {
            name: "c".into(),
            layers: vec![Layer {
                crop: [0.1, 0.0, 0.0, 0.0],
                ..Layer::main()
            }],
        };
        assert!(!cropped.is_passthrough() && cropped.shows_main());
        let brb = &sample().list[1];
        assert!(!brb.shows_main() && !brb.is_passthrough());
    }

    #[test]
    fn canvas_sizes_are_clamped_and_even() {
        for (value, want) in [
            ("auto", Some(Canvas::Auto)),
            ("1920x1080", Some(Canvas::Fixed(1920, 1080))),
            ("1921 x 1081", Some(Canvas::Fixed(1920, 1080))),
            ("1x99999", Some(Canvas::Fixed(128, 8192))),
            ("-5x10", None),
            ("1920", None),
            ("big", None),
            ("", None),
        ] {
            assert_eq!(parse_canvas(value), want, "{value:?}");
        }
        assert_eq!(parse("canvas=bogus\n").canvas, Canvas::Auto);
    }

    #[test]
    fn bad_lines_are_ignored_and_values_clamped() {
        let text = "layer=main|true|1|0,0,1,1|0,0,0,0|contain\n\
                    scenes=\n\
                    scene=\n\
                    layer=main|true|1|0,0,1,1|0,0,0,0|contain\n\
                    scene=A\n\
                    layer=main|true|7|-3,0,9,0|0,-1,0,0.5|cover\n\
                    layer=main|yes|1|0,0,1,1|0,0,0,0|contain\n\
                    layer=main|true|NaN|0,0,1,1|0,0,0,0|contain\n\
                    layer=main|true|1|0,0,1|0,0,0,0|contain\n\
                    layer=main|true|1|0,0,1,1,1|0,0,0,0|contain\n\
                    layer=main|true|1|0,0,inf,1|0,0,0,0|contain\n\
                    layer=main|true|1|0,0,1,1|0.5,0,0.5,0|contain\n\
                    layer=main|true|1|0,0,1,1|0,0,0,0|zoom\n\
                    layer=main|true|1|0,0,1,1|0,0,0,0|contain|extra\n\
                    layer=image|true|1|0,0,1,1|0,0,0,0|contain|relative.png\n\
                    layer=image|true|1|0,0,1,1|0,0,0,0|contain\n\
                    layer=camera|true|1|0,0,1,1|0,0,0,0|contain|\n\
                    layer=text|true|1|0,0,1,1|0,0,0,0|contain|size=big|x\n\
                    layer=text|true|1|0,0,1,1|0,0,0,0|contain|color=red|x\n\
                    layer=text|true|1|0,0,1,1|0,0,0,0|contain|align=up|x\n\
                    layer=text|true|1|0,0,1,1|0,0,0,0|contain|nonsense|x\n\
                    layer=text|true|1|0,0,1,1|0,0,0,0|contain|size=48\n\
                    layer=text|true|1|0,0,1,1|0,0,0,0|contain|size=9999,future=1|ok\n\
                    layer=video|true|1|0,0,1,1|0,0,0,0|contain|/a\n\
                    scene=a\n\
                    layer=main|true|1|0,0,1,1|0,0,0,0|contain\n\
                    scene=B\n";
        let s = parse(text);
        let names: Vec<_> = s.list.iter().map(|s| s.name.as_str()).collect();
        // "a" duplicates "A": ignored, and so are its layers.
        assert_eq!(names, ["A", "B"]);
        let a = &s.list[0].layers;
        assert_eq!(a.len(), 2, "{a:#?}");
        assert_eq!(a[0].opacity, 1.0);
        assert_eq!(a[0].rect, [-1.0, 0.0, 2.0, 0.001]);
        assert_eq!(a[0].crop, [0.0, 0.0, 0.0, 0.5]);
        assert_eq!(a[0].fit, Fit::Cover);
        match &a[1].source {
            LayerSource::Text(t) => assert_eq!((t.size, t.text.as_str()), (512.0, "ok")),
            other => panic!("{other:?}"),
        }
        assert!(s.list[1].layers.is_empty());
    }

    #[test]
    fn limits_hold() {
        let mut text = "scenes=\n".to_owned();
        for i in 0..MAX_SCENES + 4 {
            text += &format!("scene=S{i}\n");
            text += &"layer=main|true|1|0,0,1,1|0,0,0,0|contain\n".repeat(MAX_LAYERS + 3);
        }
        let s = parse(&text);
        assert_eq!(s.list.len(), MAX_SCENES);
        assert!(s.list.iter().all(|s| s.layers.len() == MAX_LAYERS));
        let long = "x".repeat(MAX_TEXT + 1);
        assert!(
            parse(&format!(
                "scene=A\nlayer=text|true|1|0,0,1,1|0,0,0,0|contain|size=8|{long}\n"
            ))
            .list[0]
                .layers
                .is_empty()
        );
    }

    #[test]
    fn escaping_round_trips_anything() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let alphabet = ['\\', 'n', 'r', '\n', '\r', '|', 'a', 'é', '=', ',', ' '];
        for _ in 0..2000 {
            let len = (seed % 12) as usize;
            let s: String = (0..len)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    alphabet[(seed % alphabet.len() as u64) as usize]
                })
                .collect();
            let e = escape(&s);
            assert!(!e.contains(['\n', '\r']), "{e:?}");
            assert_eq!(unescape(&e), s);
        }
        // Not from `escape`: kept as typed.
        assert_eq!(unescape("a\\tb\\"), "a\\tb\\");
    }

    /// Random lines from the format's pieces never panic, and whatever is
    /// read writes back to itself.
    #[test]
    fn hostile_input_never_panics_and_round_trips() {
        let pieces = [
            "layer=",
            "scene=",
            "scenes=",
            "scene_active=",
            "canvas=",
            "main",
            "text",
            "image",
            "camera",
            "|",
            ",",
            "true",
            "false",
            "1",
            "-1",
            "0.5",
            "NaN",
            "1e39",
            "contain",
            "cover",
            "/p",
            "size=",
            "color=",
            "ffffffff",
            "outline=",
            "font=",
            "\\",
            "\\n",
            "x",
            "é",
            "\u{0}",
            "=",
            "0,0,1,1",
            "\n",
            "\r\n",
        ];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..3000 {
            let mut text = String::new();
            for _ in 0..(seed % 40) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                text += pieces[(seed % pieces.len() as u64) as usize];
            }
            let s = parse(&text);
            let again = s.serialize();
            let back = parse(&again);
            assert_eq!(back.serialize(), again, "{text:?}");
            let _ = s.active_scene();
        }
    }

    #[test]
    fn names_stay_unique_and_valid() {
        let mut s = Scenes::default();
        assert_eq!(s.add(), Some(0));
        // The first scene is recorded: it shows what the implicit one did.
        assert_eq!(s.active, "Scene 1");
        assert_eq!(s.active_scene().layers, Scene::main().layers);
        assert_eq!(s.duplicate(0), Some(1));
        assert_eq!(s.duplicate(0), Some(1));
        let names: Vec<_> = s.list.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Scene 1", "Scene 1 copy 2", "Scene 1 copy"]);
        assert_eq!(
            s.check_name(" scene 1 ", None),
            Err("Another scene has this name")
        );
        assert_eq!(s.check_name(" Scene 1 ", Some(0)), Ok("Scene 1".into()));
        assert_eq!(s.check_name("  ", None), Err("Type a name"));
        let long = "y".repeat(64);
        assert!(s.check_name(&format!("{long}y"), None).is_err());
        assert!(s.check_name("a\u{7}b", None).is_err());
        // Renaming the active scene keeps it active.
        s.rename(0, "Gameplay").unwrap();
        assert_eq!(s.active, "Gameplay");
        assert_eq!(s.rename(1, "gameplay"), Err("Another scene has this name"));
        // A long name's copy is shortened, never invalid.
        s.rename(2, &long).unwrap();
        let i = s.duplicate(2).unwrap();
        assert!(valid_name(&s.list[i].name) && s.list[i].name != long);
        while s.add().is_some() {}
        assert_eq!(s.list.len(), MAX_SCENES);
        assert_eq!(s.duplicate(0), None);
        assert_eq!(parse(&s.serialize()), s);
    }

    #[test]
    fn removing_the_active_scene_records_the_first() {
        let mut s = sample();
        s.remove(1);
        assert_eq!(s.active, "Gameplay");
        s.remove(7);
        s.remove(0);
        assert_eq!(s.active, "Empty");
        s.remove(0);
        assert!(s.list.is_empty() && s.active.is_empty());
        assert_eq!(s.active_scene(), Scene::main());
        assert_eq!(s.active_index(), None);
    }

    #[test]
    fn layers_reorder_within_bounds() {
        let mut s = sample();
        let kinds = |s: &Scenes| {
            s.list[0]
                .layers
                .iter()
                .map(|l| std::mem::discriminant(&l.source))
                .collect::<Vec<_>>()
        };
        let before = kinds(&s);
        assert_eq!(s.move_layer(0, 0, false), None);
        assert_eq!(s.move_layer(0, 2, true), None);
        assert_eq!(s.move_layer(0, 0, true), Some(1));
        assert_eq!(kinds(&s), [before[1], before[0], before[2]]);
        assert_eq!(s.move_layer(0, 1, false), Some(0));
        assert_eq!(kinds(&s), before);
        assert_eq!(s.move_layer(9, 0, true), None);
    }

    fn close(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(b).all(|(a, b)| (a - b).abs() < 1e-5)
    }

    #[test]
    fn moves_snap_to_the_canvas_and_other_layers() {
        let other = [0.6, 0.1, 0.2, 0.2];
        let d = Drag::new(Handle::Move, [0.1, 0.1, 0.25, 0.1], (0.01, 0.01), [other]);
        // Nothing near: where the pointer says.
        let r = d.to(0.13, 0.27, false, true);
        assert!(close(r.rect, [0.23, 0.37, 0.25, 0.1]), "{r:?}");
        assert_eq!((r.guide_x, r.guide_y), (None, None));
        // The right edge onto the other layer's left edge.
        let r = d.to(0.252, 0.0, false, true);
        assert!(close(r.rect, [0.35, 0.1, 0.25, 0.1]), "{r:?}");
        assert_eq!((r.guide_x, r.guide_y), (Some(0.6), Some(0.1)));
        // The centre onto the canvas centre.
        let r = d.to(0.27, 0.352, false, true);
        assert!(close(r.rect, [0.375, 0.45, 0.25, 0.1]), "{r:?}");
        assert_eq!((r.guide_x, r.guide_y), (Some(0.5), Some(0.5)));
        // The left edge onto the canvas edge; past it without snapping.
        let r = d.to(-0.105, 0.03, false, true);
        assert!(close(r.rect, [0.0, 0.13, 0.25, 0.1]), "{r:?}");
        assert_eq!(r.guide_x, Some(0.0));
        let r = d.to(-0.105, 0.0, false, false);
        assert!(close(r.rect, [-0.005, 0.1, 0.25, 0.1]), "{r:?}");
        assert_eq!((r.guide_x, r.guide_y), (None, None));
    }

    #[test]
    fn resizes_snap_and_keep_the_ratio() {
        let start = [0.2, 0.2, 0.4, 0.2];
        let r = Drag::new(Handle::SE, start, (0.01, 0.01), []).to(-0.095, 0.0, false, true);
        assert!(close(r.rect, [0.2, 0.2, 0.3, 0.2]), "{r:?}");
        assert_eq!((r.guide_x, r.guide_y), (Some(0.5), None));
        // Shift: 2:1 kept, the larger change leads, the opposite corner stays.
        let r = Drag::new(Handle::SE, start, (0.01, 0.01), []).to(0.2, 0.01, true, false);
        assert!(close(r.rect, [0.2, 0.2, 0.6, 0.3]), "{r:?}");
        let r = Drag::new(Handle::NW, start, (0.01, 0.01), []).to(0.0, -0.1, true, false);
        assert!(close(r.rect, [0.0, 0.1, 0.6, 0.3]), "{r:?}");
        // Edge handles ignore the other axis; edges never cross.
        let r = Drag::new(Handle::W, start, (0.01, 0.01), []).to(0.9, 0.3, false, true);
        assert!(close(r.rect, [0.6 - MIN_SIDE, 0.2, MIN_SIDE, 0.2]), "{r:?}");
        let r = Drag::new(Handle::N, start, (0.01, 0.01), []).to(0.3, -0.195, false, true);
        assert!(close(r.rect, [0.2, 0.0, 0.4, 0.4]), "{r:?}");
        assert_eq!((r.guide_x, r.guide_y), (None, Some(0.0)));
        assert_eq!(Handle::from_index(0), Some(Handle::Move));
        assert_eq!(Handle::from_index(8), Some(Handle::SW));
        assert_eq!(Handle::from_index(9), None);
        assert_eq!(Handle::from_index(-1), None);
    }
}
