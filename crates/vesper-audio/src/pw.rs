//! The PipeWire thread: one capture stream, samples sent to the encode thread.

use std::sync::mpsc;
use std::time::Duration;

use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;
use tracing::{error, info};

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

/// Capture time of the first frame of a buffer of `frames` frames.
///
/// `now` is the start of the current graph cycle, when the driver handed over
/// the last captured quantum; `delay` is how long samples took to travel from
/// the device to us. The buffer's first frame is `frames` older than its last.
fn buffer_start(now_ns: i64, delay: i64, rate: spa::utils::Fraction, frames: usize) -> Duration {
    let delay_ns = if rate.denom == 0 {
        0
    } else {
        delay.max(0) as i128 * rate.num as i128 * 1_000_000_000 / rate.denom as i128
    };
    let len_ns = frames as i128 * 1_000_000_000 / SAMPLE_RATE as i128;
    let start = now_ns as i128 - delay_ns - len_ns;
    Duration::from_nanos(start.max(0) as u64)
}

/// Streams `source` into `tx` until `stop` fires. Reports setup success on
/// `ready` before streaming.
pub fn run(
    source: Source,
    tx: mpsc::Sender<Chunk>,
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
            .process(|stream, tx| {
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
                    Some(t) if t.now() > 0 => buffer_start(t.now(), t.delay(), t.rate(), frames),
                    _ => crate::monotonic_now().saturating_sub(crate::frames_to_duration(frames)),
                };
                // Fails only while stopping: the encode thread is gone then.
                let _ = tx.send(Chunk { start, samples });
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
    fn buffer_start_subtracts_delay_and_length() {
        let rate = spa::utils::Fraction {
            num: 1,
            denom: 48_000,
        };
        // 10 s, 480 frames of delay (10 ms), 960 frames long (20 ms).
        let start = buffer_start(10_000_000_000, 480, rate, 960);
        assert_eq!(start, Duration::from_millis(9_970));
        // A negative delay is ignored.
        assert_eq!(
            buffer_start(10_000_000_000, -5, rate, 0),
            Duration::from_secs(10)
        );
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
