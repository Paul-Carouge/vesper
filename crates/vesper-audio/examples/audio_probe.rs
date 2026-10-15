//! Records ~5 s of desktop audio and microphone to one MKA in the temp dir,
//! printing peaks, packet counts and pts continuity.
//!
//! `cargo run -p vesper-audio --example audio_probe [seconds]`

use std::sync::mpsc;
use std::time::Duration;

use ffmpeg::ffi as ff;
use ffmpeg_next as ffmpeg;
use vesper_audio::{AudioCapture, Source};
use vesper_core::EncodedPacket;

type Error = vesper_audio::Error;

fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let secs: u64 = std::env::args().nth(1).map_or(Ok(5), |s| s.parse())?;
    let mut tracks = Vec::new();
    for source in [Source::Desktop, Source::Microphone] {
        let (tx, rx) = mpsc::channel::<EncodedPacket>();
        let capture = AudioCapture::start(source, move |p| drop(tx.send(p)))?;
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
    for (source, capture, rx) in tracks {
        let head = capture.extradata().to_vec();
        capture.stop();
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
            "{source:?}: {} packets, {:.3} s, max pts step {max_step:?}, first pts {first:?}",
            packets.len(),
            (last - first).as_secs_f64()
        );
        if max_step > Duration::from_millis(20) {
            return Err(format!("{source:?}: gap of {max_step:?}").into());
        }
        collected.push((head, packets));
    }
    let path = std::env::temp_dir().join("vesper-audio-probe.mka");
    write_mka(&path, &collected)?;
    println!("wrote {}", path.display());
    Ok(())
}

/// Muxes the tracks, rebased to the earliest pts, into a Matroska file.
fn write_mka(
    path: &std::path::Path,
    tracks: &[(Vec<u8>, Vec<EncodedPacket>)],
) -> Result<(), Error> {
    let mut out = ffmpeg::format::output(path)?;
    for (head, _) in tracks {
        let mut st = out.add_stream(ffmpeg::encoder::find(ffmpeg::codec::Id::OPUS))?;
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
        .filter_map(|(_, p)| p.first().map(|p| p.pts))
        .min()
        .unwrap_or_default();
    let mut all: Vec<(usize, &EncodedPacket)> = tracks
        .iter()
        .enumerate()
        .flat_map(|(i, (_, p))| p.iter().map(move |p| (i, p)))
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
