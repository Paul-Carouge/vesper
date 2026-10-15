//! The PipeWire thread: one capture stream, samples sent to the encode thread.

use std::sync::mpsc;
use std::time::Duration;

use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;
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
/// - Microphone: the newest sample left the device `delay` before `now`.
/// - Desktop (sink monitor): the newest buffer was mixed at `now` and is heard
///   `delay` later. We stamp it when it is heard, as the video shows it.
fn buffer_start(
    source: Source,
    now_ns: i64,
    delay: i64,
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
    let newest_start = match source {
        Source::Microphone => now_ns as i128 - delay_ns - len_ns,
        Source::Desktop => now_ns as i128 + delay_ns,
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
        // No target object: the session manager links us to the default sink
        // (monitor) or source, and moves us when the default changes.
        let mut props = pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Production",
        };
        let name = match source {
            Source::Desktop => {
                props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
                "vesper-desktop-audio"
            }
            Source::Microphone => "vesper-microphone",
        };
        let stream = pw::stream::StreamRc::new(core.clone(), name, props)?;
        let listener = stream
            .add_local_listener_with_user_data(tx)
            .state_changed(move |_, _, old, new| {
                info!("{name}: stream state {old:?} -> {new:?}");
                if let pw::stream::StreamState::Error(e) = new {
                    // The encode thread keeps the track going with silence.
                    error!("{name}: stream error: {e}");
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
                        source,
                        t.now(),
                        t.delay(),
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
        stream.connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )?;
        Ok((context, core, stream, listener))
    })();
    let (_context, _core, stream, listener) = match setup {
        Ok(s) => s,
        Err(e) => return drop(ready.send(Err(e))),
    };
    let _stop = stop.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    let _ = ready.send(Ok(()));
    mainloop.run();
    drop(listener);
    if let Err(e) = stream.disconnect() {
        error!("pw_stream_disconnect failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_start_accounts_for_delay_backlog_and_direction() {
        let rate = spa::utils::Fraction {
            num: 1,
            denom: 48_000,
        };
        let now = 10_000_000_000;
        // 480 ticks of delay (10 ms), 960-frame buffers (20 ms), none behind.
        let mic = buffer_start(Source::Microphone, now, 480, rate, 1, 960);
        assert_eq!(mic, Duration::from_millis(9_970));
        let desktop = buffer_start(Source::Desktop, now, 480, rate, 1, 960);
        assert_eq!(desktop, Duration::from_millis(10_010));
        // Two newer buffers queued behind this one: it is 40 ms older.
        let mic = buffer_start(Source::Microphone, now, 480, rate, 3, 960);
        assert_eq!(mic, Duration::from_millis(9_930));
        let desktop = buffer_start(Source::Desktop, now, 480, rate, 3, 960);
        assert_eq!(desktop, Duration::from_millis(9_970));
        // A negative delay and an empty queue count as zero and one.
        let mic = buffer_start(Source::Microphone, now, -5, rate, 0, 0);
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
