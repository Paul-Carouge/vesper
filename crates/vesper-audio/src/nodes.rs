//! The devices and applications that can be recorded, from the PipeWire
//! registry, kept up to date while a [`NodeWatch`] runs.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread::JoinHandle;

use pipewire as pw;
use pw::spa::utils::dict::DictRef;
use pw::types::ObjectType;
use tracing::warn;

use crate::{Result, Source};

/// A recordable device or application.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Node {
    pub source: Source,
    /// Human-readable name, e.g. "Arctis Nova 7 Analog Stereo" or "Firefox".
    pub label: String,
}

/// The name an application's playback streams are matched by: its
/// `application.name`, else its binary, else the node's own name.
pub(crate) fn app_key<'a>(
    app_name: Option<&'a str>,
    binary: Option<&'a str>,
    node_name: Option<&'a str>,
) -> Option<&'a str> {
    [app_name, binary, node_name]
        .into_iter()
        .flatten()
        .find(|s| !s.is_empty())
}

/// Whether a node with these properties is a playback stream of `app`.
pub(crate) fn is_app_stream(app: &str, props: &DictRef) -> bool {
    props.get(*pw::keys::MEDIA_CLASS) == Some("Stream/Output/Audio")
        && (props.get(*pw::keys::APP_PROCESS_BINARY) == Some(app)
            || app_key(
                props.get(*pw::keys::APP_NAME),
                None,
                props.get(*pw::keys::NODE_NAME),
            ) == Some(app))
}

/// What a node offers to record, if anything.
fn classify(
    class: &str,
    node_name: Option<&str>,
    description: Option<&str>,
    app_name: Option<&str>,
    binary: Option<&str>,
) -> Option<Node> {
    if class == "Stream/Output/Audio" {
        let key = app_key(app_name, binary, node_name)?;
        return Some(Node {
            source: Source::App(key.to_owned()),
            label: key.to_owned(),
        });
    }
    let name = node_name.filter(|n| !n.is_empty())?.to_owned();
    let label = description
        .filter(|d| !d.is_empty())
        .unwrap_or(&name)
        .to_owned();
    let source = if class == "Audio/Sink" {
        Source::Output(name)
    } else if class.starts_with("Audio/Source") {
        Source::Input(name)
    } else {
        return None;
    };
    Some(Node { source, label })
}

/// Sorted by kind then label, one entry per application.
fn snapshot(nodes: &BTreeMap<u32, Node>) -> Vec<Node> {
    let mut list: Vec<Node> = nodes.values().cloned().collect();
    let rank = |n: &Node| match n.source {
        Source::Output(_) => 0,
        Source::Input(_) => 1,
        _ => 2,
    };
    list.sort_by(|a, b| (rank(a), &a.label).cmp(&(rank(b), &b.label)));
    list.dedup_by(|a, b| a.source == b.source);
    list
}

/// Watches the PipeWire registry on its own thread. Dropping it stops.
pub struct NodeWatch {
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl NodeWatch {
    /// `on_change` runs on the watch thread with the full list, once it is
    /// known and after every change.
    pub fn start(on_change: impl Fn(Vec<Node>) + Send + 'static) -> Result<Self> {
        let (stop, stop_rx) = pw::channel::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("vesper-audio-nodes".into())
            .spawn(move || run(on_change, stop_rx, ready_tx))?;
        let watch = Self {
            stop,
            thread: Some(thread),
        };
        ready_rx.recv().map_err(|_| "pipewire thread died")??;
        Ok(watch)
    }
}

impl Drop for NodeWatch {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            warn!("node watch thread panicked");
        }
    }
}

fn run(
    on_change: impl Fn(Vec<Node>) + 'static,
    stop: pw::channel::Receiver<()>,
    ready: mpsc::Sender<Result<()>>,
) {
    pw::init();
    let setup = (|| -> Result<_> {
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;
        let registry = core.get_registry_rc()?;
        Ok((mainloop, context, core, registry))
    })();
    let (mainloop, _context, core, registry) = match setup {
        Ok(s) => s,
        Err(e) => return drop(ready.send(Err(e))),
    };
    let nodes = Rc::new(RefCell::new(BTreeMap::new()));
    // Quiet until the first sync: one call with the whole initial list.
    let synced = Rc::new(RefCell::new(false));
    let on_change = Rc::new(on_change);
    let notify = {
        let (nodes, synced, on_change) = (nodes.clone(), synced.clone(), on_change.clone());
        move || {
            if *synced.borrow() {
                on_change(snapshot(&nodes.borrow()));
            }
        }
    };
    let notify = Rc::new(notify);
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let (nodes, notify) = (nodes.clone(), notify.clone());
            move |global| {
                if global.type_ != ObjectType::Node {
                    return;
                }
                let Some(props) = global.props else { return };
                let node = classify(
                    props.get(*pw::keys::MEDIA_CLASS).unwrap_or_default(),
                    props.get(*pw::keys::NODE_NAME),
                    props.get(*pw::keys::NODE_DESCRIPTION),
                    props.get(*pw::keys::APP_NAME),
                    props.get(*pw::keys::APP_PROCESS_BINARY),
                );
                if let Some(node) = node {
                    nodes.borrow_mut().insert(global.id, node);
                    notify();
                }
            }
        })
        .global_remove({
            let (nodes, notify) = (nodes.clone(), notify.clone());
            move |id| {
                let removed = nodes.borrow_mut().remove(&id).is_some();
                if removed {
                    notify();
                }
            }
        })
        .register();
    let pending = match core.sync(0) {
        Ok(seq) => seq,
        Err(e) => return drop(ready.send(Err(e.into()))),
    };
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                *synced.borrow_mut() = true;
                notify();
            }
        })
        .register();
    let _stop = stop.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    let _ = ready.send(Ok(()));
    mainloop.run();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_key_prefers_the_application_name() {
        assert_eq!(
            app_key(Some("Firefox"), Some("firefox"), Some("x")),
            Some("Firefox")
        );
        assert_eq!(app_key(Some(""), Some("mpv"), Some("x")), Some("mpv"));
        assert_eq!(app_key(None, None, Some("pw-play")), Some("pw-play"));
        assert_eq!(app_key(None, None, None), None);
    }

    #[test]
    fn app_streams_match_by_name_or_binary() {
        let stream = |props: &[(&str, &str)]| {
            pw::properties::PropertiesBox::from_iter(props.iter().copied())
        };
        let firefox = stream(&[
            ("media.class", "Stream/Output/Audio"),
            ("application.name", "Firefox"),
            ("application.process.binary", "firefox"),
            ("node.name", "Firefox"),
        ]);
        assert!(is_app_stream("Firefox", firefox.as_ref()));
        assert!(is_app_stream("firefox", firefox.as_ref()));
        assert!(!is_app_stream("mpv", firefox.as_ref()));
        // A capture stream of the same app is not its audio.
        let capture = stream(&[
            ("media.class", "Stream/Input/Audio"),
            ("application.name", "Firefox"),
        ]);
        assert!(!is_app_stream("Firefox", capture.as_ref()));
        // No application name: the node name.
        let bare = stream(&[
            ("media.class", "Stream/Output/Audio"),
            ("node.name", "pw-play"),
        ]);
        assert!(is_app_stream("pw-play", bare.as_ref()));
    }

    #[test]
    fn nodes_are_classified_by_media_class() {
        let sink = classify(
            "Audio/Sink",
            Some("alsa_output.x"),
            Some("Headset"),
            None,
            None,
        );
        assert_eq!(
            sink,
            Some(Node {
                source: Source::Output("alsa_output.x".into()),
                label: "Headset".into()
            })
        );
        let virt = classify("Audio/Source/Virtual", Some("ee_source"), None, None, None);
        assert_eq!(virt.unwrap().source, Source::Input("ee_source".into()));
        let app = classify(
            "Stream/Output/Audio",
            Some("n"),
            None,
            Some("Firefox"),
            None,
        );
        assert_eq!(app.unwrap().source, Source::App("Firefox".into()));
        assert_eq!(
            classify("Video/Source", Some("cam"), None, None, None),
            None
        );
        assert_eq!(
            classify("Stream/Input/Audio", Some("rec"), None, None, None),
            None
        );
        assert_eq!(classify("Audio/Sink", None, Some("x"), None, None), None);
    }

    #[test]
    fn snapshot_lists_each_app_once() {
        let nodes = BTreeMap::from([
            (
                5,
                classify("Stream/Output/Audio", None, None, Some("Firefox"), None).unwrap(),
            ),
            (
                6,
                classify("Stream/Output/Audio", None, None, Some("Firefox"), None).unwrap(),
            ),
            (
                7,
                classify("Audio/Sink", Some("b"), Some("B"), None, None).unwrap(),
            ),
            (
                8,
                classify("Audio/Sink", Some("a"), Some("A"), None, None).unwrap(),
            ),
        ]);
        let list = snapshot(&nodes);
        let labels: Vec<_> = list.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(labels, ["A", "B", "Firefox"]);
    }
}
