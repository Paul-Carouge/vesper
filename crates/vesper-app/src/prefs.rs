//! User settings kept across launches in `$XDG_CONFIG_HOME/vesper/settings`,
//! as `key=value` lines. Unknown keys, missing keys and bad values are
//! ignored: those settings keep their defaults. Written atomically.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::outputs::{Kind, SourceLabel};
use crate::{AppWindow, QualityPreset, SourceKind, VideoCodec};

/// Process names (`/proc/<pid>/comm`) the user allowed game capture for. Not
/// UI state, so kept here for `Prefs::from_ui`.
pub static ALLOWED_GAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The one-time "still running in the tray" notice was shown.
pub static TRAY_NOTICE_SHOWN: AtomicBool = AtomicBool::new(false);

/// Choices offered by the UI; anything else in the file is ignored.
const FPS: [i32; 4] = [30, 60, 120, 144];
const REPLAY_SECONDS: [i32; 3] = [30, 60, 120];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Prefs {
    pub codec: VideoCodec,
    pub quality: QualityPreset,
    pub fps: i32,
    pub replay_enabled: bool,
    pub replay_seconds: i32,
    pub desktop_muted: bool,
    pub mic_muted: bool,
    pub output_folder: String,
    /// Record a game (Vulkan layer) instead of a portal source.
    pub game_source: bool,
    pub allowed_games: Vec<String>,
    /// "Start with the session" (the autostart entry). Never on by default.
    pub autostart: bool,
    pub tray_notice_shown: bool,
}

impl Prefs {
    pub fn from_ui(ui: &AppWindow) -> Self {
        Self {
            codec: ui.get_codec(),
            quality: ui.get_quality(),
            fps: ui.get_fps(),
            replay_enabled: ui.get_replay_enabled(),
            replay_seconds: ui.get_replay_seconds(),
            desktop_muted: ui.get_desktop_muted(),
            mic_muted: ui.get_mic_muted(),
            output_folder: ui.get_output_folder().into(),
            game_source: ui.get_source_kind() == SourceKind::Game,
            allowed_games: ALLOWED_GAMES.lock().unwrap().clone(),
            autostart: ui.get_autostart(),
            tray_notice_shown: TRAY_NOTICE_SHOWN.load(Ordering::Relaxed),
        }
    }

    pub fn apply(&self, ui: &AppWindow) {
        ui.set_codec(self.codec);
        ui.set_quality(self.quality);
        ui.set_fps(self.fps);
        ui.set_replay_enabled(self.replay_enabled);
        ui.set_replay_seconds(self.replay_seconds);
        ui.set_desktop_muted(self.desktop_muted);
        ui.set_mic_muted(self.mic_muted);
        ui.set_output_folder(self.output_folder.as_str().into());
        if self.game_source {
            ui.set_source_kind(SourceKind::Game);
        }
        ALLOWED_GAMES
            .lock()
            .unwrap()
            .clone_from(&self.allowed_games);
        ui.set_autostart(self.autostart);
        TRAY_NOTICE_SHOWN.store(self.tray_notice_shown, Ordering::Relaxed);
    }

    /// `self` with the valid settings of `text` applied.
    pub fn parse(mut self, text: &str) -> Self {
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim_end_matches('\r');
            match key.trim() {
                "codec" => set(&mut self.codec, codec(value)),
                "quality" => set(&mut self.quality, quality(value)),
                "fps" => set(&mut self.fps, choice(value, &FPS)),
                "replay" => set(&mut self.replay_enabled, flag(value)),
                "replay_seconds" => set(&mut self.replay_seconds, choice(value, &REPLAY_SECONDS)),
                "desktop_muted" => set(&mut self.desktop_muted, flag(value)),
                "mic_muted" => set(&mut self.mic_muted, flag(value)),
                "output_folder" if Path::new(value).is_absolute() => {
                    self.output_folder = value.to_owned();
                }
                "game_source" => set(&mut self.game_source, flag(value)),
                "allow_game"
                    if valid_game(value) && !self.allowed_games.iter().any(|g| g == value) =>
                {
                    self.allowed_games.push(value.to_owned());
                }
                "autostart" => set(&mut self.autostart, flag(value)),
                "tray_notice_shown" => set(&mut self.tray_notice_shown, flag(value)),
                _ => {}
            }
        }
        self
    }

    /// The folder is left out while it is `default_folder`, so a later
    /// default (e.g. a moved `XDG_VIDEOS_DIR`) still applies.
    pub fn serialize(&self, default_folder: &str) -> String {
        let codec = match self.codec {
            VideoCodec::Av1 => "av1",
            VideoCodec::Hevc => "hevc",
            VideoCodec::H264 => "h264",
        };
        let quality = match self.quality {
            QualityPreset::Performance => "performance",
            QualityPreset::Balanced => "balanced",
            QualityPreset::Quality => "quality",
        };
        let mut text = format!(
            "# Vesper settings, rewritten on every change.\n\
             codec={codec}\nquality={quality}\nfps={}\nreplay={}\nreplay_seconds={}\n\
             desktop_muted={}\nmic_muted={}\n",
            self.fps, self.replay_enabled, self.replay_seconds, self.desktop_muted, self.mic_muted
        );
        // A line break would split the line: keep the default folder instead.
        if self.output_folder != default_folder && !self.output_folder.contains(['\n', '\r']) {
            text += &format!("output_folder={}\n", self.output_folder);
        }
        text += &format!(
            "game_source={}\nautostart={}\ntray_notice_shown={}\n",
            self.game_source, self.autostart, self.tray_notice_shown
        );
        for game in self.allowed_games.iter().filter(|g| valid_game(g)) {
            text += &format!("allow_game={game}\n");
        }
        text
    }

    /// `defaults` with the saved settings applied. A missing or unreadable
    /// file keeps the defaults.
    pub fn load(defaults: Self) -> Self {
        let Some(path) = path() else { return defaults };
        match std::fs::read(&path) {
            Ok(bytes) => defaults.parse(&String::from_utf8_lossy(&bytes)),
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    log!("could not read {}: {e}", path.display());
                }
                defaults
            }
        }
    }

    pub fn save(&self) {
        let Some(path) = path() else { return };
        let default_folder = crate::default_output_folder().unwrap_or_default();
        let text = self.serialize(&default_folder.to_string_lossy());
        if let Err(e) = write_atomically(&path, text.as_bytes()) {
            log!("could not save {}: {e}", path.display());
        }
    }
}

fn set<T>(field: &mut T, value: Option<T>) {
    if let Some(value) = value {
        *field = value;
    }
}

fn codec(value: &str) -> Option<VideoCodec> {
    match value.trim() {
        "av1" => Some(VideoCodec::Av1),
        "hevc" => Some(VideoCodec::Hevc),
        "h264" => Some(VideoCodec::H264),
        _ => None,
    }
}

fn quality(value: &str) -> Option<QualityPreset> {
    match value.trim() {
        "performance" => Some(QualityPreset::Performance),
        "balanced" => Some(QualityPreset::Balanced),
        "quality" => Some(QualityPreset::Quality),
        _ => None,
    }
}

/// A process name as the kernel reports it: 1 to 15 bytes, one line.
pub fn valid_game(name: &str) -> bool {
    (1..=15).contains(&name.len()) && name == name.trim() && !name.chars().any(char::is_control)
}

fn choice(value: &str, options: &[i32]) -> Option<i32> {
    value.trim().parse().ok().filter(|v| options.contains(v))
}

fn flag(value: &str) -> Option<bool> {
    value.trim().parse().ok()
}

/// `$XDG_CONFIG_HOME/vesper/settings`.
fn path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("vesper").join("settings"))
}

/// `$XDG_CONFIG_HOME/vesper/source`: how to show the source of the saved
/// restore token before its capture starts.
fn source_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("vesper").join("source"))
}

fn parse_source(text: &str) -> SourceLabel {
    let mut label = SourceLabel::default();
    let (mut w, mut h) = (None, None);
    for (key, value) in text.lines().filter_map(|l| l.split_once('=')) {
        match key {
            "kind" => set(&mut label.kind, Kind::from_id(value)),
            "name" if !value.is_empty() => label.name = Some(value.to_owned()),
            "width" => w = value.parse().ok(),
            "height" => h = value.parse().ok(),
            _ => {}
        }
    }
    label.size = w.zip(h);
    // Only a monitor has a name.
    if label.kind != Kind::Monitor {
        label.name = None;
    }
    label
}

fn serialize_source(label: &SourceLabel) -> String {
    let mut text = format!("kind={}\n", label.kind.id());
    if let Some(name) = label.name.as_ref().filter(|n| !n.contains(['\n', '\r'])) {
        text += &format!("name={name}\n");
    }
    if let Some((w, h)) = label.size {
        text += &format!("width={w}\nheight={h}\n");
    }
    text
}

/// The label saved with the restore token, if any.
pub fn load_source() -> Option<SourceLabel> {
    std::fs::read_to_string(source_path()?)
        .ok()
        .map(|text| parse_source(&text))
}

pub fn save_source(label: &SourceLabel) {
    let Some(path) = source_path() else { return };
    if let Err(e) = write_atomically(&path, serialize_source(label).as_bytes()) {
        log!("could not save {}: {e}", path.display());
    }
}

pub fn clear_source() {
    if let Some(path) = source_path()
        && let Err(e) = std::fs::remove_file(&path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log!("could not remove {}: {e}", path.display());
    }
}

/// Writes a temporary file next to `path`, then renames it over `path`: a
/// process crash leaves either the old or the new file, never half of one.
/// No fsync: it would block the UI thread, and settings are cheap to lose.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().ok_or(std::io::ErrorKind::NotFound)?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT: &str = "/home/u/Videos/Vesper";

    fn defaults() -> Prefs {
        Prefs {
            codec: VideoCodec::Av1,
            quality: QualityPreset::Balanced,
            fps: 60,
            replay_enabled: true,
            replay_seconds: 30,
            desktop_muted: false,
            mic_muted: false,
            output_folder: DEFAULT.into(),
            game_source: false,
            allowed_games: Vec::new(),
            autostart: false,
            tray_notice_shown: false,
        }
    }

    fn changed() -> Prefs {
        Prefs {
            codec: VideoCodec::H264,
            quality: QualityPreset::Quality,
            fps: 144,
            replay_enabled: false,
            replay_seconds: 120,
            desktop_muted: true,
            mic_muted: true,
            output_folder: "/mnt/clips/my folder = 2".into(),
            game_source: true,
            allowed_games: vec!["Game.exe".into(), "a=b c".into()],
            autostart: true,
            tray_notice_shown: true,
        }
    }

    #[test]
    fn round_trip() {
        for prefs in [defaults(), changed()] {
            assert_eq!(defaults().parse(&prefs.serialize(DEFAULT)), prefs);
        }
        let back = changed().parse(&defaults().serialize(DEFAULT));
        assert_eq!(
            back,
            Prefs {
                output_folder: changed().output_folder,
                // Saved entries add to the list.
                allowed_games: changed().allowed_games,
                ..defaults()
            }
        );
    }

    #[test]
    fn default_folder_is_not_written() {
        assert!(!defaults().serialize(DEFAULT).contains("output_folder"));
        assert!(changed().serialize(DEFAULT).contains("output_folder="));
    }

    #[test]
    fn missing_keys_keep_defaults() {
        assert_eq!(defaults().parse(""), defaults());
        let prefs = defaults().parse("fps=120\nmic_muted=true\n");
        assert_eq!(
            prefs,
            Prefs {
                fps: 120,
                mic_muted: true,
                ..defaults()
            }
        );
    }

    #[test]
    fn corrupt_values_and_unknown_keys_are_ignored() {
        let text = "codec=vp9\nquality=\nfps=59\nfps=abc\nreplay=yes\nreplay_seconds=-1\n\
                    desktop_muted=1\noutput_folder=relative/path\nnew_key=42\n\
                    garbage line\n=\n\u{0}\u{fffd}=x\n";
        assert_eq!(defaults().parse(text), defaults());
    }

    #[test]
    fn bad_allowed_games_are_ignored() {
        let text = "allow_game=\nallow_game=a_very_long_name.exe\nallow_game= ok\nallow_game=ok\nallow_game=ok\n";
        assert_eq!(defaults().parse(text).allowed_games, ["ok"]);
        let prefs = Prefs {
            allowed_games: vec!["x\ny".into(), "ok".into()],
            ..defaults()
        };
        assert_eq!(
            defaults().parse(&prefs.serialize(DEFAULT)).allowed_games,
            ["ok"]
        );
    }

    #[test]
    fn tolerates_spaces_and_crlf() {
        let prefs = defaults().parse(" codec = hevc \r\nfps=30\r\n");
        assert_eq!(prefs.codec, VideoCodec::Hevc);
        assert_eq!(prefs.fps, 30);
    }

    #[test]
    fn folder_with_a_line_break_is_not_written() {
        let prefs = Prefs {
            output_folder: "/a\nfps=30".into(),
            ..defaults()
        };
        assert_eq!(
            changed().parse(&prefs.serialize(DEFAULT)).output_folder,
            changed().output_folder
        );
        assert_eq!(defaults().parse(&prefs.serialize(DEFAULT)).fps, 60);
    }

    #[test]
    fn source_label_round_trip() {
        for label in [
            SourceLabel::default(),
            SourceLabel {
                kind: Kind::Monitor,
                name: Some("DP-1".into()),
                size: Some((3840, 2160)),
            },
            SourceLabel {
                kind: Kind::Window,
                name: None,
                size: Some((1280, 720)),
            },
            SourceLabel {
                kind: Kind::Region,
                name: None,
                size: Some((1920, 1080)),
            },
        ] {
            assert_eq!(parse_source(&serialize_source(&label)), label);
        }
        let broken = SourceLabel {
            name: Some("a\nkind=window".into()),
            ..SourceLabel::default()
        };
        assert_eq!(
            parse_source(&serialize_source(&broken)),
            SourceLabel::default()
        );
        assert_eq!(parse_source("width=1\ngarbage"), SourceLabel::default());
        // Only a monitor is named.
        assert_eq!(parse_source("kind=region\nname=DP-1\n").name, None);
    }

    #[test]
    fn write_replaces_the_file_atomically() {
        let dir = std::env::temp_dir().join(format!("vesper-prefs-{}", std::process::id()));
        let path = dir.join("settings");
        write_atomically(&path, b"fps=30\n").unwrap();
        write_atomically(&path, changed().serialize(DEFAULT).as_bytes()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(defaults().parse(&text), changed());
        assert!(!path.with_extension("tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
