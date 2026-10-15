//! Developer knobs for layout checks and screenshots (debug builds only):
//! - `VESPER_WINDOW_SIZE=360x700` sets the initial logical window size.
//! - `VESPER_DEMO=source,recording,settings,muted,tab,esc,ctrl+<key>` replays
//!   UI intents and key presses once the window is up (`ctrl+comma` for `,`).
//! - `vesper --record <seconds>` records without the UI, from the saved
//!   source or `VESPER_RESTORE_TOKEN`, with `default_settings()`.

use std::time::Duration;

use slint::ComponentHandle;
use slint::platform::{Key, WindowEvent};

use crate::AppWindow;
use crate::recorder::{Event, Recorder, Settings};

pub fn record_seconds() -> Option<u64> {
    let mut args = std::env::args().skip(1);
    (args.next()? == "--record").then(|| args.next()?.parse().ok())?
}

pub fn record(seconds: u64, settings: Settings) {
    let (tx, rx) = std::sync::mpsc::channel();
    let rec = Recorder::start(settings, move |event| drop(tx.send(event))).expect("spawn");
    for event in &rx {
        log!("{event:?}");
        match event {
            Event::Started => break,
            Event::Finished(_) => return,
            _ => {}
        }
    }
    std::thread::sleep(Duration::from_secs(seconds));
    rec.join();
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
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(300), move || {
        let Some(ui) = weak.upgrade() else { return };
        for step in demo.split(',') {
            match step.trim() {
                "source" => ui.invoke_pick_source(),
                "recording" => ui.invoke_start_recording(),
                "settings" => ui.set_settings_open(true),
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
