//! Vesper desktop app: the Slint UI and the video recording backend.
//!
//! Recording and the replay buffer (video + audio tracks) live in
//! `recorder`. Audio sources only run while one of them does, so the meters
//! rest at 0 otherwise. The Audio card is wired in `audio`.

/// Minimal stderr logger, enough until a real backend brings `tracing`.
macro_rules! log {
    ($($arg:tt)*) => { eprintln!("[vesper] {}", format_args!($($arg)*)) };
}

mod audio;
mod autostart;
mod desktop;
#[cfg(debug_assertions)]
mod dev;
mod fallback;
mod filters;
mod game;
mod instance;
mod layer;
mod live;
mod meter;
mod outputs;
mod overlay;
mod prefs;
mod profiles;
mod recorder;
mod scenes;
mod secret;
mod shortcuts;
mod stream;
mod tray;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slint::ComponentHandle;
use slint::winit_030::WinitWindowAccessor;

use game::GameSource;
use outputs::SourceLabel;
use prefs::Prefs;
use recorder::{Event, Pipeline, Settings};
use shortcuts::Action;
use tray::Tray;
use vesper_audio::TrackState;

/// The last started pipeline; it may have ended since.
type Shared = Rc<RefCell<Option<Pipeline>>>;

/// The game capture server, while the Game source is selected.
type Games = Rc<RefCell<Option<GameSource>>>;

/// Generation of the last started pipeline: a late `Stopped` from an older
/// one must not reset the UI of the current one.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The recording was stopped because the captured game closed.
static GAME_CLOSED: AtomicBool = AtomicBool::new(false);
/// The stream was ended with it: its notice says so (the stream's own would
/// replace the recording's, and its Open and Play buttons).
static GAME_CLOSED_STREAM: AtomicBool = AtomicBool::new(false);

/// The file the notice's Open and Play buttons act on.
static NOTICE_FILE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Bumped by every notice, so an older notice's timer leaves a newer one.
static NOTICE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// How long a notice stays: plain ones, and ones with file actions.
const NOTICE_TIME: Duration = Duration::from_millis(2500);
const FILE_NOTICE_TIME: Duration = Duration::from_secs(8);

/// Meter refresh period: ~30 Hz.
const METER_TICK: Duration = Duration::from_millis(33);

/// How often the game list and consent prompt are refreshed.
const GAMES_TICK: Duration = Duration::from_millis(500);

slint::include_modules!();

/// "Vesper 0.1.0.r45.g4077535": the Arch pkgver format; just the version
/// without git history.
fn version_string(version: &str, git_rev: Option<&str>) -> String {
    match git_rev {
        Some(rev) => format!("Vesper {version}.{rev}"),
        None => format!("Vesper {version}"),
    }
}

fn version() -> String {
    version_string(env!("CARGO_PKG_VERSION"), option_env!("VESPER_GIT_REV"))
}

fn main() -> Result<(), slint::PlatformError> {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("{}", version());
        return Ok(());
    }
    // First, before any thread starts, so they all inherit the blocked mask.
    handle_quit_signals();
    // The crates' warnings and errors (and our own info) on stderr; RUST_LOG overrides.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,vesper=info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    // Before any track or chain: they may hold VST3 plugins.
    filters::install_plugins();
    // Started with the session: stay in the tray (when there is one).
    let background = std::env::args().skip(1).any(|a| a == "--background");
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
    if let Some((seconds, url)) = dev::stream_game_args() {
        dev::stream_game(seconds, url, default_settings());
        return Ok(());
    }
    #[cfg(debug_assertions)]
    if let Some((buffer, wait)) = dev::replay_seconds() {
        dev::replay(buffer, wait, default_settings());
        return Ok(());
    }

    let ui = AppWindow::new()?;
    ui.set_version(version().into());
    // Before anything starts: a second launch only shows the first window.
    let weak = ui.as_weak();
    let instance = match instance::claim(!background, move || {
        let _ = weak.upgrade_in_event_loop(|ui| {
            let _ = ui.show();
            ui.invoke_tray_state_changed();
            ui.invoke_meters_wake();
        });
    }) {
        instance::Claim::First(conn) => conn,
        instance::Claim::Running => {
            log!("Vesper is already running");
            return Ok(());
        }
    };

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
    audio::show_tracks(&ui, &prefs::default_tracks());
    // Also the settings of a new profile.
    let defaults = Prefs::from_ui(&ui);
    Prefs::load(defaults.clone()).apply(&ui);
    // The entry may have been removed elsewhere (e.g. System Settings).
    ui.set_autostart(autostart::enabled());
    show_output_folder(&ui);
    update_replay_memory(&ui);
    show_layer_state(&ui);

    let recording = Shared::default();
    let games = Games::default();
    set_source_mode(&ui, &games, ui.get_source_kind() == SourceKind::Game);
    wire_callbacks(&ui, &recording, &games);
    wire_game_callbacks(&ui, &games);
    live::wire(&ui, &recording, &games);
    profiles::wire(&ui, &recording, &games, defaults);
    if prefs::SAVING_OFF.load(Ordering::Relaxed) {
        flash_notice(&ui, "Could not read the settings: changes are not saved");
    }
    wire_global_shortcuts(&ui);
    let tray = spawn_tray(&ui);
    wire_background(&ui, tray.as_ref());
    // The replay buffer runs from launch (may open the picker once).
    // In game mode without a server, the games tick starts it once it runs.
    let game_down = ui.get_source_kind() == SourceKind::Game && games.borrow().is_none();
    if ui.get_replay_enabled()
        && !game_down
        && let Err(e) = start_pipeline(&ui, &recording, &games, None, None)
    {
        flash_notice(&ui, &e);
    }

    // Refreshes game mode, and restarts a game capture server that failed
    // or could not start, backing off 1, 2, 4... up to 30 s.
    let games_tick = slint::Timer::default();
    let weak = ui.as_weak();
    let (g, rec) = (games.clone(), recording.clone());
    let (mut failures, mut retry_at) = (0u32, Instant::now());
    games_tick.start(slint::TimerMode::Repeated, GAMES_TICK, move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_source_kind() != SourceKind::Game {
            failures = 0;
            return;
        }
        let alive = g.borrow_mut().as_mut().map(|games| games.refresh(&ui));
        let now = Instant::now();
        let backoff = |n: u32| Duration::from_secs((1u64 << n.min(5)).min(30));
        match alive {
            Some(true) => {
                if now > retry_at + Duration::from_secs(60) {
                    failures = 0;
                }
                // A game that quit would leave a frozen picture recording
                // and streaming forever; a paused one stays connected and
                // keeps going.
                let closed = g.borrow_mut().as_mut().is_some_and(GameSource::take_closed);
                if closed {
                    let (stop_recording, end_stream, announce_stream) =
                        ends_on_game_closed(ui.get_rec_state(), ui.get_live_state());
                    if stop_recording {
                        log!("captured game closed: stopping the recording");
                        GAME_CLOSED.store(true, Ordering::Relaxed);
                        GAME_CLOSED_STREAM.store(end_stream, Ordering::Relaxed);
                        ui.invoke_stop_recording();
                    }
                    if end_stream {
                        live::end_for_closed_game(&ui, announce_stream);
                    }
                }
                return;
            }
            // A pipeline using the dead server reports its own error.
            Some(false) => {
                log!("game capture stopped");
                *g.borrow_mut() = None;
                game::clear(&ui);
                retry_at = now + backoff(failures);
                failures += 1;
            }
            None if now < retry_at => return,
            None => match GameSource::start() {
                Ok(source) => {
                    log!("game capture restarted");
                    *g.borrow_mut() = Some(source);
                    retry_at = now;
                    let applied = rec
                        .borrow()
                        .as_ref()
                        .is_some_and(|p| p.set_replay(replay_span(&ui)));
                    if !applied
                        && ui.get_replay_enabled()
                        && let Err(e) = start_pipeline(&ui, &rec, &g, None, None)
                    {
                        flash_notice(&ui, &e);
                    }
                    return;
                }
                Err(e) => {
                    log!("{e}");
                    retry_at = now + backoff(failures);
                    failures += 1;
                }
            },
        }
        if failures == 3 {
            flash_notice(&ui, "Game capture stopped");
        }
    });

    let _meters = audio::start_meters(&ui, &recording);

    #[cfg(debug_assertions)]
    dev::apply(&ui);

    if !(background && tray.is_some()) {
        ui.show()?;
        ui.invoke_tray_state_changed();
        ui.invoke_meters_wake();
    }
    // Hiding the window does not end the loop: only Quit, Ctrl+Q, or closing
    // the window without a tray does.
    let result = slint::run_event_loop_until_quit();
    if let Some(conn) = instance {
        instance::release(conn);
    }
    let _ = ui.hide();
    if let Some(tray) = tray {
        tray.shutdown();
    }
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

/// What the captured game quitting ends: (the recording, the stream,
/// whether the stream's end has a message of its own). One being stopped or
/// ended already is left alone. With both, the recording's message says it
/// all: a second one would replace it and its Open and Play buttons.
fn ends_on_game_closed(rec: RecState, live: LiveState) -> (bool, bool, bool) {
    let recording = rec == RecState::Recording;
    let stream = matches!(
        live,
        LiveState::Connecting | LiveState::Live | LiveState::Reconnecting
    );
    (recording, stream, stream && !recording)
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
        prefs::clear_source();
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
        // Game mode without a layer game records the game's window.
        let folder = PathBuf::from(ui.get_output_folder().as_str());
        log!("start-recording: {}", folder.display());
        ui.set_rec_state(RecState::Starting);
        // Joins the running pipeline (replay buffer), or starts one.
        let joined = rec
            .borrow()
            .as_ref()
            .is_some_and(|p| p.record(folder.clone()));
        if !joined && let Err(e) = start_pipeline(&ui, &rec, &g, Some(folder), None) {
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
        apply_replay(&ui, &rec, &g);
    });

    // Mute and volume apply live; the next recording reads the UI state.
    audio::wire(ui, recording);
    filters::wire(ui, recording);

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

    // One dialog at a time: it may stay open for long.
    static PICKING: AtomicBool = AtomicBool::new(false);
    let weak = ui.as_weak();
    ui.on_pick_output_folder(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_rec_state() != RecState::Idle || PICKING.swap(true, Ordering::Relaxed) {
            return;
        }
        log!("pick-output-folder");
        let weak = weak.clone();
        let current = PathBuf::from(ui.get_output_folder().as_str());
        desktop::pick_folder(current, move |result| {
            PICKING.store(false, Ordering::Relaxed);
            let _ = weak.upgrade_in_event_loop(move |ui| match result {
                Ok(Some(folder)) => {
                    log!("output folder: {}", folder.display());
                    ui.set_output_folder(folder.to_string_lossy().as_ref().into());
                    show_output_folder(&ui);
                    Prefs::from_ui(&ui).save();
                }
                Ok(None) => {}
                Err(e) => {
                    log!("folder picker: {e}");
                    flash_notice(&ui, "Could not open the folder picker");
                }
            });
        });
    });

    ui.on_open_notice_file(|| {
        if let Some(file) = notice_file() {
            desktop::show_in_folder(file);
        }
    });
    ui.on_play_notice_file(|| {
        if let Some(file) = notice_file() {
            desktop::open(&file);
        }
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
        // The stream runs in this pipeline: end it first.
        if ui.get_live_state() != LiveState::Off {
            flash_notice(&ui, "End the stream to change the source");
            return;
        }
        log!("source-mode-changed: game={game}");
        if let Some(p) = rec.borrow_mut().take() {
            p.set_replay(None);
        }
        set_source_mode(&ui, &g, game);
        Prefs::from_ui(&ui).save();
        start_replay(&ui, &rec, &g);
    });
}

/// Hands the replay switch and length to the pipeline, or starts one for
/// the replay buffer.
fn apply_replay(ui: &AppWindow, rec: &Shared, games: &Games) {
    let applied = rec
        .borrow()
        .as_ref()
        .is_some_and(|p| p.set_replay(replay_span(ui)));
    if !applied
        && ui.get_replay_enabled()
        && let Err(e) = start_pipeline(ui, rec, games, None, None)
    {
        ui.set_replay_enabled(false);
        flash_notice(ui, &e);
    }
}

/// Starts the replay buffer, if it is on, on the source mode just set (the
/// previous pipeline ended).
fn start_replay(ui: &AppWindow, rec: &Shared, games: &Games) {
    if !ui.get_replay_enabled() {
        return;
    }
    let game = ui.get_source_kind() == SourceKind::Game;
    // Without a saved source the portal picker would pop up right away.
    if !game && recorder::load_restore_token().is_none() {
        flash_notice(ui, "Replay starts once a source is picked");
    } else if game && games.borrow().is_none() {
        // The start error is shown; the games tick starts the replay
        // buffer once game capture runs.
    } else if let Err(e) = start_pipeline(ui, rec, games, None, None) {
        flash_notice(ui, &e);
    }
}

/// SIGTERM (the session ends) and SIGINT take the Quit path, so a recording
/// is finalized and replays being saved finish. A second one exits at once.
/// The signals are blocked in every thread and taken by a `sigwait` thread.
fn handle_quit_signals() {
    // SAFETY: plain libc calls on a local, initialized signal set.
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    };
    let spawned = std::thread::Builder::new()
        .name("vesper-signals".into())
        .spawn(move || {
            let mut quitting = false;
            loop {
                let mut signal = 0;
                // SAFETY: `set` is initialized; `signal` outlives the call.
                if unsafe { libc::sigwait(&set, &mut signal) } != 0 {
                    continue;
                }
                log!("signal {signal}: quitting");
                let queued = !quitting
                    && slint::invoke_from_event_loop(|| {
                        let _ = slint::quit_event_loop();
                    })
                    .is_ok();
                if !queued {
                    std::process::exit(128 + signal);
                }
                quitting = true;
            }
        });
    if let Err(e) = spawned {
        log!("could not start the signal thread: {e}");
    }
}

/// Registers the tray icon; `None` when the session has no tray. Its clicks
/// take the same paths as the window's buttons, on the UI thread.
fn spawn_tray(ui: &AppWindow) -> Option<Tray> {
    let weak = ui.as_weak();
    let spawned = Tray::spawn(tray_state(ui), move |command| {
        log!("tray: {command:?}");
        let _ = weak.upgrade_in_event_loop(move |ui| match command {
            tray::Command::ToggleWindow => {
                let _ = if ui.window().is_visible() {
                    ui.hide()
                } else {
                    ui.show()
                };
                ui.invoke_tray_state_changed();
                ui.invoke_meters_wake();
            }
            tray::Command::ToggleRecording => ui.invoke_global_toggle_recording(),
            tray::Command::SaveReplay => ui.invoke_global_save_replay(),
            tray::Command::ToggleReplay => ui.invoke_toggle_replay(),
            tray::Command::ToggleLive => ui.invoke_global_toggle_live(),
            tray::Command::Quit => ui.invoke_quit(),
        });
    });
    match spawned {
        Ok(tray) => Some(tray),
        Err(e) => {
            log!("no tray icon ({e}): closing the window quits");
            None
        }
    }
}

fn tray_state(ui: &AppWindow) -> tray::State {
    let rec = ui.get_rec_state();
    tray::State {
        window_visible: ui.window().is_visible(),
        recording: matches!(rec, RecState::Recording | RecState::Stopping),
        busy: matches!(rec, RecState::Starting | RecState::Stopping),
        replay: ui.get_replay_enabled(),
        live: ui.get_live_state() != LiveState::Off,
        live_busy: ui.get_live_state() == LiveState::Ending,
    }
}

/// Close-to-tray, quit, the tray's view of the state, and autostart.
fn wire_background(ui: &AppWindow, tray: Option<&Tray>) {
    ui.set_tray_available(tray.is_some());
    ui.on_quit(|| {
        log!("quit");
        let _ = slint::quit_event_loop();
    });

    let weak = ui.as_weak();
    let t = tray.cloned();
    ui.on_tray_state_changed(move || {
        if let (Some(ui), Some(tray)) = (weak.upgrade(), &t) {
            tray.set(tray_state(&ui));
        }
    });

    let weak = ui.as_weak();
    let t = tray.cloned();
    ui.window().on_close_requested(move || {
        let (Some(ui), Some(tray)) = (weak.upgrade(), &t) else {
            let _ = slint::quit_event_loop();
            return slint::CloseRequestResponse::HideWindow;
        };
        if !prefs::TRAY_NOTICE_SHOWN.swap(true, Ordering::Relaxed) {
            desktop::notify(
                "Vesper is still running".into(),
                "It stays in the tray, with the replay buffer and global shortcuts. \
                 Quit from the tray icon's menu."
                    .into(),
                None,
            );
            Prefs::from_ui(&ui).save();
        }
        // The window hides once this returns.
        tray.set(tray::State {
            window_visible: false,
            ..tray_state(&ui)
        });
        slint::CloseRequestResponse::HideWindow
    });

    let weak = ui.as_weak();
    ui.on_autostart_changed(move |on| {
        let Some(ui) = weak.upgrade() else { return };
        log!("autostart-changed: {on}");
        match autostart::set(on) {
            Ok(()) => ui.set_autostart(on),
            Err(e) => {
                log!("autostart entry: {e}");
                flash_notice(&ui, &format!("Could not change autostart: {e}"));
            }
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
    ui.on_ignore_game(move || {
        let Some(ui) = weak.upgrade() else { return };
        GameSource::ignore(&ui.get_pending_game());
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

/// Shows whether the layer is installed, and whether the one built with this
/// binary differs (the toggle's Update button installs it).
fn show_layer_state(ui: &AppWindow) {
    let lib = layer::shipped();
    ui.set_game_layer_available(lib.is_some());
    let Some(data) = layer::data_home() else {
        ui.set_game_layer_installed(false);
        return;
    };
    let installed = layer::installed(&data);
    // Offered, never done silently. Debug builds change the layer on every
    // build: no offer there.
    let outdated =
        !cfg!(debug_assertions) && installed && lib.is_some_and(|lib| layer::outdated(&data, &lib));
    ui.set_game_layer_installed(installed);
    ui.set_game_layer_outdated(outdated);
}

/// Switches between the portal source and game capture. The game capture
/// server only runs in game mode; if it cannot start, game mode stays with
/// the error shown, and the games tick retries.
fn set_source_mode(ui: &AppWindow, games: &Games, game: bool) {
    game::clear(ui);
    *games.borrow_mut() = None;
    if game {
        ui.set_source_kind(SourceKind::Game);
        match GameSource::start() {
            Ok(source) => *games.borrow_mut() = Some(source),
            Err(e) => {
                log!("{e}");
                flash_notice(ui, &e);
            }
        }
        return;
    }
    if recorder::load_restore_token().is_some() {
        match prefs::load_source() {
            Some(label) => show_source(ui, &label),
            // Saved by an older version: named once its capture starts.
            None => {
                ui.set_source_kind(SourceKind::Screen);
                ui.set_source_name("Saved source".into());
                ui.set_source_detail("Screen or window".into());
            }
        }
    } else {
        ui.set_source_kind(SourceKind::None);
    }
}

/// Shows the picked portal source in the source card.
fn show_source(ui: &AppWindow, label: &SourceLabel) {
    let (name, detail) = label.lines();
    ui.set_source_kind(if label.kind == outputs::Kind::Window {
        SourceKind::Window
    } else {
        SourceKind::Screen
    });
    ui.set_source_name(name.into());
    ui.set_source_detail(detail.into());
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
                        Action::ToggleLive => "Ctrl+L".to_owned(),
                    }
                } else {
                    trigger
                };
                match action {
                    Action::ToggleRecording => ui.set_record_shortcut(key.into()),
                    Action::SaveReplay => ui.set_replay_shortcut(key.into()),
                    Action::ToggleLive => ui.set_live_shortcut(key.into()),
                }
            });
        },
        move |action| {
            log!("global shortcut: {action:?}");
            let _ = activated.upgrade_in_event_loop(move |ui| match action {
                Action::ToggleRecording => ui.invoke_global_toggle_recording(),
                Action::SaveReplay => ui.invoke_global_save_replay(),
                Action::ToggleLive => ui.invoke_global_toggle_live(),
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
        quality: quality(ui.get_codec(), ui.get_quality()),
        max_bits_per_pixel: max_bits_per_pixel(ui.get_codec(), ui.get_quality()),
        tracks: audio::recorded_tracks(ui),
        mix: ui.get_audio_mix(),
        scene: recorder::lock(&scenes::SCENES).active_scene(),
        canvas: recorder::lock(&scenes::SCENES).canvas,
    }
}

/// The replay buffer length, `None` when it is off.
fn replay_span(ui: &AppWindow) -> Option<Duration> {
    ui.get_replay_enabled()
        .then(|| Duration::from_secs(ui.get_replay_seconds().max(1) as u64))
}

/// Starts a pipeline with the UI's settings and replay buffer, recording into
/// `record` and streaming `stream` if set. It replaces the previous, ended
/// one: that thread ends on its own.
fn start_pipeline(
    ui: &AppWindow,
    shared: &Shared,
    games: &Games,
    record: Option<PathBuf>,
    stream: Option<stream::StreamRequest>,
) -> Result<(), String> {
    if ui.get_source_kind() == SourceKind::Game && games.borrow().is_none() {
        return Err("Game capture is not running".into());
    }
    let settings = settings(ui, games);
    log!("start-pipeline: {settings:?}");
    let weak = ui.as_weak();
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    let pipeline = Pipeline::start(settings, replay_span(ui), record, stream, move |event| {
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
        // A late event of a portal pipeline must not rename a game.
        Event::Source(label) if ui.get_source_kind() != SourceKind::Game => {
            log!("source: {label:?}");
            show_source(ui, &label);
            prefs::save_source(&label);
        }
        Event::Source(_) => {}
        Event::AudioStarted => ui.invoke_meters_wake(),
        Event::Started => {
            ui.set_rec_state(RecState::Recording);
            ui.set_elapsed_seconds(0);
            ui.set_dropped_frames(0);
            ui.set_measured_fps(ui.get_fps() as f32);
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
            // The portal gives a window's size in logical pixels: show the
            // captured one. Game mode shows the game's own size (the file
            // keeps its first one); a monitor's is known already.
            if ui.get_source_kind() == SourceKind::Window {
                ui.set_source_detail(format!("{width} × {height}").into());
            }
        }
        Event::Finished(result) => {
            ui.set_rec_state(RecState::Idle);
            ui.set_elapsed_seconds(0);
            match result {
                Ok(path) => {
                    log!("saved {}", path.display());
                    let stream = GAME_CLOSED_STREAM.swap(false, Ordering::Relaxed);
                    let title = match (GAME_CLOSED.swap(false, Ordering::Relaxed), stream) {
                        (true, true) => "Game closed: recording saved, stream ended",
                        (true, false) => "Game closed, recording saved",
                        (false, _) => "Recording saved",
                    };
                    announce_saved(ui, title, path);
                }
                Err(e) => {
                    GAME_CLOSED.store(false, Ordering::Relaxed);
                    GAME_CLOSED_STREAM.store(false, Ordering::Relaxed);
                    log!("recording failed: {e}");
                    announce_failure(ui, "Recording failed", &e);
                }
            }
        }
        Event::ReplaySaved(result) => match result {
            Ok(path) => {
                log!("saved replay {}", path.display());
                announce_saved(ui, "Replay saved", path);
            }
            Err(e) => {
                log!("replay failed: {e}");
                announce_failure(ui, "Replay not saved", &e);
            }
        },
        Event::AudioTrack(name, TrackState::Reconnecting(reason)) => {
            log!("audio track {name:?}: {reason}");
            flash_notice(ui, &format!("{name}: reconnecting…"));
        }
        Event::AudioMissing(text) | Event::Notice(text) => flash_notice(ui, &text),
        Event::AudioTrack(name, TrackState::Running) => {
            flash_notice(ui, &format!("{name}: audio is back"));
        }
        // An older pipeline's stream ended with it: its late news must not
        // change the current one's.
        Event::Stream(_) | Event::StreamStats { .. }
            if generation != GENERATION.load(Ordering::Relaxed) => {}
        Event::Stream(state) => live::on_state(ui, state),
        Event::StreamStats {
            elapsed,
            bitrate,
            dropped,
        } => live::on_stats(ui, elapsed, bitrate, dropped),
        // A clean stop always follows `Finished`: nothing to reset (a new
        // pipeline may already be starting).
        Event::Stopped(None) => log!("pipeline stopped"),
        Event::Stopped(Some(e)) if generation != GENERATION.load(Ordering::Relaxed) => {
            log!("earlier pipeline stopped: {e}");
        }
        Event::Stopped(Some(e)) => {
            log!("pipeline stopped: {e}");
            // Neither a recording, the replay buffer nor a stream runs any more.
            ui.set_rec_state(RecState::Idle);
            ui.set_elapsed_seconds(0);
            ui.set_replay_enabled(false);
            live::reset(ui);
            flash_notice(ui, &e);
        }
    }
}

/// Shows the most RAM the replay buffer may take at 4K with the current
/// settings: its length (plus a 2 s GOP) times the bitrate cap.
fn update_replay_memory(ui: &AppWindow) {
    let bitrate = 3840.0
        * 2160.0
        * f64::from(ui.get_fps())
        * max_bits_per_pixel(ui.get_codec(), ui.get_quality());
    let mb = bitrate / 8e6 * f64::from(ui.get_replay_seconds() + 2);
    let quality = match ui.get_quality() {
        QualityPreset::Performance => "Performance",
        QualityPreset::Balanced => "Balanced",
        QualityPreset::Quality => "Quality",
    };
    ui.set_replay_memory(format!("≈ {} MB at 4K{} {quality}", mb.round(), ui.get_fps()).into());
}

/// Constant-quality target of each preset, on the encoder's QP scale (AV1:
/// 0-255). Measured on VCN 5 at 1440p and 4K 60 fps against the old VBR
/// presets: Balanced HEVC needs 22-32% fewer bits on fast motion for the same
/// VMAF, and 50-80% fewer on desktop content. The AV1 values match HEVC's
/// quality on fast motion.
fn quality(codec: VideoCodec, quality: QualityPreset) -> u32 {
    match (codec == VideoCodec::Av1, quality) {
        (false, QualityPreset::Performance) => 32,
        (false, QualityPreset::Balanced) => 28,
        (false, QualityPreset::Quality) => 25,
        (true, QualityPreset::Performance) => 130,
        (true, QualityPreset::Balanced) => 105,
        (true, QualityPreset::Quality) => 90,
    }
}

/// Bitrate cap of each preset in bits per pixel per frame for HEVC and AV1,
/// H.264 gets 1.5x. Only very noisy content reaches it. At 3840x2160 60 fps:
/// 30 / 50 / 80 Mbit/s.
fn max_bits_per_pixel(codec: VideoCodec, quality: QualityPreset) -> f64 {
    let bpp = match quality {
        QualityPreset::Performance => 0.06,
        QualityPreset::Balanced => 0.1,
        QualityPreset::Quality => 0.16,
    };
    if codec == VideoCodec::H264 {
        bpp * 1.5
    } else {
        bpp
    }
}

/// Settings of the debug `--record` path: HEVC, balanced, 60 fps, with the
/// saved audio tracks.
#[cfg(debug_assertions)]
fn default_settings() -> Settings {
    let saved = Prefs::load(Prefs {
        tracks: prefs::default_tracks(),
        audio_mix: true,
        ..Prefs::default()
    });
    Settings {
        codec: vesper_core::Codec::Hevc,
        fps: 60,
        quality: quality(VideoCodec::Hevc, QualityPreset::Balanced),
        max_bits_per_pixel: max_bits_per_pixel(VideoCodec::Hevc, QualityPreset::Balanced),
        // On the devices chosen in the settings, as in the UI.
        tracks: audio::resolve(
            saved.tracks,
            &audio::chosen_source(&prefs::Device::choice(saved.desktop_device.as_ref()), false),
            &audio::chosen_source(&prefs::Device::choice(saved.mic_device.as_ref()), true),
        ),
        mix: saved.audio_mix,
        game: None,
        scene: saved.scenes.active_scene(),
        canvas: saved.scenes.canvas,
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

/// A short sentence for the common failures (the raw error is logged);
/// other text as is.
fn friendly(error: &str) -> String {
    let has = |s: &str| error.contains(s);
    let text = if has("No space left on device") || has("File too large") {
        "The disk is full"
    } else if has("Read-only file system") {
        "The output folder is read-only"
    } else if has("Could not create") && has("Permission denied") {
        "Vesper cannot write to the output folder"
    } else if has("Could not create") && (has("File exists") || has("Not a directory")) {
        "The output folder is not a folder"
    } else if has("Could not create") && has("No such file or directory") {
        "The output folder is missing"
    } else if has("Could not open the encoder") {
        "The video encoder (VAAPI) is unavailable"
    } else if has("Portal request was cancelled") {
        "No source was picked"
    } else {
        return error.to_owned();
    };
    log!("{error}");
    // A recording cut short: name the partial file.
    let saved = error
        .split_once("(saved up to there: ")
        .and_then(|(_, rest)| rest.strip_suffix(')'));
    match saved.and_then(|path| Path::new(path).file_name()) {
        Some(name) => format!("{text}: {} saved up to there", name.to_string_lossy()),
        None => text.to_owned(),
    }
}

/// Shows a short confirmation in the status line, and as a notification
/// while the window is hidden in the tray.
fn flash_notice(ui: &AppWindow, text: &str) {
    let text = &friendly(text);
    if !ui.window().is_visible() {
        desktop::notify("Vesper".into(), text.into(), None);
    }
    show_notice(ui, text, None);
}

/// Shows a notice, with Open and Play buttons for `file`, then hides it.
fn show_notice(ui: &AppWindow, text: &str, file: Option<PathBuf>) {
    let generation = NOTICE_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    ui.set_notice(text.into());
    ui.set_notice_has_file(file.is_some());
    let time = if file.is_some() {
        FILE_NOTICE_TIME
    } else {
        NOTICE_TIME
    };
    if file.is_some() {
        *NOTICE_FILE.lock().unwrap_or_else(|e| e.into_inner()) = file;
    }
    let weak = ui.as_weak();
    slint::Timer::single_shot(time, move || {
        if let Some(ui) = weak.upgrade()
            && NOTICE_GENERATION.load(Ordering::Relaxed) == generation
        {
            ui.set_notice("".into());
            ui.set_notice_has_file(false);
        }
    });
}

fn notice_file() -> Option<PathBuf> {
    NOTICE_FILE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Whether the user is looking at the window. Unknown counts as not.
fn window_focused(ui: &AppWindow) -> bool {
    ui.window()
        .with_winit_window(|w| w.has_focus())
        .unwrap_or(false)
}

/// A saved recording or replay: a notice with Open and Play, and a desktop
/// notification when the window is in the background (e.g. behind a game).
fn announce_saved(ui: &AppWindow, title: &str, file: PathBuf) {
    if !window_focused(ui) {
        let name = file.file_name().map_or_else(
            || file.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        desktop::notify(title.to_owned(), name, Some(file.clone()));
    }
    show_notice(ui, title, Some(file));
}

/// A failed save: the error as the notice, and a notification in the background.
fn announce_failure(ui: &AppWindow, title: &str, error: &str) {
    let error = &friendly(error);
    if !window_focused(ui) {
        desktop::notify(title.to_owned(), error.to_owned(), None);
    }
    // Not `flash_notice`: it would send a second, untitled notification.
    show_notice(ui, error, None);
}

/// Shows the output folder's name, and its parent with the home folder as `~`.
fn show_output_folder(ui: &AppWindow) {
    let folder = PathBuf::from(ui.get_output_folder().as_str());
    let name = folder
        .file_name()
        .map_or_else(|| folder.to_string_lossy(), |n| n.to_string_lossy());
    let parent = folder.parent().map_or_else(String::new, |parent| {
        let parent = parent.to_string_lossy();
        dirs::home_dir()
            .and_then(|home| home_relative(&parent, &home.to_string_lossy()))
            .unwrap_or_else(|| parent.into_owned())
    });
    ui.set_output_folder_name(name.as_ref().into());
    ui.set_output_folder_parent(parent.into());
}

/// `folder` as `~/...` when it is inside `home`.
fn home_relative(folder: &str, home: &str) -> Option<String> {
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return None;
    }
    let rest = folder.strip_prefix(home)?;
    match rest {
        "" => Some("~".into()),
        _ if rest.starts_with('/') => Some(format!("~{rest}")),
        _ => None,
    }
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
    fn version_matches_the_arch_pkgver() {
        assert_eq!(
            version_string("0.1.0", Some("r45.g4077535")),
            "Vesper 0.1.0.r45.g4077535"
        );
        assert_eq!(version_string("0.1.0", None), "Vesper 0.1.0");
        assert!(version().starts_with(concat!("Vesper ", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn common_failures_read_as_short_sentences() {
        for (raw, text) in [
            (
                "write packet: No space left on device (saved up to there: /v/a.mkv)",
                "The disk is full: a.mkv saved up to there",
            ),
            (
                "Could not create /v/a.mkv: open output file: Read-only file system",
                "The output folder is read-only",
            ),
            (
                "Could not create /v/Vesper: Permission denied (os error 13)",
                "Vesper cannot write to the output folder",
            ),
            (
                "Could not create /v/Vesper: File exists (os error 17)",
                "The output folder is not a folder",
            ),
            (
                "Could not open the encoder: no VAAPI device",
                "The video encoder (VAAPI) is unavailable",
            ),
            (
                "Could not start the capture: Portal request was cancelled",
                "No source was picked",
            ),
            ("No game to record yet", "No game to record yet"),
        ] {
            assert_eq!(friendly(raw), text);
        }
    }

    #[test]
    fn a_closed_game_ends_the_recording_and_the_stream() {
        use LiveState as L;
        use RecState as R;
        // Both: only the recording's message, which keeps its file buttons.
        assert_eq!(
            ends_on_game_closed(R::Recording, L::Live),
            (true, true, false)
        );
        assert_eq!(
            ends_on_game_closed(R::Recording, L::Connecting),
            (true, true, false)
        );
        assert_eq!(ends_on_game_closed(R::Idle, L::Live), (false, true, true));
        assert_eq!(
            ends_on_game_closed(R::Idle, L::Connecting),
            (false, true, true)
        );
        assert_eq!(
            ends_on_game_closed(R::Stopping, L::Reconnecting),
            (false, true, true)
        );
        assert_eq!(
            ends_on_game_closed(R::Recording, L::Off),
            (true, false, false)
        );
        // Already ending: not asked twice.
        assert_eq!(
            ends_on_game_closed(R::Stopping, L::Ending),
            (false, false, false)
        );
        assert_eq!(
            ends_on_game_closed(R::Starting, L::Off),
            (false, false, false)
        );
    }

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
    fn balanced_4k60_is_capped_at_50_mbit() {
        let px = 3840.0 * 2160.0 * 60.0;
        let bps = px * max_bits_per_pixel(VideoCodec::Hevc, QualityPreset::Balanced);
        assert!((49e6..51e6).contains(&bps), "{bps}");
        assert!(
            max_bits_per_pixel(VideoCodec::H264, QualityPreset::Balanced)
                > max_bits_per_pixel(VideoCodec::Av1, QualityPreset::Balanced)
        );
    }

    #[test]
    fn home_is_shown_as_tilde() {
        let home = "/home/u";
        assert_eq!(
            home_relative("/home/u/Vidéos/Vesper", home).unwrap(),
            "~/Vidéos/Vesper"
        );
        assert_eq!(home_relative("/home/u", "/home/u/").unwrap(), "~");
        assert_eq!(home_relative("/home/user2/x", home), None);
        assert_eq!(home_relative("/mnt/clips", home), None);
    }

    #[test]
    fn better_presets_have_lower_quality_values() {
        for codec in [VideoCodec::Hevc, VideoCodec::Av1] {
            assert!(
                quality(codec, QualityPreset::Performance)
                    > quality(codec, QualityPreset::Balanced)
            );
            assert!(
                quality(codec, QualityPreset::Balanced) > quality(codec, QualityPreset::Quality)
            );
        }
    }

    #[test]
    fn output_folder_is_absolute() {
        if let Some(folder) = default_output_folder() {
            assert!(folder.is_absolute(), "{folder:?}");
            assert!(folder.ends_with("Vesper"));
        }
    }
}
