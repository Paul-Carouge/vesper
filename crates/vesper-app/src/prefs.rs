//! User settings kept across launches in `$XDG_CONFIG_HOME/vesper/settings`,
//! as `key=value` lines. Unknown keys, missing keys and bad values are
//! ignored: those settings keep their defaults. Written atomically.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use vesper_audio::{MAX_DB, MIN_DB, Source};

use crate::outputs::{Kind, SourceLabel};
use crate::recorder::{MAX_TRACKS, Track};
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
    pub tracks: Vec<Track>,
    /// A mix of all tracks as the first audio track.
    pub audio_mix: bool,
    pub output_folder: String,
    /// Record a game (Vulkan layer) instead of a portal source.
    pub game_source: bool,
    pub allowed_games: Vec<String>,
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
            tracks: crate::audio::tracks_from_ui(ui),
            audio_mix: ui.get_audio_mix(),
            output_folder: ui.get_output_folder().into(),
            game_source: ui.get_source_kind() == SourceKind::Game,
            allowed_games: ALLOWED_GAMES.lock().unwrap().clone(),
            tray_notice_shown: TRAY_NOTICE_SHOWN.load(Ordering::Relaxed),
        }
    }

    pub fn apply(&self, ui: &AppWindow) {
        ui.set_codec(self.codec);
        ui.set_quality(self.quality);
        ui.set_fps(self.fps);
        ui.set_replay_enabled(self.replay_enabled);
        ui.set_replay_seconds(self.replay_seconds);
        crate::audio::show_tracks(ui, &self.tracks);
        ui.set_audio_mix(self.audio_mix);
        ui.set_output_folder(self.output_folder.as_str().into());
        if self.game_source {
            ui.set_source_kind(SourceKind::Game);
        }
        ALLOWED_GAMES
            .lock()
            .unwrap()
            .clone_from(&self.allowed_games);
        TRAY_NOTICE_SHOWN.store(self.tray_notice_shown, Ordering::Relaxed);
    }

    /// `self` with the valid settings of `text` applied.
    ///
    /// Audio tracks: any `tracks` or `track` line replaces the list. Without
    /// them (settings of a version with fixed desktop and microphone tracks),
    /// the old `desktop_muted` and `mic_muted` apply to `self`'s tracks.
    pub fn parse(mut self, text: &str) -> Self {
        let mut tracks: Option<Vec<Track>> = None;
        let (mut desktop_muted, mut mic_muted) = (None, None);
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
                "desktop_muted" => desktop_muted = flag(value),
                "mic_muted" => mic_muted = flag(value),
                "tracks" => {
                    tracks.get_or_insert_default();
                }
                "track" => {
                    let list = tracks.get_or_insert_default();
                    if let Some(track) = parse_track(value)
                        && list.len() < MAX_TRACKS
                    {
                        list.push(track);
                    }
                }
                "audio_mix" => set(&mut self.audio_mix, flag(value)),
                "output_folder" if Path::new(value).is_absolute() => {
                    self.output_folder = value.to_owned();
                }
                "game_source" => set(&mut self.game_source, flag(value)),
                "allow_game"
                    if valid_game(value) && !self.allowed_games.iter().any(|g| g == value) =>
                {
                    self.allowed_games.push(value.to_owned());
                }
                "tray_notice_shown" => set(&mut self.tray_notice_shown, flag(value)),
                _ => {}
            }
        }
        match tracks {
            Some(tracks) => self.tracks = tracks,
            None => {
                for track in &mut self.tracks {
                    match track.source {
                        Source::Desktop => set(&mut track.muted, desktop_muted),
                        Source::Microphone => set(&mut track.muted, mic_muted),
                        _ => {}
                    }
                }
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
             codec={codec}\nquality={quality}\nfps={}\nreplay={}\nreplay_seconds={}\n",
            self.fps, self.replay_enabled, self.replay_seconds
        );
        // Marks the list as saved, even empty.
        text += "tracks=\n";
        for track in self.tracks.iter().filter(|t| valid_track(t)) {
            text += &format!("track={}\n", serialize_track(track));
        }
        text += &format!("audio_mix={}\n", self.audio_mix);
        // A line break would split the line: keep the default folder instead.
        if self.output_folder != default_folder && !self.output_folder.contains(['\n', '\r']) {
            text += &format!("output_folder={}\n", self.output_folder);
        }
        text += &format!(
            "game_source={}\ntray_notice_shown={}\n",
            self.game_source, self.tray_notice_shown
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

/// The desktop and microphone tracks of a first launch.
pub fn default_tracks() -> Vec<Track> {
    vec![
        Track::new("Desktop", Source::Desktop),
        Track::new("Microphone", Source::Microphone),
    ]
}

/// How a source is saved: `desktop`, `microphone`, `output:<node.name>`,
/// `input:<node.name>` or `app:<application>`.
pub fn audio_source_id(source: &Source) -> String {
    match source {
        Source::Desktop => "desktop".into(),
        Source::Microphone => "microphone".into(),
        Source::Output(node) => format!("output:{node}"),
        Source::Input(node) => format!("input:{node}"),
        Source::App(app) => format!("app:{app}"),
    }
}

pub fn audio_source_from_id(id: &str) -> Option<Source> {
    // Fields are `|`-separated, one per line.
    let named = |name: &str, source: fn(String) -> Source| {
        (!name.is_empty() && !name.contains('|') && !name.chars().any(char::is_control))
            .then(|| source(name.to_owned()))
    };
    match id.split_once(':') {
        None if id == "desktop" => Some(Source::Desktop),
        None if id == "microphone" => Some(Source::Microphone),
        Some(("output", node)) => named(node, Source::Output),
        Some(("input", node)) => named(node, Source::Input),
        Some(("app", app)) => named(app, Source::App),
        _ => None,
    }
}

/// A track name: 1 to 64 characters, one line, no outer spaces.
pub fn valid_track_name(name: &str) -> bool {
    (1..=64).contains(&name.chars().count())
        && name == name.trim()
        && !name.chars().any(char::is_control)
}

fn valid_track(track: &Track) -> bool {
    valid_track_name(&track.name)
        && audio_source_from_id(&audio_source_id(&track.source)).as_ref() == Some(&track.source)
}

/// `enabled|muted|volume_db|source|name`, the name last: it may hold a `|`.
fn serialize_track(track: &Track) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        track.enabled,
        track.muted,
        track.volume_db,
        audio_source_id(&track.source),
        track.name
    )
}

fn parse_track(value: &str) -> Option<Track> {
    let mut fields = value.splitn(5, '|');
    let enabled = flag(fields.next()?)?;
    let muted = flag(fields.next()?)?;
    let volume_db: f32 = fields.next()?.trim().parse().ok()?;
    let source = audio_source_from_id(fields.next()?)?;
    let name = fields.next()?;
    (volume_db.is_finite() && valid_track_name(name)).then(|| Track {
        name: name.to_owned(),
        source,
        enabled,
        muted,
        volume_db: volume_db.clamp(MIN_DB, MAX_DB),
    })
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
            tracks: default_tracks(),
            audio_mix: true,
            output_folder: DEFAULT.into(),
            game_source: false,
            allowed_games: Vec::new(),
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
            tracks: vec![
                Track {
                    muted: true,
                    volume_db: -3.5,
                    ..Track::new("Game | voice", Source::App("Elden Ring".into()))
                },
                Track {
                    enabled: false,
                    volume_db: 12.0,
                    ..Track::new(
                        "Headset",
                        Source::Output("alsa_output.usb-x.analog-stereo".into()),
                    )
                },
                Track::new("Mic", Source::Input("alsa_input.pci-0000_0c_00.6".into())),
                Track {
                    muted: true,
                    ..Track::new("Desktop", Source::Desktop)
                },
            ],
            audio_mix: false,
            output_folder: "/mnt/clips/my folder = 2".into(),
            game_source: true,
            allowed_games: vec!["Game.exe".into(), "a=b c".into()],
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
        let prefs = defaults().parse("fps=120\n");
        assert_eq!(
            prefs,
            Prefs {
                fps: 120,
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
    fn old_mute_flags_migrate_to_the_default_tracks() {
        let old = "codec=av1\ndesktop_muted=true\nmic_muted=false\n";
        let prefs = defaults().parse(old);
        assert_eq!(prefs.tracks.len(), 2);
        assert!(prefs.tracks[0].muted && prefs.tracks[0].source == Source::Desktop);
        assert!(!prefs.tracks[1].muted && prefs.tracks[1].source == Source::Microphone);
        assert!(prefs.audio_mix);
        // Saved again, the tracks are written and the old keys are gone.
        let text = prefs.serialize(DEFAULT);
        assert!(!text.contains("desktop_muted") && !text.contains("mic_muted"));
        assert_eq!(defaults().parse(&text), prefs);
        // The old keys no longer apply once tracks are saved.
        assert!(!defaults().parse(&format!("{text}mic_muted=true\n")).tracks[1].muted);
    }

    #[test]
    fn an_empty_track_list_stays_empty() {
        let prefs = Prefs {
            tracks: Vec::new(),
            ..defaults()
        };
        assert_eq!(defaults().parse(&prefs.serialize(DEFAULT)).tracks, []);
    }

    #[test]
    fn bad_tracks_are_ignored() {
        let text = "track=true|false|0|desktop|Desktop\n\
                    track=yes|false|0|desktop|A\n\
                    track=true|false|loud|desktop|B\n\
                    track=true|false|NaN|desktop|C\n\
                    track=true|false|0|speaker|D\n\
                    track=true|false|0|app:|E\n\
                    track=true|false|0|desktop|\n\
                    track=true|false|0|desktop| padded \n\
                    track=true|false|99|input:mic|Loud\n";
        let tracks = defaults().parse(text).tracks;
        let names: Vec<_> = tracks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Desktop", "Loud"]);
        // Out of range volumes are clamped.
        assert_eq!(tracks[1].volume_db, MAX_DB);
        // No more than MAX_TRACKS.
        let many = "track=true|false|0|desktop|D\n".repeat(MAX_TRACKS + 3);
        assert_eq!(defaults().parse(&many).tracks.len(), MAX_TRACKS);
        // A name that cannot be saved is left out.
        let prefs = Prefs {
            tracks: vec![Track::new("a\nfps=30", Source::Desktop)],
            ..defaults()
        };
        let back = defaults().parse(&prefs.serialize(DEFAULT));
        assert_eq!((back.tracks.len(), back.fps), (0, 60));
    }

    #[test]
    fn audio_source_ids_round_trip() {
        for source in [
            Source::Desktop,
            Source::Microphone,
            Source::Output("alsa_output.pci-0000_0c_00.4.analog-stereo".into()),
            Source::Input("easyeffects_source".into()),
            Source::App("Firefox:Nightly".into()),
        ] {
            assert_eq!(
                audio_source_from_id(&audio_source_id(&source)),
                Some(source)
            );
        }
        assert_eq!(audio_source_from_id("app:a|b"), None);
        assert_eq!(audio_source_from_id("output:"), None);
        assert_eq!(audio_source_from_id("Desktop"), None);
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
