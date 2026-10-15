//! System tray icon (StatusNotifierItem, `ksni`), so Vesper can run with its
//! window closed. `ksni` serves the item and its menu on its own thread; every
//! click goes to the `on_command` callback, which must hand it to the UI
//! thread. The UI pushes its state back with [`Tray::set`].

use std::sync::{Arc, mpsc};

use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{Icon, MenuItem, ToolTip};

use crate::shortcuts::APP_ID;

/// What a tray click asks the UI to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    ToggleWindow,
    ToggleRecording,
    SaveReplay,
    ToggleReplay,
    Quit,
}

/// What the tray shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub window_visible: bool,
    pub recording: bool,
    /// Starting or stopping: record intents are ignored.
    pub busy: bool,
    pub replay: bool,
}

type OnCommand = Arc<dyn Fn(Command) + Send + Sync>;

struct Item {
    state: State,
    /// The app icon, used when the icon theme does not have it.
    icon: Vec<Icon>,
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
        APP_ID.into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.icon.clone()
    }

    fn overlay_icon_pixmap(&self) -> Vec<Icon> {
        if self.state.recording {
            vec![red_dot(32)]
        } else {
            Vec::new()
        }
    }

    fn tool_tip(&self) -> ToolTip {
        let description = if self.state.recording {
            "Recording"
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
        let s = self.state;
        let item = |label: &str, enabled: bool, command: Command| {
            StandardItem {
                label: label.into(),
                enabled,
                activate: Box::new(move |item: &mut Self| (item.on_command)(command)),
                ..Default::default()
            }
            .into()
        };
        vec![
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
            CheckmarkItem {
                label: "Replay buffer".into(),
                checked: s.replay,
                activate: Box::new(|item: &mut Self| (item.on_command)(Command::ToggleReplay)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|item: &mut Self| (item.on_command)(Command::Quit)),
                ..Default::default()
            }
            .into(),
        ]
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
            icon: app_icon().into_iter().collect(),
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

/// The app icon rendered from its SVG (128 x 128).
fn app_icon() -> Option<Icon> {
    let svg = include_bytes!("../data/io.github.PaulCarouge.Vesper.svg");
    let pixels = slint::Image::load_from_svg_data(svg).ok()?.to_rgba8()?;
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

/// A red disc, the recording overlay.
fn red_dot(size: i32) -> Icon {
    let r = size as f32 / 2.0;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 + 0.5 - r).powi(2) + (y as f32 + 0.5 - r).powi(2)).sqrt();
            // One pixel of antialiasing on the edge.
            let alpha = ((r - d).clamp(0.0, 1.0) * 255.0) as u8;
            data.extend_from_slice(&[alpha, 0xe5, 0x39, 0x35]);
        }
    }
    Icon {
        width: size,
        height: size,
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
    fn app_icon_renders() {
        let icon = app_icon().unwrap();
        assert_eq!((icon.width, icon.height), (128, 128));
        assert_eq!(icon.data.len(), 128 * 128 * 4);
    }

    #[test]
    fn red_dot_is_round() {
        let dot = red_dot(32);
        assert_eq!(dot.data.len(), 32 * 32 * 4);
        let alpha = |x: usize, y: usize| dot.data[(y * 32 + x) * 4];
        assert_eq!(alpha(16, 16), 255);
        assert_eq!(alpha(0, 0), 0);
    }
}
