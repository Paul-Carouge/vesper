//! Vesper desktop app: the Slint UI shell.
//!
//! No backend is wired yet. The callbacks below are stubs that log the intent
//! and simulate the state a real backend would publish (elapsed time, audio
//! levels, encoder stats), so the UI can be exercised end to end.

mod sim;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use slint::ComponentHandle;

use sim::Sim;

slint::include_modules!();

/// Minimal stderr logger, enough until a real backend brings `tracing`.
macro_rules! log {
    ($($arg:tt)*) => { eprintln!("[vesper] {}", format_args!($($arg)*)) };
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;

    // `VESPER_THEME=light|dark` overrides the system color scheme.
    match std::env::var("VESPER_THEME").as_deref() {
        Ok("light") => ui
            .global::<Theme>()
            .set_scheme(slint::language::ColorScheme::Light),
        Ok("dark") => ui
            .global::<Theme>()
            .set_scheme(slint::language::ColorScheme::Dark),
        _ => {}
    }

    ui.set_output_folder(default_output_folder().into());
    ui.set_replay_enabled(true);

    let sim = Rc::new(RefCell::new(Sim::default()));
    wire_callbacks(&ui, &sim);

    // ~30 Hz is enough for meters and the clock; nothing redraws when idle
    // except the meters themselves.
    let tick = slint::Timer::default();
    let weak = ui.as_weak();
    let sim_tick = sim.clone();
    tick.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(33),
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut sim = sim_tick.borrow_mut();
            let s = sim.step(Duration::from_millis(33));
            ui.set_desktop_level(s.desktop.level);
            ui.set_desktop_peak(s.desktop.peak);
            ui.set_mic_level(s.mic.level);
            ui.set_mic_peak(s.mic.peak);
            if let Some(rec) = s.recording {
                ui.set_elapsed_seconds(rec.elapsed.as_secs() as i32);
                ui.set_measured_fps(rec.fps);
                ui.set_dropped_frames(rec.dropped as i32);
            }
        },
    );

    apply_dev_overrides(&ui);
    ui.run()
}

/// Developer knobs for layout checks and screenshots:
/// - `VESPER_WINDOW_SIZE=360x700` sets the initial logical window size.
/// - `VESPER_DEMO=source,recording,settings,muted,tab,esc,ctrl+<key>`
///   replays UI intents and key presses once the window is up.
fn apply_dev_overrides(ui: &AppWindow) {
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
                "tab" => press_key(&ui, slint::platform::Key::Tab.into(), false),
                "esc" => press_key(&ui, slint::platform::Key::Escape.into(), false),
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
    use slint::platform::{Key, WindowEvent};
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

fn wire_callbacks(ui: &AppWindow, sim: &Rc<RefCell<Sim>>) {
    let weak = ui.as_weak();
    ui.on_pick_source(move || {
        log!("pick-source: would open the xdg-desktop-portal picker");
        let Some(ui) = weak.upgrade() else { return };
        // Alternate between a screen and a window so both looks can be seen.
        if ui.get_source_kind() == SourceKind::Screen {
            ui.set_source_kind(SourceKind::Window);
            ui.set_source_name("Firefox  —  Vesper on GitHub".into());
            ui.set_source_detail("Window  ·  1920 × 1080".into());
            ui.set_resolution("1920×1080".into());
        } else {
            ui.set_source_kind(SourceKind::Screen);
            ui.set_source_name("Dell U2724D".into());
            ui.set_source_detail("DP-1  ·  2560 × 1440".into());
            ui.set_resolution("2560×1440".into());
        }
    });

    let weak = ui.as_weak();
    let s = sim.clone();
    ui.on_start_recording(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_source_kind() == SourceKind::None {
            log!("start-recording: no source yet, opening the picker first");
            ui.invoke_pick_source();
        }
        log!(
            "start-recording: codec={:?} quality={:?} fps={}",
            vesper_core::Codec::from(ui.get_codec()),
            ui.get_quality(),
            ui.get_fps()
        );
        s.borrow_mut().start_recording(ui.get_fps() as f32);
        ui.set_elapsed_seconds(0);
        ui.set_dropped_frames(0);
        ui.set_measured_fps(ui.get_fps() as f32);
        ui.set_recording(true);
    });

    let weak = ui.as_weak();
    let s = sim.clone();
    ui.on_stop_recording(move || {
        let Some(ui) = weak.upgrade() else { return };
        log!("stop-recording");
        s.borrow_mut().stop_recording();
        ui.set_recording(false);
        ui.set_elapsed_seconds(0);
        flash_notice(&ui, "Recording saved");
    });

    let weak = ui.as_weak();
    ui.on_save_replay(move || {
        let Some(ui) = weak.upgrade() else { return };
        log!("save-replay: last {} s", ui.get_replay_seconds());
        flash_notice(&ui, "Replay saved");
    });

    ui.on_replay_changed(|enabled, seconds| {
        log!("replay-changed: enabled={enabled} seconds={seconds}");
    });

    let s = sim.clone();
    ui.on_desktop_mute_changed(move |muted| {
        log!("desktop-mute-changed: {muted}");
        s.borrow_mut().desktop_muted = muted;
    });

    let s = sim.clone();
    ui.on_mic_mute_changed(move |muted| {
        log!("mic-mute-changed: {muted}");
        s.borrow_mut().mic_muted = muted;
    });

    let weak = ui.as_weak();
    ui.on_output_settings_changed(move || {
        let Some(ui) = weak.upgrade() else { return };
        log!(
            "output-settings-changed: codec={:?} quality={:?} fps={}",
            ui.get_codec(),
            ui.get_quality(),
            ui.get_fps()
        );
    });

    ui.on_pick_output_folder(|| {
        log!("pick-output-folder: would open the FileChooser portal");
    });
}

impl From<VideoCodec> for vesper_core::Codec {
    fn from(codec: VideoCodec) -> Self {
        match codec {
            VideoCodec::Av1 => Self::Av1,
            VideoCodec::Hevc => Self::Hevc,
            VideoCodec::H264 => Self::H264,
        }
    }
}

/// Shows a short confirmation in the status line.
fn flash_notice(ui: &AppWindow, text: &str) {
    ui.set_notice(text.into());
    let weak = ui.as_weak();
    let shown = text.to_owned();
    slint::Timer::single_shot(Duration::from_millis(2500), move || {
        if let Some(ui) = weak.upgrade()
            && ui.get_notice() == shown.as_str()
        {
            ui.set_notice("".into());
        }
    });
}

fn default_output_folder() -> String {
    std::env::var("HOME")
        .map(|home| format!("{home}/Videos/Vesper"))
        .unwrap_or_else(|_| "~/Videos/Vesper".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_codec_maps_to_core_codec() {
        assert_eq!(
            vesper_core::Codec::from(VideoCodec::Av1),
            vesper_core::Codec::Av1
        );
        assert_eq!(
            vesper_core::Codec::from(VideoCodec::Hevc),
            vesper_core::Codec::Hevc
        );
        assert_eq!(
            vesper_core::Codec::from(VideoCodec::H264),
            vesper_core::Codec::H264
        );
    }
}
