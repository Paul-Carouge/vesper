//! Game mode: the game capture server (`vesper-gamecapture`) while the Game
//! source is selected, its consent prompt and its game picker.
//!
//! Consent: Steam games are allowed by the server itself; any other game
//! waits until the user clicks Allow, which persists its process name in the
//! settings. Nothing else is ever allowed automatically.
//!
//! The server only asks a game for frames while a pipeline consumes them
//! (`GameFrames` attached); otherwise games stay a pass-through.

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use slint::{ModelRc, VecModel};
use vesper_gamecapture::{Consent, Game, GameCapture};

use crate::prefs::{ALLOWED_GAMES, valid_game};
use crate::recorder::GameFrames;
use crate::{AppWindow, GameEntry};

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
        })
    }

    /// Whether there is a game to record.
    pub fn has_game(&self) -> bool {
        candidate(&self.capture.games(), self.selected).is_some()
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
        if let Some(pid) = self.captured
            && !games.iter().any(|g| g.pid == pid)
        {
            self.closed = true;
        }
        self.captured = games.iter().find(|g| g.captured).map(|g| g.pid);
        let shown_game = candidate(&games, self.selected).cloned();
        ui.set_source_name(shown_game.as_ref().map_or("", |g| g.exe.as_str()).into());
        let detail = shown_game
            .as_ref()
            .filter(|g| g.width > 0)
            .map_or(String::new(), |g| format!("{} × {}", g.width, g.height));
        ui.set_source_detail(detail.into());

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

/// Clears what game mode showed.
pub fn clear(ui: &AppWindow) {
    ui.set_source_name("".into());
    ui.set_source_detail("".into());
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
