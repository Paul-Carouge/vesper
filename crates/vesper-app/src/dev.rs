//! Developer knobs for layout checks and screenshots (debug builds only):
//! - `VESPER_WINDOW_SIZE=360x700` sets the initial logical window size.
//! - `VESPER_DEMO=source,game,allow,ignore,recording,recording-look,settings,muted,browse,saved,notify,tab,esc,ctrl+<key>`
//!   replays UI intents and key presses once the window is up (`ctrl+comma`
//!   for `,`). `recording-look` fakes the recording state (no capture).
//!   `saved` shows the saved-file notice and `notify` sends the desktop
//!   notification, both for `VESPER_DEMO_FILE`.
//!   Looks without a capture or a game, for screenshots: `screen-look`,
//!   `game-look`, `consent-look`, `starting-look`, `stopping-look`, `layer-installed`, `layer-missing`,
//!   `replay-on`, `replay-off`; `scroll-end` scrolls the settings to the end.
//! - `VESPER_SNAPSHOT=<file.ppm>` saves the window as a PPM image once the
//!   demo settled, then quits. Nothing outside the window is captured.
//! - `VESPER_DEMO_DELAY=<ms>` runs `VESPER_DEMO` that long after launch
//!   (default 300), e.g. once a game asks for consent (`allow`, `ignore`).
//! - `vesper --record <seconds>` records without the UI, from the saved
//!   source or `VESPER_RESTORE_TOKEN`, with `default_settings()`.
//! - `vesper --record-game <seconds>` records the game capture the same way,
//!   with the saved allow list, once a game sends frames.
//! - `vesper --replay <buffer_s> <wait_s>` runs the replay buffer for
//!   `wait_s`, recording during the second half, then saves the replay.

use std::time::Duration;

use slint::ComponentHandle;
use slint::platform::{Key, WindowEvent};

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use crate::AppWindow;
use crate::game::GameSource;
use crate::prefs::{ALLOWED_GAMES, Prefs};
use crate::recorder::{Event, Pipeline, Settings};

pub fn record_seconds() -> Option<u64> {
    let mut args = std::env::args().skip(1);
    (args.next()? == "--record").then(|| args.next()?.parse().ok())?
}

pub fn record_game_seconds() -> Option<u64> {
    let mut args = std::env::args().skip(1);
    (args.next()? == "--record-game").then(|| args.next()?.parse().ok())?
}

pub fn replay_seconds() -> Option<(u64, u64)> {
    let mut args = std::env::args().skip(1);
    if args.next()? != "--replay" {
        return None;
    }
    Some((args.next()?.parse().ok()?, args.next()?.parse().ok()?))
}

fn folder() -> PathBuf {
    crate::default_output_folder().unwrap_or_else(|| PathBuf::from("."))
}

/// Logs events until `want` matches one; false if the pipeline stopped first.
fn wait_for(rx: &Receiver<Event>, want: impl Fn(&Event) -> bool) -> bool {
    for event in rx {
        log!("{event:?}");
        if want(&event) {
            return true;
        }
        if matches!(event, Event::Stopped(_)) {
            return false;
        }
    }
    false
}

pub fn record(seconds: u64, settings: Settings) {
    let (tx, rx) = std::sync::mpsc::channel();
    let p = Pipeline::start(settings, None, Some(folder()), move |event| {
        drop(tx.send(event))
    })
    .expect("spawn");
    if !wait_for(&rx, |e| matches!(e, Event::Started)) {
        return;
    }
    std::thread::sleep(Duration::from_secs(seconds));
    p.join();
    for event in rx {
        log!("{event:?}");
    }
}

pub fn record_game(seconds: u64, mut settings: Settings) {
    // Measurement knob: `VESPER_FPS=144`.
    #[cfg(feature = "perf-audit")]
    if let Some(fps) = std::env::var("VESPER_FPS")
        .ok()
        .and_then(|f| f.parse().ok())
    {
        settings.fps = fps;
    }
    let prefs = Prefs::load(Prefs::default());
    *ALLOWED_GAMES.lock().unwrap() = prefs.allowed_games;
    let source = GameSource::start().expect("game capture");
    settings.game = Some(source.frames.clone());
    log!("waiting for an allowed game");
    record(seconds, settings);
    drop(source);
}

pub fn replay(buffer: u64, wait: u64, settings: Settings) {
    let (tx, rx) = std::sync::mpsc::channel();
    let span = Some(Duration::from_secs(buffer));
    let p =
        Pipeline::start(settings, span, None, move |event| drop(tx.send(event))).expect("spawn");
    std::thread::sleep(Duration::from_secs(wait / 2));
    if !p.record(folder()) || !wait_for(&rx, |e| matches!(e, Event::Started)) {
        return;
    }
    std::thread::sleep(Duration::from_secs(wait - wait / 2));
    p.stop_recording();
    p.save_replay(folder());
    // Both the recording and the replay, in either order.
    for _ in 0..2 {
        if !wait_for(&rx, |e| {
            matches!(e, Event::Finished(_) | Event::ReplaySaved(_))
        }) {
            return;
        }
    }
    p.join();
    for event in rx {
        log!("{event:?}");
    }
}

pub fn apply(ui: &AppWindow) {
    if let Ok(size) = std::env::var("VESPER_WINDOW_SIZE")
        && let Some((w, h)) = size.split_once('x')
        && let (Ok(w), Ok(h)) = (w.parse::<f32>(), h.parse::<f32>())
    {
        ui.window().set_size(slint::LogicalSize::new(w, h));
    }
    let Ok(demo) = std::env::var("VESPER_DEMO") else {
        return;
    };
    let delay = std::env::var("VESPER_DEMO_DELAY")
        .ok()
        .and_then(|d| d.parse().ok())
        .unwrap_or(300);
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(delay), move || {
        let Some(ui) = weak.upgrade() else { return };
        for step in demo.split(',') {
            if look(&ui, step.trim()) {
                continue;
            }
            match step.trim() {
                "source" => ui.invoke_pick_source(),
                "recording" => ui.invoke_start_recording(),
                "settings" => ui.set_settings_open(true),
                "game" => ui.invoke_source_mode_changed(true),
                "allow" => ui.invoke_allow_game(),
                "ignore" => ui.invoke_ignore_game(),
                "browse" => ui.invoke_pick_output_folder(),
                "saved" => crate::show_notice(&ui, "Replay saved", Some(demo_file())),
                "notify" => crate::desktop::notify(
                    "Replay saved".into(),
                    demo_file().display().to_string(),
                    Some(demo_file()),
                ),
                "scroll-end" => {
                    let weak = ui.as_weak();
                    slint::Timer::single_shot(Duration::from_millis(400), move || {
                        let Some(ui) = weak.upgrade() else { return };
                        let size = ui.window().size().to_logical(ui.window().scale_factor());
                        let position =
                            slint::LogicalPosition::new(size.width - 40.0, size.height / 2.0);
                        let window = ui.window();
                        window.dispatch_event(WindowEvent::PointerMoved { position });
                        window.dispatch_event(WindowEvent::PointerScrolled {
                            position,
                            delta_x: 0.0,
                            delta_y: -5000.0,
                        });
                    });
                }
                "muted" => ui.invoke_track_mute_toggled(1),
                "track-open" => ui.invoke_open_track_settings(0),
                "track-picker" => ui.invoke_open_track_picker(),
                "tab" => press_key(&ui, Key::Tab.into(), false),
                "esc" => press_key(&ui, Key::Escape.into(), false),
                other => match other.strip_prefix("ctrl+") {
                    Some("comma") => press_key(&ui, ",".into(), true),
                    Some(key) if !key.is_empty() => press_key(&ui, key.into(), true),
                    _ => log!("VESPER_DEMO: unknown step {other:?}"),
                },
            }
        }
    });
    if let Ok(path) = std::env::var("VESPER_SNAPSHOT") {
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(delay + 1200), move || {
            let Some(ui) = weak.upgrade() else { return };
            // Again: the games tick may have reset a game look meanwhile.
            let demo = std::env::var("VESPER_DEMO").unwrap_or_default();
            for step in demo.split(',') {
                look(&ui, step.trim());
            }
            match ui.window().take_snapshot() {
                Ok(image) => {
                    let mut ppm =
                        format!("P6\n{} {}\n255\n", image.width(), image.height()).into_bytes();
                    ppm.extend(image.as_slice().iter().flat_map(|p| [p.r, p.g, p.b]));
                    if let Err(e) = std::fs::write(&path, ppm) {
                        log!("VESPER_SNAPSHOT: {e}");
                    }
                }
                Err(e) => log!("VESPER_SNAPSHOT: {e}"),
            }
            drop(slint::quit_event_loop());
        });
    }
}

/// Applies a look step: UI state only, no capture. False for other steps.
fn look(ui: &AppWindow, step: &str) -> bool {
    match step {
        // The recording look without a capture, for screenshots.
        "recording-look" => {
            ui.set_rec_state(crate::RecState::Recording);
            ui.set_elapsed_seconds(83);
            ui.set_measured_fps(59.9);
            ui.set_resolution("3840×2160".into());
        }
        "screen-look" => {
            ui.set_source_kind(crate::SourceKind::Screen);
            ui.set_source_name("DP-1".into());
            ui.set_source_detail("2560 × 1440".into());
        }
        "game-look" => {
            ui.set_source_kind(crate::SourceKind::Game);
            ui.set_game_layer_installed(true);
            ui.set_source_name("eldenring.exe".into());
            ui.set_source_detail("3840 × 2160".into());
            let others = [crate::GameEntry {
                name: "factorio".into(),
                pid: 4242,
            }];
            ui.set_other_games(std::rc::Rc::new(slint::VecModel::from(others.to_vec())).into());
        }
        "consent-look" => {
            ui.set_game_layer_installed(true);
            ui.set_pending_game("eldenring.exe".into());
        }
        "layer-installed" => ui.set_game_layer_installed(true),
        "layer-missing" => {
            ui.set_game_layer_installed(false);
            ui.set_game_layer_available(false);
        }
        "starting-look" => ui.set_rec_state(crate::RecState::Starting),
        "stopping-look" => {
            ui.set_measured_fps(59.9);
            ui.set_rec_state(crate::RecState::Stopping);
            ui.set_elapsed_seconds(83);
        }
        "tracks-look" => {
            use crate::recorder::Track;
            use vesper_audio::Source;
            let tracks = [
                Track::new("Desktop", Source::Desktop),
                Track::new("Microphone", Source::Microphone),
                Track {
                    volume_db: -6.0,
                    ..Track::new("Firefox", Source::App("Firefox".into()))
                },
                Track {
                    muted: true,
                    ..Track::new("Headset", Source::Output("alsa_output.usb".into()))
                },
            ];
            crate::audio::show_tracks(ui, &tracks);
            let levels = [0.62, 0.35, 0.48, 0.0];
            let model = |v: &[f32]| std::rc::Rc::new(slint::VecModel::from(v.to_vec())).into();
            ui.set_track_levels(model(&levels));
            ui.set_track_peaks(model(&levels.map(|l: f32| (l + 0.12).min(1.0))));
        }
        "replay-on" => ui.set_replay_enabled(true),
        "replay-off" => ui.set_replay_enabled(false),
        _ => return false,
    }
    true
}

fn demo_file() -> PathBuf {
    std::env::var_os("VESPER_DEMO_FILE").map_or_else(|| folder().join("demo.mkv"), PathBuf::from)
}

fn press_key(ui: &AppWindow, text: slint::SharedString, control: bool) {
    let window = ui.window();
    let ctrl: slint::SharedString = Key::Control.into();
    if control {
        window.dispatch_event(WindowEvent::KeyPressed { text: ctrl.clone() });
    }
    window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window.dispatch_event(WindowEvent::KeyReleased { text });
    if control {
        window.dispatch_event(WindowEvent::KeyReleased { text: ctrl });
    }
}
