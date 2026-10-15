//! The "Filters" section of a track's settings: rows shown in and read back
//! from the UI, live changes handed to the pipeline, and the gain reduction
//! meters of the open track.
//!
//! Like the tracks, the UI's filter rows are the truth. Edits change the
//! track's filter model in place, so sliders and focus survive them.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, VecModel};
use vesper_dsp::FilterSpec;

use crate::prefs::Prefs;
use crate::recorder::{MAX_FILTERS, TrackFilter};
use crate::{AppWindow, FilterKind, FilterParam, FilterRow, Shared};

/// Parameter changes are saved once the slider rests this long. A parameter
/// that needs a new chain (it resets the filters' state, a short dropout) is
/// also applied only then, not at every step of a drag.
const SAVE_DELAY: Duration = Duration::from_millis(400);

/// Title shown for a filter.
fn title(kind: &str) -> &'static str {
    match kind {
        "noise_suppression" => "Noise suppression",
        "expander" => "Expander",
        "compressor" => "Compressor",
        "de_esser" => "De-esser",
        "limiter" => "Limiter",
        "high_pass" => "High-pass",
        "gain" => "Gain",
        _ => "Filter",
    }
}

/// Filters that reduce gain, with a meter.
fn meters(kind: &str) -> bool {
    matches!(kind, "expander" | "compressor" | "de_esser" | "limiter")
}

/// How a parameter is shown: label, unit, slider step, behind "More settings".
fn param_meta(name: &str) -> (&'static str, &'static str, f32, bool) {
    match name {
        "max_attenuation_db" => ("Suppression", "dB", 1.0, false),
        "threshold_db" => ("Threshold", "dB", 0.5, false),
        "ratio" => ("Ratio", "ratio", 0.1, false),
        "attack_ms" => ("Attack", "ms", 0.1, true),
        "release_ms" => ("Release", "ms", 1.0, true),
        "range_db" => ("Range", "dB", 0.5, false),
        "knee_db" => ("Knee", "dB", 0.5, true),
        "makeup_db" => ("Makeup", "dB", 0.5, false),
        "freq_hz" => ("Frequency", "Hz", 1.0, false),
        "ceiling_db" => ("Ceiling", "dB", 0.1, false),
        "lookahead_ms" => ("Lookahead", "ms", 0.1, true),
        "db" => ("Gain", "dB", 0.5, false),
        _ => ("Value", "", 0.1, false),
    }
}

/// Exceptions to `param_meta` by filter: what a voice needs most stays shown.
fn advanced(spec: &FilterSpec, name: &str) -> bool {
    match (spec.name(), name) {
        ("expander", "ratio") | ("de_esser", "range_db") => true,
        (_, name) => param_meta(name).3,
    }
}

fn row(filter: &TrackFilter) -> FilterRow {
    let spec = filter.spec;
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
            let mut spec = FilterSpec::default_for(&r.kind)?;
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

/// What "Add filter" offers.
pub fn show_kinds(ui: &AppWindow) {
    let kinds: Vec<_> = FilterSpec::DEFAULTS
        .iter()
        .map(|s| FilterKind {
            kind: s.name().into(),
            title: title(s.name()).into(),
        })
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
}

pub fn wire(ui: &AppWindow, rec: &Shared) {
    show_kinds(ui);

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
        if structural {
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
        let done = edit(&ui, &r, i, |m| {
            if usize::try_from(j).is_ok_and(|j| j < m.row_count()) {
                m.remove(j as usize);
            }
        });
        if done.is_some() {
            save(&ui);
        }
    });

    let (weak, r) = (ui.as_weak(), rec.clone());
    ui.on_filter_added(move |i, kind| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(spec) = FilterSpec::default_for(&kind) else {
            return;
        };
        let done = edit(&ui, &r, i, |m| {
            if m.row_count() < MAX_FILTERS {
                m.push(row(&TrackFilter {
                    enabled: true,
                    spec,
                }));
            }
        });
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
        if edit(&ui, &r, i, |m| m.set_vec(voice)).is_some() {
            save(&ui);
        }
    });
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
    fn values_snap_to_the_step() {
        assert_eq!(snap(2.349, 0.1), 2.3);
        assert_eq!(snap(-20.26, 0.5), -20.5);
        assert_eq!(snap(6_523.0, 50.0), 6_500.0);
    }
}
