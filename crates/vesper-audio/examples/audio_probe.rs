//! Records ~5 s of desktop audio, microphone and the given applications,
//! plus their mix, to one MKA in the temp dir, printing peaks, packet counts
//! and pts continuity.
//!
//! `cargo run -p vesper-audio --example audio_probe [seconds] [app | output:<sink>...]`
//!
//! `VESPER_PROBE_VOICE=desktop,microphone` runs the voice filter chain on
//! those tracks (the `Source` names, lowercase), e.g. to check that its
//! delay is compensated against an unfiltered `output:<sink>` track.

use std::sync::mpsc;
use std::time::Duration;

use ffmpeg::ffi as ff;
use ffmpeg_next as ffmpeg;
use vesper_audio::{AudioCapture, Mix, Options, Source, monotonic_now};
use vesper_core::EncodedPacket;

type Error = vesper_audio::Error;

fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let secs: u64 = std::env::args().nth(1).map_or(Ok(5), |s| s.parse())?;
    // `output:<node.name>` records a sink's monitor, anything else an app.
    let apps = std::env::args()
        .skip(2)
        .map(|a| match a.strip_prefix("output:") {
            Some(node) => Source::Output(node.into()),
            None => Source::App(a),
        });
    let sources: Vec<_> = [Source::Desktop, Source::Microphone]
        .into_iter()
        .chain(apps)
        .collect();
    let voice = std::env::var("VESPER_PROBE_VOICE").unwrap_or_default();
    let mut tracks = Vec::new();
    let anchor = monotonic_now();
    let (mix_tx, mix_rx) = mpsc::channel::<EncodedPacket>();
    let mix = Mix::start(anchor, move |p| drop(mix_tx.send(p)))?;
    let mut inputs: Vec<_> = sources.iter().map(|_| Some(mix.input())).collect();
    for (source, input) in sources.into_iter().zip(&mut inputs) {
        let (tx, rx) = mpsc::channel::<EncodedPacket>();
        let options = Options {
            anchor,
            muted: false,
            volume_db: 0.0,
            mix: input.take(),
            process: voice_chain(&voice, &source),
            on_state: None,
        };
        let capture = AudioCapture::start(source.clone(), options, move |p| drop(tx.send(p)))?;
        tracks.push((source, capture, rx));
    }
    for _ in 0..secs * 2 {
        std::thread::sleep(Duration::from_millis(500));
        let peaks: Vec<_> = tracks
            .iter()
            .map(|(s, c, _)| format!("{s:?} {:.3}", c.peak()))
            .collect();
        println!("peak: {}", peaks.join("  "));
    }
    let mut collected = Vec::new();
    let mut streams: Vec<_> = tracks
        .into_iter()
        .map(|(source, capture, rx)| {
            let head = capture.extradata().to_vec();
            capture.stop();
            (format!("{source:?}"), head, rx)
        })
        .collect();
    // After its inputs: flushes the mix.
    let mix_head = mix.extradata().to_vec();
    drop(mix);
    streams.insert(0, ("Mix".into(), mix_head, mix_rx));
    for (source, head, rx) in streams {
        let packets: Vec<_> = rx.iter().collect();
        let max_step = packets
            .windows(2)
            .map(|w| w[1].pts.checked_sub(w[0].pts).ok_or("pts went backwards"))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max()
            .unwrap_or_default();
        let first = packets.first().ok_or("no packets")?.pts;
        let last = packets.last().ok_or("no packets")?.pts;
        println!(
            "{source}: {} packets, {:.3} s, max pts step {max_step:?}, first pts {first:?}",
            packets.len(),
            (last - first).as_secs_f64()
        );
        if max_step > Duration::from_millis(20) {
            return Err(format!("{source}: gap of {max_step:?}").into());
        }
        collected.push((source, head, packets));
    }
    let path = std::env::temp_dir().join("vesper-audio-probe.mka");
    write_mka(&path, &collected)?;
    println!("wrote {}", path.display());
    Ok(())
}

/// The voice chain, if `voice` names this source.
fn voice_chain(voice: &str, source: &Source) -> Option<vesper_audio::Process> {
    let name = format!("{source:?}").to_lowercase();
    if !voice.split(',').any(|v| !v.is_empty() && name == v) {
        return None;
    }
    let (mut chain, _) = vesper_dsp::live(&vesper_dsp::FilterSpec::voice_chain(), 2);
    Some(Box::new(move |buf| {
        chain.process(buf);
        chain.latency()
    }))
}

/// Muxes the tracks, rebased to the earliest pts, into a Matroska file.
fn write_mka(
    path: &std::path::Path,
    tracks: &[(String, Vec<u8>, Vec<EncodedPacket>)],
) -> Result<(), Error> {
    let mut out = ffmpeg::format::output(path)?;
    for (name, head, _) in tracks {
        let mut st = out.add_stream(ffmpeg::encoder::find(ffmpeg::codec::Id::OPUS))?;
        let mut title = ffmpeg::Dictionary::new();
        title.set("title", name);
        st.set_metadata(title);
        st.set_time_base((1, 48_000));
        // SAFETY: fresh codecpar of a stream we own; extradata is av_malloc'd.
        unsafe {
            let par = (*st.as_mut_ptr()).codecpar;
            (*par).codec_type = ff::AVMediaType::AVMEDIA_TYPE_AUDIO;
            (*par).codec_id = ff::AVCodecID::AV_CODEC_ID_OPUS;
            (*par).sample_rate = 48_000;
            ff::av_channel_layout_default(&mut (*par).ch_layout, 2);
            (*par).initial_padding = i32::from(u16::from_le_bytes([head[10], head[11]]));
            let buf = ff::av_mallocz(head.len() + ff::AV_INPUT_BUFFER_PADDING_SIZE as usize);
            std::ptr::copy_nonoverlapping(head.as_ptr(), buf as *mut u8, head.len());
            (*par).extradata = buf as *mut u8;
            (*par).extradata_size = head.len() as i32;
        }
    }
    out.write_header()?;
    let base = tracks
        .iter()
        .filter_map(|(_, _, p)| p.first().map(|p| p.pts))
        .min()
        .unwrap_or_default();
    let mut all: Vec<(usize, &EncodedPacket)> = tracks
        .iter()
        .enumerate()
        .flat_map(|(i, (_, _, p))| p.iter().map(move |p| (i, p)))
        .collect();
    all.sort_by_key(|(_, p)| p.pts);
    for (i, p) in all {
        let mut pkt = ffmpeg::Packet::copy(&p.data);
        let pts = ((p.pts - base).as_nanos() * 48_000 / 1_000_000_000) as i64;
        pkt.set_stream(i);
        pkt.set_pts(Some(pts));
        pkt.set_dts(Some(pts));
        pkt.set_duration(960);
        pkt.rescale_ts((1, 48_000), out.stream(i).ok_or("stream")?.time_base());
        pkt.write_interleaved(&mut out)?;
    }
    out.write_trailer()?;
    Ok(())
}
