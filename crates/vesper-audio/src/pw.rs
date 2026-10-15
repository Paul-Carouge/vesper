//! The PipeWire thread: one capture stream, samples sent to the encode thread.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use pipewire as pw;
use pw::properties::PropertiesBox;
use pw::registry::GlobalObject;
use pw::spa;
use pw::spa::pod::Pod;
use pw::types::ObjectType;
use tracing::{error, info, warn};

use crate::{CHANNELS, Error, SAMPLE_RATE, Source};

/// Interleaved F32 samples whose first frame was captured at `start`
/// (`CLOCK_MONOTONIC`).
pub struct Chunk {
    pub start: Duration,
    pub samples: Vec<f32>,
}

/// Our only `EnumFormat`: F32 interleaved, 48 kHz, stereo. The stream's
/// adapter converts whatever the device has (mono mics get upmixed).
fn format_pod() -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(u32::from(CHANNELS));
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .expect("serializing a fixed audio format cannot fail")
    .0
    .into_inner()
}

/// Capture time of the first frame of the oldest queued buffer, `frames` long.
///
/// `now` is the start of the last graph cycle and `queued_buffers` counts the
/// buffers waiting for us, this one included (read before dequeuing): older
/// ones are a quantum earlier each. `delay` (ticks of `rate`) excludes them.
///
/// - Input (microphone, input device): the newest sample left the device
///   `delay` before `now`.
/// - Playback (sink monitor): the newest buffer was mixed at `now` and is
///   heard `delay` later. We stamp it when it is heard, as the video shows it.
///   An application's streams are linked to us directly, without the sink:
///   `delay` is ours only, and `sink_ns` adds what the monitor path adds
///   (see `AppLinks`).
fn buffer_start(
    input: bool,
    now_ns: i64,
    delay: i64,
    sink_ns: i128,
    rate: spa::utils::Fraction,
    queued_buffers: u32,
    frames: usize,
) -> Duration {
    let delay_ns = if rate.denom == 0 {
        0
    } else {
        delay.max(0) as i128 * rate.num as i128 * 1_000_000_000 / rate.denom as i128
    };
    let len_ns = frames as i128 * 1_000_000_000 / SAMPLE_RATE as i128;
    // shortcut: assumes backlogged buffers are as long as this one; fine while the quantum is stable.
    let backlog_ns = i128::from(queued_buffers.max(1) - 1) * len_ns;
    let newest_start = if input {
        now_ns as i128 - delay_ns - len_ns
    } else {
        now_ns as i128 + delay_ns + sink_ns
    };
    Duration::from_nanos((newest_start - backlog_ns).max(0) as u64)
}

/// Streams `source` into `tx` until `stop` fires. Reports setup success on
/// `ready` before streaming.
pub fn run(
    source: Source,
    tx: mpsc::SyncSender<Chunk>,
    stop: pw::channel::Receiver<()>,
    ready: mpsc::Sender<Result<(), Error>>,
) {
    pw::init();
    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(m) => m,
        Err(e) => return drop(ready.send(Err(e.into()))),
    };
    let setup = (|| -> Result<_, Error> {
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;
        let mut props = pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Production",
        };
        // Desktop and microphone have no target: the session manager links
        // them to the default sink (monitor) or source, and moves them when
        // the default changes. A device is targeted by name, never replaced
        // by the default while it is missing. An app's streams are linked by
        // us (`AppLinks`).
        let (name, target) = match &source {
            Source::Desktop => ("vesper-desktop-audio", None),
            Source::Microphone => ("vesper-microphone", None),
            Source::Output(node) => ("vesper-device-audio", Some(node)),
            Source::Input(node) => ("vesper-device-audio", Some(node)),
            Source::App(_) => ("vesper-app-audio", None),
        };
        if matches!(source, Source::Desktop | Source::Output(_)) {
            props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
        }
        if let Some(node) = target {
            props.insert(*pw::keys::TARGET_OBJECT, node.as_str());
            props.insert("node.dont-fallback", "true");
        }
        let input = source.is_input();
        // Latency of the sink an app plays to; zero for other sources.
        let latency = Rc::new(Cell::new(Latency::default()));
        let app = match &source {
            Source::App(app) => Some(AppLinks::new(app.clone(), latency.clone())),
            _ => None,
        };
        let stream = pw::stream::StreamRc::new(core.clone(), name, props)?;
        let listener = stream
            .add_local_listener_with_user_data(tx)
            .state_changed({
                let (app, core) = (app.clone(), core.clone());
                move |stream, _, old, new| {
                    info!("{name}: stream state {old:?} -> {new:?}");
                    // Our node id may only be known now: link the ports seen so far.
                    if let Some(app) = &app
                        && matches!(
                            new,
                            pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming
                        )
                    {
                        app.borrow_mut().update(&core, stream.node_id());
                    }
                    if let pw::stream::StreamState::Error(e) = new {
                        // The encode thread keeps the track going with silence.
                        error!("{name}: stream error: {e}");
                    }
                }
            })
            .process(move |stream, tx| {
                let time = stream.time().ok();
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
                let Some(bytes) = data.data().and_then(|d| d.get(offset..offset + size)) else {
                    return;
                };
                let samples: Vec<f32> = bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|&b| f32::from_le_bytes(b))
                    .collect();
                let frames = samples.len() / usize::from(CHANNELS);
                let start = match time {
                    Some(t) if t.now() > 0 => buffer_start(
                        input,
                        t.now(),
                        t.delay(),
                        2 * latency.get().to_ns(
                            frames as i128 * 1_000_000_000 / SAMPLE_RATE as i128,
                            t.rate(),
                        ),
                        t.rate(),
                        t.queued_buffers(),
                        frames,
                    ),
                    _ => crate::monotonic_now().saturating_sub(crate::frames_to_duration(frames)),
                };
                // Never block the PipeWire loop: the timeline fills the hole.
                if let Err(mpsc::TrySendError::Full(_)) = tx.try_send(Chunk { start, samples }) {
                    warn!("{name}: encoder is behind, audio buffer dropped");
                }
            })
            .register()?;
        let pod = format_pod();
        let mut params = [Pod::from_bytes(&pod).ok_or("invalid audio format pod")?];
        let mut flags = pw::stream::StreamFlags::MAP_BUFFERS;
        if !matches!(source, Source::App(_)) {
            flags |= pw::stream::StreamFlags::AUTOCONNECT;
        }
        stream.connect(spa::utils::Direction::Input, None, flags, &mut params)?;
        let links = match &app {
            Some(app) => Some((AppLinks::watch(app, &core, &stream)?, app.clone())),
            None => None,
        };
        Ok((context, core, stream, listener, links))
    })();
    let (_context, _core, stream, listener, links) = match setup {
        Ok(s) => s,
        Err(e) => return drop(ready.send(Err(e))),
    };
    let _stop = stop.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    let _ = ready.send(Ok(()));
    mainloop.run();
    // Our links go away with their proxies.
    drop(links);
    drop(listener);
    if let Err(e) = stream.disconnect() {
        error!("pw_stream_disconnect failed: {e}");
    }
}

/// Our input channels an application's output channel is linked to: left
/// and right to their side, center, mono and the rest to both. LFE is left
/// out, as in a usual stereo downmix.
fn link_targets(channel: &str) -> &'static [&'static str] {
    match channel {
        "LFE" | "LFE2" => &[],
        "FL" | "SL" | "RL" | "FLC" | "RLC" | "TFL" | "TRL" | "FLW" => &["FL"],
        "FR" | "SR" | "RR" | "FRC" | "RRC" | "TFR" | "TRR" | "FRW" => &["FR"],
        _ => &["FL", "FR"],
    }
}

/// A port seen in the registry.
struct Port {
    node: u32,
    output: bool,
    channel: String,
}

/// A `Latency` param: in quanta, graph-rate frames and nanoseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Latency {
    quanta: f32,
    frames: i32,
    ns: i64,
}

impl Latency {
    /// In nanoseconds, with a quantum of `quantum_ns` and the graph `rate`.
    fn to_ns(self, quantum_ns: i128, rate: spa::utils::Fraction) -> i128 {
        let frames_ns = if rate.denom == 0 {
            0
        } else {
            i128::from(self.frames) * rate.num as i128 * 1_000_000_000 / rate.denom as i128
        };
        (f64::from(self.quanta) * quantum_ns as f64) as i128 + frames_ns + i128::from(self.ns)
    }
}

/// The output-direction latency in a `Latency` param, if that is one.
fn parse_latency(pod: &Pod) -> Option<Latency> {
    use spa::pod::Value;
    use spa::pod::deserialize::PodDeserializer;
    let (_, Value::Object(obj)) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?
    else {
        return None;
    };
    let mut latency = Latency::default();
    let mut output = false;
    // Keys of `spa_param_latency` (spa/param/latency.h).
    for p in obj.properties {
        match (p.key, p.value) {
            (1, Value::Id(dir)) => output = dir.0 == spa::sys::SPA_DIRECTION_OUTPUT,
            (2, Value::Float(q)) => latency.quanta = q,
            (4, Value::Int(f)) => latency.frames = f,
            (6, Value::Long(ns)) => latency.ns = ns,
            _ => {}
        }
    }
    output.then_some(latency)
}

/// Links every playback stream of one application to our capture stream,
/// following streams as they come and go. Several streams mix on our ports.
///
/// The app's samples reach us in the graph cycle the sink mixes them. A
/// sink's monitor hands out that mix its monitor latency later, and the
/// desktop track adds that latency again, read as playback delay. So the
/// same sound gets the same pts in both tracks (and the mix does not comb)
/// when an app track adds twice the monitor latency of the sink it plays to,
/// kept in `latency`.
pub(crate) struct AppLinks {
    app: String,
    /// The application's playback stream nodes.
    nodes: HashSet<u32>,
    ports: HashMap<u32, Port>,
    /// Ours, by (output port, input port).
    links: HashMap<(u32, u32), pw::link::Link>,
    /// Every sink.
    sinks: HashSet<u32>,
    /// Monitor ports, by node, to bind the one of the app's sink.
    monitors: HashMap<u32, GlobalObject<PropertiesBox>>,
    /// Every link in the graph: id -> (output node, input node).
    graph: HashMap<u32, (u32, u32)>,
    /// A monitor port of the sink the app plays to: (sink, port id, proxy).
    sink: Option<(u32, u32, pw::port::Port, pw::port::PortListener)>,
    latency: Rc<Cell<Latency>>,
    registry: Option<pw::registry::RegistryRc>,
}

impl AppLinks {
    pub(crate) fn new(app: String, latency: Rc<Cell<Latency>>) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            app,
            nodes: HashSet::new(),
            ports: HashMap::new(),
            links: HashMap::new(),
            sinks: HashSet::new(),
            monitors: HashMap::new(),
            graph: HashMap::new(),
            sink: None,
            latency,
            registry: None,
        }))
    }

    /// Follows the registry. Keep the listener alive while capturing.
    fn watch(
        state: &Rc<RefCell<Self>>,
        core: &pw::core::CoreRc,
        stream: &pw::stream::StreamRc,
    ) -> Result<pw::registry::Listener, Error> {
        let registry = core.get_registry_rc()?;
        state.borrow_mut().registry = Some(registry.clone());
        let listener = registry
            .add_listener_local()
            .global({
                let (state, core, stream) = (state.clone(), core.clone(), stream.clone());
                move |global| {
                    let Some(props) = global.props else { return };
                    let mut s = state.borrow_mut();
                    let get = |k: &str| props.get(k).unwrap_or_default();
                    match global.type_ {
                        ObjectType::Node if crate::nodes::is_app_stream(&s.app, props) => {
                            info!("{}: linking stream node {}", s.app, global.id);
                            s.nodes.insert(global.id);
                        }
                        ObjectType::Node if get(*pw::keys::MEDIA_CLASS) == "Audio/Sink" => {
                            s.sinks.insert(global.id);
                        }
                        ObjectType::Port => {
                            let Ok(node) = get(*pw::keys::NODE_ID).parse() else {
                                return;
                            };
                            let port = Port {
                                node,
                                output: get(*pw::keys::PORT_DIRECTION) == "out",
                                channel: get(*pw::keys::AUDIO_CHANNEL).to_owned(),
                            };
                            if get(*pw::keys::PORT_MONITOR) == "true" {
                                s.monitors.insert(global.id, global.to_owned());
                            }
                            s.ports.insert(global.id, port);
                        }
                        ObjectType::Link => {
                            let ends = (
                                get(*pw::keys::LINK_OUTPUT_NODE).parse(),
                                get(*pw::keys::LINK_INPUT_NODE).parse(),
                            );
                            let (Ok(out), Ok(inp)) = ends else { return };
                            s.graph.insert(global.id, (out, inp));
                        }
                        _ => return,
                    }
                    s.update(&core, stream.node_id());
                }
            })
            .global_remove({
                let state = state.clone();
                move |id| {
                    let mut s = state.borrow_mut();
                    s.nodes.remove(&id);
                    s.sinks.remove(&id);
                    s.monitors.remove(&id);
                    s.graph.remove(&id);
                    if s.ports.remove(&id).is_some() {
                        // Gone with the port on the server: forget our proxy.
                        s.links.retain(|&(o, i), _| o != id && i != id);
                    }
                    if s.sink
                        .as_ref()
                        .is_some_and(|(sink, port, ..)| *sink == id || *port == id)
                    {
                        s.sink = None;
                        s.latency.set(Latency::default());
                    }
                }
            })
            .register();
        Ok(listener)
    }

    /// Creates the links still missing, and follows the app's sink.
    pub(crate) fn update(&mut self, core: &pw::core::Core, own: u32) {
        self.follow_sink();
        let Self {
            nodes,
            ports,
            links,
            app,
            ..
        } = self;
        for (&out_id, out) in ports.iter() {
            if !out.output || !nodes.contains(&out.node) {
                continue;
            }
            for target in link_targets(&out.channel) {
                let Some((&in_id, _)) = ports
                    .iter()
                    .find(|(_, p)| p.node == own && !p.output && p.channel == *target)
                else {
                    continue;
                };
                if links.contains_key(&(out_id, in_id)) {
                    continue;
                }
                let props = pw::properties::properties! {
                    *pw::keys::LINK_OUTPUT_NODE => out.node.to_string(),
                    *pw::keys::LINK_OUTPUT_PORT => out_id.to_string(),
                    *pw::keys::LINK_INPUT_NODE => own.to_string(),
                    *pw::keys::LINK_INPUT_PORT => in_id.to_string(),
                };
                match core.create_object::<pw::link::Link>("link-factory", &props) {
                    Ok(link) => {
                        links.insert((out_id, in_id), link);
                    }
                    Err(e) => warn!("{app}: could not link port {out_id}: {e}"),
                }
            }
        }
    }

    /// Binds a monitor port of the sink the app plays to (the first one
    /// found), to follow its latency.
    fn follow_sink(&mut self) {
        let sink = self
            .graph
            .values()
            .find(|(out, inp)| self.nodes.contains(out) && self.sinks.contains(inp))
            .map(|&(_, sink)| sink);
        let port = sink.and_then(|sink| {
            let (&id, _) = self
                .monitors
                .iter()
                .find(|(id, _)| self.ports.get(id).is_some_and(|p| p.node == sink))?;
            Some(id)
        });
        if port == self.sink.as_ref().map(|(_, port, ..)| *port) {
            return;
        }
        self.sink = None;
        self.latency.set(Latency::default());
        let (Some(sink), Some(port), Some(registry)) = (sink, port, &self.registry) else {
            return;
        };
        let Ok(proxy) = registry.bind::<pw::port::Port, _>(&self.monitors[&port]) else {
            return;
        };
        let latency = self.latency.clone();
        let app = self.app.clone();
        let listener = proxy
            .add_listener_local()
            .param(move |_, _, _, _, pod| {
                if let Some(l) = pod.and_then(parse_latency)
                    && l != latency.get()
                {
                    info!("{app}: monitor latency of its sink {l:?}");
                    latency.set(l);
                }
            })
            .register();
        proxy.subscribe_params(&[spa::param::ParamType::Latency]);
        self.sink = Some((sink, port, proxy, listener));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sink_latency_is_read_and_added_to_playback() {
        use spa::pod::{Object, Property, PropertyFlags, Value, serialize::PodSerializer};
        let latency_pod = |direction: u32| {
            let prop = |key, value| Property {
                key,
                flags: PropertyFlags::empty(),
                value,
            };
            let obj = Object {
                type_: spa::utils::SpaTypes::ObjectParamLatency.as_raw(),
                id: spa::param::ParamType::Latency.as_raw(),
                properties: vec![
                    prop(1, Value::Id(spa::utils::Id(direction))),
                    prop(2, Value::Float(1.0)),
                    prop(4, Value::Int(512)),
                    prop(6, Value::Long(1_000)),
                ],
            };
            PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
                .unwrap()
                .0
                .into_inner()
        };
        let output = latency_pod(spa::sys::SPA_DIRECTION_OUTPUT);
        let l = parse_latency(Pod::from_bytes(&output).unwrap()).unwrap();
        let rate = spa::utils::Fraction {
            num: 1,
            denom: 48_000,
        };
        // One 1024-frame quantum, 512 frames and 1 us.
        let quantum_ns = 1_024 * 1_000_000_000 / 48_000;
        assert_eq!(l.to_ns(quantum_ns, rate), quantum_ns + 10_666_666 + 1_000);
        let input = latency_pod(spa::sys::SPA_DIRECTION_INPUT);
        assert_eq!(parse_latency(Pod::from_bytes(&input).unwrap()), None);
        // Playback is stamped that much later; input never.
        let now = 10_000_000_000;
        let app = buffer_start(false, now, 0, 20_000_000, rate, 1, 960);
        assert_eq!(app, Duration::from_millis(10_020));
        assert_eq!(
            buffer_start(true, now, 0, 20_000_000, rate, 1, 960),
            Duration::from_millis(9_980)
        );
    }

    #[test]
    fn app_channels_downmix_to_stereo() {
        assert_eq!(link_targets("FL"), ["FL"]);
        assert_eq!(link_targets("RR"), ["FR"]);
        assert_eq!(link_targets("MONO"), ["FL", "FR"]);
        assert_eq!(link_targets("FC"), ["FL", "FR"]);
        assert_eq!(link_targets("AUX0"), ["FL", "FR"]);
        assert!(link_targets("LFE").is_empty());
    }

    #[test]
    fn buffer_start_accounts_for_delay_backlog_and_direction() {
        let rate = spa::utils::Fraction {
            num: 1,
            denom: 48_000,
        };
        let now = 10_000_000_000;
        // 480 ticks of delay (10 ms), 960-frame buffers (20 ms), none behind.
        let mic = buffer_start(true, now, 480, 0, rate, 1, 960);
        assert_eq!(mic, Duration::from_millis(9_970));
        let desktop = buffer_start(false, now, 480, 0, rate, 1, 960);
        assert_eq!(desktop, Duration::from_millis(10_010));
        // Two newer buffers queued behind this one: it is 40 ms older.
        let mic = buffer_start(true, now, 480, 0, rate, 3, 960);
        assert_eq!(mic, Duration::from_millis(9_930));
        let desktop = buffer_start(false, now, 480, 0, rate, 3, 960);
        assert_eq!(desktop, Duration::from_millis(9_970));
        // A negative delay and an empty queue count as zero and one.
        let mic = buffer_start(true, now, -5, 0, rate, 0, 0);
        assert_eq!(mic, Duration::from_secs(10));
    }

    #[test]
    fn format_pod_parses_back() {
        let bytes = format_pod();
        let pod = Pod::from_bytes(&bytes).unwrap();
        let mut info = spa::param::audio::AudioInfoRaw::new();
        info.parse(pod).unwrap();
        assert_eq!(info.format(), spa::param::audio::AudioFormat::F32LE);
        assert_eq!(info.rate(), SAMPLE_RATE);
        assert_eq!(info.channels(), u32::from(CHANNELS));
    }
}
