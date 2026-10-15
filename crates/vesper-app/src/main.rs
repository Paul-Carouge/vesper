//! Vesper desktop app: the Slint UI shell.
//!
//! No backend is wired yet. The callbacks below are stubs that log the intent
//! and simulate the state a real backend would publish (elapsed time, audio
//! levels, encoder stats), so the UI can be exercised end to end.

/// Minimal stderr logger, enough until a real backend brings `tracing`.
macro_rules! log {
    ($($arg:tt)*) => { eprintln!("[vesper] {}", format_args!($($arg)*)) };
}

#[cfg(debug_assertions)]
mod dev;
mod sim;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::ComponentHandle;

use sim::Sim;

slint::include_modules!();

/// Simulated latency of the portal picker / encoder start and of the final
/// flush, so the transient `starting` / `stopping` states are visible.
const FAKE_START_DELAY: Duration = Duration::from_millis(350);
const FAKE_PICKER_DELAY: Duration = Duration::from_millis(600);
const FAKE_STOP_DELAY: Duration = Duration::from_millis(450);

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

    if let Some(folder) = default_output_folder() {
        ui.set_output_folder(folder.to_string_lossy().as_ref().into());
    }
    ui.set_replay_enabled(true);

    let sim = Rc::new(RefCell::new(Sim::default()));
    wire_callbacks(&ui, &sim);

    // ~30 Hz is enough for meters and the clock.
    let tick = slint::Timer::default();
    let weak = ui.as_weak();
    let sim_tick = sim.clone();
    tick.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(33),
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let s = sim_tick.borrow_mut().tick(Instant::now());
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

    #[cfg(debug_assertions)]
    dev::apply(&ui);

    ui.run()
}

/// Stub handlers.
///
/// Contract for the real backend: callbacks run on the UI thread and must
/// return immediately. Do the work elsewhere and push every state change back
/// with `weak.upgrade_in_event_loop(..)` or `slint::invoke_from_event_loop(..)`;
/// never block inside a callback. `rec-state` goes
/// `idle -> starting -> recording -> stopping -> idle`; the UI ignores record
/// intents while it is `starting` or `stopping`.
fn wire_callbacks(ui: &AppWindow, sim: &Rc<RefCell<Sim>>) {
    let weak = ui.as_weak();
    ui.on_pick_source(move || {
        log!("pick-source: would open the xdg-desktop-portal picker");
        let Some(ui) = weak.upgrade() else { return };
        cycle_fake_source(&ui);
    });

    let weak = ui.as_weak();
    let s = sim.clone();
    ui.on_start_recording(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_rec_state() != RecState::Idle {
            return;
        }
        log!(
            "start-recording: codec={:?} quality={:?} fps={}",
            vesper_core::Codec::from(ui.get_codec()),
            ui.get_quality(),
            ui.get_fps()
        );
        ui.set_rec_state(RecState::Starting);
        // Without a source, the portal picker opens first (async).
        let needs_source = ui.get_source_kind() == SourceKind::None;
        let delay = if needs_source {
            FAKE_PICKER_DELAY
        } else {
            FAKE_START_DELAY
        };
        let weak = ui.as_weak();
        let s = s.clone();
        slint::Timer::single_shot(delay, move || {
            let Some(ui) = weak.upgrade() else { return };
            if needs_source {
                cycle_fake_source(&ui);
            }
            s.borrow_mut()
                .start_recording(ui.get_fps() as f32, Instant::now());
            ui.set_elapsed_seconds(0);
            ui.set_dropped_frames(0);
            ui.set_measured_fps(ui.get_fps() as f32);
            ui.set_rec_state(RecState::Recording);
        });
    });

    let weak = ui.as_weak();
    let s = sim.clone();
    ui.on_stop_recording(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_rec_state() != RecState::Recording {
            return;
        }
        log!("stop-recording");
        ui.set_rec_state(RecState::Stopping);
        let weak = ui.as_weak();
        let s = s.clone();
        // Encoder flush + muxer finalization.
        slint::Timer::single_shot(FAKE_STOP_DELAY, move || {
            let Some(ui) = weak.upgrade() else { return };
            s.borrow_mut().stop_recording();
            ui.set_rec_state(RecState::Idle);
            ui.set_elapsed_seconds(0);
            flash_notice(&ui, "Recording saved");
        });
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
            vesper_core::Codec::from(ui.get_codec()),
            ui.get_quality(),
            ui.get_fps()
        );
    });

    ui.on_pick_output_folder(|| {
        log!("pick-output-folder: would open the FileChooser portal");
    });
}

/// Alternates between a screen and a window so both looks can be seen.
fn cycle_fake_source(ui: &AppWindow) {
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

/// `$XDG_VIDEOS_DIR/Vesper`, falling back to `$HOME/Videos/Vesper`.
fn default_output_folder() -> Option<PathBuf> {
    dirs::video_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Videos")))
        .map(|videos| videos.join("Vesper"))
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

    #[test]
    fn output_folder_is_absolute() {
        if let Some(folder) = default_output_folder() {
            assert!(folder.is_absolute(), "{folder:?}");
            assert!(folder.ends_with("Vesper"));
        }
    }
}
