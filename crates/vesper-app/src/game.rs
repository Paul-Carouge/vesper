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
        })
    }

    /// The game a recording would capture: the captured one, else the
    /// picked one, else the newest allowed game with a swapchain.
    fn candidate(&self, games: &[Game]) -> Option<Game> {
        let ready = |g: &&Game| g.consent == Consent::Allowed && g.width > 0;
        games
            .iter()
            .find(|g| g.captured)
            .or_else(|| {
                games
                    .iter()
                    .filter(ready)
                    .find(|g| Some(g.pid) == self.selected)
            })
            .or_else(|| games.iter().rfind(|g| ready(g)))
            .cloned()
    }

    /// Whether there is a game to record.
    pub fn has_game(&self) -> bool {
        self.candidate(&self.capture.games()).is_some()
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
        let shown_game = self.candidate(&games);
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
