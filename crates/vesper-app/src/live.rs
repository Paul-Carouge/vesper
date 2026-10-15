//! The Live card and the stream settings: going live and ending, the
//! stream's state and stats, and the stream key (see `secret`). The key is
//! read and written on short-lived threads: D-Bus and an unlock prompt must
//! never block the UI.

use std::cell::RefCell;
use std::time::Duration;

use slint::ComponentHandle;

use crate::prefs::Prefs;
use crate::secret::{self, Store};
use crate::stream::{Service, StreamKey, StreamRequest, StreamSettings, StreamState};
use crate::{AppWindow, Games, LiveService, LiveState, Shared, flash_notice};

/// The stream settings of the UI.
pub fn settings(ui: &AppWindow) -> StreamSettings {
    let height = ui.get_live_height();
    StreamSettings {
        service: match ui.get_live_service() {
            LiveService::Twitch => Service::Twitch,
            LiveService::Youtube => Service::YouTube,
            LiveService::Custom => Service::Custom,
        },
        custom_url: ui.get_live_url().trim().to_owned(),
        height: u32::try_from(height).ok().filter(|&h| h > 0),
        fps: ui.get_live_fps().clamp(1, 240) as u32,
        bitrate: (f64::from(ui.get_live_bitrate()).clamp(0.5, 50.0) * 1e6) as u64,
        codec: ui.get_live_codec().into(),
    }
}

fn show_store(ui: &AppWindow, store: Option<Store>) {
    ui.set_live_key_state(
        match store {
            Some(Store::Keyring) => "Saved in the system keyring",
            Some(Store::File) => "Saved in a private file (no keyring found)",
            None => "",
        }
        .into(),
    );
}

/// Runs `work` on a new thread, then `done` with its result on the UI thread.
fn off_ui<T: Send + 'static>(
    ui: &AppWindow,
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(&AppWindow, T) + Send + 'static,
) {
    let weak = ui.as_weak();
    let spawned = std::thread::Builder::new()
        .name("vesper-secret".into())
        .spawn(move || {
            let result = work();
            let _ = weak.upgrade_in_event_loop(move |ui| done(&ui, result));
        });
    if let Err(e) = spawned {
        log!("could not start a thread: {e}");
    }
}

thread_local! {
    /// The pipeline and game capture, for work finished off the UI thread.
    static CONTEXT: RefCell<Option<(Shared, Games)>> = const { RefCell::new(None) };
}

pub fn wire(ui: &AppWindow, recording: &Shared, games: &Games) {
    CONTEXT.with(|c| *c.borrow_mut() = Some((recording.clone(), games.clone())));
    // Whether a key is saved; nothing is unlocked for that.
    off_ui(ui, secret::saved, show_store);

    let weak = ui.as_weak();
    let rec = recording.clone();
    let g = games.clone();
    ui.on_toggle_live(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_live_state() != LiveState::Off {
            log!("end-stream");
            let ending = rec.borrow().as_ref().is_some_and(|p| p.stop_stream());
            if ending {
                ui.set_live_state(LiveState::Ending);
            } else {
                reset(&ui);
            }
            return;
        }
        let settings = settings(&ui);
        // Checked before the key is read: a URL error needs no unlock.
        let check = StreamRequest {
            settings: settings.clone(),
            key: StreamKey::new("check"),
        };
        if let Err(e) = check.url() {
            flash_notice(&ui, &e);
            return;
        }
        if ui.get_source_kind() == crate::SourceKind::Game
            && !g
                .borrow()
                .as_ref()
                .is_some_and(crate::game::GameSource::has_game)
        {
            flash_notice(&ui, "No game to stream yet");
            return;
        }
        log!("go-live: {settings:?}");
        ui.set_live_state(LiveState::Connecting);
        ui.set_live_detail("".into());
        off_ui(&ui, secret::load, move |ui, key| {
            // Ended meanwhile (e.g. the pipeline stopped).
            let Some((rec, g)) = CONTEXT.with(|c| c.borrow().clone()) else {
                return;
            };
            match ui.get_live_state() {
                LiveState::Connecting => {}
                // Ended while the key was read: nothing was started.
                LiveState::Ending => return reset(ui),
                _ => return,
            }
            let request = StreamRequest { settings, key };
            if let Err(e) = request.url() {
                ui.set_live_state(LiveState::Off);
                flash_notice(ui, &e);
                return;
            }
            let joined = rec
                .borrow()
                .as_ref()
                .is_some_and(|p| p.start_stream(request.clone()));
            if !joined && let Err(e) = crate::start_pipeline(ui, &rec, &g, None, Some(request)) {
                ui.set_live_state(LiveState::Off);
                flash_notice(ui, &e);
            }
        });
    });

    let weak = ui.as_weak();
    ui.on_live_settings_changed(move || {
        let Some(ui) = weak.upgrade() else { return };
        // A key pasted into the URL goes to the keyring, not the settings.
        let (url, key) = crate::stream::split_secret(ui.get_live_url().trim());
        if let Some(key) = key.as_deref().and_then(StreamKey::new) {
            ui.set_live_url(url.as_str().into());
            off_ui(
                &ui,
                move || secret::store(&key),
                |ui, result| match result {
                    Ok(store) => {
                        show_store(ui, Some(store));
                        flash_notice(ui, "The key in the URL was moved to the stream key");
                    }
                    Err(e) => flash_notice(ui, &e),
                },
            );
        }
        ui.set_live_url_warning(crate::stream::cleartext_warning(&url).into());
        log!("live-settings-changed: {:?}", settings(&ui));
        Prefs::from_ui(&ui).save();
    });

    let weak = ui.as_weak();
    ui.on_live_key_entered(move |key| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(key) = StreamKey::new(&key) else {
            return;
        };
        log!("live-key-entered");
        off_ui(
            &ui,
            move || secret::store(&key),
            |ui, result| match result {
                Ok(store) => {
                    show_store(ui, Some(store));
                    flash_notice(ui, "Stream key saved");
                }
                Err(e) => flash_notice(ui, &e),
            },
        );
    });

    let weak = ui.as_weak();
    ui.on_live_key_removed(move || {
        let Some(ui) = weak.upgrade() else { return };
        log!("live-key-removed");
        off_ui(&ui, secret::clear, |ui, ()| {
            show_store(ui, None);
            flash_notice(ui, "Stream key removed");
        });
    });
}

/// Shows a state change of the stream.
pub fn on_state(ui: &AppWindow, state: StreamState) {
    log!("live: {state:?}");
    match state {
        // An end requested meanwhile stays shown.
        StreamState::Connecting if ui.get_live_state() == LiveState::Ending => {}
        StreamState::Connecting => {
            ui.set_live_state(LiveState::Connecting);
            ui.set_live_detail("".into());
        }
        StreamState::Live => {
            match ui.get_live_state() {
                LiveState::Connecting => {
                    ui.set_live_seconds(0);
                    ui.set_live_dropped(0);
                    ui.set_live_detail("Starting…".into());
                }
                LiveState::Reconnecting => ui.set_live_detail("Reconnected".into()),
                // An end requested meanwhile stays shown.
                LiveState::Ending => return,
                _ => {}
            }
            ui.set_live_state(LiveState::Live);
        }
        StreamState::Reconnecting { error, retry_in } => {
            // An end requested meanwhile stays shown.
            if ui.get_live_state() != LiveState::Ending {
                ui.set_live_state(LiveState::Reconnecting);
            }
            ui.set_live_detail(format!("{error} · next try in {} s", retry_in.as_secs()).into());
        }
        StreamState::Ended(error) => {
            let was_on_air = ui.get_live_seconds() > 0;
            reset(ui);
            match error {
                Some(e) => crate::announce_failure(ui, "Stream ended", &e),
                None if was_on_air => flash_notice(ui, "Stream ended"),
                None => {}
            }
        }
    }
}

/// Shows the stats of the last second.
pub fn on_stats(ui: &AppWindow, elapsed: Duration, bitrate: u64, dropped: u64) {
    ui.set_live_seconds(elapsed.as_secs().min(i32::MAX as u64) as i32);
    ui.set_live_detail(format!("{:.1} Mbit/s", bitrate as f64 / 1e6).into());
    ui.set_live_dropped(dropped.min(i32::MAX as u64) as i32);
}

/// No stream any more (it ended, or its pipeline did).
pub fn reset(ui: &AppWindow) {
    ui.set_live_state(LiveState::Off);
    ui.set_live_seconds(0);
    ui.set_live_detail("".into());
    ui.set_live_dropped(0);
}
