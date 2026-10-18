//! The Live card and the stream settings: going live and ending, the
//! stream's state and stats, and the stream key (see `secret`). The key is
//! read and written on a thread of its own, in the order asked: D-Bus and
//! an unlock prompt must never block the UI.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use slint::ComponentHandle;

use crate::prefs::Prefs;
use crate::secret::{self, Store};
use crate::stream::{Service, StreamKey, StreamRequest, StreamSettings, StreamState};
use crate::{AppWindow, Games, LiveService, LiveState, Shared, flash_error, flash_notice};

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
            Some(Store::Keyring) => tr!("Saved in the system keyring"),
            Some(Store::File) => tr!("Saved in a private file (no keyring found)"),
            None => "",
        }
        .into(),
    );
}

/// Shows whether the selected service has a key saved (each has its own);
/// nothing is unlocked for that. A late answer for another service is
/// dropped.
pub fn show_key_state(ui: &AppWindow) {
    // "Saved" was about the key and service shown before.
    show_save(ui, "", false);
    let Some(id) = secret::key_id(&settings(ui)) else {
        show_store(ui, None);
        return;
    };
    let asked = id.clone();
    off_ui(
        ui,
        move || secret::saved(&asked),
        move |ui, store| {
            if secret::key_id(&settings(ui)).as_deref() == Some(id.as_str()) {
                show_store(ui, store);
            }
        },
    );
}

fn no_server() -> &'static str {
    tr!("Enter the server URL first: the key is saved for that server")
}

/// Saves `key` for the selected service, then says `done`.
fn store_key(ui: &AppWindow, key: StreamKey, done: &'static str) {
    let Some(id) = secret::key_id(&settings(ui)) else {
        flash_error(ui, no_server());
        return;
    };
    save_key(ui, id, key, Some(done));
}

/// Saves `key` for `id`, shown next to the key field; `done`: said too.
fn save_key(ui: &AppWindow, id: String, key: StreamKey, done: Option<&'static str>) {
    show_save(ui, tr!("Saving…"), false);
    let draft = (id.clone(), key.clone());
    off_ui(
        ui,
        move || secret::store(&id, &key),
        move |ui, result| {
            // The note is about the service shown, not one switched away from.
            let shown = secret::key_id(&settings(ui)).as_ref() == Some(&draft.0);
            match result {
                Ok(_) => {
                    show_key_state(ui);
                    if shown {
                        show_save(ui, tr!("Saved"), false);
                    }
                    if let Some(done) = done {
                        flash_notice(ui, done);
                    }
                }
                Err(e) => {
                    // Confirming the same key again tries again.
                    DRAFT.with(|d| d.borrow_mut().failed(&draft));
                    if shown {
                        show_save(ui, &e, true);
                    }
                    flash_error(ui, &e);
                }
            }
        },
    );
}

fn show_save(ui: &AppWindow, state: &str, failed: bool) {
    ui.set_live_key_save_state(state.into());
    ui.set_live_key_save_failed(failed);
}

/// The key typed in the field and not saved yet, with the service it was
/// typed for: switching service before it is saved never moves it.
#[derive(Default)]
struct Draft {
    pending: Option<(String, StreamKey)>,
    /// The last one handed to a save: confirming it again saves nothing.
    sent: Option<(String, StreamKey)>,
}

impl Draft {
    /// The field now reads `text`, for `id`.
    fn edited(&mut self, id: Option<String>, text: &str) {
        self.pending = id
            .zip(StreamKey::new(text))
            .filter(|draft| self.sent.as_ref() != Some(draft));
    }

    /// The save of `draft` failed: it is not saved.
    fn failed(&mut self, draft: &(String, StreamKey)) {
        if self.sent.as_ref() == Some(draft) {
            self.sent = None;
        }
    }

    /// The draft to save now, if any.
    fn take(&mut self) -> Option<(String, StreamKey)> {
        let draft = self.pending.take()?;
        self.sent = Some(draft.clone());
        Some(draft)
    }
}

/// The key field now reads `text`; true when it holds a key to save.
fn draft_edited(ui: &AppWindow, text: &str) -> bool {
    let id = secret::key_id(&settings(ui));
    if id.is_none() && StreamKey::new(text).is_some() {
        show_save(ui, no_server(), true);
    }
    DRAFT.with(|d| {
        let mut d = d.borrow_mut();
        d.edited(id, text);
        d.pending.is_some()
    })
}

/// How long typing must pause before the key is saved.
const KEY_DEBOUNCE: Duration = Duration::from_millis(800);

thread_local! {
    static DRAFT: RefCell<Draft> = RefCell::new(Draft::default());
    static DRAFT_TIMER: slint::Timer = slint::Timer::default();
}

/// Saves the key typed in the field, if not saved yet (Enter, focus out,
/// a pause in typing, Go live, quit).
fn save_draft(ui: &AppWindow) {
    DRAFT_TIMER.with(slint::Timer::stop);
    if let Some((id, key)) = DRAFT.with(|d| d.borrow_mut().take()) {
        save_key(ui, id, key, None);
    }
}

/// Saves the key still being typed and waits (at most `limit`) for the
/// keyring work given before: called once the event loop ended, so no
/// answer reaches the UI. Never logs the key.
pub fn flush_on_quit(limit: Duration) {
    if let Some((id, key)) = DRAFT.with(|d| d.borrow_mut().take()) {
        log!("saving the stream key typed before quitting");
        in_order(Box::new(move || {
            if let Err(e) = secret::store(&id, &key) {
                log!("stream key not saved: {e}");
            }
        }));
    }
    if !drain(limit) {
        log!("the keyring did not finish in time");
    }
}

/// Waits until the keyring work given so far is done; false after `limit`.
fn drain(limit: Duration) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    in_order(Box::new(move || {
        let _ = tx.send(());
    }));
    rx.recv_timeout(limit).is_ok()
}

/// Runs `work` on the keyring thread, then `done` with its result on the
/// UI thread.
fn off_ui<T: Send + 'static>(
    ui: &AppWindow,
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(&AppWindow, T) + Send + 'static,
) {
    let weak = ui.as_weak();
    in_order(Box::new(move || {
        let result = work();
        let _ = weak.upgrade_in_event_loop(move |ui| done(&ui, result));
    }));
}

type Job = Box<dyn FnOnce() + Send>;

/// Runs `job` on one thread, after the jobs given before it: a key entered
/// right before Go live is saved before it is read, never after.
fn in_order(job: Job) {
    static QUEUE: std::sync::OnceLock<Option<std::sync::mpsc::Sender<Job>>> =
        std::sync::OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("vesper-secret".into())
            .spawn(move || rx.into_iter().for_each(|job| job()))
            .inspect_err(|e| log!("could not start a thread: {e}"))
            .ok()
            .map(|_| tx)
    });
    if let Some(queue) = queue {
        let _ = queue.send(job);
    }
}

thread_local! {
    /// The pipeline and game capture, for work finished off the UI thread.
    static CONTEXT: RefCell<Option<(Shared, Games)>> = const { RefCell::new(None) };
}

/// The service the single key of a version before per-service keys was
/// saved for: the one selected in that version's settings (`legacy`, the
/// copy kept by the profile migration). `None` when unknown, or when the
/// settings could not be read (nothing is written then): the old key stays
/// as it is, never given to another service.
fn old_key_service(saving_off: bool, legacy: Option<Prefs>) -> Option<String> {
    if saving_off {
        return None;
    }
    secret::key_id(&legacy?.live)
}

/// What to say when `request` has no key and its service needs one; `other`
/// is a service that has a key, never used for this one.
fn no_key(request: &StreamRequest, other: Option<&str>) -> Option<String> {
    let name = match request.settings.service {
        _ if request.key.is_some() => return None,
        Service::Twitch => "Twitch",
        Service::YouTube => "YouTube",
        Service::Custom => return None,
    };
    Some(match other {
        Some(o) => tr!(
            "No {0} stream key saved (one is saved for {1}): paste {0}'s key in the settings",
            name,
            o
        ),
        None => tr!(
            "No {0} stream key saved: paste {0}'s key in the settings",
            name
        ),
    })
}

/// Says where the key of an older version went: it was saved for the
/// service selected back then, which may not be the one it belongs to.
fn say_moved(ui: &AppWindow, id: &str) {
    flash_error(
        ui,
        &tr!(
            "Your previous stream key is now saved for {}: paste it again if it is another service's",
            id
        ),
    );
}

pub fn wire(ui: &AppWindow, recording: &Shared, games: &Games) {
    CONTEXT.with(|c| *c.borrow_mut() = Some((recording.clone(), games.clone())));
    // An older version's single key goes to the service it was saved for,
    // then whether the selected service has a key is shown.
    let saving_off = crate::prefs::SAVING_OFF.load(std::sync::atomic::Ordering::Relaxed);
    match old_key_service(saving_off, crate::prefs::legacy_settings()) {
        Some(id) => off_ui(
            ui,
            move || secret::migrate(&id).then_some(id),
            |ui, moved| {
                if let Some(id) = moved {
                    say_moved(ui, &id);
                }
                show_key_state(ui);
            },
        ),
        None => show_key_state(ui),
    }

    let weak = ui.as_weak();
    let rec = recording.clone();
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
        // Without a source, going live would open a second picker.
        if ui.get_source_picking() && ui.get_source_kind() == crate::SourceKind::None {
            flash_notice(&ui, tr!("Choose a source first"));
            return;
        }
        // A key still being typed is saved first: the read below follows it.
        save_draft(&ui);
        let settings = settings(&ui);
        // Checked before the key is read: a URL error needs no unlock.
        let check = StreamRequest {
            settings: settings.clone(),
            key: StreamKey::new("check"),
        };
        if let Err(e) = check.url() {
            flash_error(&ui, &e);
            return;
        }
        log!("go-live: {settings:?}");
        ui.set_live_state(LiveState::Connecting);
        ui.set_live_detail("".into());
        // Only the key saved for this service (or server): never another's.
        let id = secret::key_id(&settings);
        let load = move || {
            let key = id.as_deref().and_then(secret::load);
            // None: which other service has one, to say so (never used).
            let other = key
                .is_none()
                .then(|| {
                    ["twitch", "youtube"]
                        .into_iter()
                        .filter(|o| id.as_deref() != Some(o))
                        .find(|o| secret::saved(o).is_some())
                })
                .flatten();
            (key, other)
        };
        off_ui(&ui, load, move |ui, (key, other)| {
            // Ended meanwhile (e.g. the pipeline stopped).
            let Some((rec, g)) = CONTEXT.with(|c| c.borrow().clone()) else {
                return;
            };
            match ui.get_live_state() {
                LiveState::Connecting => {}
                // Ended while the key was read: nothing was started.
                LiveState::Ending => {
                    reset_after_game(ui);
                    return;
                }
                _ => return,
            }
            let request = StreamRequest { settings, key };
            if let Err(e) = request.url() {
                ui.set_live_state(LiveState::Off);
                let e = no_key(&request, other).unwrap_or(e);
                flash_error(ui, &e);
                return;
            }
            let joined = rec
                .borrow()
                .as_ref()
                .is_some_and(|p| p.start_stream(request.clone()));
            if !joined && let Err(e) = crate::start_pipeline(ui, &rec, &g, None, Some(request)) {
                ui.set_live_state(LiveState::Off);
                flash_error(ui, &e);
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
            // Saved for the server of the URL without the key.
            store_key(
                &ui,
                key,
                tr!("The key in the URL was moved to the stream key"),
            );
        }
        ui.set_live_url_warning(crate::stream::cleartext_warning(&url).into());
        log!("live-settings-changed: {:?}", settings(&ui));
        Prefs::from_ui(&ui).save();
        show_key_state(&ui);
    });

    let weak = ui.as_weak();
    ui.on_live_key_entered(move |key| {
        let Some(ui) = weak.upgrade() else { return };
        if !draft_edited(&ui, &key) {
            return;
        }
        log!("live-key-entered");
        save_draft(&ui);
    });

    // Saved once typing pauses: a pasted key never waits for Enter.
    let weak = ui.as_weak();
    ui.on_live_key_edited(move |key| {
        let Some(ui) = weak.upgrade() else { return };
        if !draft_edited(&ui, &key) {
            DRAFT_TIMER.with(slint::Timer::stop);
            return;
        }
        let weak = ui.as_weak();
        DRAFT_TIMER.with(|t| {
            t.start(slint::TimerMode::SingleShot, KEY_DEBOUNCE, move || {
                if let Some(ui) = weak.upgrade() {
                    save_draft(&ui);
                }
            });
        });
    });

    let weak = ui.as_weak();
    ui.on_live_key_removed(move || {
        let Some(ui) = weak.upgrade() else { return };
        let Some(id) = secret::key_id(&settings(&ui)) else {
            return;
        };
        log!("live-key-removed");
        // The same key pasted again is saved again.
        DRAFT.with(|d| d.borrow_mut().sent = None);
        off_ui(
            &ui,
            move || secret::clear(&id),
            |ui, ()| {
                show_key_state(ui);
                flash_notice(ui, tr!("Stream key removed"));
            },
        );
    });
}

/// Why the stream is ending: [`NOT_GAME`], or the captured game closed and
/// [`GAME_SAID`] (said once it ended) or [`GAME_QUIET`] (the recording's
/// message says it).
static GAME_CLOSED: AtomicU8 = AtomicU8::new(NOT_GAME);
const NOT_GAME: u8 = 0;
const GAME_SAID: u8 = 1;
const GAME_QUIET: u8 = 2;

/// Ends the stream because the captured game quit (a frozen picture would go
/// out forever), the way End does: the stream is flushed and closed.
/// `announce`: says so once it ended; otherwise nothing is said.
pub fn end_for_closed_game(ui: &AppWindow, announce: bool) {
    log!("captured game closed: ending the stream");
    let why = if announce { GAME_SAID } else { GAME_QUIET };
    GAME_CLOSED.store(why, Ordering::Relaxed);
    ui.invoke_toggle_live();
    // No pipeline any more: ended at once (and the reason reset).
    if ui.get_live_state() == LiveState::Off && announce {
        announce_game_closed(ui);
    }
}

/// Resets the Live card, then tells why if the game closed. False when
/// nothing more may be said: that, or the recording's message, told.
fn reset_after_game(ui: &AppWindow) -> bool {
    let why = GAME_CLOSED.load(Ordering::Relaxed);
    reset(ui);
    match why {
        GAME_SAID => {
            announce_game_closed(ui);
            false
        }
        GAME_QUIET => false,
        _ => true,
    }
}

fn announce_game_closed(ui: &AppWindow) {
    let text = tr!("Game closed: stream ended");
    if !crate::window_focused(ui) {
        crate::desktop::notify("Vesper".into(), text.into(), None);
    }
    crate::show_notice(ui, text, None);
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
                    ui.set_live_detail(tr!("Starting…").into());
                }
                LiveState::Reconnecting => ui.set_live_detail(tr!("Reconnected").into()),
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
            ui.set_live_detail(tr!("{} · next try in {} s", error, retry_in.as_secs()).into());
        }
        StreamState::Ended(error) => {
            let was_on_air = ui.get_live_seconds() > 0;
            if !reset_after_game(ui) {
                if let Some(e) = error {
                    log!("stream ended: {e}");
                }
                return;
            }
            match error {
                Some(e) => crate::announce_failure(ui, tr!("Stream ended"), &e),
                None if was_on_air => flash_notice(ui, tr!("Stream ended")),
                None => {}
            }
        }
    }
}

/// Shows the stats of the last second.
pub fn on_stats(ui: &AppWindow, elapsed: Duration, bitrate: u64, dropped: u64) {
    ui.set_live_seconds(elapsed.as_secs().min(i32::MAX as u64) as i32);
    ui.set_live_detail(tr!("{} Mbit/s", crate::i18n::decimal(bitrate as f64 / 1e6, 1)).into());
    ui.set_live_dropped(dropped.min(i32::MAX as u64) as i32);
}

/// No stream any more (it ended, or its pipeline did).
pub fn reset(ui: &AppWindow) {
    GAME_CLOSED.store(NOT_GAME, Ordering::Relaxed);
    ui.set_live_state(LiveState::Off);
    ui.set_live_seconds(0);
    ui.set_live_detail("".into());
    ui.set_live_dropped(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn going_live_without_a_key_says_which_service_lacks_one() {
        let request = |service, key: Option<&str>| StreamRequest {
            settings: StreamSettings {
                service,
                custom_url: "rtmp://h.example/app".into(),
                ..StreamSettings::default()
            },
            key: key.and_then(StreamKey::new),
        };
        assert_eq!(
            no_key(&request(Service::YouTube, None), Some("twitch")).as_deref(),
            Some(
                "No YouTube stream key saved (one is saved for twitch): paste YouTube's key in the settings"
            )
        );
        assert_eq!(
            no_key(&request(Service::Twitch, None), None).as_deref(),
            Some("No Twitch stream key saved: paste Twitch's key in the settings")
        );
        assert_eq!(no_key(&request(Service::YouTube, Some("k")), None), None);
        // A custom server may take no key.
        assert_eq!(no_key(&request(Service::Custom, None), None), None);
    }

    fn draft(id: &str, k: &str) -> Option<(String, StreamKey)> {
        Some((id.to_owned(), StreamKey::new(k).unwrap()))
    }

    /// Paste, no Enter, Go live: Go live takes the draft to save it before
    /// the key is read; the focus leaving later saves nothing again.
    #[test]
    fn a_pasted_key_is_saved_without_enter() {
        let mut d = Draft::default();
        d.edited(Some("youtube".into()), "  yt_key ");
        assert_eq!(d.take(), draft("youtube", "yt_key"));
        assert_eq!(d.take(), None);
        d.edited(Some("youtube".into()), "yt_key");
        assert_eq!(d.take(), None);
        // A new key is saved again.
        d.edited(Some("youtube".into()), "yt_key2");
        assert_eq!(d.take(), draft("youtube", "yt_key2"));
        // Cleared, or for a custom URL without a host: nothing to save.
        d.edited(Some("youtube".into()), "  ");
        assert_eq!(d.take(), None);
        d.edited(None, "k");
        assert_eq!(d.take(), None);
    }

    /// A save that failed (locked, no answer) or a key removed: the same
    /// key confirmed again is saved again, never taken as saved.
    #[test]
    fn the_same_key_is_saved_again_after_a_failure_or_a_removal() {
        let mut d = Draft::default();
        d.edited(Some("youtube".into()), "yt_key");
        let sent = d.take().unwrap();
        d.failed(&sent);
        d.edited(Some("youtube".into()), "yt_key");
        assert_eq!(d.take(), draft("youtube", "yt_key"));
        // A failure of an older save does not forget a newer one.
        d.edited(Some("youtube".into()), "yt_key2");
        let newer = d.take().unwrap();
        d.failed(&sent);
        assert_eq!(d.sent, Some(newer));
        // Removed: as on_live_key_removed does.
        d.sent = None;
        d.edited(Some("youtube".into()), "yt_key2");
        assert_eq!(d.take(), draft("youtube", "yt_key2"));
    }

    /// Paste, then quit before the pause ends: the draft is still there for
    /// the quit flush, with the service it was typed for.
    #[test]
    fn a_key_pasted_right_before_quitting_is_kept_for_its_service() {
        let mut d = Draft::default();
        d.edited(Some("youtube".into()), "yt_key");
        assert_eq!(d.pending, draft("youtube", "yt_key"));
        assert_eq!(d.take(), draft("youtube", "yt_key"));
    }

    #[test]
    fn drain_waits_for_the_work_given_before() {
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let d = done.clone();
        in_order(Box::new(move || {
            std::thread::sleep(Duration::from_millis(100));
            d.store(true, Ordering::SeqCst);
        }));
        assert!(drain(Duration::from_secs(5)));
        assert!(done.load(Ordering::SeqCst));
    }

    /// Paste then Go live, and paste then quit, against the fake Secret
    /// Service on a private bus (see `secret`'s keyring tests).
    #[test]
    #[ignore = "needs a throwaway Secret Service on a private session bus"]
    fn keyring_pasted_key_reaches_go_live_and_survives_quit() {
        let id = "test-draft";
        // Go live: the draft's save is given before the read.
        DRAFT.with(|d| d.borrow_mut().edited(Some(id.into()), "first_key"));
        let (draft_id, key) = DRAFT.with(|d| d.borrow_mut().take()).unwrap();
        in_order(Box::new(move || {
            secret::store(&draft_id, &key).unwrap();
        }));
        let (tx, rx) = std::sync::mpsc::channel();
        in_order(Box::new(move || tx.send(secret::load(id)).unwrap()));
        assert_eq!(rx.recv().unwrap(), StreamKey::new("first_key"));
        // Quit before the pause: flushed and waited for.
        DRAFT.with(|d| d.borrow_mut().edited(Some(id.into()), "second_key"));
        flush_on_quit(Duration::from_secs(10));
        assert_eq!(secret::load(id), StreamKey::new("second_key"));
        secret::clear(id);
    }

    /// A slow save given before a read finishes first: the read gets the
    /// key just entered, not the one before it.
    #[test]
    fn keyring_work_runs_in_the_order_it_was_given() {
        let (tx, rx) = std::sync::mpsc::channel();
        let saved = std::sync::Arc::new(std::sync::Mutex::new("old"));
        let (s, t) = (saved.clone(), tx.clone());
        in_order(Box::new(move || {
            std::thread::sleep(Duration::from_millis(100));
            *s.lock().unwrap() = "new";
            t.send("saved").unwrap();
        }));
        in_order(Box::new(move || {
            tx.send(*saved.lock().unwrap()).unwrap();
        }));
        let got: Vec<_> = rx.iter().take(2).collect();
        assert_eq!(got, ["saved", "new"]);
    }

    #[test]
    fn the_old_key_goes_only_to_the_service_it_was_saved_for() {
        let old = |text: &str| Some(Prefs::default().parse(text));
        assert_eq!(
            old_key_service(false, old("live_service=youtube\n")).as_deref(),
            Some("youtube")
        );
        let custom = old("live_service=custom\nlive_url=rtmp://h.example/app\n");
        assert_eq!(
            old_key_service(false, custom).as_deref(),
            Some("custom:h.example")
        );
        // Unknown: no copy of the old settings, a custom service without a
        // server, or settings that could not be read.
        assert_eq!(old_key_service(false, None), None);
        assert_eq!(old_key_service(false, old("live_service=custom\n")), None);
        assert_eq!(old_key_service(true, old("live_service=youtube\n")), None);
    }
}
