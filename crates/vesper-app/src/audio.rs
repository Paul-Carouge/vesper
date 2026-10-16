//! The Audio card: tracks shown in and read back from the UI, live mute and
//! volume, the source pickers ("Add track", a track's Source, the settings'
//! audio devices), and the level meters.
//!
//! The UI's track list is the truth: the pipeline gets a copy on start, then
//! live changes (mute, volume, name) or a new list (added, removed, enabled,
//! another source or device). Desktop audio and Microphone tracks record the
//! devices chosen in the settings: see [`recorded_tracks`].

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, VecModel};
use vesper_audio::{MAX_DB, MIN_DB, Node, NodeWatch, Source};

use crate::meter::{MeterState, visible_change};
use crate::prefs::{self, Device, Prefs};
use crate::recorder::{MAX_TRACKS, Pipeline, Track};
use crate::{AppWindow, AudioTrack, DeviceChoice, METER_TICK, Shared, SourceOption, TrackKind};

/// Devices and applications, from a [`NodeWatch`] that runs while the UI
/// shows a list of them, and once at startup for the tracks' device names.
#[derive(Default)]
struct Sources {
    watch: Option<NodeWatch>,
    /// The latest list; `None` until one came.
    nodes: Option<Vec<Node>>,
}

thread_local! {
    // UI thread only.
    static SOURCES: RefCell<Sources> = RefCell::default();
}

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

/// The source a Desktop audio (`mic` false) or Microphone track records.
pub fn chosen_source(choice: &DeviceChoice, mic: bool) -> Source {
    match (choice.name.is_empty(), mic) {
        (true, false) => Source::Desktop,
        (true, true) => Source::Microphone,
        (false, false) => Source::Output(choice.name.to_string()),
        (false, true) => Source::Input(choice.name.to_string()),
    }
}

/// What a track on `source` records: Desktop audio and Microphone on these.
fn resolved(source: &Source, desktop: &Source, mic: &Source) -> Source {
    match source {
        Source::Desktop => desktop.clone(),
        Source::Microphone => mic.clone(),
        other => other.clone(),
    }
}

/// `tracks` with the Desktop audio and Microphone ones on these sources.
pub fn resolve(mut tracks: Vec<Track>, desktop: &Source, mic: &Source) -> Vec<Track> {
    for track in &mut tracks {
        track.source = resolved(&track.source, desktop, mic);
    }
    tracks
}

/// What Desktop audio and Microphone tracks record now.
fn chosen_sources(ui: &AppWindow) -> (Source, Source) {
    (
        chosen_source(&ui.get_desktop_device(), false),
        chosen_source(&ui.get_mic_device(), true),
    )
}

/// Whether one of `tracks` records what a track on `source` would: one
/// track per node.
fn recorded(tracks: &[Track], source: &Source, desktop: &Source, mic: &Source) -> bool {
    let source = resolved(source, desktop, mic);
    tracks
        .iter()
        .any(|t| resolved(&t.source, desktop, mic) == source)
}

/// Whether choosing `device` for the Desktop audio (`mic` false) or
/// Microphone tracks would record it twice: such a track exists and
/// another track records `device` already.
fn device_taken(
    tracks: &[Track],
    mic: bool,
    device: &Source,
    desktop: &Source,
    mic_src: &Source,
) -> bool {
    let follows = if mic {
        Source::Microphone
    } else {
        Source::Desktop
    };
    tracks.iter().any(|t| t.source == follows)
        && tracks
            .iter()
            .any(|t| t.source != follows && &resolved(&t.source, desktop, mic_src) == device)
}

/// The tracks as they are recorded: Desktop audio and Microphone on the
/// devices chosen in the settings.
pub fn recorded_tracks(ui: &AppWindow) -> Vec<Track> {
    let (desktop, mic) = chosen_sources(ui);
    resolve(tracks_from_ui(ui), &desktop, &mic)
}

/// A device's label, and whether it is plugged in (true while unknown).
fn device_label(source: &Source, fallback: &str, nodes: Option<&[Node]>) -> (String, bool) {
    match nodes.map(|n| n.iter().find(|n| &n.source == source)) {
        Some(Some(node)) => (node.label.clone(), true),
        Some(None) => (fallback.to_owned(), false),
        None => (fallback.to_owned(), true),
    }
}

/// What a track's Source selector shows, and the device of its subtitle.
fn track_labels(
    source: &Source,
    desktop: &DeviceChoice,
    mic: &DeviceChoice,
    nodes: Option<&[Node]>,
) -> (String, String) {
    let unplugged = |label: String, connected: bool| {
        if connected {
            label
        } else {
            format!("{label} (disconnected)")
        }
    };
    let chosen = |choice: &DeviceChoice| {
        if choice.name.is_empty() {
            String::new()
        } else {
            unplugged(choice.label.to_string(), choice.connected)
        }
    };
    match source {
        Source::Desktop => ("Desktop audio".into(), chosen(desktop)),
        Source::Microphone => ("Microphone".into(), chosen(mic)),
        Source::Output(name) | Source::Input(name) => {
            let (label, connected) = device_label(source, name, nodes);
            let label = unplugged(label, connected);
            // Unnamed until listed: the subtitle shows the kind meanwhile.
            let device = if nodes.is_some() {
                label.clone()
            } else {
                String::new()
            };
            (label, device)
        }
        Source::App(app) => (app.clone(), String::new()),
    }
}

/// Track `track`'s row, its labels from the latest device list.
fn row(ui: &AppWindow, track: &Track) -> AudioTrack {
    let (source_label, device) = SOURCES.with(|s| {
        track_labels(
            &track.source,
            &ui.get_desktop_device(),
            &ui.get_mic_device(),
            s.borrow().nodes.as_deref(),
        )
    });
    AudioTrack {
        name: track.name.as_str().into(),
        source: prefs::audio_source_id(&track.source).into(),
        source_label: source_label.into(),
        device: device.into(),
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
    let rows: Vec<_> = tracks.iter().map(|t| row(ui, t)).collect();
    ui.set_tracks(ModelRc::new(VecModel::from(rows)));
    ui.set_max_tracks(MAX_TRACKS as i32);
}

/// A name the settings can keep: one line, at most 64 characters.
pub fn clean_name(name: &str) -> Option<String> {
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

/// A name the settings can store (not one with a `|`, say).
fn storable(source: &Source) -> bool {
    prefs::audio_source_from_id(&prefs::audio_source_id(source)).is_some()
}

/// Options of a picker, each group headed by `heading`'s name for it.
fn grouped<'a>(
    nodes: impl Iterator<Item = (&'a Node, String)>,
    heading: impl Fn(&Source) -> &'static str,
    added: impl Fn(&Source) -> bool,
) -> Vec<SourceOption> {
    let mut section = None;
    nodes
        .filter(|(node, _)| storable(&node.source))
        .map(|(node, detail)| {
            let heading = heading(&node.source);
            let first = section.replace(heading) != Some(heading);
            SourceOption {
                label: node.label.as_str().into(),
                source: prefs::audio_source_id(&node.source).into(),
                kind: kind(&node.source),
                section: if first { heading.into() } else { "".into() },
                added: added(&node.source),
                detail: detail.into(),
            }
        })
        .collect()
}

fn node(source: Source, label: &str) -> Node {
    Node {
        source,
        label: label.into(),
        default: false,
    }
}

/// "Default" for the system's default device.
fn default_mark(node: &Node) -> String {
    if node.default { "Default" } else { "" }.into()
}

/// The choices of "Add track" and of a track's Source: Desktop audio and
/// Microphone (with the device they record), then devices and playing apps.
fn options(
    nodes: &[Node],
    tracks: &[Track],
    desktop: &DeviceChoice,
    mic: &DeviceChoice,
) -> Vec<SourceOption> {
    let device = |choice: &DeviceChoice| match (choice.name.is_empty(), choice.connected) {
        (true, _) => "System default".into(),
        (false, true) => choice.label.to_string(),
        (false, false) => format!("{} (disconnected)", choice.label),
    };
    let defaults = [
        (node(Source::Desktop, "Desktop audio"), device(desktop)),
        (node(Source::Microphone, "Microphone"), device(mic)),
    ];
    let heading = |source: &Source| match source {
        Source::Output(_) => "OUTPUT DEVICES",
        Source::Input(_) => "INPUT DEVICES",
        Source::App(_) => "APPLICATIONS",
        _ => "DESKTOP AND MICROPHONE",
    };
    let listed = nodes.iter().map(|n| (n, default_mark(n)));
    let (desktop, mic) = (chosen_source(desktop, false), chosen_source(mic, true));
    grouped(
        defaults.iter().map(|(n, d)| (n, d.clone())).chain(listed),
        heading,
        |source| recorded(tracks, source, &desktop, &mic),
    )
}

/// The choices of the Desktop audio (`mic` false) or Microphone device: the
/// system default, then the devices, the chosen one listed even unplugged.
/// `taken`: choosing it would record a node twice.
fn device_options(
    nodes: &[Node],
    mic: bool,
    choice: &DeviceChoice,
    taken: impl Fn(&Source) -> bool,
) -> Vec<SourceOption> {
    let (default, heading) = if mic {
        (Source::Microphone, "INPUT DEVICES")
    } else {
        (Source::Desktop, "OUTPUT DEVICES")
    };
    let chosen = chosen_source(choice, mic);
    let devices = nodes.iter().filter(|n| {
        matches!(
            (&n.source, mic),
            (Source::Output(_), false) | (Source::Input(_), true)
        )
    });
    let missing = (chosen != default && !nodes.iter().any(|n| n.source == chosen))
        .then(|| node(chosen.clone(), &choice.label));
    let system = node(default, "System default");
    let list = std::iter::once((&system, String::new()))
        .chain(devices.map(|n| (n, default_mark(n))))
        .chain(missing.iter().map(|n| (n, "Disconnected".into())));
    grouped(
        list,
        |source| {
            if matches!(source, Source::Desktop | Source::Microphone) {
                ""
            } else {
                heading
            }
        },
        taken,
    )
}

/// `choice` as `nodes` show it: plugged in or not, with the device's
/// current label. True if the label changed (it is saved with the choice).
fn listed_choice(choice: &mut DeviceChoice, mic: bool, nodes: &[Node]) -> bool {
    let source = chosen_source(choice, mic);
    let node = nodes.iter().find(|n| n.source == source);
    choice.connected = choice.name.is_empty() || node.is_some();
    match node.and_then(|n| Device::new(&choice.name, &n.label)) {
        Some(device) if device.label != choice.label.as_str() => {
            choice.label = device.label.into();
            true
        }
        _ => false,
    }
}

/// Shows `nodes` in the pickers, the device selectors and the tracks.
fn show_sources(ui: &AppWindow, nodes: &[Node]) {
    let (mut desktop, mut mic) = (ui.get_desktop_device(), ui.get_mic_device());
    let relabeled =
        listed_choice(&mut desktop, false, nodes) | listed_choice(&mut mic, true, nodes);
    let model = |options: Vec<SourceOption>| ModelRc::new(VecModel::from(options));
    let tracks = tracks_from_ui(ui);
    let options = options(nodes, &tracks, &desktop, &mic);
    ui.set_apps_playing(options.iter().any(|o| o.kind == TrackKind::App));
    ui.set_source_options(model(options));
    let (desktop_src, mic_src) = (chosen_source(&desktop, false), chosen_source(&mic, true));
    for on_mic in [false, true] {
        let choice = if on_mic { &mic } else { &desktop };
        let list = device_options(nodes, on_mic, choice, |s| {
            device_taken(&tracks, on_mic, s, &desktop_src, &mic_src)
        });
        if on_mic {
            ui.set_mic_device_options(model(list));
        } else {
            ui.set_desktop_device_options(model(list));
        }
    }
    ui.set_desktop_device(desktop);
    ui.set_mic_device(mic);
    if relabeled {
        Prefs::from_ui(ui).save();
    }
    refresh_rows(ui);
}

/// Shows the latest device list again (after a track or device change), or
/// just the tracks before any list came.
pub fn show_known_sources(ui: &AppWindow) {
    match SOURCES.with(|s| s.borrow().nodes.clone()) {
        Some(nodes) => show_sources(ui, &nodes),
        None => refresh_rows(ui),
    }
}

/// Updates the tracks' source labels in place: re-creating the rows would
/// end a slider drag.
fn refresh_rows(ui: &AppWindow) {
    let model = ui.get_tracks();
    for i in 0..model.row_count() {
        let Some(mut old) = model.row_data(i) else {
            continue;
        };
        let Some(source) = prefs::audio_source_from_id(&old.source) else {
            continue;
        };
        let new = row(ui, &Track::new("", source));
        if (&new.source_label, &new.device) != (&old.source_label, &old.device) {
            (old.source_label, old.device) = (new.source_label, new.device);
            model.set_row_data(i, old);
        }
    }
}

/// Starts (`on`) or stops watching devices and applications.
fn watch_sources(ui: &AppWindow, on: bool) {
    let running = SOURCES.with(|s| s.borrow().watch.is_some());
    if !on {
        // Dropped outside the borrow: it joins the watch thread.
        let watch = SOURCES.with(|s| s.borrow_mut().watch.take());
        drop(watch);
        return;
    }
    if running {
        return;
    }
    show_known_sources(ui);
    let weak = ui.as_weak();
    let started = NodeWatch::start(move |nodes| {
        let _ = weak.upgrade_in_event_loop(move |ui| {
            SOURCES.with(|s| s.borrow_mut().nodes = Some(nodes.clone()));
            show_sources(&ui, &nodes);
            // The startup listing: once is enough.
            if !ui.get_watching_sources() {
                watch_sources(&ui, false);
            }
        });
    });
    match started {
        Ok(w) => SOURCES.with(|s| s.borrow_mut().watch = Some(w)),
        Err(e) => log!("could not list audio devices: {e}"),
    }
}

/// Hands a new track list to the running pipeline, if any. It restarts its
/// audio once nothing records; a new pipeline reads the UI anyway.
pub fn apply_tracks(ui: &AppWindow, rec: &Shared) {
    if let Some(p) = rec.borrow().as_ref() {
        p.set_tracks(recorded_tracks(ui), ui.get_audio_mix());
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
            // Its plugins' hosts stop and their settings files go.
            if let Some(track) = ui.get_tracks().row_data(i as usize) {
                crate::filters::forget_plugins(track.filters.iter());
            }
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
        let (desktop, mic) = chosen_sources(&ui);
        if tracks.len() >= MAX_TRACKS || recorded(&tracks, &source, &desktop, &mic) {
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

    let weak = ui.as_weak();
    let r = rec.clone();
    ui.on_track_source_changed(move |i, option| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(source) = prefs::audio_source_from_id(&option.source) else {
            return;
        };
        let mut tracks = tracks_from_ui(&ui);
        let Some(i) = usize::try_from(i).ok().filter(|&i| i < tracks.len()) else {
            return;
        };
        // One track per source, as "Add track" has it.
        let (desktop, mic) = chosen_sources(&ui);
        if recorded(&tracks, &source, &desktop, &mic) {
            return;
        }
        log!("track-source: {} ({})", tracks[i].name, option.source);
        tracks[i].source = source;
        show_tracks(&ui, &tracks);
        show_known_sources(&ui);
        Prefs::from_ui(&ui).save();
        apply_tracks(&ui, &r);
    });

    let weak = ui.as_weak();
    let r = rec.clone();
    ui.on_device_chosen(move |mic, option| {
        let Some(ui) = weak.upgrade() else { return };
        let device = match prefs::audio_source_from_id(&option.source) {
            Some(Source::Desktop) if !mic => None,
            Some(Source::Microphone) if mic => None,
            Some(Source::Output(name)) if !mic => Device::new(&name, &option.label),
            Some(Source::Input(name)) if mic => Device::new(&name, &option.label),
            _ => return,
        };
        let choice = Device::choice(device.as_ref());
        let old = if mic {
            ui.get_mic_device()
        } else {
            ui.get_desktop_device()
        };
        let (desktop, mic_src) = chosen_sources(&ui);
        let taken = device_taken(
            &tracks_from_ui(&ui),
            mic,
            &chosen_source(&choice, mic),
            &desktop,
            &mic_src,
        );
        if choice.name == old.name || taken {
            return;
        }
        log!("device-chosen: {}", option.source);
        if mic {
            ui.set_mic_device(choice);
        } else {
            ui.set_desktop_device(choice);
        }
        show_known_sources(&ui);
        Prefs::from_ui(&ui).save();
        // Only the tracks on that device restart.
        let follows = if mic {
            Source::Microphone
        } else {
            Source::Desktop
        };
        if tracks_from_ui(&ui).iter().any(|t| t.source == follows) {
            apply_tracks(&ui, &r);
        }
    });

    // Devices and apps are watched while a list shows them, and once now
    // for the tracks' device names.
    let weak = ui.as_weak();
    ui.on_sources_watch_changed(move |on| {
        if let Some(ui) = weak.upgrade() {
            watch_sources(&ui, on);
        }
    });
    watch_sources(ui, true);
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

    fn choice(name: &str, label: &str, connected: bool) -> DeviceChoice {
        DeviceChoice {
            name: name.into(),
            label: label.into(),
            connected,
        }
    }

    fn nodes() -> Vec<Node> {
        vec![
            node(Source::Output("a".into()), "Headset"),
            Node {
                default: true,
                ..node(Source::Output("b".into()), "Speakers")
            },
            node(Source::Input("m".into()), "USB Mic"),
            node(Source::App("Firefox".into()), "Firefox"),
            node(Source::App("a|b".into()), "a|b"),
        ]
    }

    fn column<T>(options: &[SourceOption], f: impl Fn(&SourceOption) -> T) -> Vec<T> {
        options.iter().map(f).collect()
    }

    #[test]
    fn picker_groups_sources_and_marks_added_ones() {
        let tracks = [Track::new("Desktop", Source::Desktop)];
        let default = choice("", "", true);
        let mic = choice("m", "USB Mic", true);
        let options = options(&nodes(), &tracks, &default, &mic);
        assert_eq!(
            column(&options, |o| o.section.to_string()),
            [
                "DESKTOP AND MICROPHONE",
                "",
                "OUTPUT DEVICES",
                "",
                "INPUT DEVICES",
                "APPLICATIONS"
            ]
        );
        assert_eq!(
            column(&options, |o| o.added),
            [true, false, false, false, false, false]
        );
        // Desktop and Microphone name their device, the default is marked.
        assert_eq!(
            column(&options, |o| o.detail.to_string()),
            ["System default", "USB Mic", "", "Default", "", ""]
        );
        assert_eq!(options[5].source, "app:Firefox");
    }

    #[test]
    fn device_selectors_list_their_kind_and_keep_an_unplugged_choice() {
        let desktop = device_options(&nodes(), false, &choice("", "", true), |_| false);
        assert_eq!(
            column(&desktop, |o| o.label.to_string()),
            ["System default", "Headset", "Speakers"]
        );
        assert_eq!(
            column(&desktop, |o| o.section.to_string()),
            ["", "OUTPUT DEVICES", ""]
        );
        assert_eq!(
            column(&desktop, |o| o.detail.to_string()),
            ["", "", "Default"]
        );
        assert_eq!(desktop[0].source, "desktop");

        let gone = choice("usb", "USB Interface", false);
        let mic = device_options(&nodes(), true, &gone, |_| false);
        assert_eq!(
            column(&mic, |o| (
                o.label.to_string(),
                o.source.to_string(),
                o.detail.to_string()
            )),
            [
                ("System default".into(), "microphone".into(), "".into()),
                ("USB Mic".into(), "input:m".into(), "".into()),
                (
                    "USB Interface".into(),
                    "input:usb".into(),
                    "Disconnected".into()
                ),
            ]
        );
    }

    #[test]
    fn a_node_is_recorded_by_one_track_only() {
        let dac = choice("b", "Speakers", true);
        let none = choice("", "", true);
        // Desktop audio on "b": "b" itself is taken, and the other way round.
        let tracks = [Track::new("Desktop", Source::Desktop)];
        let list = options(&nodes(), &tracks, &dac, &none);
        let added = |label: &str| list.iter().find(|o| o.label == label).unwrap().added;
        assert!(added("Desktop audio") && added("Speakers") && !added("Headset"));
        let tracks = [Track::new("Speakers", Source::Output("b".into()))];
        let list = super::options(&nodes(), &tracks, &dac, &none);
        assert!(list[0].added);

        // Choosing "b" for Desktop audio while a track records it: only with
        // a Desktop audio track would it be recorded twice.
        let b = Source::Output("b".into());
        let (desktop, mic) = (Source::Desktop, Source::Microphone);
        let mut tracks = vec![Track::new("Speakers", b.clone())];
        assert!(!device_taken(&tracks, false, &b, &desktop, &mic));
        tracks.push(Track::new("Desktop", Source::Desktop));
        assert!(device_taken(&tracks, false, &b, &desktop, &mic));
        assert!(!device_taken(
            &tracks,
            false,
            &Source::Output("a".into()),
            &desktop,
            &mic
        ));
        assert!(!device_taken(
            &tracks,
            true,
            &Source::Input("m".into()),
            &desktop,
            &mic
        ));
        let list = device_options(&nodes(), false, &none, |s| {
            device_taken(&tracks, false, s, &desktop, &mic)
        });
        assert_eq!(column(&list, |o| o.added), [false, false, true]);
    }

    #[test]
    fn a_chosen_device_takes_its_listed_label() {
        let mut stale = choice("b", "Old name", true);
        assert!(listed_choice(&mut stale, false, &nodes()));
        assert_eq!((stale.label.as_str(), stale.connected), ("Speakers", true));
        assert!(!listed_choice(&mut stale, false, &nodes()));
        let mut gone = choice("x", "Kept", true);
        assert!(!listed_choice(&mut gone, false, &nodes()));
        assert_eq!((gone.label.as_str(), gone.connected), ("Kept", false));
    }

    #[test]
    fn desktop_and_microphone_tracks_record_the_chosen_devices() {
        let tracks = vec![
            Track::new("Desktop", Source::Desktop),
            Track::new("Mic", Source::Microphone),
            Track::new("Game", Source::App("game".into())),
            Track::new("Headset", Source::Output("a".into())),
        ];
        let desktop = chosen_source(&choice("dac", "DAC", true), false);
        let mic = chosen_source(&choice("", "", true), true);
        let sources: Vec<_> = resolve(tracks.clone(), &desktop, &mic)
            .into_iter()
            .map(|t| t.source)
            .collect();
        assert_eq!(
            sources,
            [
                Source::Output("dac".into()),
                Source::Microphone,
                Source::App("game".into()),
                Source::Output("a".into()),
            ]
        );
        // The defaults change nothing.
        assert_eq!(
            resolve(tracks.clone(), &Source::Desktop, &Source::Microphone),
            tracks
        );
    }

    #[test]
    fn tracks_name_their_device() {
        let dac = choice("dac", "Speakers (USB DAC)", true);
        let none = choice("", "", true);
        let list = nodes();
        let labels = |source: Source, desktop: &DeviceChoice, nodes| {
            track_labels(&source, desktop, &none, nodes)
        };
        let pair = |a: &str, b: &str| (a.to_owned(), b.to_owned());
        assert_eq!(
            labels(Source::Desktop, &dac, None),
            pair("Desktop audio", "Speakers (USB DAC)")
        );
        assert_eq!(
            labels(Source::Desktop, &none, None),
            pair("Desktop audio", "")
        );
        let unplugged = choice("dac", "Speakers (USB DAC)", false);
        assert_eq!(
            labels(Source::Desktop, &unplugged, Some(&list)),
            pair("Desktop audio", "Speakers (USB DAC) (disconnected)")
        );
        let headset = Source::Output("a".into());
        assert_eq!(
            labels(headset.clone(), &none, Some(&list)),
            pair("Headset", "Headset")
        );
        // Not listed yet: the node name, no subtitle; listed and gone: disconnected.
        assert_eq!(labels(headset, &none, None), pair("a", ""));
        assert_eq!(
            labels(Source::Input("x".into()), &none, Some(&list)),
            pair("x (disconnected)", "x (disconnected)")
        );
        assert_eq!(
            labels(Source::App("Firefox".into()), &none, Some(&list)),
            pair("Firefox", "")
        );
    }
}
