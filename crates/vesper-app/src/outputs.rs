//! Names the picked portal source. The ScreenCast portal gives a monitor's
//! position and logical size but not its name: the Wayland outputs give it
//! (`wl_output` v4 `name`, e.g. "DP-1"), matched on that position. Window
//! titles are not exposed at all.

use vesper_capture::SourceInfo;
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

/// The picked source, as the UI shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceLabel {
    pub window: bool,
    /// Output name, e.g. "DP-1"; `None` when unknown (always for windows).
    pub name: Option<String>,
    /// Size in pixels: the output's mode for a named monitor, else the
    /// portal's logical size.
    pub size: Option<(u32, u32)>,
}

impl SourceLabel {
    /// Kind of source, as a word.
    pub fn kind(&self) -> &'static str {
        if self.window { "Window" } else { "Screen" }
    }

    /// Title and secondary line: "DP-1" / "Screen · 3840 × 2160", or
    /// "Screen" / "3840 × 2160" when the name is unknown.
    pub fn lines(&self) -> (String, String) {
        let size = self.size.map(|(w, h)| format!("{w} × {h}"));
        match (&self.name, size) {
            (Some(name), Some(size)) => (name.clone(), format!("{} · {size}", self.kind())),
            (Some(name), None) => (name.clone(), self.kind().to_owned()),
            (None, size) => (self.kind().to_owned(), size.unwrap_or_default()),
        }
    }
}

/// An output as Wayland describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Output {
    name: Option<String>,
    position: (i32, i32),
    /// Current mode, in pixels, after the output transform.
    size: Option<(u32, u32)>,
    /// Rotated by a quarter turn: the mode's sides swap.
    quarter_turn: bool,
}

/// Names `source`. Blocks for two Wayland roundtrips: call it off the UI thread.
pub fn describe(source: &SourceInfo) -> SourceLabel {
    log!("portal stream: {source:?}");
    let unnamed = SourceLabel {
        window: source.window,
        name: None,
        size: source
            .size
            .and_then(|(w, h)| Some((u32::try_from(w).ok()?, u32::try_from(h).ok()?))),
    };
    if source.window {
        return unnamed;
    }
    let outputs = match outputs() {
        Ok(outputs) => outputs,
        Err(e) => {
            log!("could not list the Wayland outputs: {e}");
            return unnamed;
        }
    };
    match pick(&outputs, source.position) {
        Some(output) => SourceLabel {
            window: false,
            name: output.name.clone(),
            size: output.size.or(unnamed.size),
        },
        None => unnamed,
    }
}

/// The output at `position`; the only output when there is one.
fn pick(outputs: &[Output], position: Option<(i32, i32)>) -> Option<&Output> {
    if let [only] = outputs {
        return Some(only);
    }
    let position = position?;
    let mut at = outputs.iter().filter(|o| o.position == position);
    // Mirrored outputs share a position: no way to tell them apart.
    match (at.next(), at.next()) {
        (Some(output), None) => Some(output),
        _ => None,
    }
}

#[derive(Default)]
struct State {
    outputs: Vec<Output>,
}

fn outputs() -> Result<Vec<Output>, Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue();
    let _registry = conn.display().get_registry(&queue.handle(), ());
    let mut state = State::default();
    // The globals, then the outputs' own events.
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    for output in &mut state.outputs {
        if output.quarter_turn
            && let Some((w, h)) = output.size
        {
            output.size = Some((h, w));
        }
    }
    Ok(state.outputs)
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
            && interface == "wl_output"
        {
            // `name` needs version 4; older outputs stay unnamed.
            registry.bind::<wl_output::WlOutput, _, _>(
                name,
                version.min(4),
                queue,
                state.outputs.len(),
            );
            state.outputs.push(Output::default());
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(*index) else {
            return;
        };
        match event {
            wl_output::Event::Geometry {
                x, y, transform, ..
            } => {
                output.position = (x, y);
                output.quarter_turn = matches!(
                    transform,
                    WEnum::Value(
                        wl_output::Transform::_90
                            | wl_output::Transform::_270
                            | wl_output::Transform::Flipped90
                            | wl_output::Transform::Flipped270
                    )
                );
            }
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } if flags.contains(wl_output::Mode::Current) => {
                if let (Ok(w), Ok(h)) = (u32::try_from(width), u32::try_from(height)) {
                    output.size = Some((w, h));
                }
            }
            wl_output::Event::Name { name } => output.name = Some(name),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(name: &str, position: (i32, i32)) -> Output {
        Output {
            name: Some(name.into()),
            position,
            size: Some((2560, 1440)),
            quarter_turn: false,
        }
    }

    #[test]
    fn picks_the_output_at_the_stream_position() {
        let outputs = [output("DP-1", (0, 0)), output("HDMI-A-1", (2560, 0))];
        assert_eq!(pick(&outputs, Some((2560, 0))), Some(&outputs[1]));
        assert_eq!(pick(&outputs, Some((1, 0))), None);
        assert_eq!(pick(&outputs, None), None);
        // Mirrored: ambiguous.
        let mirrored = [output("DP-1", (0, 0)), output("DP-2", (0, 0))];
        assert_eq!(pick(&mirrored, Some((0, 0))), None);
        // A single output is the source whatever the portal says.
        assert_eq!(pick(&outputs[..1], None), Some(&outputs[0]));
    }

    /// Names the monitor at (0, 0) of the running Wayland session.
    #[test]
    #[ignore = "needs a Wayland session"]
    fn names_a_real_monitor() {
        let label = describe(&SourceInfo {
            window: false,
            position: Some((0, 0)),
            size: None,
        });
        eprintln!("{label:?} {:?}", label.lines());
        assert!(label.name.is_some());
    }

    #[test]
    fn labels() {
        let named = SourceLabel {
            window: false,
            name: Some("DP-1".into()),
            size: Some((3840, 2160)),
        };
        assert_eq!(
            named.lines(),
            ("DP-1".into(), "Screen · 3840 × 2160".into())
        );
        let window = SourceLabel {
            window: true,
            name: None,
            size: Some((1280, 720)),
        };
        assert_eq!(window.lines(), ("Window".into(), "1280 × 720".into()));
        assert_eq!(
            SourceLabel::default().lines(),
            ("Screen".into(), String::new())
        );
    }
}
