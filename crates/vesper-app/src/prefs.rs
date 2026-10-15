//! User settings kept across launches in `$XDG_CONFIG_HOME/vesper/settings`,
//! as `key=value` lines. Unknown keys, missing keys and bad values are
//! ignored: those settings keep their defaults. Written atomically.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{AppWindow, QualityPreset, VideoCodec};

/// Choices offered by the UI; anything else in the file is ignored.
const FPS: [i32; 4] = [30, 60, 120, 144];
const REPLAY_SECONDS: [i32; 3] = [30, 60, 120];

#[derive(Clone, Debug, PartialEq)]
pub struct Prefs {
    pub codec: VideoCodec,
    pub quality: QualityPreset,
    pub fps: i32,
    pub replay_enabled: bool,
    pub replay_seconds: i32,
    pub desktop_muted: bool,
    pub mic_muted: bool,
    pub output_folder: String,
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
                _ => {}
            }
        }
        self
    }

    pub fn serialize(&self) -> String {
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
        if !self.output_folder.contains(['\n', '\r']) {
            text += &format!("output_folder={}\n", self.output_folder);
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
        if let Err(e) = write_atomically(&path, self.serialize().as_bytes()) {
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

/// Writes a temporary file next to `path`, then renames it over `path`: a
/// crash leaves either the old or the new file, never half of one.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().ok_or(std::io::ErrorKind::NotFound)?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Prefs {
        Prefs {
            codec: VideoCodec::Av1,
            quality: QualityPreset::Balanced,
            fps: 60,
            replay_enabled: true,
            replay_seconds: 30,
            desktop_muted: false,
            mic_muted: false,
            output_folder: "/home/u/Videos/Vesper".into(),
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
        }
    }

    #[test]
    fn round_trip() {
        for prefs in [defaults(), changed()] {
            assert_eq!(defaults().parse(&prefs.serialize()), prefs);
        }
        assert_eq!(changed().parse(&defaults().serialize()), defaults());
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
            changed().parse(&prefs.serialize()).output_folder,
            changed().output_folder
        );
        assert_eq!(defaults().parse(&prefs.serialize()).fps, 60);
    }

    #[test]
    fn write_replaces_the_file_atomically() {
        let dir = std::env::temp_dir().join(format!("vesper-prefs-{}", std::process::id()));
        let path = dir.join("settings");
        write_atomically(&path, b"fps=30\n").unwrap();
        write_atomically(&path, changed().serialize().as_bytes()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(defaults().parse(&text), changed());
        assert!(!path.with_extension("tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
