//! System tray icon (StatusNotifierItem, `ksni`), so Vesper can run with its
//! window closed. `ksni` serves the item and its menu on its own thread; every
//! click goes to the `on_command` callback, which must hand it to the UI
//! thread. The UI pushes its state back with [`Tray::set`].

use std::sync::{Arc, mpsc};

use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{Icon, MenuItem, ToolTip};

use crate::shortcuts::APP_ID;

/// What a tray click asks the UI to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    ToggleWindow,
    ToggleRecording,
    SaveReplay,
    ToggleReplay,
    ToggleLive,
    /// Record scene `n` (the profile's order).
    Scene(usize),
    Quit,
}

/// What the tray shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub window_visible: bool,
    pub recording: bool,
    /// Starting or stopping: record intents are ignored.
    pub busy: bool,
    pub replay: bool,
    /// A live stream runs (or connects).
    pub live: bool,
    /// The stream is ending: toggling it is ignored.
    pub live_busy: bool,
    /// The profile's scenes (none: no Scene menu), and the recorded one.
    pub scenes: Vec<String>,
    pub active_scene: Option<usize>,
}

type OnCommand = Arc<dyn Fn(Command) + Send + Sync>;

/// The tray icon, one per state: the most public activity wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Look {
    Idle,
    Replay,
    Recording,
    Live,
}

impl Look {
    const ALL: [Self; 4] = [Self::Idle, Self::Replay, Self::Recording, Self::Live];

    fn of(state: &State) -> Self {
        if state.live {
            Self::Live
        } else if state.recording {
            Self::Recording
        } else if state.replay {
            Self::Replay
        } else {
            Self::Idle
        }
    }

    /// Symbolic icon names, installed under `hicolor/scalable/`: Plasma
    /// recolors them to the panel's text color.
    fn icon_name(self) -> String {
        match self {
            Self::Idle => format!("{APP_ID}-symbolic"),
            Self::Replay => format!("{APP_ID}-replay-symbolic"),
            Self::Recording => format!("{APP_ID}-recording-symbolic"),
            Self::Live => format!("{APP_ID}-live-symbolic"),
        }
    }

    fn svg(self) -> &'static str {
        match self {
            Self::Idle => include_str!(
                "../data/icons/hicolor/scalable/apps/io.github.PaulCarouge.Vesper-symbolic.svg"
            ),
            Self::Replay => include_str!(
                "../data/icons/hicolor/scalable/status/io.github.PaulCarouge.Vesper-replay-symbolic.svg"
            ),
            Self::Recording => include_str!(
                "../data/icons/hicolor/scalable/status/io.github.PaulCarouge.Vesper-recording-symbolic.svg"
            ),
            Self::Live => include_str!(
                "../data/icons/hicolor/scalable/status/io.github.PaulCarouge.Vesper-live-symbolic.svg"
            ),
        }
    }
}

struct Item {
    state: State,
    /// Per [`Look`], shown when the icon theme does not have its name (not
    /// installed): rendered once, at a few sizes.
    pixmaps: [Vec<Icon>; 4],
    on_command: OnCommand,
}

impl ksni::Tray for Item {
    fn id(&self) -> String {
        APP_ID.into()
    }

    fn title(&self) -> String {
        "Vesper".into()
    }

    fn icon_name(&self) -> String {
        Look::of(&self.state).icon_name()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.pixmaps[Look::of(&self.state) as usize].clone()
    }

    fn tool_tip(&self) -> ToolTip {
        let description = if self.state.recording && self.state.live {
            "Recording and live"
        } else if self.state.recording {
            "Recording"
        } else if self.state.live {
            "Live"
        } else if self.state.replay {
            "Replay buffer on"
        } else {
            "Idle"
        };
        ToolTip {
            title: "Vesper".into(),
            description: description.into(),
            ..Default::default()
        }
    }

    // Left click.
    fn activate(&mut self, _x: i32, _y: i32) {
        (self.on_command)(Command::ToggleWindow);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let s = &self.state;
        let item = |label: &str, enabled: bool, command: Command| {
            StandardItem {
                label: label.into(),
                enabled,
                activate: Box::new(move |item: &mut Self| (item.on_command)(command)),
                ..Default::default()
            }
            .into()
        };
        let scenes = (!s.scenes.is_empty()).then(|| {
            SubMenu {
                label: "Scene".into(),
                submenu: vec![
                    RadioGroup {
                        selected: s.active_scene.unwrap_or(usize::MAX),
                        select: Box::new(|item: &mut Self, i| {
                            (item.on_command)(Command::Scene(i));
                        }),
                        options: s
                            .scenes
                            .iter()
                            .map(|name| RadioItem {
                                // An underscore marks an access key.
                                label: name.replace('_', "__"),
                                ..Default::default()
                            })
                            .collect(),
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into()
        });
        let mut menu = vec![
            item(
                if s.window_visible {
                    "Hide window"
                } else {
                    "Show window"
                },
                true,
                Command::ToggleWindow,
            ),
            MenuItem::Separator,
            item(
                if s.recording {
                    "Stop recording"
                } else {
                    "Start recording"
                },
                !s.busy,
                Command::ToggleRecording,
            ),
            item("Save replay", s.replay, Command::SaveReplay),
            item(
                if s.live { "End stream" } else { "Go live" },
                !s.live_busy,
                Command::ToggleLive,
            ),
            CheckmarkItem {
                label: "Replay buffer".into(),
                checked: s.replay,
                activate: Box::new(|item: &mut Self| (item.on_command)(Command::ToggleReplay)),
                ..Default::default()
            }
            .into(),
        ];
        menu.extend(scenes);
        menu.extend([
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|item: &mut Self| (item.on_command)(Command::Quit)),
                ..Default::default()
            }
            .into(),
        ]);
        menu
    }
}

/// The registered tray item. Cheap to clone; it goes away with the process.
#[derive(Clone)]
pub struct Tray {
    handle: Handle<Item>,
    /// To the `vesper-tray` thread, which applies the latest state.
    states: mpsc::Sender<State>,
}

impl Tray {
    /// Registers the item with the StatusNotifierWatcher. Fails when the
    /// session has none (no tray), so the caller keeps the window-only mode.
    /// Blocks briefly on D-Bus.
    pub fn spawn(
        state: State,
        on_command: impl Fn(Command) + Send + Sync + 'static,
    ) -> Result<Self, ksni::Error> {
        let item = Item {
            state,
            pixmaps: Look::ALL.map(|look| {
                PIXMAP_SIZES
                    .iter()
                    .filter_map(|&size| pixmap(look, size))
                    .collect()
            }),
            on_command: Arc::new(on_command),
        };
        let handle = item.spawn()?;
        // `update` waits on the D-Bus service, which a slow watcher may
        // stall: never on the UI thread.
        let (states, rx) = mpsc::channel::<State>();
        let h = handle.clone();
        let spawned = std::thread::Builder::new()
            .name("vesper-tray".into())
            .spawn(move || {
                while let Ok(mut state) = rx.recv() {
                    // Only the latest state matters.
                    state = rx.try_iter().last().unwrap_or(state);
                    h.update(|item| item.state = state);
                }
            });
        if let Err(e) = spawned {
            log!("tray: could not start its thread, its menu stays as is: {e}");
        }
        Ok(Self { handle, states })
    }

    /// Removes the item now, so it does not linger while the app finishes.
    pub fn shutdown(&self) {
        self.handle.shutdown().wait();
    }

    /// Shows `state` soon; `ksni` only signals the properties that changed.
    pub fn set(&self, state: State) {
        let _ = self.states.send(state);
    }
}

/// Fallback pixmap sizes: a 22 px panel at 1x, 1.5x and 2x.
const PIXMAP_SIZES: [u32; 3] = [22, 32, 48];

/// Pixmaps are not recolored by the panel: this violet keeps 3:1 or more on
/// both light and dark Breeze panels (#EFF0F1, #1B1E20). The red dot stays.
const PIXMAP_COLOR: &str = "#8577f2";

/// `look`'s symbolic icon, rendered `size` px wide in [`PIXMAP_COLOR`].
fn pixmap(look: Look, size: u32) -> Option<Icon> {
    let svg = look.svg().replacen("#232629", PIXMAP_COLOR, 1).replacen(
        r#"width="22" height="22""#,
        &format!(r#"width="{size}" height="{size}""#),
        1,
    );
    let pixels = slint::Image::load_from_svg_data(svg.as_bytes())
        .ok()?
        .to_rgba8()?;
    Some(argb_icon(
        pixels.as_bytes(),
        pixels.width() as i32,
        pixels.height() as i32,
    ))
}

/// SNI pixmaps are ARGB32 in network byte order.
fn argb_icon(rgba: &[u8], width: i32, height: i32) -> Icon {
    let mut data = rgba.to_vec();
    for pixel in data.as_chunks_mut::<4>().0 {
        pixel.rotate_right(1);
    }
    Icon {
        width,
        height,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_becomes_argb() {
        let icon = argb_icon(&[1, 2, 3, 4], 1, 1);
        assert_eq!(icon.data, [4, 1, 2, 3]);
    }

    #[test]
    fn look_follows_the_most_public_activity() {
        let s = |recording, replay, live| State {
            recording,
            replay,
            live,
            ..Default::default()
        };
        assert_eq!(Look::of(&s(false, false, false)), Look::Idle);
        assert_eq!(Look::of(&s(false, true, false)), Look::Replay);
        assert_eq!(Look::of(&s(true, true, false)), Look::Recording);
        assert_eq!(Look::of(&s(true, true, true)), Look::Live);
        assert_eq!(Look::Idle.icon_name(), format!("{APP_ID}-symbolic"));
    }

    #[test]
    fn pixmaps_render_every_look() {
        for look in Look::ALL {
            for size in PIXMAP_SIZES {
                let icon = pixmap(look, size).unwrap();
                let n = size as i32;
                assert_eq!((icon.width, icon.height), (n, n));
                assert_eq!(icon.data.len(), (n * n * 4) as usize);
                // Not blank.
                assert!(icon.data.chunks(4).any(|p| p[0] == 255));
            }
        }
    }

    #[test]
    fn pixmaps_are_violet_and_the_recording_dot_red() {
        // ARGB of the center pixel, inside the dot.
        let center = |look| pixmap(look, 22).unwrap().data[(11 * 22 + 11) * 4..][..4].to_vec();
        assert_eq!(center(Look::Idle), [255, 0x85, 0x77, 0xf2]);
        assert_eq!(center(Look::Recording), [255, 0xda, 0x44, 0x53]);
    }
}
