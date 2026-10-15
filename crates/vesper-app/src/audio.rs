//! The Audio card: tracks shown in and read back from the UI, live mute and
//! volume, the "Add track" source picker, and the level meters.
//!
//! The UI's track list is the truth: the pipeline gets a copy on start, then
//! live changes (mute, volume, name) or a new list (added, removed, enabled).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, VecModel};
use vesper_audio::{MAX_DB, MIN_DB, Node, NodeWatch, Source};

use crate::meter::{MeterState, visible_change};
use crate::prefs::{self, Prefs};
use crate::recorder::{MAX_TRACKS, Pipeline, Track};
use crate::{AppWindow, AudioTrack, METER_TICK, Shared, SourceOption, TrackKind};

/// Volume changes are saved once the slider rests this long.
const SAVE_DELAY: Duration = Duration::from_millis(400);

fn kind(source: &Source) -> TrackKind {
    match source {
        Source::Desktop => TrackKind::Desktop,
        Source::Microphone => TrackKind::Microphone,
        Source::Output(_) => TrackKind::Output,
        Source::Input(_) => TrackKind::Input,
        Source::App(_) => TrackKind::App,
    }
}

fn row(track: &Track) -> AudioTrack {
    AudioTrack {
        name: track.name.as_str().into(),
        source: prefs::audio_source_id(&track.source).into(),
        kind: kind(&track.source),
        enabled: track.enabled,
        muted: track.muted,
        volume_db: track.volume_db,
        filters: crate::filters::rows(&track.filters),
    }
}

/// The tracks as the UI shows them.
pub fn tracks_from_ui(ui: &AppWindow) -> Vec<Track> {
    ui.get_tracks()
        .iter()
        .filter_map(|t| {
            Some(Track {
                name: t.name.into(),
                source: prefs::audio_source_from_id(&t.source)?,
                enabled: t.enabled,
                muted: t.muted,
                volume_db: t.volume_db,
                filters: crate::filters::from_rows(&t.filters),
            })
        })
        .collect()
}

pub fn show_tracks(ui: &AppWindow, tracks: &[Track]) {
    let rows: Vec<_> = tracks.iter().map(row).collect();
    ui.set_tracks(ModelRc::new(VecModel::from(rows)));
    ui.set_max_tracks(MAX_TRACKS as i32);
}

/// A name the settings can keep: one line, at most 64 characters.
fn clean_name(name: &str) -> Option<String> {
    let name: String = name
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(64)
        .collect();
    let name = name.trim();
    prefs::valid_track_name(name).then(|| name.to_owned())
}

/// Volume as the slider sets it: 0.5 dB steps within the range.
fn snap_db(db: f32) -> f32 {
    ((db * 2.0).round() / 2.0).clamp(MIN_DB, MAX_DB)
}

/// The picker's choices: the defaults, then devices and playing apps.
fn options(nodes: &[Node], tracks: &[Track]) -> Vec<SourceOption> {
    let defaults = [
        Node {
            source: Source::Desktop,
            label: "Desktop audio".into(),
        },
        Node {
            source: Source::Microphone,
            label: "Microphone".into(),
        },
    ];
    let mut section = None;
    defaults
        .iter()
        .chain(nodes)
        // A name the settings cannot store (e.g. with a `|`) is left out.
        .filter(|node| prefs::audio_source_from_id(&prefs::audio_source_id(&node.source)).is_some())
        .map(|node| {
            let heading = match node.source {
                Source::Output(_) => "OUTPUT DEVICES",
                Source::Input(_) => "INPUT DEVICES",
                Source::App(_) => "APPLICATIONS",
                _ => "ADD A TRACK",
            };
            let first = section.replace(heading) != Some(heading);
            SourceOption {
                label: node.label.as_str().into(),
                source: prefs::audio_source_id(&node.source).into(),
                kind: kind(&node.source),
                section: if first { heading.into() } else { "".into() },
                added: tracks.iter().any(|t| t.source == node.source),
            }
        })
        .collect()
}

fn show_options(ui: &AppWindow, nodes: &[Node]) {
    let options = options(nodes, &tracks_from_ui(ui));
    ui.set_apps_playing(options.iter().any(|o| o.kind == TrackKind::App));
    ui.set_source_options(ModelRc::new(VecModel::from(options)));
}

/// Hands a new track list to the running pipeline, if any. It restarts its
/// audio once nothing records; a new pipeline reads the UI anyway.
fn apply_tracks(ui: &AppWindow, rec: &Shared) {
    if let Some(p) = rec.borrow().as_ref() {
        p.set_tracks(tracks_from_ui(ui), ui.get_audio_mix());
    }
}

/// Changes row `i` of the UI's tracks; false if there is none.
fn edit_row(ui: &AppWindow, i: i32, f: impl FnOnce(&mut AudioTrack)) -> Option<AudioTrack> {
    let model = ui.get_tracks();
    let i = usize::try_from(i).ok()?;
    let mut row = model.row_data(i)?;
    f(&mut row);
    // In place: re-creating the rows would end a slider drag.
    model.set_row_data(i, row.clone());
    Some(row)
}

pub fn wire(ui: &AppWindow, rec: &Shared) {
    let weak = ui.as_weak();
    let r = rec.clone();
    ui.on_track_mute_toggled(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(row) = edit_row(&ui, i, |t| t.muted = !t.muted) else {
            return;
        };
        if let Some(p) = r.borrow().as_ref() {
            p.set_muted(i as usize, row.muted);
        }
        Prefs::from_ui(&ui).save();
    });

    let weak = ui.as_weak();
    let r = rec.clone();
    let save = Rc::new(slint::Timer::default());
    ui.on_track_volume_changed(move |i, db| {
        let Some(ui) = weak.upgrade() else { return };
        let db = snap_db(db);
        let Some(_) = edit_row(&ui, i, |t| t.volume_db = db) else {
            return;
        };
        if let Some(p) = r.borrow().as_ref() {
            p.set_volume(i as usize, db);
        }
        let weak = weak.clone();
        save.start(slint::TimerMode::SingleShot, SAVE_DELAY, move || {
            if let Some(ui) = weak.upgrade() {
                Prefs::from_ui(&ui).save();
            }
        });
    });

    let weak = ui.as_weak();
    let r = rec.clone();
    ui.on_track_renamed(move |i, name| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(name) = clean_name(&name) else {
            return;
        };
        if edit_row(&ui, i, |t| t.name = name.as_str().into()).is_none() {
            return;
        }
        if let Some(p) = r.borrow().as_ref() {
            p.set_name(i as usize, &name);
        }
        Prefs::from_ui(&ui).save();
    });

    let weak = ui.as_weak();
    let r = rec.clone();
    ui.on_track_enabled_changed(move |i, on| {
        let Some(ui) = weak.upgrade() else { return };
        if edit_row(&ui, i, |t| t.enabled = on).is_some() {
            Prefs::from_ui(&ui).save();
            apply_tracks(&ui, &r);
        }
    });

    let weak = ui.as_weak();
    let r = rec.clone();
    ui.on_track_removed(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        let mut tracks = tracks_from_ui(&ui);
        if usize::try_from(i).is_ok_and(|i| i < tracks.len()) {
            tracks.remove(i as usize);
            show_tracks(&ui, &tracks);
            Prefs::from_ui(&ui).save();
            apply_tracks(&ui, &r);
        }
    });

    let weak = ui.as_weak();
    let r = rec.clone();
    ui.on_track_added(move |option| {
        let Some(ui) = weak.upgrade() else { return };
        let mut tracks = tracks_from_ui(&ui);
        let Some(source) = prefs::audio_source_from_id(&option.source) else {
            return;
        };
        if tracks.len() >= MAX_TRACKS || tracks.iter().any(|t| t.source == source) {
            return;
        }
        let name = clean_name(&option.label).unwrap_or_else(|| "Track".into());
        log!("track-added: {name} ({})", option.source);
        tracks.push(Track::new(&name, source));
        show_tracks(&ui, &tracks);
        Prefs::from_ui(&ui).save();
        apply_tracks(&ui, &r);
    });

    let weak = ui.as_weak();
    let r = rec.clone();
    ui.on_audio_mix_changed(move |_| {
        let Some(ui) = weak.upgrade() else { return };
        Prefs::from_ui(&ui).save();
        apply_tracks(&ui, &r);
    });

    // Devices and apps are only watched while the picker is open.
    let weak = ui.as_weak();
    let watch: RefCell<Option<NodeWatch>> = RefCell::new(None);
    ui.on_track_picker_changed(move |open| {
        let Some(ui) = weak.upgrade() else { return };
        if !open {
            drop(watch.borrow_mut().take());
            return;
        }
        show_options(&ui, &[]);
        let weak = weak.clone();
        let started = NodeWatch::start(move |nodes| {
            let _ = weak.upgrade_in_event_loop(move |ui| show_options(&ui, &nodes));
        });
        match started {
            Ok(w) => *watch.borrow_mut() = Some(w),
            Err(e) => log!("could not list audio devices: {e}"),
        }
    });
}

/// Feeds the meters from the pipeline's peaks at ~30 Hz. The timer stops
/// once no audio runs and the meters rest, or the window is hidden;
/// `meters-wake` restarts it. Keep the timer.
pub fn start_meters(ui: &AppWindow, rec: &Shared) -> Rc<slint::Timer> {
    let tick = Rc::new(slint::Timer::default());
    let weak = ui.as_weak();
    let rec = rec.clone();
    let this = Rc::downgrade(&tick);
    let mut meters: Vec<MeterState> = Vec::new();
    let (levels, peaks) = (Rc::new(VecModel::default()), Rc::new(VecModel::default()));
    ui.set_track_levels(levels.clone().into());
    ui.set_track_peaks(peaks.clone().into());
    let reductions = Rc::new(VecModel::default());
    ui.set_filter_reductions(reductions.clone().into());
    tick.start(slint::TimerMode::Repeated, METER_TICK, move || {
        let Some(ui) = weak.upgrade() else { return };
        let n = ui.get_tracks().row_count();
        if meters.len() != n {
            meters = vec![MeterState::default(); n];
            levels.set_vec(vec![0.0; n]);
            peaks.set_vec(vec![0.0; n]);
        }
        crate::filters::feed_reductions(&ui, &rec, &reductions);
        let rec = rec.borrow();
        let audio = rec.as_ref().is_some_and(Pipeline::audio_running);
        for (i, meter) in meters.iter_mut().enumerate() {
            meter.feed(rec.as_ref().map_or(0.0, |r| r.peak(i)), METER_TICK);
            // Only visible steps redraw.
            if visible_change(levels.row_data(i).unwrap_or_default(), meter.level) {
                levels.set_row_data(i, meter.level);
            }
            if visible_change(peaks.row_data(i).unwrap_or_default(), meter.peak) {
                peaks.set_row_data(i, meter.peak);
            }
        }
        let rested = !audio
            && meters.iter().all(|m| m.level == 0.0 && m.peak == 0.0)
            && reductions.iter().all(|r| r == 0.0);
        if (rested || !ui.window().is_visible())
            && let Some(tick) = this.upgrade()
        {
            tick.stop();
        }
    });
    let t = Rc::downgrade(&tick);
    ui.on_meters_wake(move || {
        if let Some(t) = t.upgrade()
            && !t.running()
        {
            t.restart();
        }
    });
    tick
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned_to_one_line() {
        assert_eq!(
            clean_name("  Game\tvoice \n").as_deref(),
            Some("Game voice")
        );
        assert_eq!(clean_name(" \n "), None);
        assert_eq!(clean_name(&"x".repeat(100)).map(|n| n.len()), Some(64));
    }

    #[test]
    fn volume_snaps_to_half_db_in_range() {
        assert_eq!(snap_db(-3.26), -3.5);
        assert_eq!(snap_db(0.2), 0.0);
        assert_eq!(snap_db(-80.0), MIN_DB);
        assert_eq!(snap_db(20.0), MAX_DB);
    }

    #[test]
    fn picker_groups_sources_and_marks_added_ones() {
        let nodes = [
            Node {
                source: Source::Output("a".into()),
                label: "Headset".into(),
            },
            Node {
                source: Source::Output("b".into()),
                label: "Speakers".into(),
            },
            Node {
                source: Source::App("Firefox".into()),
                label: "Firefox".into(),
            },
            Node {
                source: Source::App("a|b".into()),
                label: "a|b".into(),
            },
        ];
        let tracks = [Track::new("Desktop", Source::Desktop)];
        let options = options(&nodes, &tracks);
        let sections: Vec<_> = options.iter().map(|o| o.section.as_str()).collect();
        assert_eq!(
            sections,
            ["ADD A TRACK", "", "OUTPUT DEVICES", "", "APPLICATIONS"]
        );
        let added: Vec<_> = options.iter().map(|o| o.added).collect();
        assert_eq!(added, [true, false, false, false, false]);
        assert_eq!(options[4].source, "app:Firefox");
    }
}
