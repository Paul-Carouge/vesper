//! Profiles: named sets of recording settings, switched in one click from
//! the window's header (e.g. "Game — 1440p144 AV1" and "Desktop — HEVC").
//!
//! Each is a file `$XDG_CONFIG_HOME/vesper/profiles/<slug>.conf` in the
//! settings' `key=value` format ([`Prefs::serialize_profile`], read back by
//! [`Prefs::parse`]): its name, source mode, video, replay, audio tracks
//! (with their filters), output folder and stream settings. The stream key
//! stays in the keyring; the app's own settings (autostart, game layer,
//! allowed games, shortcuts, theme) are no profile's. `settings` names the
//! current profile; settings of a version without profiles become the
//! "Default" profile.
//!
//! A VST3 filter is `vst3:id=<id>` with its state in a settings file of its
//! own (`vesper_vst::sidecar`): a duplicated profile gets copies under new
//! ids, so no two profiles share a plugin's state, and a deleted profile's
//! plugin files go with it.
//!
//! Switching applies the settings the way editing them would: audio and
//! replay changes go to the running pipeline, another source mode or video
//! encoding restarts the replay buffer. Locked while recording or streaming.

use std::path::{Path, PathBuf};

use slint::{ComponentHandle, ModelRc, VecModel};
use vesper_dsp::FilterSpec;

use crate::prefs::Prefs;
use crate::recorder::Track;
use crate::{AppWindow, Games, LiveState, ProfileEntry, RecState, Shared, flash_notice};

/// The profile settings of a version without profiles become.
const DEFAULT_SLUG: &str = "default";
const DEFAULT_NAME: &str = "Default";

/// Longest file name made from a profile name, before a `-2` suffix.
const MAX_SLUG: usize = 40;

/// A saved profile: its file name (without `.conf`) and its name.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub slug: String,
    pub name: String,
}

/// `$XDG_CONFIG_HOME/vesper/profiles`.
pub fn dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("vesper").join("profiles"))
}

pub fn file(dir: &Path, slug: &str) -> PathBuf {
    dir.join(format!("{slug}.conf"))
}

/// A profile's file name: 1 to 48 of `a-z`, `0-9` and `-`, a letter or
/// digit first. Never a path (`/`, `.`, `..`).
pub fn valid_slug(slug: &str) -> bool {
    (1..=48).contains(&slug.len())
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !slug.starts_with('-')
}

/// The file name for `name`: its ASCII letters and digits lowercased, any
/// other run as one `-`; "profile" when none is left (e.g. "..", "日本").
fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(MAX_SLUG);
    let out = out.trim_end_matches('-');
    if out.is_empty() { "profile" } else { out }.to_owned()
}

/// A file name for `name` no profile in `dir` has: `slug(name)`, else with
/// `-2`, `-3`...
fn free_slug(dir: &Path, name: &str) -> String {
    let base = slug(name);
    (1..)
        .map(|n| match n {
            1 => base.clone(),
            n => format!("{base}-{n}"),
        })
        .find(|s| !file(dir, s).exists())
        .expect("a free name")
}

/// The profiles in `dir`, by name. A file without a valid name is named
/// after its file.
pub fn list(dir: &Path) -> Vec<Entry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut list: Vec<Entry> = read
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let slug = name.to_str()?.strip_suffix(".conf")?;
            valid_slug(slug).then(|| slug.to_owned())
        })
        .map(|slug| {
            let name = Prefs::default().read(&file(dir, &slug)).profile_name;
            Entry {
                name: if name.is_empty() { slug.clone() } else { name },
                slug,
            }
        })
        .collect();
    list.sort_by_cached_key(|e| (e.name.to_lowercase(), e.slug.clone()));
    list
}

/// `name` as typed, cleaned to one line of at most 64 characters, if no
/// profile but `except` (a slug) has it already, in any case.
pub fn check_name(
    list: &[Entry],
    name: &str,
    except: Option<&str>,
) -> Result<String, &'static str> {
    let name = crate::audio::clean_name(name).ok_or("Enter a name")?;
    let lower = name.to_lowercase();
    let taken = list
        .iter()
        .any(|e| Some(e.slug.as_str()) != except && e.name.to_lowercase() == lower);
    if taken {
        return Err("Another profile has this name");
    }
    Ok(name)
}

/// Profile `entry` of `dir`: `defaults` with its settings applied.
pub fn load(dir: &Path, entry: &Entry, defaults: &Prefs) -> Prefs {
    Prefs {
        profile: entry.slug.clone(),
        profile_name: entry.name.clone(),
        ..defaults.clone().read(&file(dir, &entry.slug))
    }
}

/// The settings at startup: the app's own from `settings`, the recording
/// ones from the profile it names (else the first one), on `defaults`.
///
/// Without any profile, `settings` is from a version without profiles and
/// holds the recording settings too: they become the "Default" profile,
/// and `settings` keeps only the app's own.
pub fn open(settings: &Path, dir: &Path, defaults: Prefs) -> Prefs {
    let app = defaults.clone().read(settings);
    let profiles = list(dir);
    let current = profiles
        .iter()
        .find(|e| e.slug == app.profile)
        .or(profiles.first());
    if let Some(entry) = current {
        return load(dir, entry, &defaults).with_app_settings(&app);
    }
    let prefs = Prefs {
        profile: DEFAULT_SLUG.into(),
        profile_name: DEFAULT_NAME.into(),
        ..app
    };
    log!("settings moved into the {DEFAULT_NAME:?} profile");
    prefs.save_to(settings, dir, &defaults.output_folder);
    prefs
}

/// The VST3 plugin instances of `tracks`.
fn plugin_ids(tracks: &[Track]) -> impl Iterator<Item = u32> + '_ {
    tracks
        .iter()
        .flat_map(|t| &t.filters)
        .filter_map(|f| match f.spec {
            FilterSpec::Vst3 { id } => Some(id),
            _ => None,
        })
}

/// `tracks` with each VST3 filter on a copy of its plugin instance, made
/// by `copy` (its new id). A filter whose plugin cannot be copied is left
/// out rather than shared.
fn copy_plugins(tracks: &mut [Track], mut copy: impl FnMut(u32) -> Option<u32>) {
    for track in tracks {
        track.filters.retain_mut(|f| match f.spec {
            FilterSpec::Vst3 { id } => copy(id)
                .map(|id| f.spec = FilterSpec::Vst3 { id })
                .is_some(),
            _ => true,
        });
    }
}

/// Copies plugin instance `id`'s settings file in `dir` to a new id.
fn copy_plugin(dir: &Path, id: u32) -> Option<u32> {
    use vesper_vst::sidecar::{self, Sidecar};
    let copied = Sidecar::load(dir, id).and_then(|state| {
        let new = sidecar::next_id(dir)?;
        state.save(dir, new).map(|()| new)
    });
    copied
        .inspect_err(|e| log!("could not copy VST3 plugin {id}: {e}"))
        .ok()
}

/// What switching from `old` to `new` needs from the pipeline.
#[derive(Debug, PartialEq)]
enum Plan {
    /// Another source mode or video encoding: a new replay buffer.
    Restart { source_mode: bool },
    /// The running pipeline takes the replay settings, and the tracks if
    /// they changed.
    Update { tracks: bool },
}

fn plan(old: &Prefs, new: &Prefs) -> Plan {
    let source_mode = old.game_source != new.game_source;
    if source_mode || (old.codec, old.quality, old.fps) != (new.codec, new.quality, new.fps) {
        return Plan::Restart { source_mode };
    }
    let audio = |p: &Prefs| {
        (
            p.tracks.clone(),
            p.audio_mix,
            p.desktop_device.clone(),
            p.mic_device.clone(),
        )
    };
    Plan::Update {
        tracks: audio(old) != audio(new),
    }
}

/// Switching is refused while a recording or a stream runs: their settings
/// cannot change under them.
fn locked(ui: &AppWindow) -> bool {
    let locked = ui.get_rec_state() != RecState::Idle || ui.get_live_state() != LiveState::Off;
    if locked {
        flash_notice(ui, "Profiles are locked while recording or streaming");
    }
    locked
}

fn show_list(ui: &AppWindow, dir: &Path) {
    let rows: Vec<_> = list(dir)
        .into_iter()
        .map(|e| ProfileEntry {
            slug: e.slug.into(),
            name: e.name.into(),
        })
        .collect();
    ui.set_profiles(ModelRc::new(VecModel::from(rows)));
}

/// Saves the current profile, then shows and applies `new` (saved too).
fn activate(ui: &AppWindow, rec: &Shared, games: &Games, dir: &Path, new: Prefs) {
    let old = Prefs::from_ui(ui);
    old.save();
    if let Some(r) = vesper_vst::global() {
        r.persist_all();
    }
    let new = new.with_app_settings(&old);
    log!("profile: {:?} ({})", new.profile_name, new.profile);
    new.apply(ui);
    crate::show_output_folder(ui);
    crate::update_replay_memory(ui);
    crate::audio::show_known_sources(ui);
    new.save();
    show_list(ui, dir);
    match plan(&old, &new) {
        Plan::Restart { source_mode } => {
            if let Some(p) = rec.borrow_mut().take() {
                p.set_replay(None);
            }
            if source_mode {
                crate::set_source_mode(ui, games, new.game_source);
            }
            crate::start_replay(ui, rec, games);
        }
        Plan::Update { tracks } => {
            crate::apply_replay(ui, rec, games);
            if tracks {
                crate::audio::apply_tracks(ui, rec);
            }
        }
    }
}

/// Shows `error` under the name field; true if there is none.
fn report(ui: &AppWindow, error: Option<&str>) -> bool {
    ui.set_profile_error(error.unwrap_or_default().into());
    error.is_none()
}

/// The profile menu. `defaults`: the settings of a new profile.
pub fn wire(ui: &AppWindow, rec: &Shared, games: &Games, defaults: Prefs) {
    let Some(dir) = dir() else {
        return;
    };
    show_list(ui, &dir);

    let (weak, d) = (ui.as_weak(), dir.clone());
    ui.on_profile_menu_opened(move || {
        if let Some(ui) = weak.upgrade() {
            report(&ui, None);
            show_list(&ui, &d);
        }
    });

    let (weak, r, g, d) = (ui.as_weak(), rec.clone(), games.clone(), dir.clone());
    let fresh = defaults.clone();
    ui.on_profile_switch(move |slug| {
        let Some(ui) = weak.upgrade() else { return };
        if slug == ui.get_profile_slug() || locked(&ui) {
            return;
        }
        match list(&d).into_iter().find(|e| e.slug == slug.as_str()) {
            Some(entry) => activate(&ui, &r, &g, &d, load(&d, &entry, &fresh)),
            None => {
                flash_notice(&ui, "That profile is gone");
                show_list(&ui, &d);
            }
        }
    });

    // New: the default settings. Duplicate: the current ones, with copies
    // of their plugins.
    for duplicate in [false, true] {
        let (weak, r, g, d) = (ui.as_weak(), rec.clone(), games.clone(), dir.clone());
        let fresh = defaults.clone();
        let made = move |name: slint::SharedString| {
            let Some(ui) = weak.upgrade() else {
                return false;
            };
            if locked(&ui) {
                return false;
            }
            let name = match check_name(&list(&d), &name, None) {
                Ok(name) => name,
                Err(e) => return report(&ui, Some(e)),
            };
            let mut new = if duplicate {
                let mut current = Prefs::from_ui(&ui);
                if let Some(r) = vesper_vst::global() {
                    r.persist_all();
                }
                let plugins = crate::filters::plugin_dir();
                copy_plugins(&mut current.tracks, |id| copy_plugin(plugins.as_ref()?, id));
                current
            } else {
                fresh.clone()
            };
            new.profile = free_slug(&d, &name);
            new.profile_name = name;
            log!("new profile {:?} ({})", new.profile_name, new.profile);
            activate(&ui, &r, &g, &d, new);
            report(&ui, None)
        };
        if duplicate {
            ui.on_profile_duplicate(made);
        } else {
            ui.on_profile_create(made);
        }
    }

    let (weak, d) = (ui.as_weak(), dir.clone());
    ui.on_profile_rename(move |name| {
        let Some(ui) = weak.upgrade() else {
            return false;
        };
        let slug = ui.get_profile_slug();
        match check_name(&list(&d), &name, Some(&slug)) {
            Ok(name) => {
                log!("profile {slug} renamed {name:?}");
                ui.set_profile_name(name.into());
                Prefs::from_ui(&ui).save();
                show_list(&ui, &d);
                report(&ui, None)
            }
            Err(e) => report(&ui, Some(e)),
        }
    });

    // The current profile goes; the first other one takes its place.
    let (weak, r, g, d) = (ui.as_weak(), rec.clone(), games.clone(), dir);
    ui.on_profile_delete(move || {
        let Some(ui) = weak.upgrade() else { return };
        if locked(&ui) {
            return;
        }
        let gone = Prefs::from_ui(&ui);
        let Some(next) = list(&d).into_iter().find(|e| e.slug != gone.profile) else {
            flash_notice(&ui, "The last profile cannot be deleted");
            return;
        };
        log!("delete profile {:?} ({})", gone.profile_name, gone.profile);
        activate(&ui, &r, &g, &d, load(&d, &next, &defaults));
        if let Err(e) = std::fs::remove_file(file(&d, &gone.profile)) {
            log!("could not delete profile {}: {e}", gone.profile);
        }
        // Its plugin instances were its own.
        if let Some(registry) = vesper_vst::global() {
            plugin_ids(&gone.tracks).for_each(|id| registry.remove(id));
        }
        show_list(&ui, &d);
        flash_notice(&ui, &format!("Deleted {}", gone.profile_name));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::TrackFilter;
    use crate::{QualityPreset, VideoCodec};
    use vesper_audio::Source;

    /// A fresh folder for one test.
    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("vesper-profiles-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn defaults() -> Prefs {
        Prefs {
            codec: VideoCodec::Av1,
            quality: QualityPreset::Balanced,
            fps: 60,
            replay_enabled: true,
            replay_seconds: 30,
            tracks: crate::prefs::default_tracks(),
            audio_mix: true,
            output_folder: "/home/u/Videos/Vesper".into(),
            ..Prefs::default()
        }
    }

    fn game() -> Prefs {
        Prefs {
            codec: VideoCodec::Av1,
            fps: 144,
            replay_seconds: 120,
            game_source: true,
            tracks: vec![Track {
                filters: vec![
                    TrackFilter {
                        enabled: true,
                        spec: FilterSpec::Vst3 { id: 3 },
                    },
                    TrackFilter {
                        enabled: true,
                        spec: FilterSpec::Gain { db: -2.0 },
                    },
                ],
                ..Track::new("Mic", Source::Microphone)
            }],
            output_folder: "/mnt/clips".into(),
            profile: "game".into(),
            profile_name: "Game — 1440p144 AV1".into(),
            ..defaults()
        }
    }

    #[test]
    fn a_profile_round_trips_through_its_file() {
        let root = temp("round-trip");
        let (settings, dir) = (root.join("settings"), root.join("profiles"));
        let prefs = Prefs {
            allowed_games: vec!["game.exe".into()],
            ..game()
        };
        prefs.save_to(&settings, &dir, &defaults().output_folder);
        let entry = Entry {
            slug: "game".into(),
            name: "Game — 1440p144 AV1".into(),
        };
        assert_eq!(list(&dir), std::slice::from_ref(&entry));
        // The app's own settings are not in the profile.
        assert_eq!(load(&dir, &entry, &defaults()), game());
        assert_eq!(open(&settings, &dir, defaults()), prefs);
        // Private files.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(file(&dir, "game"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn old_settings_become_the_default_profile() {
        let root = temp("migrate");
        let (settings, dir) = (root.join("settings"), root.join("profiles"));
        std::fs::write(
            &settings,
            "codec=hevc\nfps=30\nreplay_seconds=120\ntracks=\n\
             track=true|true|-3|desktop|Desktop\nfilter=true|gain:db=2\n\
             output_folder=/mnt/clips\ngame_source=false\ntray_notice_shown=true\n\
             allow_game=game.exe\nlive_service=youtube\n",
        )
        .unwrap();
        let prefs = open(&settings, &dir, defaults());
        assert_eq!(
            (prefs.codec, prefs.fps, prefs.replay_seconds),
            (VideoCodec::Hevc, 30, 120)
        );
        assert_eq!(prefs.tracks.len(), 1);
        assert!(prefs.tracks[0].muted && prefs.tracks[0].filters.len() == 1);
        assert_eq!(prefs.output_folder, "/mnt/clips");
        assert_eq!(
            (prefs.profile.as_str(), prefs.profile_name.as_str()),
            ("default", "Default")
        );
        assert!(prefs.tray_notice_shown && prefs.allowed_games == ["game.exe"]);
        // Split into both files, and read back the same.
        let text = std::fs::read_to_string(&settings).unwrap();
        assert!(text.contains("profile=default\n") && text.contains("allow_game=game.exe\n"));
        assert!(!text.contains("codec") && !text.contains("track="));
        assert_eq!(
            list(&dir),
            [Entry {
                slug: "default".into(),
                name: "Default".into()
            }]
        );
        assert_eq!(open(&settings, &dir, defaults()), prefs);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_missing_current_profile_falls_back_to_the_first() {
        let root = temp("fallback");
        let (settings, dir) = (root.join("settings"), root.join("profiles"));
        game().save_to(&settings, &dir, "/x");
        std::fs::write(&settings, "profile=gone\ncodec=h264\n").unwrap();
        let prefs = open(&settings, &dir, defaults());
        assert_eq!(prefs.profile, "game");
        // Stale recording settings in `settings` do not leak in.
        assert_eq!(prefs.codec, VideoCodec::Av1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn names_make_safe_distinct_file_names() {
        assert_eq!(slug("Game — 1440p144 AV1"), "game-1440p144-av1");
        assert_eq!(slug("../etc/passwd"), "etc-passwd");
        for name in ["..", ".", "/", "日本語", "🎮"] {
            assert_eq!(slug(name), "profile");
        }
        assert_eq!(slug("Jeu vidéo"), "jeu-vid-o");
        assert!(slug(&"a".repeat(100)).len() <= MAX_SLUG);
        for name in ["x", "Game / Desktop", "..", "日本語", &"é".repeat(64)] {
            assert!(valid_slug(&slug(name)), "{name}");
        }
        for bad in ["", ".", "..", "a/b", "-a", "A", "a.conf", &"a".repeat(49)] {
            assert!(!valid_slug(bad), "{bad}");
        }

        let dir = temp("slugs");
        assert_eq!(free_slug(&dir, ".."), "profile");
        std::fs::write(file(&dir, "profile"), "").unwrap();
        std::fs::write(file(&dir, "profile-2"), "").unwrap();
        assert_eq!(free_slug(&dir, "日本"), "profile-3");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn names_are_cleaned_and_unique_in_any_case() {
        let list = [
            Entry {
                slug: "game".into(),
                name: "Game".into(),
            },
            Entry {
                slug: "desk".into(),
                name: "Écran".into(),
            },
        ];
        assert_eq!(
            check_name(&list, "  Desktop\n60 ", None),
            Ok("Desktop 60".into())
        );
        assert_eq!(check_name(&list, " \t", None), Err("Enter a name"));
        assert!(check_name(&list, "GAME", None).is_err());
        assert!(check_name(&list, "écran", None).is_err());
        // Renaming a profile to its own name (another case) is fine.
        assert_eq!(check_name(&list, "game", Some("game")), Ok("game".into()));
        assert_eq!(check_name(&list, "../..", None), Ok("../..".into()));
        assert_eq!(
            check_name(&list, &"x".repeat(80), None).map(|n| n.len()),
            Ok(64)
        );
    }

    #[test]
    fn listing_skips_foreign_files_and_names_unnamed_ones() {
        let dir = temp("list");
        std::fs::write(file(&dir, "b"), "name=beta\n").unwrap();
        std::fs::write(file(&dir, "a"), "name=Alpha\n").unwrap();
        std::fs::write(file(&dir, "c"), "codec=av1\n").unwrap();
        std::fs::write(dir.join("Bad Name.conf"), "name=x\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "name=x\n").unwrap();
        std::fs::write(dir.join(".a.conf.tmp.1.2.3"), "name=x\n").unwrap();
        let names: Vec<_> = list(&dir).into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["Alpha", "beta", "c"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn duplicated_plugins_get_their_own_settings() {
        use vesper_vst::sidecar::{self, Sidecar};
        let dir = temp("plugins");
        let state = Sidecar {
            path: "/plugins/A.vst3".into(),
            cid: [7; 16],
            name: "A".into(),
            component: vec![1, 2, 3],
            controller: vec![4],
        };
        state.save(&dir, 3).unwrap();
        let mut tracks = game().tracks;
        tracks.push(Track {
            filters: vec![TrackFilter {
                enabled: false,
                // No settings file: left out rather than shared.
                spec: FilterSpec::Vst3 { id: 9 },
            }],
            ..Track::new("Desktop", Source::Desktop)
        });
        copy_plugins(&mut tracks, |id| copy_plugin(&dir, id));
        let ids: Vec<_> = plugin_ids(&tracks).collect();
        assert_eq!(ids.len(), 1);
        assert_ne!(ids[0], 3);
        assert_eq!(Sidecar::load(&dir, ids[0]).unwrap(), state);
        assert_eq!(tracks[0].filters[1].spec, FilterSpec::Gain { db: -2.0 });
        assert!(tracks[1].filters.is_empty());
        // The original stays, and the next one gets yet another id.
        assert!(sidecar::file(&dir, 3).exists());
        let again = copy_plugin(&dir, 3).unwrap();
        assert!(again != 3 && again != ids[0]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn switching_restarts_only_for_source_or_video() {
        let desktop = Prefs {
            codec: VideoCodec::Hevc,
            ..defaults()
        };
        assert_eq!(plan(&desktop, &game()), Plan::Restart { source_mode: true });
        let same_video = Prefs {
            replay_seconds: 120,
            output_folder: "/elsewhere".into(),
            ..desktop.clone()
        };
        assert_eq!(plan(&desktop, &same_video), Plan::Update { tracks: false });
        let muted = Prefs {
            tracks: vec![Track {
                muted: true,
                ..Track::new("Desktop", Source::Desktop)
            }],
            ..desktop.clone()
        };
        assert_eq!(plan(&desktop, &muted), Plan::Update { tracks: true });
        let av1 = Prefs {
            codec: VideoCodec::Av1,
            ..desktop.clone()
        };
        assert_eq!(plan(&desktop, &av1), Plan::Restart { source_mode: false });
    }
}
