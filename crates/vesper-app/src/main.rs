//! Vesper desktop app: the Slint UI and the video recording backend.
//!
//! Recording (video + desktop and microphone audio) lives in `recorder`.
//! Audio sources only run while recording, so the meters rest at 0 when idle.

/// Minimal stderr logger, enough until a real backend brings `tracing`.
macro_rules! log {
    ($($arg:tt)*) => { eprintln!("[vesper] {}", format_args!($($arg)*)) };
}

#[cfg(debug_assertions)]
mod dev;
mod meter;
mod recorder;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use slint::ComponentHandle;
use vesper_audio::Source;

use meter::MeterState;
use recorder::{Event, Recorder, Settings};

type Recording = Rc<RefCell<Option<Recorder>>>;

/// Meter refresh period: ~30 Hz.
const METER_TICK: Duration = Duration::from_millis(33);

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    #[cfg(debug_assertions)]
    if let Some(seconds) = dev::record_seconds() {
        dev::record(seconds, default_settings());
        return Ok(());
    }

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
    if recorder::load_restore_token().is_some() {
        ui.set_source_kind(SourceKind::Screen);
        ui.set_source_name("Last picked source".into());
    }

    let recording = Recording::default();
    wire_callbacks(&ui, &recording);

    let tick = slint::Timer::default();
    let weak = ui.as_weak();
    let rec = recording.clone();
    let (mut desktop, mut mic) = (MeterState::default(), MeterState::default());
    tick.start(slint::TimerMode::Repeated, METER_TICK, move || {
        let Some(ui) = weak.upgrade() else { return };
        let peak = |source| rec.borrow().as_ref().map_or(0.0, |r| r.peak(source));
        desktop.feed(peak(Source::Desktop), METER_TICK);
        mic.feed(peak(Source::Microphone), METER_TICK);
        ui.set_desktop_level(desktop.level);
        ui.set_desktop_peak(desktop.peak);
        ui.set_mic_level(mic.level);
        ui.set_mic_peak(mic.peak);
    });

    #[cfg(debug_assertions)]
    dev::apply(&ui);

    let result = ui.run();
    // Closing the window mid-recording still finalizes the file. While
    // `starting`, the thread may wait on the portal picker: exit without it.
    if let Some(rec) = recording.take()
        && ui.get_rec_state() != RecState::Starting
    {
        rec.join();
    }
    result
}

/// Handlers.
///
/// Contract: callbacks run on the UI thread and must return immediately. Do
/// the work elsewhere and push every state change back with
/// `weak.upgrade_in_event_loop(..)` or `slint::invoke_from_event_loop(..)`;
/// never block inside a callback. `rec-state` goes
/// `idle -> starting -> recording -> stopping -> idle`, or `starting -> idle`
/// when the picker is cancelled or the pipeline fails to start; the UI ignores
/// record intents while it is `starting` or `stopping`.
fn wire_callbacks(ui: &AppWindow, recording: &Recording) {
    let weak = ui.as_weak();
    ui.on_pick_source(move || {
        let Some(ui) = weak.upgrade() else { return };
        // The portal has no "pick only" call: forget the source so the next
        // recording opens the picker.
        recorder::clear_restore_token();
        ui.set_source_kind(SourceKind::None);
        ui.set_source_name("".into());
        ui.set_source_detail("".into());
        flash_notice(&ui, "The picker opens when you record");
    });

    let weak = ui.as_weak();
    let rec = recording.clone();
    ui.on_start_recording(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_rec_state() != RecState::Idle {
            return;
        }
        let settings = Settings {
            codec: ui.get_codec().into(),
            fps: ui.get_fps().max(1) as u32,
            bits_per_pixel: bits_per_pixel(ui.get_codec(), ui.get_quality()),
            folder: PathBuf::from(ui.get_output_folder().as_str()),
            desktop_muted: ui.get_desktop_muted(),
            mic_muted: ui.get_mic_muted(),
        };
        log!("start-recording: {settings:?}");
        ui.set_rec_state(RecState::Starting);
        let weak = ui.as_weak();
        let started = Recorder::start(settings, move |event| {
            let _ = weak.upgrade_in_event_loop(move |ui| on_recorder_event(&ui, event));
        });
        match started {
            Ok(r) => *rec.borrow_mut() = Some(r),
            Err(e) => {
                ui.set_rec_state(RecState::Idle);
                flash_notice(&ui, &format!("Could not start: {e}"));
            }
        }
    });

    let weak = ui.as_weak();
    let rec = recording.clone();
    ui.on_stop_recording(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_rec_state() != RecState::Recording {
            return;
        }
        log!("stop-recording");
        ui.set_rec_state(RecState::Stopping);
        if let Some(r) = rec.borrow().as_ref() {
            r.stop();
        }
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

    // Applied live while recording; the next recording reads the UI state.
    let rec = recording.clone();
    ui.on_desktop_mute_changed(move |muted| {
        if let Some(r) = rec.borrow().as_ref() {
            r.set_muted(Source::Desktop, muted);
        }
    });

    let rec = recording.clone();
    ui.on_mic_mute_changed(move |muted| {
        if let Some(r) = rec.borrow().as_ref() {
            r.set_muted(Source::Microphone, muted);
        }
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

/// Applies a recorder event on the UI thread.
fn on_recorder_event(ui: &AppWindow, event: Event) {
    match event {
        Event::Started => {
            ui.set_rec_state(RecState::Recording);
            ui.set_elapsed_seconds(0);
            ui.set_dropped_frames(0);
            ui.set_measured_fps(ui.get_fps() as f32);
            if ui.get_source_kind() == SourceKind::None {
                // shortcut: Capture exposes neither the source type nor its name; show them once it does.
                ui.set_source_kind(SourceKind::Screen);
                ui.set_source_name("Picked source".into());
            }
        }
        Event::Stats {
            width,
            height,
            elapsed,
            fps,
            dropped,
        } => {
            ui.set_elapsed_seconds(elapsed.as_secs() as i32);
            ui.set_measured_fps(fps);
            ui.set_dropped_frames(dropped.min(i32::MAX as u64) as i32);
            ui.set_resolution(format!("{width}×{height}").into());
            ui.set_source_detail(format!("{width} × {height}").into());
        }
        Event::Finished(result) => {
            ui.set_rec_state(RecState::Idle);
            ui.set_elapsed_seconds(0);
            match result {
                Ok(path) => {
                    log!("saved {}", path.display());
                    flash_notice(ui, "Recording saved");
                }
                Err(e) => {
                    log!("recording failed: {e}");
                    flash_notice(ui, &e);
                }
            }
        }
    }
}

/// Target bits per pixel per frame of each quality preset for HEVC and AV1;
/// H.264 gets 1.5x. At 3840x2160 60 fps: 15 / 25 / 40 Mbit/s.
fn bits_per_pixel(codec: VideoCodec, quality: QualityPreset) -> f64 {
    let bpp = match quality {
        QualityPreset::Performance => 0.03,
        QualityPreset::Balanced => 0.05,
        QualityPreset::Quality => 0.08,
    };
    if codec == VideoCodec::H264 {
        bpp * 1.5
    } else {
        bpp
    }
}

/// Settings of the debug `--record` path: HEVC, balanced, 60 fps.
#[cfg(debug_assertions)]
fn default_settings() -> Settings {
    Settings {
        codec: vesper_core::Codec::Hevc,
        fps: 60,
        bits_per_pixel: bits_per_pixel(VideoCodec::Hevc, QualityPreset::Balanced),
        folder: default_output_folder().unwrap_or_else(|| PathBuf::from(".")),
        desktop_muted: false,
        mic_muted: false,
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
    fn balanced_4k60_is_about_25_mbit() {
        let px = 3840.0 * 2160.0 * 60.0;
        let bps = px * bits_per_pixel(VideoCodec::Hevc, QualityPreset::Balanced);
        assert!((24e6..26e6).contains(&bps), "{bps}");
        assert!(
            bits_per_pixel(VideoCodec::H264, QualityPreset::Balanced)
                > bits_per_pixel(VideoCodec::Av1, QualityPreset::Balanced)
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
