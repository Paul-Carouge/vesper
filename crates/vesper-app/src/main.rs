//! Vesper desktop app: the Slint UI and the video recording backend.
//!
//! Recording and the replay buffer (video + desktop and microphone audio)
//! live in `recorder`. Audio sources only run while one of them does, so the
//! meters rest at 0 otherwise.

/// Minimal stderr logger, enough until a real backend brings `tracing`.
macro_rules! log {
    ($($arg:tt)*) => { eprintln!("[vesper] {}", format_args!($($arg)*)) };
}

#[cfg(debug_assertions)]
mod dev;
mod game;
mod layer;
mod meter;
mod prefs;
mod recorder;
mod shortcuts;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use slint::ComponentHandle;
use vesper_audio::Source;

use game::GameSource;
use meter::MeterState;
use prefs::Prefs;
use recorder::{Event, Pipeline, Settings};
use shortcuts::Action;

/// The last started pipeline; it may have ended since.
type Shared = Rc<RefCell<Option<Pipeline>>>;

/// The game capture server, while the Game source is selected.
type Games = Rc<RefCell<Option<GameSource>>>;

/// Generation of the last started pipeline: a late `Stopped` from an older
/// one must not reset the UI of the current one.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Meter refresh period: ~30 Hz.
const METER_TICK: Duration = Duration::from_millis(33);

/// How often the game list and consent prompt are refreshed.
const GAMES_TICK: Duration = Duration::from_millis(500);

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    if let Err(e) = futures_lite::future::block_on(shortcuts::register_app_id()) {
        log!(
            "could not register the app id with the portal (is {}.desktop installed? see README): {e}",
            shortcuts::APP_ID
        );
    }
    #[cfg(debug_assertions)]
    if let Some(seconds) = dev::record_seconds() {
        dev::record(seconds, default_settings());
        return Ok(());
    }
    #[cfg(debug_assertions)]
    if let Some(seconds) = dev::record_game_seconds() {
        dev::record_game(seconds, default_settings());
        return Ok(());
    }
    #[cfg(debug_assertions)]
    if let Some((buffer, wait)) = dev::replay_seconds() {
        dev::replay(buffer, wait, default_settings());
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
    Prefs::load(Prefs::from_ui(&ui)).apply(&ui);
    update_replay_memory(&ui);
    show_layer_state(&ui);

    let recording = Shared::default();
    let games = Games::default();
    set_source_mode(&ui, &games, ui.get_source_kind() == SourceKind::Game);
    wire_callbacks(&ui, &recording, &games);
    wire_game_callbacks(&ui, &games);
    wire_global_shortcuts(&ui);
    // The replay buffer runs from launch (may open the picker once).
    if ui.get_replay_enabled()
        && let Err(e) = start_pipeline(&ui, &recording, &games, None)
    {
        flash_notice(&ui, &e);
    }

    let games_tick = slint::Timer::default();
    let weak = ui.as_weak();
    let g = games.clone();
    games_tick.start(slint::TimerMode::Repeated, GAMES_TICK, move || {
        let Some(ui) = weak.upgrade() else { return };
        let alive = g
            .borrow_mut()
            .as_mut()
            .is_none_or(|games| games.refresh(&ui));
        if !alive {
            // A pipeline using the dead server reports its own error.
            log!("game capture stopped: restarting it");
            set_source_mode(&ui, &g, true);
        }
    });

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
    // Closing the window mid-recording still finalizes the file. Before the
    // capture started, the thread may wait on the portal picker: exit without it.
    // Replays being saved are finished too.
    if let Some(rec) = recording.take()
        && rec.started()
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
fn wire_callbacks(ui: &AppWindow, recording: &Shared, games: &Games) {
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
    let g = games.clone();
    ui.on_start_recording(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_rec_state() != RecState::Idle {
            return;
        }
        if ui.get_output_folder().is_empty() {
            flash_notice(&ui, "Choose an output folder first");
            return;
        }
        if ui.get_source_kind() == SourceKind::Game
            && !g.borrow().as_ref().is_some_and(GameSource::capturing)
        {
            flash_notice(&ui, "No game to record yet");
            return;
        }
        let folder = PathBuf::from(ui.get_output_folder().as_str());
        log!("start-recording: {}", folder.display());
        ui.set_rec_state(RecState::Starting);
        // Joins the running pipeline (replay buffer), or starts one.
        let joined = rec
            .borrow()
            .as_ref()
            .is_some_and(|p| p.record(folder.clone()));
        if !joined && let Err(e) = start_pipeline(&ui, &rec, &g, Some(folder)) {
            ui.set_rec_state(RecState::Idle);
            flash_notice(&ui, &e);
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
            r.stop_recording();
        }
    });

    let weak = ui.as_weak();
    let rec = recording.clone();
    ui.on_save_replay(move || {
        let Some(ui) = weak.upgrade() else { return };
        log!("save-replay: last {} s", ui.get_replay_seconds());
        if ui.get_output_folder().is_empty() {
            flash_notice(&ui, "Choose an output folder first");
            return;
        }
        let folder = PathBuf::from(ui.get_output_folder().as_str());
        let saving =
            ui.get_replay_enabled() && rec.borrow().as_ref().is_some_and(|p| p.save_replay(folder));
        if !saving {
            flash_notice(&ui, "The replay buffer is not running");
        }
    });

    let weak = ui.as_weak();
    let rec = recording.clone();
    let g = games.clone();
    ui.on_replay_changed(move |enabled, seconds| {
        let Some(ui) = weak.upgrade() else { return };
        log!("replay-changed: enabled={enabled} seconds={seconds}");
        update_replay_memory(&ui);
        Prefs::from_ui(&ui).save();
        let applied = rec
            .borrow()
            .as_ref()
            .is_some_and(|p| p.set_replay(replay_span(&ui)));
        if !applied
            && enabled
            && let Err(e) = start_pipeline(&ui, &rec, &g, None)
        {
            ui.set_replay_enabled(false);
            flash_notice(&ui, &e);
        }
    });

    // Applied live while recording; the next recording reads the UI state.
    let weak = ui.as_weak();
    let rec = recording.clone();
    ui.on_desktop_mute_changed(move |muted| {
        if let Some(r) = rec.borrow().as_ref() {
            r.set_muted(Source::Desktop, muted);
        }
        if let Some(ui) = weak.upgrade() {
            Prefs::from_ui(&ui).save();
        }
    });

    let weak = ui.as_weak();
    let rec = recording.clone();
    ui.on_mic_mute_changed(move |muted| {
        if let Some(r) = rec.borrow().as_ref() {
            r.set_muted(Source::Microphone, muted);
        }
        if let Some(ui) = weak.upgrade() {
            Prefs::from_ui(&ui).save();
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
        update_replay_memory(&ui);
        Prefs::from_ui(&ui).save();
    });

    ui.on_pick_output_folder(|| {
        log!("pick-output-folder: would open the FileChooser portal");
    });

    // Another source type needs another pipeline: end the replay buffer's,
    // and start one on the new source.
    let weak = ui.as_weak();
    let rec = recording.clone();
    let g = games.clone();
    ui.on_source_mode_changed(move |game| {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_rec_state() != RecState::Idle {
            return;
        }
        log!("source-mode-changed: game={game}");
        if let Some(p) = rec.borrow_mut().take() {
            p.set_replay(None);
        }
        set_source_mode(&ui, &g, game);
        Prefs::from_ui(&ui).save();
        if ui.get_replay_enabled()
            && let Err(e) = start_pipeline(&ui, &rec, &g, None)
        {
            flash_notice(&ui, &e);
        }
    });
}

/// Consent prompt, game picker and the layer toggle.
fn wire_game_callbacks(ui: &AppWindow, games: &Games) {
    let weak = ui.as_weak();
    let g = games.clone();
    ui.on_allow_game(move || {
        let Some(ui) = weak.upgrade() else { return };
        let exe = ui.get_pending_game();
        log!("allow-game: {exe}");
        if let Some(games) = g.borrow().as_ref()
            && games.allow(&exe)
        {
            Prefs::from_ui(&ui).save();
        }
        ui.set_pending_game("".into());
    });

    let weak = ui.as_weak();
    let g = games.clone();
    ui.on_ignore_game(move || {
        let Some(ui) = weak.upgrade() else { return };
        if let Some(games) = g.borrow_mut().as_mut() {
            games.ignore(&ui.get_pending_game());
        }
        ui.set_pending_game("".into());
    });

    let g = games.clone();
    ui.on_select_game(move |pid| {
        if let (Some(games), Ok(pid)) = (g.borrow_mut().as_mut(), u32::try_from(pid)) {
            games.select(pid);
        }
    });

    let weak = ui.as_weak();
    ui.on_game_layer_changed(move |on| {
        let Some(ui) = weak.upgrade() else { return };
        log!("game-layer-changed: {on}");
        let result = match (layer::data_home(), on) {
            (None, _) => Err("No data folder (HOME is not set)".to_owned()),
            (Some(data), true) => match layer::shipped() {
                Some(lib) => layer::install(&data, &lib).map_err(|e| e.to_string()),
                None => Err("The layer was not built with Vesper".to_owned()),
            },
            (Some(data), false) => layer::uninstall(&data).map_err(|e| e.to_string()),
        };
        match result {
            Ok(()) if on => flash_notice(&ui, "Game capture installed"),
            Ok(()) => flash_notice(&ui, "Game capture removed"),
            Err(e) => {
                log!("game capture layer: {e}");
                flash_notice(&ui, &format!("Could not change game capture: {e}"));
            }
        }
        show_layer_state(&ui);
    });
}

/// Shows whether the layer is installed. An installed layer older than the
/// one built with this binary is updated (the user opted in already).
fn show_layer_state(ui: &AppWindow) {
    let lib = layer::shipped();
    ui.set_game_layer_available(lib.is_some());
    let Some(data) = layer::data_home() else {
        ui.set_game_layer_installed(false);
        return;
    };
    let installed = layer::installed(&data);
    if installed
        && let Some(lib) = lib
        && layer::outdated(&data, &lib)
    {
        log!("updating the game capture layer from {}", lib.display());
        if let Err(e) = layer::install(&data, &lib) {
            log!("could not update the game capture layer: {e}");
        }
    }
    ui.set_game_layer_installed(installed);
}

/// Switches between the portal source and game capture. The game capture
/// server only runs in game mode.
fn set_source_mode(ui: &AppWindow, games: &Games, game: bool) {
    game::clear(ui);
    if game {
        match GameSource::start() {
            Ok(source) => {
                *games.borrow_mut() = Some(source);
                ui.set_source_kind(SourceKind::Game);
                return;
            }
            Err(e) => {
                log!("{e}");
                flash_notice(ui, &e);
            }
        }
    }
    *games.borrow_mut() = None;
    if recorder::load_restore_token().is_some() {
        ui.set_source_kind(SourceKind::Screen);
        ui.set_source_name("Last picked source".into());
    } else {
        ui.set_source_kind(SourceKind::None);
    }
}

/// Global shortcuts take the same path as the in-window ones, minus the
/// settings-open guard. The hints show the keys the user bound.
fn wire_global_shortcuts(ui: &AppWindow) {
    let bound = ui.as_weak();
    let activated = ui.as_weak();
    shortcuts::spawn(
        move |action, trigger| {
            let _ = bound.upgrade_in_event_loop(move |ui| {
                // Unbound in the KDE dialog: fall back to the in-window key.
                let key = if trigger.is_empty() {
                    match action {
                        Action::ToggleRecording => "Ctrl+R".to_owned(),
                        Action::SaveReplay => "Ctrl+S".to_owned(),
                    }
                } else {
                    trigger
                };
                match action {
                    Action::ToggleRecording => ui.set_record_shortcut(key.into()),
                    Action::SaveReplay => ui.set_replay_shortcut(key.into()),
                }
            });
        },
        move |action| {
            log!("global shortcut: {action:?}");
            let _ = activated.upgrade_in_event_loop(move |ui| match action {
                Action::ToggleRecording => ui.invoke_global_toggle_recording(),
                Action::SaveReplay => ui.invoke_global_save_replay(),
            });
        },
    );
}

/// Encoder settings from the UI.
fn settings(ui: &AppWindow, games: &Games) -> Settings {
    Settings {
        game: games.borrow().as_ref().map(|g| g.frames.clone()),
        codec: ui.get_codec().into(),
        fps: ui.get_fps().max(1) as u32,
        bits_per_pixel: bits_per_pixel(ui.get_codec(), ui.get_quality()),
        desktop_muted: ui.get_desktop_muted(),
        mic_muted: ui.get_mic_muted(),
    }
}

/// The replay buffer length, `None` when it is off.
fn replay_span(ui: &AppWindow) -> Option<Duration> {
    ui.get_replay_enabled()
        .then(|| Duration::from_secs(ui.get_replay_seconds().max(1) as u64))
}

/// Starts a pipeline with the UI's settings and replay buffer, recording into
/// `record` if set. It replaces the previous, ended one: that thread ends on
/// its own.
fn start_pipeline(
    ui: &AppWindow,
    shared: &Shared,
    games: &Games,
    record: Option<PathBuf>,
) -> Result<(), String> {
    let settings = settings(ui, games);
    log!("start-pipeline: {settings:?}");
    let weak = ui.as_weak();
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    let pipeline = Pipeline::start(settings, replay_span(ui), record, move |event| {
        let _ = weak.upgrade_in_event_loop(move |ui| on_recorder_event(&ui, generation, event));
    })
    .map_err(|e| format!("Could not start: {e}"))?;
    *shared.borrow_mut() = Some(pipeline);
    Ok(())
}

/// Applies a pipeline event on the UI thread.
/// Events of an older pipeline still count (its recording or replay really
/// ended), except `Stopped(Some)`, which would reset the current one.
fn on_recorder_event(ui: &AppWindow, generation: u64, event: Event) {
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
            // Game mode shows the game's own size (the file keeps its first one).
            if ui.get_source_kind() != SourceKind::Game {
                ui.set_source_detail(format!("{width} × {height}").into());
            }
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
        Event::ReplaySaved(result) => match result {
            Ok(path) => {
                log!("saved replay {}", path.display());
                flash_notice(ui, "Replay saved");
            }
            Err(e) => {
                log!("replay failed: {e}");
                flash_notice(ui, &e);
            }
        },
        // A clean stop always follows `Finished`: nothing to reset (a new
        // pipeline may already be starting).
        Event::Stopped(None) => log!("pipeline stopped"),
        Event::Stopped(Some(e)) if generation != GENERATION.load(Ordering::Relaxed) => {
            log!("earlier pipeline stopped: {e}");
        }
        Event::Stopped(Some(e)) => {
            log!("pipeline stopped: {e}");
            // Neither a recording nor the replay buffer runs any more.
            ui.set_rec_state(RecState::Idle);
            ui.set_elapsed_seconds(0);
            ui.set_replay_enabled(false);
            flash_notice(ui, &e);
        }
    }
}

/// Shows the RAM the replay buffer may take at 4K with the current settings:
/// its length (plus a 2 s GOP) times the target bitrate.
fn update_replay_memory(ui: &AppWindow) {
    let bitrate = 3840.0
        * 2160.0
        * f64::from(ui.get_fps())
        * bits_per_pixel(ui.get_codec(), ui.get_quality());
    let mb = bitrate / 8e6 * f64::from(ui.get_replay_seconds() + 2);
    let quality = match ui.get_quality() {
        QualityPreset::Performance => "Performance",
        QualityPreset::Balanced => "Balanced",
        QualityPreset::Quality => "Quality",
    };
    ui.set_replay_memory(format!("≈ {} MB at 4K{} {quality}", mb.round(), ui.get_fps()).into());
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
        desktop_muted: false,
        mic_muted: false,
        game: None,
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
