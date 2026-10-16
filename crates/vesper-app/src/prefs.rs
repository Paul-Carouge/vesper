//! User settings kept across launches, as `key=value` lines: the app's own
//! in `$XDG_CONFIG_HOME/vesper/settings` (with the current profile), the
//! recording settings in the current profile's file (see `profiles`). Both
//! go through [`Prefs::parse`]. Unknown keys, missing keys and bad values are
//! ignored: those settings keep their defaults. Written atomically.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vesper_audio::{MAX_DB, MIN_DB, Source};

use crate::outputs::{Kind, SourceLabel};
use crate::recorder::{MAX_FILTERS, MAX_TRACKS, Track, TrackFilter};
use crate::stream::{Service, StreamSettings};
use crate::{AppWindow, LiveService, QualityPreset, SourceKind, VideoCodec};

/// Process names (`/proc/<pid>/comm`) the user allowed game capture for. Not
/// UI state, so kept here for `Prefs::from_ui`.
pub static ALLOWED_GAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The one-time "still running in the tray" notice was shown.
pub static TRAY_NOTICE_SHOWN: AtomicBool = AtomicBool::new(false);

/// Choices offered by the UI; anything else in the file is ignored.
const FPS: [i32; 4] = [30, 60, 120, 144];
const REPLAY_SECONDS: [i32; 3] = [30, 60, 120];
const LIVE_HEIGHTS: [i32; 3] = [0, 720, 1080];
const LIVE_FPS: [i32; 2] = [30, 60];
/// Stream bitrates offered, kbit/s: the slider's range and step.
const LIVE_KBPS: (u64, u64, u64) = (1000, 20_000, 500);

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
    /// Output device the Desktop audio tracks record; `None`: the default.
    pub desktop_device: Option<Device>,
    /// Input device the Microphone tracks record; `None`: the default.
    pub mic_device: Option<Device>,
    pub output_folder: String,
    /// Record a game (Vulkan layer) instead of a portal source.
    pub game_source: bool,
    pub allowed_games: Vec<String>,
    pub tray_notice_shown: bool,
    /// Live stream settings; the key is never here (see `secret`).
    pub live: StreamSettings,
    /// The current profile's file name (see `profiles`); in `settings`.
    pub profile: String,
    /// The current profile's name; in its file.
    pub profile_name: String,
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
            desktop_device: Device::from_choice(&ui.get_desktop_device()),
            mic_device: Device::from_choice(&ui.get_mic_device()),
            output_folder: ui.get_output_folder().into(),
            game_source: ui.get_source_kind() == SourceKind::Game,
            allowed_games: ALLOWED_GAMES.lock().unwrap().clone(),
            tray_notice_shown: TRAY_NOTICE_SHOWN.load(Ordering::Relaxed),
            live: crate::live::settings(ui),
            profile: ui.get_profile_slug().into(),
            profile_name: ui.get_profile_name().into(),
        }
    }

    pub fn apply(&self, ui: &AppWindow) {
        ui.set_codec(self.codec);
        ui.set_quality(self.quality);
        ui.set_fps(self.fps);
        ui.set_replay_enabled(self.replay_enabled);
        ui.set_replay_seconds(self.replay_seconds);
        // Before the tracks, which show them.
        ui.set_desktop_device(Device::choice(self.desktop_device.as_ref()));
        ui.set_mic_device(Device::choice(self.mic_device.as_ref()));
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
        let live = &self.live;
        ui.set_live_service(match live.service {
            Service::Twitch => LiveService::Twitch,
            Service::YouTube => LiveService::Youtube,
            Service::Custom => LiveService::Custom,
        });
        ui.set_live_url(live.custom_url.as_str().into());
        ui.set_live_url_warning(crate::stream::cleartext_warning(&live.custom_url).into());
        ui.set_live_height(live.height.map_or(0, |h| h as i32));
        ui.set_live_fps(live.fps as i32);
        ui.set_live_bitrate(live.bitrate as f32 / 1e6);
        ui.set_live_codec(match live.codec {
            vesper_core::Codec::Av1 => VideoCodec::Av1,
            vesper_core::Codec::Hevc => VideoCodec::Hevc,
            vesper_core::Codec::H264 => VideoCodec::H264,
        });
        ui.set_profile_slug(self.profile.as_str().into());
        ui.set_profile_name(self.profile_name.as_str().into());
    }

    /// `self` with the app's own settings of `app`: the ones no profile has.
    pub fn with_app_settings(self, app: &Self) -> Self {
        Self {
            allowed_games: app.allowed_games.clone(),
            tray_notice_shown: app.tray_notice_shown,
            ..self
        }
    }

    /// `self` with the valid settings of `text` applied.
    ///
    /// Audio tracks: any `tracks` or `track` line replaces the list. Without
    /// them (settings of a version with fixed desktop and microphone tracks),
    /// the old `desktop_muted` and `mic_muted` apply to `self`'s tracks.
    pub fn parse(mut self, text: &str) -> Self {
        let mut tracks: Option<Vec<Track>> = None;
        // The track the next `filter` lines belong to: the last one read, if valid.
        let mut last_track: Option<usize> = None;
        let (mut desktop_muted, mut mic_muted) = (None, None);
        // Duplicate check in O(1): a huge allow list must not stall startup.
        let mut allowed: HashSet<String> = self.allowed_games.iter().cloned().collect();
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
                    last_track = None;
                }
                "track" => {
                    let list = tracks.get_or_insert_default();
                    last_track = None;
                    if let Some(track) = parse_track(value)
                        && list.len() < MAX_TRACKS
                    {
                        list.push(track);
                        last_track = Some(list.len() - 1);
                    }
                }
                "filter" => {
                    let track = last_track.and_then(|i| tracks.as_mut()?.get_mut(i));
                    if let Some(track) = track
                        && let Some(filter) = parse_filter(value)
                        && track.filters.len() < MAX_FILTERS
                    {
                        track.filters.push(filter);
                    }
                }
                "audio_mix" => set(&mut self.audio_mix, flag(value)),
                "desktop_device" => set(&mut self.desktop_device, Device::parse(value)),
                "mic_device" => set(&mut self.mic_device, Device::parse(value)),
                "output_folder" if Path::new(value).is_absolute() => {
                    self.output_folder = value.to_owned();
                }
                "game_source" => set(&mut self.game_source, flag(value)),
                "allow_game" if valid_game(value) && allowed.insert(value.to_owned()) => {
                    self.allowed_games.push(value.to_owned());
                }
                "tray_notice_shown" => set(&mut self.tray_notice_shown, flag(value)),
                "live_service" => set(&mut self.live.service, service(value)),
                "live_url" if one_line(value) => self.live.custom_url = value.trim().to_owned(),
                "live_height" => {
                    if let Some(h) = choice(value, &LIVE_HEIGHTS) {
                        self.live.height = (h > 0).then_some(h as u32);
                    }
                }
                "live_fps" => {
                    if let Some(fps) = choice(value, &LIVE_FPS) {
                        self.live.fps = fps as u32;
                    }
                }
                "live_kbps" => {
                    let (min, max, step) = LIVE_KBPS;
                    let kbps = value.trim().parse::<u64>().ok();
                    if let Some(kbps) = kbps.filter(|k| (min..=max).contains(k) && k % step == 0) {
                        self.live.bitrate = kbps * 1000;
                    }
                }
                "live_codec" => set(&mut self.live.codec, codec(value).map(Into::into)),
                "profile" if crate::profiles::valid_slug(value) => self.profile = value.to_owned(),
                "name" if valid_track_name(value) => self.profile_name = value.to_owned(),
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

    /// The `settings` file: the app's own settings and the current profile.
    pub fn serialize_settings(&self) -> String {
        let mut text = "# Vesper settings, rewritten on every change.\n".to_owned();
        if crate::profiles::valid_slug(&self.profile) {
            text += &format!("profile={}\n", self.profile);
        }
        text += &format!("tray_notice_shown={}\n", self.tray_notice_shown);
        for game in self.allowed_games.iter().filter(|g| valid_game(g)) {
            text += &format!("allow_game={game}\n");
        }
        text
    }

    /// A profile's file: its name and the recording settings.
    ///
    /// The folder is left out while it is `default_folder`, so a later
    /// default (e.g. a moved `XDG_VIDEOS_DIR`) still applies.
    pub fn serialize_profile(&self, default_folder: &str) -> String {
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
        let mut text = "# Vesper profile, rewritten on every change.\n".to_owned();
        if valid_track_name(&self.profile_name) {
            text += &format!("name={}\n", self.profile_name);
        }
        text += &format!(
            "codec={codec}\nquality={quality}\nfps={}\nreplay={}\nreplay_seconds={}\n",
            self.fps, self.replay_enabled, self.replay_seconds
        );
        // Marks the list as saved, even empty.
        text += "tracks=\n";
        for track in self.tracks.iter().filter(|t| valid_track(t)) {
            text += &format!("track={}\n", serialize_track(track));
            for filter in &track.filters {
                text += &format!("filter={}|{}\n", filter.enabled, filter.spec);
            }
        }
        text += &format!("audio_mix={}\n", self.audio_mix);
        // Empty: the default device.
        for (key, device) in [
            ("desktop_device", &self.desktop_device),
            ("mic_device", &self.mic_device),
        ] {
            let value = device.as_ref().map_or(String::new(), Device::serialize);
            text += &format!("{key}={value}\n");
        }
        // A line break would split the line: keep the default folder instead.
        if self.output_folder != default_folder && !self.output_folder.contains(['\n', '\r']) {
            text += &format!("output_folder={}\n", self.output_folder);
        }
        text += &format!("game_source={}\n", self.game_source);
        let live = &self.live;
        let service = match live.service {
            Service::Twitch => "twitch",
            Service::YouTube => "youtube",
            Service::Custom => "custom",
        };
        let codec = match live.codec {
            vesper_core::Codec::Av1 => "av1",
            vesper_core::Codec::Hevc => "hevc",
            vesper_core::Codec::H264 => "h264",
        };
        text += &format!(
            "live_service={service}\nlive_height={}\nlive_fps={}\nlive_kbps={}\nlive_codec={codec}\n",
            live.height.unwrap_or(0),
            live.fps,
            live.bitrate / 1000
        );
        // Never a key pasted into the URL.
        let (url, _) = crate::stream::split_secret(&live.custom_url);
        if one_line(&url) {
            text += &format!("live_url={url}\n");
        }
        text
    }

    /// `defaults` with the saved settings applied: the app's own from
    /// `settings`, the recording ones from the current profile (see
    /// [`crate::profiles::open`]). Missing files keep the defaults. A file
    /// that cannot be read keeps them too, and turns saving off (see
    /// [`SAVING_OFF`]): the user's settings are never overwritten with them.
    pub fn load(defaults: Self) -> Self {
        let (Some(path), Some(dir)) = (path(), crate::profiles::dir()) else {
            return defaults;
        };
        match crate::profiles::open(&path, &dir, defaults.clone()) {
            Ok(prefs) => prefs,
            Err(e) => {
                log!("{e}: settings are not saved until Vesper restarts");
                SAVING_OFF.store(true, Ordering::Relaxed);
                defaults
            }
        }
    }

    /// `self` with the settings of file `path` applied; unchanged when the
    /// file does not exist. An error for a file that cannot be read.
    pub fn try_read(self, path: &Path) -> std::io::Result<Self> {
        match read_capped(path) {
            Ok(bytes) => Ok(self.parse(&String::from_utf8_lossy(&bytes))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(self),
            Err(e) => Err(std::io::Error::new(
                e.kind(),
                format!("could not read {}: {e}", path.display()),
            )),
        }
    }

    pub fn save(&self) {
        if SAVING_OFF.load(Ordering::Relaxed) {
            return;
        }
        let (Some(path), Some(dir)) = (path(), crate::profiles::dir()) else {
            return;
        };
        let default_folder = crate::default_output_folder().unwrap_or_default();
        if let Err(e) = self.save_to(&path, &dir, &default_folder.to_string_lossy(), false) {
            log!("{e}");
        }
    }

    /// Writes the current profile's file into `profiles`, then `settings`:
    /// the settings never name a profile that was not written. `durable`:
    /// each file and its folder reach the disk before the next is written.
    pub fn save_to(
        &self,
        settings: &Path,
        profiles: &Path,
        default_folder: &str,
        durable: bool,
    ) -> std::io::Result<()> {
        let mut files = Vec::with_capacity(2);
        if crate::profiles::valid_slug(&self.profile) {
            let file = crate::profiles::file(profiles, &self.profile);
            files.push((file, self.serialize_profile(default_folder)));
        }
        files.push((settings.to_owned(), self.serialize_settings()));
        for (path, text) in files {
            write_file(&path, text.as_bytes(), durable).map_err(|e| {
                std::io::Error::new(e.kind(), format!("could not save {}: {e}", path.display()))
            })?;
        }
        Ok(())
    }
}

/// The settings of the version before profiles, as they were (see
/// `profiles::backup_path`); `None` without a readable copy.
pub fn legacy_settings() -> Option<Prefs> {
    let backup = crate::profiles::backup_path(&path()?);
    backup.is_file().then_some(())?;
    Prefs::default()
        .try_read(&backup)
        .inspect_err(|e| log!("{e}"))
        .ok()
}

/// The settings could not be read at startup: nothing is saved, so the
/// defaults shown never replace them.
pub static SAVING_OFF: AtomicBool = AtomicBool::new(false);

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

fn service(value: &str) -> Option<Service> {
    match value.trim() {
        "twitch" => Some(Service::Twitch),
        "youtube" => Some(Service::YouTube),
        "custom" => Some(Service::Custom),
        _ => None,
    }
}

/// A value that stays on its line.
fn one_line(value: &str) -> bool {
    !value.chars().any(char::is_control)
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

/// A device chosen in the settings for the Desktop audio or Microphone
/// tracks: its `node.name`, and its label to show while it is unplugged.
#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub name: String,
    pub label: String,
}

impl Device {
    /// A device as the UI holds it; `None` for the default.
    pub fn from_choice(choice: &crate::DeviceChoice) -> Option<Self> {
        Self::new(&choice.name, &choice.label)
    }

    pub fn choice(device: Option<&Self>) -> crate::DeviceChoice {
        crate::DeviceChoice {
            name: device.map_or("", |d| &d.name).into(),
            label: device.map_or("", |d| &d.label).into(),
            connected: true,
        }
    }

    /// `None` unless `name` is one the settings can store. The label is
    /// kept to one line of at most 128 characters, else is the name.
    pub fn new(name: &str, label: &str) -> Option<Self> {
        audio_source_from_id(&format!("output:{name}"))?;
        let label: String = label
            .chars()
            .filter(|c| !c.is_control())
            .take(128)
            .collect();
        let label = label.trim();
        Some(Self {
            name: name.to_owned(),
            label: if label.is_empty() { name } else { label }.to_owned(),
        })
    }

    /// `node.name|label`, the label last: it may hold a `|`.
    fn serialize(&self) -> String {
        format!("{}|{}", self.name, self.label)
    }

    /// `Some(None)` for an empty value (the default), `None` if invalid.
    fn parse(value: &str) -> Option<Option<Self>> {
        if value.is_empty() {
            return Some(None);
        }
        let (name, label) = value.split_once('|').unwrap_or((value, value));
        Self::new(name, label).map(Some)
    }
}

/// How a source is saved:`desktop`, `microphone`, `output:<node.name>`,
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
        filters: Vec::new(),
    })
}

/// `enabled|name:key=value,...`, after its track's line.
fn parse_filter(value: &str) -> Option<TrackFilter> {
    let (enabled, spec) = value.split_once('|')?;
    Some(TrackFilter {
        enabled: flag(enabled)?,
        spec: spec.parse().ok()?,
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

/// Most of the settings file read; the rest is ignored. Real files are a few
/// KB: this only bounds a corrupt or hostile one.
const MAX_SETTINGS_SIZE: u64 = 1 << 20;

/// `path`'s first [`MAX_SETTINGS_SIZE`] bytes, cut after the last full line.
fn read_capped(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_SETTINGS_SIZE + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SETTINGS_SIZE {
        log!(
            "{} is larger than {MAX_SETTINGS_SIZE} bytes: the rest is ignored",
            path.display()
        );
        bytes.truncate(MAX_SETTINGS_SIZE as usize);
        let end = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        bytes.truncate(end);
    }
    Ok(bytes)
}

/// Writes a temporary file next to `path`, then renames it over `path`: a
/// process crash leaves either the old or the new file, never half of one.
/// No fsync: it would block the UI thread, and settings are cheap to lose.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_file(path, bytes, false)
}

/// [`write_atomically`]; `durable`: the file, then its folder (the rename),
/// reach the disk before it returns. For one-time changes not to be lost.
pub fn write_file(path: &Path, bytes: &[u8], durable: bool) -> std::io::Result<()> {
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().ok_or(std::io::ErrorKind::NotFound)?;
    std::fs::create_dir_all(dir)?;
    // One temporary file per writer (threads, a second instance), so a
    // writer never renames another one's half-written file.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let n = WRITES.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{}.{n}.{nanos:x}", std::process::id()));
    // Private (0600): the settings name devices and folders, and the
    // stream key fallback goes through here too.
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            if durable {
                file.sync_all()?;
            }
            Ok(())
        })
        .and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    if durable {
        sync_dir(dir)?;
    }
    Ok(())
}

/// Makes the files created or renamed in `dir` durable.
pub fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use vesper_dsp::FilterSpec;

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
            desktop_device: None,
            mic_device: None,
            output_folder: DEFAULT.into(),
            game_source: false,
            allowed_games: Vec::new(),
            tray_notice_shown: false,
            live: StreamSettings::default(),
            profile: String::new(),
            profile_name: String::new(),
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
                Track {
                    filters: FilterSpec::voice_chain()
                        .into_iter()
                        .enumerate()
                        .map(|(i, spec)| TrackFilter {
                            enabled: i != 2,
                            spec,
                        })
                        .collect(),
                    ..Track::new("Mic", Source::Input("alsa_input.pci-0000_0c_00.6".into()))
                },
                Track {
                    filters: vec![TrackFilter {
                        enabled: true,
                        spec: FilterSpec::Gain { db: -3.25 },
                    }],
                    muted: true,
                    ..Track::new("Desktop", Source::Desktop)
                },
            ],
            audio_mix: false,
            desktop_device: Device::new("alsa_output.usb-DAC.analog-stereo", "Speakers | USB DAC"),
            mic_device: Device::new("alsa_input.usb-mic", "USB Microphone"),
            output_folder: "/mnt/clips/my folder = 2".into(),
            game_source: true,
            allowed_games: vec!["Game.exe".into(), "a=b c".into()],
            tray_notice_shown: true,
            live: StreamSettings {
                service: Service::Custom,
                custom_url: "srt://host:9000?latency=200000".into(),
                height: None,
                fps: 30,
                bitrate: 4_500_000,
                codec: vesper_core::Codec::Av1,
            },
            profile: "game-1440p".into(),
            profile_name: "Game — 1440p / AV1".into(),
        }
    }

    impl Prefs {
        /// Both files in one.
        fn serialize(&self, default_folder: &str) -> String {
            self.serialize_settings() + &self.serialize_profile(default_folder)
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
                // Unnamed: the current profile stays.
                profile: changed().profile,
                profile_name: changed().profile_name,
                ..defaults()
            }
        );
    }

    #[test]
    fn app_settings_and_profile_settings_go_to_their_file() {
        let prefs = changed();
        let settings = prefs.serialize_settings();
        let profile = prefs.serialize_profile(DEFAULT);
        assert!(settings.contains("profile=game-1440p\n") && settings.contains("allow_game="));
        assert!(!settings.contains("codec=") && !settings.contains("track="));
        assert!(profile.contains("name=Game — 1440p / AV1\n") && profile.contains("codec=h264"));
        assert!(!profile.contains("allow_game") && !profile.contains("tray_notice"));
        assert!(!profile.contains("profile="));
        // A profile file holds no app setting, even a stale one.
        let back = defaults().parse(&profile).with_app_settings(&defaults());
        assert_eq!(
            back,
            Prefs {
                allowed_games: Vec::new(),
                tray_notice_shown: false,
                profile: String::new(),
                ..changed()
            }
        );
    }

    #[test]
    fn audio_devices_are_saved_and_bad_ones_ignored() {
        let text = changed().serialize(DEFAULT);
        assert!(
            text.contains("desktop_device=alsa_output.usb-DAC.analog-stereo|Speakers | USB DAC\n")
        );
        // Settings from before device choices: the defaults.
        let old = "codec=av1\ntracks=\ntrack=true|false|0|desktop|Desktop\n";
        let prefs = changed().parse(old);
        assert_eq!(
            (prefs.desktop_device, prefs.mic_device),
            (changed().desktop_device, changed().mic_device)
        );
        let prefs = defaults().parse(old);
        assert_eq!((prefs.desktop_device, prefs.mic_device), (None, None));
        // An empty value is the default; a bad one changes nothing.
        let prefs = changed().parse("desktop_device=\nmic_device=a|b|c\n");
        assert_eq!(prefs.desktop_device, None);
        assert_eq!(prefs.mic_device, Device::new("a", "b|c"));
        for bad in ["x\u{1}y", "|label"] {
            let prefs = changed().parse(&format!("mic_device={bad}\n"));
            assert_eq!(prefs.mic_device, changed().mic_device);
        }
        // A missing or unprintable label is the name.
        assert_eq!(
            defaults().parse("mic_device=m\n").mic_device,
            Device::new("m", "m")
        );
        assert_eq!(Device::new("m", " \n ").unwrap().label, "m");
        assert_eq!(Device::new("m", &"x".repeat(300)).unwrap().label.len(), 128);
    }

    #[test]
    fn bad_live_settings_are_ignored() {
        let text = "live_service=kick\nlive_height=1440\nlive_fps=144\nlive_kbps=6200\n\
                    live_kbps=999999\nlive_codec=vp9\nlive_url=a\u{1}b\n";
        assert_eq!(defaults().parse(text), defaults());
        let prefs = Prefs {
            live: StreamSettings {
                custom_url: "rtmp://h/app/live_secret".into(),
                ..StreamSettings::default()
            },
            ..defaults()
        };
        let text = prefs.serialize(DEFAULT);
        assert!(text.contains("live_url=rtmp://h/app\n") && !text.contains("secret"));
        let prefs = defaults().parse("live_height=0\nlive_kbps=8000\n");
        assert_eq!(prefs.live.height, None);
        assert_eq!(prefs.live.bitrate, 8_000_000);
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
    fn filters_follow_their_track_and_bad_ones_are_ignored() {
        let text = "filter=true|gain:db=1\n\
                    tracks=\n\
                    filter=true|gain:db=2\n\
                    track=true|false|0|desktop|Desktop\n\
                    filter=true|gain:db=3\n\
                    filter=true|reverb:size=9\n\
                    filter=maybe|gain:db=4\n\
                    filter=gain:db=5\n\
                    filter=false|compressor:ratio=8,bogus=1\n\
                    filter=false|limiter : ceiling_db = -40\n\
                    track=true|false|loud|desktop|Bad\n\
                    filter=true|gain:db=6\n\
                    track=true|false|0|microphone|Mic\n\
                    filter=true|high_pass\n";
        let tracks = defaults().parse(text).tracks;
        assert_eq!(tracks.len(), 2);
        let limiter = FilterSpec::default_for("limiter").unwrap().with(0, -24.0);
        assert_eq!(
            tracks[0].filters,
            [
                TrackFilter {
                    enabled: true,
                    spec: FilterSpec::Gain { db: 3.0 },
                },
                // Clamped to the ceiling's range.
                TrackFilter {
                    enabled: false,
                    spec: limiter,
                },
            ]
        );
        assert_eq!(
            tracks[1].filters[0].spec,
            FilterSpec::HighPass { freq_hz: 80.0 }
        );
        // No more than MAX_FILTERS per track.
        let many = format!(
            "track=true|false|0|desktop|D\n{}",
            "filter=true|gain\n".repeat(MAX_FILTERS + 2)
        );
        assert_eq!(defaults().parse(&many).tracks[0].filters.len(), MAX_FILTERS);
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
    // --- robustness (audit) ---

    #[test]
    fn hostile_files_never_panic_and_keep_defaults() {
        let base = defaults();
        let mut junk: Vec<String> = vec![
            String::new(),
            "\0\0\0".into(),
            "=".into(),
            "====\n====\n".into(),
            "fps=\nreplay_seconds=\n".into(),
            "track=|||||\ntrack=true|true|NaN|desktop|x\ntrack=true|true|inf|desktop|x\n".into(),
            "track=true|true|-1e999|desktop|x\n".into(),
            "output_folder=\n".into(),
            "output_folder=relative/path\n".into(),
            "allow_game=\nallow_game= \nallow_game=\u{0}\n".into(),
            "\u{feff}fps=30\n".into(),
        ];
        // Binary garbage, as the lossy UTF-8 decoding in `load` would give it.
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        for len in [1usize, 16, 4096, 1 << 20] {
            let bytes: Vec<u8> = (0..len)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    x as u8
                })
                .collect();
            junk.push(String::from_utf8_lossy(&bytes).into_owned());
        }
        for text in &junk {
            let got = base.clone().parse(text);
            // Out-of-range or non-finite values never get through.
            assert!(FPS.contains(&got.fps));
            assert!(REPLAY_SECONDS.contains(&got.replay_seconds));
            assert!(got.tracks.len() <= MAX_TRACKS);
            for t in &got.tracks {
                assert!(t.volume_db.is_finite() && (MIN_DB..=MAX_DB).contains(&t.volume_db));
            }
            assert!(Path::new(&got.output_folder).is_absolute());
        }
    }

    #[test]
    fn a_huge_allow_list_does_not_stall_startup() {
        // 40k distinct names (a ~600 KB file) must parse in well under a second.
        // `parse` checks every new name against the whole list (quadratic).
        let text: String = (0..40_000).map(|i| format!("allow_game=g{i}\n")).collect();
        let t = std::time::Instant::now();
        let got = defaults().parse(&text);
        let took = t.elapsed();
        assert_eq!(got.allowed_games.len(), 40_000);
        assert!(
            took < std::time::Duration::from_secs(1),
            "parse took {took:?}"
        );
    }

    /// Settings are rewritten on every change from the UI thread, and a
    /// second instance (no session bus: the guard is off) or a stray `--record`
    /// can write at once. A temporary file shared by the writers would make
    /// a reader see a mix of them: truncated or interleaved content.
    #[test]
    fn concurrent_writers_never_tear_the_file() {
        let dir = std::env::temp_dir().join(format!("vesper-prefs-race-{}", std::process::id()));
        let path = dir.join("settings");
        write_atomically(&path, &[b'0'; 64 * 1024]).unwrap();
        let stop = std::sync::atomic::AtomicBool::new(false);
        let torn = std::thread::scope(|s| {
            for w in 1..=4u8 {
                let (path, stop) = (&path, &stop);
                s.spawn(move || {
                    let payload = vec![b'0' + w; 64 * 1024];
                    while !stop.load(Ordering::Relaxed) {
                        let _ = write_atomically(path, &payload);
                    }
                });
            }
            let mut torn = None;
            let end = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while std::time::Instant::now() < end && torn.is_none() {
                if let Ok(bytes) = std::fs::read(&path)
                    && (bytes.len() != 64 * 1024 || bytes.iter().any(|b| *b != bytes[0]))
                {
                    torn = Some(bytes.len());
                }
            }
            stop.store(true, Ordering::Relaxed);
            torn
        });
        std::fs::remove_dir_all(dir).unwrap();
        assert_eq!(torn, None, "a reader saw a torn settings file");
    }

    #[test]
    fn a_huge_settings_file_is_read_up_to_the_cap() {
        let dir = std::env::temp_dir().join(format!("vesper-prefs-big-{}", std::process::id()));
        let path = dir.join("settings");
        let line = "allow_game=some_game\n";
        let text = line.repeat(2 * MAX_SETTINGS_SIZE as usize / line.len());
        write_atomically(&path, format!("fps=30\n{text}").as_bytes()).unwrap();
        let bytes = read_capped(&path).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        assert!(bytes.len() as u64 <= MAX_SETTINGS_SIZE);
        // Cut after a full line: every line read is whole.
        assert!(bytes.ends_with(b"\n"));
        assert_eq!(defaults().parse(&String::from_utf8_lossy(&bytes)).fps, 30);
    }

    #[test]
    fn unwritable_settings_dir_reports_an_error() {
        // Not root-proof (root ignores modes): checks the error path only when it applies.
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("vesper-prefs-ro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = write_atomically(&dir.join("settings"), b"fps=30\n");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let denied = std::fs::File::create(dir.join("probe")).is_err();
        std::fs::remove_dir_all(&dir).unwrap();
        if denied {
            assert!(result.is_err());
        }
    }
}
