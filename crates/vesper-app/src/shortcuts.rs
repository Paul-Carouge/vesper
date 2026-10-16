//! Global shortcuts through the xdg-desktop-portal GlobalShortcuts interface,
//! so recording can be driven from a fullscreen game.
//!
//! One session lives for the whole app on a `vesper-shortcuts` thread. The
//! first bind makes KDE show its own dialog to confirm or change the keys.
//! When the portal service restarts, the thread opens a new session and binds
//! again. Without the portal, it logs once and ends: the in-window shortcuts
//! keep working.

use std::time::Duration;

use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut, Shortcut};
use futures_lite::StreamExt;
use futures_lite::future::block_on;

/// The id the portals know Vesper by. KDE refuses global shortcuts without
/// one, and keys the screencast restore token to it.
pub const APP_ID: &str = "io.github.PaulCarouge.Vesper";

/// Registers `APP_ID` for this host (non-Flatpak) process. It must precede
/// every other portal call on the shared connection, and be repeated when the
/// portal restarts: the portal process keeps it per connection.
pub async fn register_app_id() -> ashpd::Result<()> {
    ashpd::register_host_app(APP_ID.parse()?).await
}

/// What a global shortcut does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    ToggleRecording,
    SaveReplay,
    ToggleLive,
    NextScene,
    /// Scene 1 to 4, in the profile's order.
    Scene(u8),
}

impl Action {
    const ALL: [Self; 8] = [
        Self::ToggleRecording,
        Self::SaveReplay,
        Self::ToggleLive,
        Self::NextScene,
        Self::Scene(1),
        Self::Scene(2),
        Self::Scene(3),
        Self::Scene(4),
    ];

    fn id(self) -> &'static str {
        match self {
            Self::ToggleRecording => "toggle-recording",
            Self::SaveReplay => "save-replay",
            Self::ToggleLive => "toggle-live",
            Self::NextScene => "next-scene",
            Self::Scene(1) => "scene-1",
            Self::Scene(2) => "scene-2",
            Self::Scene(3) => "scene-3",
            Self::Scene(_) => "scene-4",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.id() == id)
    }

    /// ShadowPlay's keys (Alt+F8 is its broadcast key); none is bound by a
    /// default KDE Plasma 6 setup. Format of the XDG shortcuts spec. Scenes
    /// get none: the user binds the ones they use.
    fn preferred_trigger(self) -> Option<&'static str> {
        match self {
            Self::ToggleRecording => Some("ALT+F9"),
            Self::SaveReplay => Some("ALT+F10"),
            Self::ToggleLive => Some("ALT+F8"),
            Self::NextScene | Self::Scene(_) => None,
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::ToggleRecording => "Start/stop recording",
            Self::SaveReplay => "Save replay",
            Self::ToggleLive => "Go live/end stream",
            Self::NextScene => "Next scene",
            Self::Scene(1) => "Scene 1",
            Self::Scene(2) => "Scene 2",
            Self::Scene(3) => "Scene 3",
            Self::Scene(_) => "Scene 4",
        }
    }
}

/// Starts the shortcuts thread. `on_bound` gets each action's key as the
/// portal describes it (e.g. "Alt+F9"; empty when the user left it unbound),
/// after every bind and every change. Both callbacks run on that thread.
pub fn spawn(
    on_bound: impl Fn(Action, String) + Send + 'static,
    on_activated: impl Fn(Action) + Send + 'static,
) {
    let spawned = std::thread::Builder::new()
        .name("vesper-shortcuts".into())
        .spawn(move || block_on(run(&on_bound, &on_activated)));
    if let Err(e) = spawned {
        log!("global shortcuts: could not start the thread: {e}");
    }
}

async fn run(on_bound: &impl Fn(Action, String), on_activated: &impl Fn(Action)) {
    loop {
        match session(on_bound, on_activated).await {
            // The portal service restarted: bind again on the new one.
            Ok(()) => log!("global shortcuts: portal restarted, binding again"),
            Err(e) => {
                log!("global shortcuts unavailable ({e}); in-window shortcuts still work");
                return;
            }
        }
        // Let the new portal finish starting. The thread runs nothing else.
        std::thread::sleep(Duration::from_secs(1));
        // The new portal process forgot the id. A failure shows in the bind.
        let _ = register_app_id().await;
    }
}

enum Signal {
    Activated(String),
    Changed(Vec<Shortcut>),
    PortalGone,
}

/// Binds the shortcuts and forwards their signals until the portal service
/// goes away (`Ok`). Fails when the portal or the interface is missing, or
/// the bind is refused.
async fn session(
    on_bound: &impl Fn(Action, String),
    on_activated: &impl Fn(Action),
) -> ashpd::Result<()> {
    let portal = GlobalShortcuts::new().await?;
    // Subscribe before binding so no signal is missed.
    let activated = portal
        .receive_activated()
        .await?
        .map(|a| Signal::Activated(a.shortcut_id().to_owned()));
    let changed = portal
        .receive_shortcuts_changed()
        .await?
        .map(|c| Signal::Changed(c.shortcuts().to_vec()));
    // The well-known name changes owner when the portal restarts or exits.
    let gone = portal
        .receive_owner_changed()
        .await?
        .map(|_| Signal::PortalGone);
    // Kept for the app lifetime: dropping it does not close it, and exiting
    // the process does.
    let session = portal.create_session(Default::default()).await?;
    let shortcuts = Action::ALL.map(|a| {
        NewShortcut::new(a.id(), a.description()).preferred_trigger(a.preferred_trigger())
    });
    let bound = portal
        .bind_shortcuts(&session, &shortcuts, None, Default::default())
        .await?
        .response()?;
    log!("global shortcuts bound");
    report(bound.shortcuts(), on_bound);

    let mut signals = activated.or(changed).or(gone);
    while let Some(signal) = signals.next().await {
        match signal {
            Signal::Activated(id) => match Action::from_id(&id) {
                Some(action) => on_activated(action),
                None => log!("global shortcuts: unknown shortcut {id:?}"),
            },
            Signal::Changed(shortcuts) => report(&shortcuts, on_bound),
            Signal::PortalGone => return Ok(()),
        }
    }
    Ok(())
}

fn report(shortcuts: &[Shortcut], on_bound: &impl Fn(Action, String)) {
    for s in shortcuts {
        log!("global shortcut {}: {:?}", s.id(), s.trigger_description());
        if let Some(action) = Action::from_id(s.id()) {
            on_bound(action, s.trigger_description().to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        for a in Action::ALL {
            assert_eq!(Action::from_id(a.id()), Some(a));
        }
        assert_eq!(Action::from_id("nope"), None);
    }
}
