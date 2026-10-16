//! The "Filters" section of a track's settings: rows shown in and read back
//! from the UI, live changes handed to the pipeline, and the gain reduction
//! meters of the open track.
//!
//! Like the tracks, the UI's filter rows are the truth. Edits change the
//! track's filter model in place, so sliders and focus survive them.
//!
//! VST3 plugins (`vesper-vst`) are rows too: kind `vst3:id=<id>`, their
//! parameters as generic sliders (0-100 %, the first ones shown, the rest
//! behind "More settings"), their state saved beside the settings. Their
//! title says when a plugin is bypassed (host crashed, restarting) or failed.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, VecModel};
use vesper_dsp::FilterSpec;
use vesper_vst::{PluginInfo, Status};

use crate::prefs::Prefs;
use crate::recorder::{MAX_FILTERS, TrackFilter};
use crate::{AppWindow, FilterKind, FilterParam, FilterRow, Shared};

/// Parameter changes are saved once the slider rests this long. A parameter
/// that needs a new chain (it resets the filters' state, a short dropout) is
/// also applied only then, not at every step of a drag.
const SAVE_DELAY: Duration = Duration::from_millis(400);

/// How often plugin rows check their plugin's status.
const PLUGIN_TICK: Duration = Duration::from_millis(500);

/// Plugin parameters shown before "More settings".
const PLUGIN_MAIN_PARAMS: usize = 8;

/// "Add filter" kinds of the scanned plugins: this prefix, then the index
/// in `PLUGINS`.
const PLUGIN_KIND: &str = "vst3-plugin:";

/// The installed VST3 effects, once scanned.
static PLUGINS: Mutex<Vec<PluginInfo>> = Mutex::new(Vec::new());

/// Makes the tracks' chains able to run VST3 plugins (settings in
/// `~/.config/vesper/vst3/`, hosts from `vesper-vst-host` next to Vesper).
pub fn install_plugins() {
    if let Some(dir) = plugin_dir() {
        vesper_vst::install(vesper_vst::Registry::new(
            dir,
            vesper_vst::HostCommand::installed(),
        ));
    }
}

/// `$XDG_CONFIG_HOME/vesper/vst3`: the plugin instances' settings files.
pub fn plugin_dir() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("vesper").join("vst3"))
}

/// Title shown for a filter.
fn title(kind: &str) -> &'static str {
    match kind {
        "noise_suppression" => tr!("Noise suppression"),
        "expander" => tr!("Expander"),
        "compressor" => tr!("Compressor"),
        "de_esser" => tr!("De-esser"),
        "limiter" => tr!("Limiter"),
        "high_pass" => tr!("High-pass"),
        "gain" => tr!("Gain"),
        _ => tr!("Filter"),
    }
}

/// Filters that reduce gain, with a meter.
fn meters(kind: &str) -> bool {
    matches!(kind, "expander" | "compressor" | "de_esser" | "limiter")
}

/// How a parameter is shown: label, unit, slider step, behind "More settings".
fn param_meta(name: &str) -> (&'static str, &'static str, f32, bool) {
    match name {
        "max_attenuation_db" => (tr!("Suppression"), "dB", 1.0, false),
        "threshold_db" => (tr!("Threshold"), "dB", 0.5, false),
        "ratio" => (tr!("Ratio"), "ratio", 0.1, false),
        "attack_ms" => (tr!("Attack"), "ms", 0.1, true),
        "release_ms" => (tr!("Release"), "ms", 1.0, true),
        "range_db" => (tr!("Range"), "dB", 0.5, false),
        "knee_db" => (tr!("Knee"), "dB", 0.5, true),
        "makeup_db" => (tr!("Makeup"), "dB", 0.5, false),
        "freq_hz" => (tr!("Frequency"), "Hz", 1.0, false),
        "ceiling_db" => (tr!("Ceiling"), "dB", 0.1, false),
        "lookahead_ms" => (tr!("Lookahead"), "ms", 0.1, true),
        "db" => (tr!("Gain"), "dB", 0.5, false),
        _ => (tr!("Value"), "", 0.1, false),
    }
}

/// Exceptions to `param_meta` by filter: what a voice needs most stays shown.
fn advanced(spec: &FilterSpec, name: &str) -> bool {
    match (spec.name(), name) {
        ("expander", "ratio") | ("de_esser", "range_db") => true,
        (_, name) => param_meta(name).3,
    }
}

/// The id of a plugin row's instance.
fn plugin_id(kind: &str) -> Option<u32> {
    match kind.parse() {
        Ok(FilterSpec::Vst3 { id }) => Some(id),
        _ => None,
    }
}

/// A plugin's row: its parameters as 0-100 % sliders.
fn plugin_row(enabled: bool, id: u32) -> FilterRow {
    let view = vesper_vst::global()
        .and_then(|r| r.instance(id).ok())
        .map(|i| i.view());
    let (title, params) = match view {
        Some(v) => {
            let title = match v.status {
                Status::Running => v.name,
                Status::Starting => tr!("{} (starting)", v.name),
                Status::Restarting(why) => tr!("{} (bypassed: {}; restarting)", v.name, why),
                Status::Failed(why) => tr!("{} (bypassed: {})", v.name, why),
            };
            (title, v.params)
        }
        None => (tr!("VST3 plugin (unavailable)").to_owned(), Vec::new()),
    };
    let params: Vec<_> = params
        .iter()
        .enumerate()
        .map(|(i, p)| FilterParam {
            index: i as i32,
            label: p.title.as_str().into(),
            unit: "%".into(),
            value: (p.value * 100.0) as f32,
            minimum: 0.0,
            maximum: 100.0,
            step: if p.steps > 0 {
                100.0 / p.steps as f32
            } else {
                0.1
            },
        })
        .collect();
    let split = params.len().min(PLUGIN_MAIN_PARAMS);
    FilterRow {
        kind: FilterSpec::Vst3 { id }.to_string().into(),
        title: title.into(),
        enabled,
        meters: false,
        main: ModelRc::new(VecModel::from(params[..split].to_vec())),
        advanced: ModelRc::new(VecModel::from(params[split..].to_vec())),
    }
}

fn row(filter: &TrackFilter) -> FilterRow {
    let spec = filter.spec;
    if let FilterSpec::Vst3 { id } = spec {
        return plugin_row(filter.enabled, id);
    }
    let (mut main, mut more) = (Vec::new(), Vec::new());
    let params = spec
        .param_names()
        .iter()
        .zip(spec.ranges())
        .zip(spec.values());
    for (i, ((name, &(minimum, maximum)), value)) in params.enumerate() {
        let (label, unit, step, _) = param_meta(name);
        let param = FilterParam {
            index: i as i32,
            label: label.into(),
            unit: unit.into(),
            value,
            minimum,
            maximum,
            // The de-esser's band moves in 50 Hz steps over 10 kHz.
            step: if spec.name() == "de_esser" && unit == "Hz" {
                50.0
            } else {
                step
            },
        };
        if advanced(&spec, name) {
            more.push(param);
        } else {
            main.push(param);
        }
    }
    FilterRow {
        kind: spec.name().into(),
        title: title(spec.name()).into(),
        enabled: filter.enabled,
        meters: meters(spec.name()),
        main: ModelRc::new(VecModel::from(main)),
        advanced: ModelRc::new(VecModel::from(more)),
    }
}

/// The filter rows of a track, for its `AudioTrack`.
pub fn rows(filters: &[TrackFilter]) -> ModelRc<FilterRow> {
    ModelRc::new(VecModel::from(filters.iter().map(row).collect::<Vec<_>>()))
}

/// The filters as the UI shows them. Rows of unknown filters are left out.
pub fn from_rows(rows: &ModelRc<FilterRow>) -> Vec<TrackFilter> {
    rows.iter()
        .filter_map(|r| {
            let mut spec: FilterSpec = r.kind.parse().ok()?;
            for p in r.main.iter().chain(r.advanced.iter()) {
                spec = spec.with(usize::try_from(p.index).ok()?, p.value);
            }
            Some(TrackFilter {
                enabled: r.enabled,
                spec,
            })
        })
        .collect()
}

/// What "Add filter" offers: the built-in filters, then the scanned plugins.
pub fn show_kinds(ui: &AppWindow) {
    let plugins = PLUGINS.lock().unwrap_or_else(|e| e.into_inner());
    let kinds: Vec<_> = FilterSpec::DEFAULTS
        .iter()
        .map(|s| FilterKind {
            kind: s.name().into(),
            title: title(s.name()).into(),
        })
        .chain(plugins.iter().enumerate().map(|(i, p)| FilterKind {
            kind: format!("{PLUGIN_KIND}{i}").into(),
            title: format!("VST3: {}", p.name).into(),
        }))
        .collect();
    ui.set_filter_kinds(ModelRc::new(VecModel::from(kinds)));
    ui.set_max_filters(MAX_FILTERS as i32);
}

/// A value as its slider sets it: on the step, without float noise.
fn snap(value: f32, step: f32) -> f32 {
    ((value / step).round() * step * 1000.0).round() / 1000.0
}

/// Runs `f` on the filter model of track `track`, then hands the track's
/// filters to the pipeline. None if there is no such track.
fn edit(
    ui: &AppWindow,
    rec: &Shared,
    track: i32,
    f: impl FnOnce(&VecModel<FilterRow>),
) -> Option<()> {
    let rows = ui
        .get_tracks()
        .row_data(usize::try_from(track).ok()?)?
        .filters;
    f(rows.as_any().downcast_ref::<VecModel<FilterRow>>()?);
    apply(ui, rec, track);
    Some(())
}

/// Hands the UI's filters of track `track` to the pipeline.
fn apply(ui: &AppWindow, rec: &Shared, track: i32) {
    let Ok(index) = usize::try_from(track) else {
        return;
    };
    if let (Some(row), Some(p)) = (ui.get_tracks().row_data(index), rec.borrow().as_ref()) {
        p.set_filters(index, from_rows(&row.filters));
    }
}

fn save(ui: &AppWindow) {
    Prefs::from_ui(ui).save();
    if let Some(r) = vesper_vst::global() {
        r.persist_all();
    }
}

/// Forgets the plugin instances of `rows` (their settings file goes too).
pub(crate) fn forget_plugins(rows: impl IntoIterator<Item = FilterRow>) {
    if let Some(r) = vesper_vst::global() {
        rows.into_iter()
            .filter_map(|row| plugin_id(&row.kind))
            .for_each(|id| r.remove(id));
    }
}

/// A plugin scan is running.
static SCANNING: AtomicBool = AtomicBool::new(false);

/// Lists the installed plugins in the background, then offers them. Run
/// when "Add filter" opens (unchanged bundles come from the cache), not at
/// startup: no plugin code runs unless plugins are wanted.
fn scan_plugins(ui: &AppWindow) {
    let Some(registry) = vesper_vst::global() else {
        return;
    };
    if SCANNING.swap(true, Ordering::AcqRel) {
        return;
    }
    let weak = ui.as_weak();
    let scan = std::thread::Builder::new()
        .name("vst3-scan".into())
        .spawn(move || {
            let found = registry.scan();
            *PLUGINS.lock().unwrap_or_else(|e| e.into_inner()) = found;
            SCANNING.store(false, Ordering::Release);
            let _ = weak.upgrade_in_event_loop(|ui| show_kinds(&ui));
        });
    if let Err(e) = scan {
        SCANNING.store(false, Ordering::Release);
        log!("cannot scan the VST3 plugins: {e}");
    }
}

/// Refreshes plugin rows whose plugin changed (loaded, bypassed, back).
fn refresh_plugins(ui: &AppWindow, seen: &mut HashMap<u32, u64>) {
    let Some(registry) = vesper_vst::global() else {
        return;
    };
    for track in ui.get_tracks().iter() {
        let Some(model) = track.filters.as_any().downcast_ref::<VecModel<FilterRow>>() else {
            continue;
        };
        for (j, r) in model.iter().enumerate() {
            let Some(id) = plugin_id(&r.kind) else {
                continue;
            };
            let Ok(instance) = registry.instance(id) else {
                continue;
            };
            let generation = instance.generation();
            if seen.insert(id, generation) != Some(generation) {
                model.set_row_data(j, plugin_row(r.enabled, id));
            }
        }
    }
}

pub fn wire(ui: &AppWindow, rec: &Shared) {
    show_kinds(ui);
    let weak = ui.as_weak();
    ui.on_filter_menu_opened(move || {
        if let Some(ui) = weak.upgrade() {
            scan_plugins(&ui);
        }
    });
    let weak = ui.as_weak();
    let seen = RefCell::new(HashMap::new());
    let plugin_timer = slint::Timer::default();
    plugin_timer.start(slint::TimerMode::Repeated, PLUGIN_TICK, move || {
        if let Some(ui) = weak.upgrade() {
            refresh_plugins(&ui, &mut seen.borrow_mut());
        }
    });
    // Ticks as long as the app runs.
    std::mem::forget(plugin_timer);

    let (weak, r) = (ui.as_weak(), rec.clone());
    ui.on_filter_toggled(move |i, j, on| {
        let Some(ui) = weak.upgrade() else { return };
        let done = edit(&ui, &r, i, |m| {
            let j = j as usize;
            if let Some(mut row) = m.row_data(j) {
                row.enabled = on;
                m.set_row_data(j, row);
            }
        });
        if done.is_some() {
            save(&ui);
        }
    });

    let (weak, r) = (ui.as_weak(), rec.clone());
    let timer = Rc::new(slint::Timer::default());
    // Tracks with a structural change waiting for the slider to rest: one
    // timer for all, so none is dropped when another track is touched.
    let pending: Rc<RefCell<BTreeSet<i32>>> = Rc::default();
    ui.on_filter_param_changed(move |i, j, k, value| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(model) = usize::try_from(i)
            .ok()
            .and_then(|i| ui.get_tracks().row_data(i))
            .map(|t| t.filters)
        else {
            return;
        };
        let plugin = model
            .row_data(j as usize)
            .and_then(|row| plugin_id(&row.kind));
        let structural = model.row_data(j as usize).is_some_and(|row| {
            FilterSpec::default_for(&row.kind).is_some_and(|s| s.is_structural(k as usize))
        });
        let edited = |m: &VecModel<FilterRow>| {
            let Some(row) = m.row_data(j as usize) else {
                return;
            };
            // In place, in whichever list holds it: a drag goes on.
            for params in [&row.main, &row.advanced] {
                if let Some(at) = params.iter().position(|p| p.index == k) {
                    let mut p = params.row_data(at).expect("found");
                    p.value = snap(value, p.step).clamp(p.minimum, p.maximum);
                    params.set_row_data(at, p);
                }
            }
        };
        let Some(m) = model.as_any().downcast_ref::<VecModel<FilterRow>>() else {
            return;
        };
        edited(m);
        if let Some(id) = plugin {
            // Straight to the plugin's host: its chain does not change.
            let instance = vesper_vst::global().and_then(|r| r.instance(id).ok());
            let value = model
                .row_data(j as usize)
                .and_then(|row| {
                    row.main
                        .iter()
                        .chain(row.advanced.iter())
                        .find(|p| p.index == k)
                })
                .map_or(value, |p| p.value);
            if let (Some(instance), Ok(k)) = (instance, usize::try_from(k)) {
                instance.set_param(k, f64::from(value) / 100.0);
            }
        } else if structural {
            pending.borrow_mut().insert(i);
        } else {
            apply(&ui, &r, i);
        }
        let (weak, r, pending) = (weak.clone(), r.clone(), pending.clone());
        timer.start(slint::TimerMode::SingleShot, SAVE_DELAY, move || {
            if let Some(ui) = weak.upgrade() {
                for track in std::mem::take(&mut *pending.borrow_mut()) {
                    apply(&ui, &r, track);
                }
                save(&ui);
            }
        });
    });

    let (weak, r) = (ui.as_weak(), rec.clone());
    ui.on_filter_moved(move |i, j, d| {
        let Some(ui) = weak.upgrade() else { return };
        let done = edit(&ui, &r, i, |m| {
            let (a, b) = (j as usize, (j + d) as usize);
            if let (Some(x), Some(y)) = (m.row_data(a), m.row_data(b)) {
                m.set_row_data(a, y);
                m.set_row_data(b, x);
            }
        });
        if done.is_some() {
            save(&ui);
        }
    });

    let (weak, r) = (ui.as_weak(), rec.clone());
    ui.on_filter_removed(move |i, j| {
        let Some(ui) = weak.upgrade() else { return };
        let mut removed = None;
        let done = edit(&ui, &r, i, |m| {
            if usize::try_from(j).is_ok_and(|j| j < m.row_count()) {
                removed = Some(m.remove(j as usize));
            }
        });
        forget_plugins(removed);
        if done.is_some() {
            save(&ui);
        }
    });

    let (weak, r) = (ui.as_weak(), rec.clone());
    ui.on_filter_added(move |i, kind| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(spec) = new_filter(&kind) else {
            return;
        };
        let mut added = false;
        let done = edit(&ui, &r, i, |m| {
            if m.row_count() < MAX_FILTERS {
                m.push(row(&TrackFilter {
                    enabled: true,
                    spec,
                }));
                added = true;
            }
        });
        if let (false, FilterSpec::Vst3 { id }) = (added, spec)
            && let Some(r) = vesper_vst::global()
        {
            r.remove(id);
        }
        if done.is_some() {
            save(&ui);
        }
    });

    let (weak, r) = (ui.as_weak(), rec.clone());
    ui.on_filter_voice_preset(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        let voice: Vec<_> = FilterSpec::voice_chain()
            .into_iter()
            .map(|spec| {
                row(&TrackFilter {
                    enabled: true,
                    spec,
                })
            })
            .collect();
        let mut old = Vec::new();
        let done = edit(&ui, &r, i, |m| {
            old = m.iter().collect();
            m.set_vec(voice);
        });
        forget_plugins(old);
        if done.is_some() {
            save(&ui);
        }
    });
}

/// The filter "Add filter" makes of `kind`: a built-in one, or a new
/// instance of a scanned plugin.
fn new_filter(kind: &str) -> Option<FilterSpec> {
    let Some(index) = kind.strip_prefix(PLUGIN_KIND) else {
        return FilterSpec::default_for(kind);
    };
    let plugin = PLUGINS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(index.parse::<usize>().ok()?)?
        .clone();
    match vesper_vst::global()?.add(&plugin) {
        Ok(id) => Some(FilterSpec::Vst3 { id }),
        Err(e) => {
            log!("cannot add VST3 plugin {:?}: {e}", plugin.name);
            None
        }
    }
}

/// Gain reduction of the open track's filters, from the pipeline, into
/// `model`. Called by the meter tick.
pub fn feed_reductions(ui: &AppWindow, rec: &Shared, model: &VecModel<f32>) {
    let open = ui.get_open_track();
    let rows = usize::try_from(open)
        .ok()
        .and_then(|i| ui.get_tracks().row_data(i))
        .map(|t| t.filters);
    let rows: Vec<FilterRow> = rows.map(|r| r.iter().collect()).unwrap_or_default();
    if model.row_count() != rows.len() {
        model.set_vec(vec![0.0; rows.len()]);
    }
    let rec = rec.borrow();
    // The pipeline counts the enabled filters only.
    let mut running = 0;
    for (j, row) in rows.iter().enumerate() {
        let value = match rec.as_ref() {
            Some(p) if row.enabled && row.meters => p.reduction_db(open as usize, running),
            _ => 0.0,
        };
        running += usize::from(row.enabled);
        // Resting meters cost nothing; reaching 0 always shows, so the meters
        // timer sees them at rest.
        let shown = model.row_data(j).unwrap_or_default();
        if (shown - value).abs() >= 0.05 || (shown == 0.0) != (value == 0.0) {
            model.set_row_data(j, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_round_trip_every_filter() {
        let filters: Vec<_> = FilterSpec::DEFAULTS
            .iter()
            .chain(&[FilterSpec::Gain { db: -3.5 }])
            .enumerate()
            .map(|(i, &spec)| TrackFilter {
                enabled: i % 2 == 0,
                spec,
            })
            .collect();
        assert_eq!(from_rows(&rows(&filters)), filters);
        for f in &filters {
            let r = row(f);
            assert_eq!(
                r.main.row_count() + r.advanced.row_count(),
                f.spec.param_names().len()
            );
            assert!(r.main.row_count() > 0, "{}", f.spec.name());
            assert_ne!(r.title, "Filter");
            for p in r.main.iter().chain(r.advanced.iter()) {
                assert_ne!(p.label, "Value", "{}", f.spec.name());
            }
        }
    }

    #[test]
    fn plugin_rows_round_trip() {
        let filters = vec![
            TrackFilter {
                enabled: false,
                spec: FilterSpec::Vst3 { id: 5 },
            },
            TrackFilter {
                enabled: true,
                spec: FilterSpec::Gain { db: 1.0 },
            },
        ];
        let rows = rows(&filters);
        assert_eq!(rows.row_data(0).unwrap().kind, "vst3:id=5");
        assert_eq!(plugin_id("vst3:id=5"), Some(5));
        assert_eq!(plugin_id("gain"), None);
        assert_eq!(from_rows(&rows), filters);
        assert_eq!(new_filter("gain"), FilterSpec::default_for("gain"));
        assert_eq!(new_filter("vst3-plugin:99"), None);
    }

    #[test]
    fn forgetting_plugin_rows_deletes_only_their_settings() {
        let dir = std::env::temp_dir().join(format!("vesper-app-vst-{}", std::process::id()));
        let reg = vesper_vst::install(vesper_vst::Registry::new(
            &dir,
            vesper_vst::HostCommand::new("/nonexistent/vesper-vst-host"),
        ))
        .unwrap();
        let plugin = PluginInfo {
            path: "/nowhere/A.vst3".into(),
            cid: [1; 16],
            name: "A".into(),
            vendor: String::new(),
            category: "Fx".into(),
        };
        let (a, b) = (reg.add(&plugin).unwrap(), reg.add(&plugin).unwrap());
        let track_rows = rows(&[
            TrackFilter {
                enabled: true,
                spec: FilterSpec::Vst3 { id: a },
            },
            TrackFilter {
                enabled: true,
                spec: FilterSpec::Gain { db: 0.0 },
            },
        ]);
        forget_plugins(track_rows.iter());
        assert!(!vesper_vst::sidecar::file(&dir, a).exists());
        assert!(vesper_vst::sidecar::file(&dir, b).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn values_snap_to_the_step() {
        assert_eq!(snap(2.349, 0.1), 2.3);
        assert_eq!(snap(-20.26, 0.5), -20.5);
        assert_eq!(snap(6_523.0, 50.0), 6_500.0);
    }
}
