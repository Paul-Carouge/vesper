//! Game mode without a layer game: the game's window, through the
//! ScreenCast portal, so a game records with no setup.
//!
//! [`Switch`] decides when (pure, tested here); [`crate::game`] runs the
//! portal and feeds the window's frames into the same route as the layer's.
//! The layer (or the OpenGL interposer) wins whenever it captures a game: it
//! costs the game less and has no compositor in between.
//!
//! The picker shows only when something needs a source: a recording or a
//! stream asked for (after [`GRACE`], so a layer game can connect first), or
//! the replay buffer while a Steam game runs (after [`BACKGROUND_AFTER`]).
//! A picker the user cancels, or a window that closes, is not asked again
//! for that game until another game runs or the user starts a recording or
//! a stream: never a picker in a loop.
//!
//! Restore tokens are kept per game (`window-tokens`), so each game's
//! picker shows once. The key is the game's Steam id (`SteamAppId`, else
//! `SteamGameId` for a non-Steam shortcut) read from its environment: no
//! game is connected to tell its name, and the portal names no app. It
//! stays the same across launches and updates, unlike a pid, and unlike a
//! process name it never mixes two games (Proton games are all `wine`
//! children with `.exe` names). Games outside Steam share one key.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Time without a layer game before a recording or stream falls back to
/// the window: a layer game starting with it connects within it.
pub const GRACE: Duration = Duration::from_secs(2);

/// Time a Steam game runs, without a layer game, before the replay buffer
/// asks for its window (its window exists by then).
pub const BACKGROUND_AFTER: Duration = Duration::from_secs(10);

/// Key of the window of a game outside Steam.
pub const OTHER_GAME: &str = "window";

/// What a source is needed for, in increasing order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Demand {
    /// No pipeline runs.
    None,
    /// Only the replay buffer.
    Background,
    /// A recording or a stream, asked for by the user.
    Explicit,
}

/// The game's window capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Closed,
    /// The portal is asked (picker or restore).
    Opening,
    Open,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub demand: Demand,
    /// A layer or interposer game is captured, waits for consent, or was
    /// just lost and still runs.
    pub layer: bool,
    /// The layer game lost earlier exited (before its window was captured):
    /// its window is not asked for, an opening one is dropped.
    pub quit: bool,
    /// Key of the Steam game running, if any.
    pub game: Option<String>,
    pub window: Window,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    /// Capture the window of the game of this key.
    Open(String),
    /// Stop the window capture (or drop it once opened).
    Close,
}

/// When to open and close the game's window capture.
#[derive(Debug, Default)]
pub struct Switch {
    /// Since when a source is needed and no layer game is captured.
    no_layer_since: Option<Instant>,
    /// The Steam game seen, and since when.
    game_since: Option<(String, Instant)>,
    /// Not opened again (cancelled, or its window closed).
    blocked: Option<String>,
    last_demand: Option<Demand>,
}

impl Switch {
    pub fn tick(&mut self, now: Instant, input: &Input) -> Action {
        let rising = input.demand == Demand::Explicit && self.last_demand != Some(Demand::Explicit);
        self.last_demand = Some(input.demand);
        match (&input.game, &self.game_since) {
            (Some(k), Some((seen, _))) if k == seen => {}
            (Some(k), _) => self.game_since = Some((k.clone(), now)),
            (None, _) => self.game_since = None,
        }
        let want = input.game.as_deref().unwrap_or(OTHER_GAME);
        // A new request from the user, or another game: ask again.
        if rising || self.blocked.as_deref().is_some_and(|b| b != want) {
            self.blocked = None;
        }
        // The lost layer game exited after all: its window is gone too.
        if input.quit {
            self.blocked = Some(want.to_owned());
        }
        if input.layer || input.quit || input.demand == Demand::None {
            self.no_layer_since = None;
            return match input.window {
                Window::Closed => Action::None,
                _ => Action::Close,
            };
        }
        let since = *self.no_layer_since.get_or_insert(now);
        if input.window != Window::Closed || self.blocked.as_deref() == Some(want) {
            return Action::None;
        }
        let ready = now.duration_since(since) >= GRACE
            && match input.demand {
                Demand::Explicit => true,
                Demand::Background => self
                    .game_since
                    .as_ref()
                    .is_some_and(|(_, t)| now.duration_since(*t) >= BACKGROUND_AFTER),
                Demand::None => false,
            };
        if ready {
            Action::Open(want.to_owned())
        } else {
            Action::None
        }
    }

    /// Not opened again for `key` until another game or a new request.
    pub fn block(&mut self, key: &str) {
        self.blocked = Some(key.to_owned());
    }
}

/// The Steam id in `environ` (NUL-separated `KEY=VALUE`): `SteamAppId`, else
/// `SteamGameId` (non-Steam shortcuts); empty or 0 is none.
pub fn steam_id(environ: &[u8]) -> Option<String> {
    let var = |name: &[u8]| {
        environ.split(|b| *b == 0).find_map(|kv| {
            let (k, v) = kv.split_at(kv.iter().position(|b| *b == b'=')?);
            let v = &v[1..];
            (k == name && !v.is_empty() && v != b"0" && v.iter().all(u8::is_ascii_digit))
                .then(|| String::from_utf8_lossy(v).into_owned())
        })
    };
    var(b"SteamAppId").or_else(|| var(b"SteamGameId"))
}

/// Processes that come with every Steam or Proton game: never its name.
const HELPERS: &[&str] = &[
    "reaper",
    "pv-bwrap",
    "pressure-vessel",
    "srt-bwrap",
    "steam-runtime-l",
    "steam-launch-wr",
    "python3",
    "sh",
    "bash",
    "wine",
    "wine64",
    "wine-preloader",
    "wine64-preloade",
    "wineserver",
    "services.exe",
    "winedevice.exe",
    "plugplay.exe",
    "svchost.exe",
    "explorer.exe",
    "rpcss.exe",
    "tabtip.exe",
    "start.exe",
    "conhost.exe",
    "gameoverlayui",
];

/// The Steam game this user runs: its key and a process name to show.
/// The newest game process if several (two games at once: the newest).
// shortcut: reads every process's environment; fine every few seconds, watch Steam's own state if it ever shows in profiles.
pub fn running_game() -> Option<(String, String)> {
    let uid = unsafe { libc::getuid() };
    let mut best: Option<(u32, String, String)> = None;
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let dir = entry.path();
        // Other users' environments are not readable anyway.
        let mine = entry
            .metadata()
            .is_ok_and(|m| std::os::unix::fs::MetadataExt::uid(&m) == uid);
        if !mine {
            continue;
        }
        let Some(id) = read_bounded(&dir.join("environ")).and_then(|e| steam_id(&e)) else {
            continue;
        };
        let comm = std::fs::read_to_string(dir.join("comm")).unwrap_or_default();
        let comm = comm.trim();
        if vesper_gamecapture::client::is_blocked(comm) {
            continue;
        }
        let helper = HELPERS.iter().any(|h| h.eq_ignore_ascii_case(comm));
        // A helper only names the game when nothing else does.
        let rank = if helper { 0 } else { pid.max(1) };
        if best.as_ref().is_none_or(|(r, _, _)| rank > *r) {
            let name = if helper {
                String::new()
            } else {
                comm.to_owned()
            };
            best = Some((rank, format!("steam:{id}"), name));
        }
    }
    best.map(|(_, key, name)| (key, name))
}

/// At most the first 256 KiB of `path`: a process's environment can be huge.
fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(256 * 1024)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(bytes)
}

/// Whether the process `pid` still runs: a zombie (exited, not reaped yet)
/// does not.
pub fn alive(pid: u32) -> bool {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    // `pid (comm) state ...`: the name may hold spaces and parentheses.
    stat.rsplit_once(')')
        .is_some_and(|(_, rest)| !rest.trim_start().starts_with(['Z', 'X']))
}

/// `$XDG_CONFIG_HOME/vesper/window-tokens`: `<key>\t<token>` lines.
fn tokens_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("vesper").join("window-tokens"))
}

/// The token lines; `Err` when the file cannot be read (then it is never
/// rewritten).
fn read_tokens(path: &Path) -> std::io::Result<Vec<(String, String)>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(text
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .filter(|(k, t)| !k.is_empty() && !t.trim().is_empty())
        .map(|(k, t)| (k.to_owned(), t.trim().to_owned()))
        .collect())
}

fn load_token_at(path: &Path, key: &str) -> Option<String> {
    read_tokens(path)
        .inspect_err(|e| log!("could not read {}: {e}", path.display()))
        .ok()?
        .into_iter()
        .find_map(|(k, t)| (k == key).then_some(t))
}

/// Sets (`Some`) or removes `key`'s token, the others as they are. Written
/// atomically, mode 0600: a token records that window without asking.
fn write_token_at(path: &Path, key: &str, token: Option<&str>) -> std::io::Result<()> {
    let bad = |s: &str| s.contains(['\n', '\r', '\t']);
    if bad(key) || token.is_some_and(bad) {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let mut tokens = read_tokens(path)?;
    tokens.retain(|(k, _)| k != key);
    if let Some(token) = token {
        tokens.push((key.to_owned(), token.to_owned()));
    }
    let text: String = tokens.iter().map(|(k, t)| format!("{k}\t{t}\n")).collect();
    crate::prefs::write_atomically(path, text.as_bytes())
}

/// The saved restore token of `key`'s window.
pub fn load_token(key: &str) -> Option<String> {
    load_token_at(&tokens_path()?, key)
}

/// Saves (`Some`) or forgets `key`'s restore token; failures are logged.
pub fn store_token(key: &str, token: Option<&str>) {
    let Some(path) = tokens_path() else { return };
    if let Err(e) = write_token_at(&path, key, token) {
        log!("could not save the game window token: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(demand: Demand, layer: bool, game: Option<&str>, window: Window) -> Input {
        Input {
            demand,
            layer,
            quit: false,
            game: game.map(str::to_owned),
            window,
        }
    }

    #[test]
    fn a_lost_layer_game_that_exits_late_drops_its_opening_window() {
        let (mut sw, t) = (Switch::default(), Instant::now());
        // The layer game disconnected; its process outlived the grace.
        let lost = input(Demand::Explicit, false, Some("steam:1"), Window::Closed);
        sw.tick(t, &lost);
        assert_eq!(sw.tick(t + GRACE, &lost), Action::Open("steam:1".into()));
        // It exits while the portal is asked: dropped, never asked again.
        let quit = Input {
            quit: true,
            ..input(Demand::Explicit, false, Some("steam:1"), Window::Opening)
        };
        assert_eq!(sw.tick(t + 3 * S, &quit), Action::Close);
        assert_eq!(sw.tick(t + 10 * S, &lost), Action::None);
        let bg = input(Demand::Background, false, Some("steam:1"), Window::Closed);
        assert_eq!(sw.tick(t + 60 * S, &bg), Action::None);
        // Exiting before the window was asked: never asked.
        let (mut sw, t) = (Switch::default(), Instant::now());
        sw.tick(t, &lost);
        let quit = Input {
            quit: true,
            ..lost.clone()
        };
        assert_eq!(sw.tick(t + S, &quit), Action::None);
        assert_eq!(sw.tick(t + 10 * S, &lost), Action::None);
    }

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn a_recording_without_a_layer_game_opens_the_window_after_the_grace() {
        let (mut sw, t) = (Switch::default(), Instant::now());
        let i = input(Demand::Explicit, false, None, Window::Closed);
        assert_eq!(sw.tick(t, &i), Action::None);
        assert_eq!(sw.tick(t + S, &i), Action::None);
        assert_eq!(sw.tick(t + GRACE, &i), Action::Open(OTHER_GAME.into()));
        // Opening, then open: nothing more to do.
        let opening = input(Demand::Explicit, false, None, Window::Opening);
        assert_eq!(sw.tick(t + 3 * S, &opening), Action::None);
        // A Steam game names its own token.
        let (mut sw, t) = (Switch::default(), Instant::now());
        let i = input(Demand::Explicit, false, Some("steam:570"), Window::Closed);
        sw.tick(t, &i);
        assert_eq!(sw.tick(t + GRACE, &i), Action::Open("steam:570".into()));
    }

    #[test]
    fn a_layer_game_wins_and_its_loss_falls_back() {
        let (mut sw, t) = (Switch::default(), Instant::now());
        let open = input(Demand::Explicit, false, Some("steam:1"), Window::Open);
        sw.tick(t, &open);
        // The layer captures: the window goes.
        let layer = input(Demand::Explicit, true, Some("steam:1"), Window::Open);
        assert_eq!(sw.tick(t + S, &layer), Action::Close);
        let layer = input(Demand::Explicit, true, Some("steam:1"), Window::Closed);
        assert_eq!(sw.tick(t + 2 * S, &layer), Action::None);
        // Lost: back to the window once the grace is over.
        let lost = input(Demand::Explicit, false, Some("steam:1"), Window::Closed);
        assert_eq!(sw.tick(t + 3 * S, &lost), Action::None);
        assert_eq!(
            sw.tick(t + 3 * S + GRACE, &lost),
            Action::Open("steam:1".into())
        );
    }

    #[test]
    fn the_replay_buffer_alone_waits_for_a_steam_game_to_run_a_while() {
        let (mut sw, t) = (Switch::default(), Instant::now());
        // No game: never a picker for the replay buffer alone.
        let idle = input(Demand::Background, false, None, Window::Closed);
        assert_eq!(sw.tick(t, &idle), Action::None);
        assert_eq!(sw.tick(t + 60 * S, &idle), Action::None);
        let game = input(Demand::Background, false, Some("steam:7"), Window::Closed);
        assert_eq!(sw.tick(t + 61 * S, &game), Action::None);
        assert_eq!(sw.tick(t + 70 * S, &game), Action::None);
        assert_eq!(
            sw.tick(t + 61 * S + BACKGROUND_AFTER, &game),
            Action::Open("steam:7".into())
        );
        // Another game starts the wait again.
        let other = input(Demand::Background, false, Some("steam:8"), Window::Closed);
        assert_eq!(sw.tick(t + 80 * S, &other), Action::None);
    }

    #[test]
    fn nothing_needs_a_source_closes_the_window() {
        let (mut sw, t) = (Switch::default(), Instant::now());
        let none = input(Demand::None, false, Some("steam:1"), Window::Open);
        assert_eq!(sw.tick(t, &none), Action::Close);
        let none = input(Demand::None, false, Some("steam:1"), Window::Closed);
        assert_eq!(sw.tick(t + 60 * S, &none), Action::None);
    }

    #[test]
    fn a_cancelled_picker_is_not_shown_again_until_the_user_asks() {
        let (mut sw, t) = (Switch::default(), Instant::now());
        let rec = input(Demand::Explicit, false, Some("steam:1"), Window::Closed);
        sw.tick(t, &rec);
        assert_eq!(sw.tick(t + GRACE, &rec), Action::Open("steam:1".into()));
        sw.block("steam:1");
        assert_eq!(sw.tick(t + 10 * S, &rec), Action::None);
        // The recording failed: the replay buffer does not ask either.
        let bg = input(Demand::Background, false, Some("steam:1"), Window::Closed);
        assert_eq!(sw.tick(t + 60 * S, &bg), Action::None);
        // A new recording asks again.
        assert_eq!(sw.tick(t + 61 * S, &rec), Action::Open("steam:1".into()));
    }

    #[test]
    fn a_closed_window_is_not_reopened_until_another_game_runs() {
        let (mut sw, t) = (Switch::default(), Instant::now());
        let bg = input(Demand::Background, false, Some("steam:1"), Window::Closed);
        sw.tick(t, &bg);
        assert_eq!(
            sw.tick(t + BACKGROUND_AFTER, &bg),
            Action::Open("steam:1".into())
        );
        // The game quit: its window closed, its processes linger.
        sw.block("steam:1");
        assert_eq!(sw.tick(t + 30 * S, &bg), Action::None);
        let gone = input(Demand::Background, false, None, Window::Closed);
        assert_eq!(sw.tick(t + 31 * S, &gone), Action::None);
        // Launched again: asked again (the token restores it silently).
        sw.tick(t + 40 * S, &bg);
        assert_eq!(
            sw.tick(t + 40 * S + BACKGROUND_AFTER, &bg),
            Action::Open("steam:1".into())
        );
    }

    #[test]
    fn steam_ids_come_from_the_environment() {
        assert_eq!(steam_id(b"A=1\0SteamAppId=570\0"), Some("570".into()));
        // Non-Steam shortcuts only get SteamGameId.
        let shortcut = b"SteamAppId=0\0SteamGameId=12345678901234\0";
        assert_eq!(steam_id(shortcut), Some("12345678901234".into()));
        assert_eq!(steam_id(b"SteamAppId=\0"), None);
        assert_eq!(steam_id(b"XSteamAppId=5\0SteamAppIdX=5\0"), None);
        assert_eq!(steam_id(b"SteamAppId=5\tx\0"), None);
        assert_eq!(steam_id(b""), None);
    }

    #[test]
    fn a_zombie_is_not_alive() {
        assert!(alive(std::process::id()));
        let mut child = std::process::Command::new("true").spawn().unwrap();
        // Exited, not reaped yet: a zombie.
        let pid = child.id();
        let t = Instant::now();
        while alive(pid) && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!alive(pid));
        assert!(
            Path::new(&format!("/proc/{pid}")).exists(),
            "still a zombie"
        );
        child.wait().unwrap();
        assert!(!alive(u32::MAX));
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vesper-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("window-tokens")
    }

    #[test]
    fn tokens_are_kept_per_game_in_a_private_file() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp("tokens");
        assert_eq!(load_token_at(&path, "steam:1"), None);
        write_token_at(&path, "steam:1", Some("a")).unwrap();
        write_token_at(&path, OTHER_GAME, Some("b")).unwrap();
        write_token_at(&path, "steam:1", Some("c")).unwrap();
        assert_eq!(load_token_at(&path, "steam:1").as_deref(), Some("c"));
        assert_eq!(load_token_at(&path, OTHER_GAME).as_deref(), Some("b"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // Forgetting one keeps the other.
        write_token_at(&path, "steam:1", None).unwrap();
        assert_eq!(load_token_at(&path, "steam:1"), None);
        assert_eq!(load_token_at(&path, OTHER_GAME).as_deref(), Some("b"));
        // Never a line break into the file.
        assert!(write_token_at(&path, "steam:2", Some("x\ny")).is_err());
        assert!(write_token_at(&path, "a\tb", Some("x")).is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_unreadable_token_file_is_not_rewritten() {
        let path = temp("unreadable");
        // A folder where the file should be: reading fails.
        std::fs::create_dir(&path).unwrap();
        assert!(write_token_at(&path, "steam:1", Some("a")).is_err());
        assert!(path.is_dir());
        assert_eq!(load_token_at(&path, "steam:1"), None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
