//! The apps the desktop and output tracks leave out, because their own
//! track records them (see [`Exclusions`]).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

/// Told the apps left out, after each change.
pub type OnExclusions = Box<dyn Fn(Vec<String>) + Send>;

/// The apps left out of the desktop and output tracks of one recording,
/// shared by its tracks through [`crate::Options::exclusions`].
///
/// An app is left out only while something holds it: its app track while
/// that captures (from its connection to PipeWire until it is lost), and a
/// [`Hold`] taken before the tracks start, so the desktop track opens
/// without it from the start. An app whose track fails to start or loses
/// PipeWire is back in the desktop track as soon as nothing holds it: it is
/// never recorded nowhere.
#[derive(Default)]
pub struct Exclusions {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// How many holders each app has.
    apps: BTreeMap<String, usize>,
    /// The desktop and output tracks' PipeWire threads, woken on a change.
    watchers: Vec<pipewire::channel::Sender<()>>,
    on_change: Option<OnExclusions>,
}

impl Exclusions {
    /// `on_change` is told the apps left out after each change, on the
    /// thread that made it (a capture's PipeWire thread, or the caller's).
    pub fn new(on_change: Option<OnExclusions>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                on_change,
                ..State::default()
            }),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The apps left out now, sorted.
    pub fn apps(&self) -> Vec<String> {
        self.state().apps.keys().cloned().collect()
    }

    /// Leaves `app` out until the hold is dropped.
    pub fn hold(self: &Arc<Self>, app: &str) -> Hold {
        self.add(app);
        Hold {
            exclusions: self.clone(),
            app: app.to_owned(),
        }
    }

    /// No longer tells `on_change`: the tracks are stopping, their apps
    /// leave one by one.
    pub fn quiet(&self) {
        self.state().on_change = None;
    }

    pub(crate) fn add(&self, app: &str) {
        let mut s = self.state();
        let count = s.apps.entry(app.to_owned()).or_default();
        *count += 1;
        if *count == 1 {
            s.changed();
        }
    }

    pub(crate) fn remove(&self, app: &str) {
        let mut s = self.state();
        let Some(count) = s.apps.get_mut(app) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            s.apps.remove(app);
            s.changed();
        }
    }

    /// Wakes `tx` after each change, until its receiver is gone.
    pub(crate) fn watch(&self, tx: pipewire::channel::Sender<()>) {
        self.state().watchers.push(tx);
    }
}

impl State {
    fn changed(&mut self) {
        self.watchers.retain(|w| w.send(()).is_ok());
        if let Some(f) = &self.on_change {
            f(self.apps.keys().cloned().collect());
        }
    }
}

/// Leaves an app out while it lives, see [`Exclusions::hold`].
pub struct Hold {
    exclusions: Arc<Exclusions>,
    app: String,
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.exclusions.remove(&self.app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn an_app_is_left_out_while_something_holds_it() {
        let (tx, rx) = mpsc::channel();
        let ex = Exclusions::new(Some(Box::new(move |apps| tx.send(apps).unwrap())));
        // Held before the tracks start, then by its running track.
        let hold = ex.hold("Firefox");
        ex.add("Firefox");
        ex.add("mpv");
        drop(hold);
        assert_eq!(ex.apps(), ["Firefox", "mpv"]);
        // Its track lost PipeWire: back in the desktop.
        ex.remove("Firefox");
        assert_eq!(ex.apps(), ["mpv"]);
        // A track that never started: released with its hold.
        drop(ex.hold("Spotify"));
        ex.remove("not held");
        let told: Vec<_> = rx.try_iter().collect();
        assert_eq!(
            told,
            [
                vec!["Firefox"],
                vec!["Firefox", "mpv"],
                vec!["mpv"],
                vec!["Spotify", "mpv"],
                vec!["mpv"],
            ]
        );
        ex.quiet();
        ex.remove("mpv");
        assert!(ex.apps().is_empty() && rx.try_recv().is_err());
    }
}
