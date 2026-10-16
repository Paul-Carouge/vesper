//! Game mode: the game capture server (`vesper-gamecapture`) while the Game
//! source is selected, its consent prompt and its game picker.
//!
//! Consent: Steam games are allowed by the server itself; any other game
//! waits until the user clicks Allow, which persists its process name in the
//! settings. Nothing else is ever allowed automatically.
//!
//! The server only asks a game for frames while a pipeline consumes them
//! (`GameFrames` attached); otherwise games stay a pass-through.
//!
//! Without a layer game, the game's window feeds the same route through the
//! ScreenCast portal ([`crate::fallback`] decides when).

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::{ModelRc, VecModel};
use vesper_capture::{Cancel, Capture};
use vesper_gamecapture::{Consent, Game, GameCapture};

use crate::fallback::{self, Action, Demand, Switch};
use crate::prefs::{ALLOWED_GAMES, valid_game};
use crate::recorder::GameFrames;
use crate::{AppWindow, GameEntry, LiveState, RecState};

/// How often the running Steam game is looked up while one may be needed.
const STEAM_SCAN: Duration = Duration::from_secs(2);

/// Games the user ignored this session: not asked again until Vesper
/// restarts, even if game capture restarts.
static IGNORED: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub struct GameSource {
    capture: Arc<GameCapture>,
    pub frames: GameFrames,
    /// Set when the server thread fails.
    failed: Arc<AtomicBool>,
    /// Pid picked in the UI, until that game goes away.
    selected: Option<u32>,
    /// Last list shown, so the UI model is only replaced on change.
    shown: Vec<(String, u32)>,
    /// Pid of the game last seen captured, to notice it closing.
    captured: Option<u32>,
    /// The captured game disconnected since the last `take_closed`.
    closed: bool,
    /// The captured game disconnected but its process still runs, and since
    /// when: its window takes over after the grace. Watched until its window
    /// is captured: exiting before stops the recording, never asks.
    lost: Option<(u32, Instant)>,
    /// How the shown game is captured, by pid ("Vulkan", "OpenGL", "").
    api: Option<(u32, &'static str)>,
    switch: Switch,
    window: Option<GameWindow>,
    /// The last Steam game lookup (key, name), and when.
    steam: Option<(Instant, Option<(String, String)>)>,
}

/// The game's window, captured through the portal.
struct GameWindow {
    /// Its restore token's key (see [`fallback`]).
    key: String,
    /// Shown as the game's name; empty if unknown.
    name: String,
    state: WindowState,
}

enum WindowState {
    /// The portal is asked; `discard`: not needed any more (its request is
    /// cancelled, a capture it still starts is stopped).
    Opening {
        answer: Receiver<Opened>,
        cancel: Cancel,
        discard: bool,
    },
    Open {
        capture: Capture,
        /// Its frames go to the pipeline.
        live: Arc<AtomicBool>,
        /// The window's stream ended (closed).
        ended: Arc<AtomicBool>,
    },
}

/// The started capture with its `live` and `ended` flags, or whether the
/// user cancelled and the error.
type Opened = Result<(Capture, Arc<AtomicBool>, Arc<AtomicBool>), (bool, String)>;

/// Asks the portal for `key`'s window on a thread of its own (the picker
/// may stay open for long), restoring it silently when its token still
/// works. A token the portal refuses is replaced once the picker asks
/// again; a D-Bus failure keeps it. `cancel` closes the portal request.
fn open_window(key: String, frames: GameFrames, fps: u32, cancel: Cancel) -> Receiver<Opened> {
    let (tx, rx) = std::sync::mpsc::channel();
    let started = std::thread::Builder::new()
        .name("vesper-game-window".into())
        .spawn(move || {
            let live = Arc::new(AtomicBool::new(false));
            let ended = Arc::new(AtomicBool::new(false));
            let on_frame = || {
                let (frames, live, ended) = (frames.clone(), live.clone(), ended.clone());
                move |event: vesper_capture::Result<vesper_core::VideoFrame>| match event {
                    // Not taken yet, or the layer took over: released at once.
                    Ok(frame) if live.load(Ordering::Relaxed) => frames.push(Ok(frame)),
                    Ok(_) => {}
                    // The window closed: never an error for the pipeline.
                    Err(e) => {
                        log!("game window capture ended: {e}");
                        ended.store(true, Ordering::Relaxed);
                    }
                }
            };
            // Twice the recording rate, as for a portal source.
            let max_fps = fps.saturating_mul(2);
            let token = fallback::load_token(&key);
            let start =
                |token: Option<&str>| Capture::start_window(token, &cancel, max_fps, on_frame());
            let result = match start(token.as_deref()) {
                Err(e) if token.is_some() && vesper_capture::is_refused(&e) => {
                    log!("could not restore the game window ({e}): asking again");
                    start(None)
                }
                result => result,
            };
            let answer = match result {
                Ok(capture) => {
                    if let Some(token) = capture.restore_token() {
                        fallback::store_token(&key, Some(token));
                    }
                    Ok((capture, live, ended))
                }
                Err(e) => Err((vesper_capture::is_cancelled(&e), e.to_string())),
            };
            // The game source is gone: the capture stops here.
            let _ = tx.send(answer);
        });
    if let Err(e) = started {
        log!("could not start the game window capture: {e}");
    }
    rx
}

/// Stops a window capture off the UI thread (closing the portal session
/// may take a while).
fn stop_window(capture: Capture) {
    let stopped = std::thread::Builder::new()
        .name("vesper-game-window-stop".into())
        .spawn(move || drop(capture));
    if let Err(e) = stopped {
        log!("could not stop the game window capture: {e}");
    }
}

/// Whether a layer game with a swapchain waits for the user's Allow (not
/// ignored): its prompt shows, so the window picker does not.
fn waits_for_consent(games: &[Game], ignored: &[String]) -> bool {
    games
        .iter()
        .any(|g| g.consent == Consent::Pending && g.width > 0 && !ignored.contains(&g.exe))
}

/// How the game `pid` is captured, from the capture library it loaded:
/// "Vulkan" (layer), "OpenGL" (`vesper-gl`), "" when both or unknown.
fn api_of(pid: u32) -> &'static str {
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).unwrap_or_default();
    match (
        maps.contains("libvesper_vklayer"),
        maps.contains("libvesper_glcapture"),
    ) {
        (true, false) => "Vulkan",
        (false, true) => "OpenGL",
        _ => "",
    }
}

/// The game a recording would capture, as the server's `choose` picks it:
/// the captured one, or the picked one only (if allowed, with a swapchain),
/// or the newest allowed game with a swapchain.
fn candidate(games: &[Game], selected: Option<u32>) -> Option<&Game> {
    let ready = |g: &&Game| g.consent == Consent::Allowed && g.width > 0;
    match selected {
        Some(pid) => games
            .iter()
            .find(|g| g.pid == pid && (g.captured || ready(g))),
        None => games
            .iter()
            .find(|g| g.captured)
            .or_else(|| games.iter().filter(ready).max_by_key(|g| g.seq)),
    }
}

impl GameSource {
    /// Starts the server and allows the games of the saved allow list.
    pub fn start() -> Result<Self, String> {
        let frames = GameFrames::default();
        let failed = Arc::new(AtomicBool::new(false));
        let (f, fail) = (frames.clone(), failed.clone());
        let capture = GameCapture::start(move |event| {
            if event.is_err() {
                fail.store(true, Ordering::Relaxed);
            }
            f.push(event.map_err(|e| format!("Game capture failed: {e}")));
        })
        .map_err(|e| format!("Could not start game capture: {e}"))?;
        let capture = Arc::new(capture);
        frames.bind(&capture);
        for exe in ALLOWED_GAMES.lock().unwrap().iter() {
            capture.allow(exe);
        }
        Ok(Self {
            capture,
            frames,
            failed,
            selected: None,
            shown: Vec::new(),
            captured: None,
            closed: false,
            lost: None,
            api: None,
            switch: Switch::default(),
            window: None,
            steam: None,
        })
    }

    /// Whether the captured game disconnected (quit or crashed; a paused
    /// game stays connected) since the last call.
    pub fn take_closed(&mut self) -> bool {
        std::mem::take(&mut self.closed)
    }

    /// Allows `exe` now and in later sessions. False if the name is not a
    /// plausible process name.
    pub fn allow(&self, exe: &str) -> bool {
        if !valid_game(exe) {
            return false;
        }
        let mut allowed = ALLOWED_GAMES.lock().unwrap();
        if !allowed.iter().any(|g| g == exe) {
            allowed.push(exe.to_owned());
        }
        drop(allowed);
        self.capture.allow(exe);
        true
    }

    pub fn ignore(exe: &str) {
        IGNORED.lock().unwrap().push(exe.to_owned());
    }

    pub fn select(&mut self, pid: u32) {
        self.selected = Some(pid);
        self.capture.select(self.selected);
    }

    /// Stops the window capture, if any.
    fn close_window(&mut self) {
        match self.window.take().map(|w| w.state) {
            Some(WindowState::Open { capture, live, .. }) => {
                live.store(false, Ordering::Relaxed);
                self.frames.set_window(false);
                stop_window(capture);
            }
            // Its thread stops what the portal still answers.
            Some(WindowState::Opening { cancel, .. }) => cancel.cancel(),
            None => {}
        }
        self.frames.set_picking(false);
    }

    /// Runs the window fallback: takes the portal's answer, notices a closed
    /// window, and opens or closes the window as [`Switch`] decides. `layer`:
    /// a layer game is captured, waits for consent, or was just lost and
    /// still runs; `quit`: the lost one exited before its window was taken.
    fn drive_window(&mut self, ui: &AppWindow, layer: bool, quit: bool) {
        let now = Instant::now();
        let demand = match self.frames.fps() {
            None => Demand::None,
            Some(_)
                if ui.get_rec_state() != RecState::Idle
                    || ui.get_live_state() != LiveState::Off =>
            {
                Demand::Explicit
            }
            Some(_) => Demand::Background,
        };
        if let Some(WindowState::Opening {
            answer, discard, ..
        }) = self.window.as_ref().map(|w| &w.state)
        {
            let discard = *discard;
            match answer.try_recv() {
                Err(TryRecvError::Empty) => {}
                Ok(Ok((capture, live, ended))) if !discard => {
                    log!("capturing the game window");
                    live.store(true, Ordering::Relaxed);
                    self.frames.set_window(true);
                    if let Some(w) = self.window.as_mut() {
                        w.state = WindowState::Open {
                            capture,
                            live,
                            ended,
                        };
                    }
                }
                Ok(Ok((capture, ..))) => {
                    self.window = None;
                    stop_window(capture);
                }
                Ok(Err((cancelled, e))) => {
                    let key = self.window.take().map(|w| w.key).unwrap_or_default();
                    if !cancelled {
                        log!("could not capture the game window: {e}");
                    }
                    // Not asked again until another game or request.
                    self.switch.block(&key);
                }
                // Its thread could not start: not retried every tick.
                Err(TryRecvError::Disconnected) => {
                    let key = self.window.take().map(|w| w.key).unwrap_or_default();
                    self.switch.block(&key);
                }
            }
        }
        let ended = matches!(
            self.window.as_ref().map(|w| &w.state),
            Some(WindowState::Open { ended, .. }) if ended.load(Ordering::Relaxed)
        );
        if ended {
            let key = self
                .window
                .as_ref()
                .map(|w| w.key.clone())
                .unwrap_or_default();
            self.close_window();
            self.switch.block(&key);
            // As when a layer game quits: a recording stops.
            if !layer {
                self.closed = true;
            }
        }
        if demand != Demand::None
            && !layer
            && self
                .steam
                .as_ref()
                .is_none_or(|(t, _)| now.duration_since(*t) >= STEAM_SCAN)
        {
            self.steam = Some((now, fallback::running_game()));
        }
        let input = fallback::Input {
            demand,
            layer,
            quit,
            game: self
                .steam
                .as_ref()
                .and_then(|(_, g)| g.as_ref().map(|(k, _)| k.clone())),
            window: match self.window.as_ref().map(|w| &w.state) {
                None => fallback::Window::Closed,
                Some(WindowState::Opening { .. }) => fallback::Window::Opening,
                Some(WindowState::Open { .. }) => fallback::Window::Open,
            },
        };
        match self.switch.tick(now, &input) {
            Action::None => {}
            Action::Close => match self.window.as_mut().map(|w| &mut w.state) {
                Some(WindowState::Opening {
                    cancel, discard, ..
                }) => {
                    log!("game window not needed any more: closing its request");
                    cancel.cancel();
                    *discard = true;
                }
                _ => {
                    log!("game window capture stopped");
                    self.close_window();
                }
            },
            Action::Open(key) => {
                let name = match &self.steam {
                    Some((_, Some((k, name)))) if *k == key => name.clone(),
                    _ => String::new(),
                };
                let fps = self.frames.fps().unwrap_or(60);
                log!("no game captured by the layer: asking for the game window ({key})");
                let cancel = Cancel::default();
                let answer = open_window(key.clone(), self.frames.clone(), fps, cancel.clone());
                self.window = Some(GameWindow {
                    key,
                    name,
                    state: WindowState::Opening {
                        answer,
                        cancel,
                        discard: false,
                    },
                });
            }
        }
        let picking = matches!(
            self.window.as_ref().map(|w| &w.state),
            Some(WindowState::Opening { discard: false, .. })
        );
        self.frames.set_picking(picking);
    }

    /// Shows the game to record, the other allowed ones and the first game
    /// waiting for consent. False once the server failed: restart it.
    pub fn refresh(&mut self, ui: &AppWindow) -> bool {
        let games = self.capture.games();
        // A picked game that quit: back to automatic choice.
        if let Some(pid) = self.selected
            && !games
                .iter()
                .any(|g| g.pid == pid && g.consent == Consent::Allowed)
        {
            self.selected = None;
            self.capture.select(None);
        }
        // Gone with its process: the game quit. Still running: its window
        // takes over after the grace.
        if let Some(pid) = self.captured
            && !games.iter().any(|g| g.pid == pid)
        {
            self.lost = Some((pid, Instant::now()));
        }
        self.captured = games.iter().find(|g| g.captured).map(|g| g.pid);
        let window_open = matches!(
            self.window.as_ref().map(|w| &w.state),
            Some(WindowState::Open { .. })
        );
        if self.captured.is_some() || window_open {
            self.lost = None;
        }
        let mut quit = false;
        if let Some((pid, _)) = self.lost
            && !fallback::alive(pid)
        {
            self.lost = None;
            self.closed = true;
            quit = true;
            // Its lingering processes (wineserver...) name the game whose
            // window is not asked for.
            self.steam = Some((Instant::now(), fallback::running_game()));
        }
        let lost = self
            .lost
            .is_some_and(|(_, t)| t.elapsed() < fallback::GRACE);
        let pending = waits_for_consent(&games, &IGNORED.lock().unwrap());
        self.drive_window(ui, self.captured.is_some() || lost || pending, quit);
        let shown_game = candidate(&games, self.selected).cloned();
        let window = match &self.window {
            Some(GameWindow {
                name,
                state: WindowState::Open { capture, .. },
                ..
            }) if shown_game.is_none() => Some((name.clone(), capture.source().size)),
            _ => None,
        };
        let (name, detail, via) = match (&shown_game, window) {
            (Some(g), _) => {
                let api = match self.api {
                    Some((pid, api)) if pid == g.pid => api,
                    _ => api_of(g.pid),
                };
                self.api = Some((g.pid, api));
                let size = (g.width > 0).then(|| format!("{} × {}", g.width, g.height));
                (g.exe.clone(), size.unwrap_or_default(), api)
            }
            (None, Some((name, size))) => {
                let name = if name.is_empty() { "Game".into() } else { name };
                let size = size.map(|(w, h)| format!("{w} × {h}"));
                (name, size.unwrap_or_default(), "window")
            }
            (None, None) => (String::new(), String::new(), ""),
        };
        ui.set_source_name(name.into());
        ui.set_source_detail(detail.into());
        ui.set_game_source(via.into());

        // Only games with a swapchain: Vulkan tools without a window never
        // show up or ask for consent.
        let shown_pid = shown_game.map(|g| g.pid);
        let others: Vec<(String, u32)> = games
            .iter()
            .filter(|g| g.consent == Consent::Allowed && g.width > 0 && Some(g.pid) != shown_pid)
            .map(|g| (g.exe.clone(), g.pid))
            .collect();
        if others != self.shown {
            let entries: Vec<GameEntry> = others
                .iter()
                .map(|(name, pid)| GameEntry {
                    name: name.as_str().into(),
                    pid: *pid as i32,
                })
                .collect();
            ui.set_other_games(ModelRc::from(Rc::new(VecModel::from(entries))));
            self.shown = others;
        }
        // The prompt shown stays while its game still asks, so Allow never
        // lands on a name that changed under the cursor.
        let ignored = IGNORED.lock().unwrap();
        let asking =
            |g: &&Game| g.consent == Consent::Pending && g.width > 0 && !ignored.contains(&g.exe);
        let prompt = ui.get_pending_game();
        if !games
            .iter()
            .filter(asking)
            .any(|g| g.exe == prompt.as_str())
        {
            let pending = games.iter().find(asking);
            ui.set_pending_game(pending.map_or("", |g| g.exe.as_str()).into());
        }
        // The server thread ended (its error went to a pipeline, if any).
        !self.failed.load(Ordering::Relaxed)
    }
}

impl Drop for GameSource {
    fn drop(&mut self) {
        self.close_window();
    }
}

/// Clears what game mode showed.
pub fn clear(ui: &AppWindow) {
    ui.set_source_name("".into());
    ui.set_source_detail("".into());
    ui.set_game_source("".into());
    ui.set_pending_game("".into());
    ui.set_other_games(ModelRc::default());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(pid: u32, consent: Consent, width: u32, captured: bool, seq: u64) -> Game {
        Game {
            pid,
            exe: format!("g{pid}"),
            width,
            height: width,
            consent,
            captured,
            seq,
        }
    }

    #[test]
    fn a_game_asking_for_consent_holds_the_window_picker_back() {
        let pending = [game(1, Consent::Pending, 640, false, 1)];
        assert!(waits_for_consent(&pending, &[]));
        // Ignored, or no swapchain yet (no prompt shows): the window may go.
        assert!(!waits_for_consent(&pending, &["g1".into()]));
        assert!(!waits_for_consent(
            &[game(2, Consent::Pending, 0, false, 1)],
            &[]
        ));
        assert!(!waits_for_consent(
            &[game(3, Consent::Allowed, 640, false, 1)],
            &[]
        ));
        assert!(!waits_for_consent(&[], &[]));
    }

    #[test]
    fn candidate_mirrors_the_server_choice() {
        let pid = |g: Option<&Game>| g.map(|g| g.pid);
        let a = game(1, Consent::Allowed, 640, false, 2);
        let b = game(2, Consent::Allowed, 640, false, 1);
        let pending = game(3, Consent::Pending, 640, false, 9);
        let no_swapchain = game(4, Consent::Allowed, 0, false, 8);
        let all = [a.clone(), b.clone(), pending, no_swapchain];
        // Newest allowed game with a swapchain, not list order.
        assert_eq!(pid(candidate(&all, None)), Some(1));
        // The captured game wins.
        let captured = [a.clone(), game(2, Consent::Allowed, 640, true, 1)];
        assert_eq!(pid(candidate(&captured, None)), Some(2));
        // A pick sticks, and never falls back to another game.
        assert_eq!(pid(candidate(&all, Some(2))), Some(2));
        assert_eq!(pid(candidate(&all, Some(3))), None);
        assert_eq!(pid(candidate(&all, Some(4))), None);
        assert_eq!(pid(candidate(&all, Some(99))), None);
        assert_eq!(pid(candidate(&[], None)), None);
    }
}
