//! Developer knobs for layout checks and screenshots (debug builds only):
//! - `VESPER_WINDOW_SIZE=360x700` sets the initial logical window size.
//! - `VESPER_DEMO=source,game,allow,ignore,recording,settings,muted,browse,saved,notify,tab,esc,ctrl+<key>`
//!   replays UI intents and key presses once the window is up (`ctrl+comma`
//!   for `,`). `saved` shows the saved-file notice and `notify` sends the
//!   desktop notification, both for `VESPER_DEMO_FILE`.
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
                "muted" => {
                    ui.set_mic_muted(true);
                    ui.invoke_mic_mute_changed(true);
                }
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
