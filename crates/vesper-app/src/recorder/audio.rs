//! Audio tracks: their settings, their captures and mix, and their packets.

use super::*;

/// Most audio tracks a recording has, the mix aside.
pub const MAX_TRACKS: usize = 8;

/// Most filters a track has.
pub const MAX_FILTERS: usize = 16;

/// Name of the mix in the files.
pub(super) const MIX_NAME: &str = "Mix";

/// One audio track as the user set it up.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub name: String,
    pub source: Source,
    /// Recorded at all.
    pub enabled: bool,
    pub muted: bool,
    /// See `vesper_audio::db_to_gain`.
    pub volume_db: f32,
    /// Processing, in order, before the volume.
    pub filters: Vec<TrackFilter>,
    /// Voice ducking and background music (not for voice tracks).
    pub ducking: Ducking,
    /// An app track's streams are left out of the desktop and output
    /// tracks, so they are not recorded twice.
    pub leave_desktop: bool,
}

/// How much, for [`Ducking`]: the duck's depth, the music's loudness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Level {
    Low,
    #[default]
    Medium,
    High,
}

impl Level {
    pub const ALL: [Self; 3] = [Self::Low, Self::Medium, Self::High];
}

/// A track that is a voice, lowered while a voice speaks, or played as
/// background music (see `vesper_dsp::Ducker`). A level is kept while its
/// switch is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ducking {
    /// A voice: the other tracks duck under it, and it does not duck.
    /// By default only the Microphone track (see [`Ducking::for_source`]).
    pub voice: bool,
    /// Lowered while the voice speaks, by `duck_level`.
    pub duck: bool,
    pub duck_level: Level,
    /// Background music at `music_level`: leveled under the voice, peaks
    /// tamed, voice band dipped, ducked softly unless `duck` says more.
    pub music: bool,
    pub music_level: Level,
}

impl Ducking {
    /// The default of a track on `source`: a voice for the Microphone.
    pub fn for_source(source: &Source) -> Self {
        Self {
            voice: *source == Source::Microphone,
            ..Self::default()
        }
    }

    pub fn settings(&self) -> DuckSettings {
        let duck_db = match self.duck_level {
            Level::Low => 6.0,
            Level::Medium => 12.0,
            Level::High => 20.0,
        };
        let lufs = match self.music_level {
            Level::Low => -30.0,
            Level::Medium => -26.0,
            Level::High => -22.0,
        };
        DuckSettings {
            duck_db: if self.duck { duck_db } else { 0.0 },
            music_lufs: self.music.then_some(lufs),
        }
    }
}

/// One filter of a track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackFilter {
    /// Off: left out of the chain, settings kept.
    pub enabled: bool,
    pub spec: FilterSpec,
}

/// The filters that run: the enabled ones, in order.
pub fn active_filters(filters: &[TrackFilter]) -> Vec<FilterSpec> {
    filters
        .iter()
        .filter(|f| f.enabled)
        .map(|f| f.spec)
        .collect()
}

impl Track {
    pub fn new(name: &str, source: Source) -> Self {
        Self {
            name: name.to_owned(),
            enabled: true,
            muted: false,
            volume_db: 0.0,
            filters: Vec::new(),
            ducking: Ducking::for_source(&source),
            leave_desktop: true,
            source,
        }
    }
}

/// The apps the desktop and output tracks leave out: those of the enabled
/// app tracks that ask to. A disabled track does not record its app, so the
/// desktop keeps it.
pub fn desktop_except(tracks: &[Track]) -> Vec<String> {
    tracks
        .iter()
        .filter(|t| t.enabled && t.leave_desktop)
        .filter_map(|t| match &t.source {
            Source::App(app) => Some(app.clone()),
            _ => None,
        })
        .collect()
}

/// Audio tracks and their captures, shared with the UI thread. One lock, so
/// a mute toggled while the picker is open applies once the captures start.
#[derive(Default)]
pub(super) struct Audio {
    /// The tracks, kept current by the UI (mute, volume, name).
    pub(super) tracks: Vec<Track>,
    pub(super) mix: bool,
    /// Set when `tracks` changed shape: the audio restarts once nothing records.
    pub(super) changed: bool,
    /// Running captures with their index in `tracks`, in file order.
    pub(super) running: Vec<(usize, AudioCapture)>,
    /// Filter controls of the running captures, with their index in `tracks`.
    pub(super) filters: Vec<(usize, LiveControl)>,
    /// Runs whenever a track does: a live stream's audio. First in the
    /// files when `mix_in_files`.
    pub(super) mixer: Option<Mix>,
    pub(super) mix_in_files: bool,
    /// The running live stream's audio, fed by the mix.
    pub(super) stream_tap: Option<Arc<AudioTap>>,
    /// The apps the running desktop and output tracks leave out.
    pub(super) exclusions: Option<Arc<Exclusions>>,
}

impl Audio {
    /// Number of audio streams in a file.
    pub(super) fn streams(&self) -> usize {
        usize::from(self.mix_in_files) + self.running.len()
    }

    /// Feeds `tap` (or nothing) with the mix, now and after audio restarts.
    pub(super) fn set_stream_tap(&mut self, tap: Option<Arc<AudioTap>>) {
        if let Some(m) = &self.mixer {
            m.set_tap(tap.as_ref().map(AudioTap::tap));
        }
        self.stream_tap = tap;
    }

    /// Audio stream descriptions for a new file, with the current names.
    pub(super) fn file_tracks(&self) -> Vec<AudioTrack> {
        let track = |name: &str, extradata: &[u8]| AudioTrack {
            name: name.to_owned(),
            sample_rate: SAMPLE_RATE,
            channels: u32::from(CHANNELS),
            extradata: extradata.to_vec(),
        };
        let mix = self
            .mixer
            .iter()
            .filter(|_| self.mix_in_files)
            .map(|m| track(MIX_NAME, m.extradata()));
        let tracks = self.running.iter().map(|(i, capture)| {
            let name = self.tracks.get(*i).map_or("", |t| t.name.as_str());
            track(name, capture.extradata())
        });
        mix.chain(tracks).collect()
    }

    pub(super) fn track(&mut self, index: usize) -> (Option<&mut Track>, Option<&AudioCapture>) {
        let capture = self.running.iter().find(|(i, _)| *i == index);
        (self.tracks.get_mut(index), capture.map(|(_, c)| c))
    }
}

pub(super) type AudioSources = Arc<Mutex<Audio>>;

/// Writes an audio packet of file stream `index` (the video aside) to the sinks.
pub(super) fn write_audio(sinks: &SharedSinks, index: usize, packet: EncodedPacket) {
    let mut s = lock(sinks);
    if let Some(r) = s.rec.as_mut()
        && let Err(e) = r.mux.write_audio(index, &packet)
    {
        log!("audio track {index}: {e}");
    }
    if let Some(ring) = s.ring.as_mut() {
        ring.push_audio(index, packet);
    }
}

/// File stream of a track among the audio streams: after the mix, if any.
pub(super) struct StreamIndex {
    first: Arc<AtomicUsize>,
    track: usize,
}

impl StreamIndex {
    pub(super) fn new(first: &Arc<AtomicUsize>, track: usize) -> Self {
        Self {
            first: first.clone(),
            track,
        }
    }

    pub(super) fn get(&self) -> usize {
        self.first.load(Ordering::Relaxed) + self.track
    }
}

/// Starts the enabled tracks and their mix. A track that fails to start is
/// left out of the files (e.g. no microphone); the mix needs two tracks.
pub(super) fn start_audio(sources: &AudioSources, sinks: &SharedSinks, on_event: &OnEvent) {
    let (tracks, mix) = {
        let mut audio = lock(sources);
        audio.changed = false;
        (audio.tracks.clone(), audio.mix)
    };
    let enabled: Vec<usize> = (0..tracks.len()).filter(|&i| tracks[i].enabled).collect();
    // One clock for every track, so the mix lines their samples up.
    let anchor = monotonic_now();
    // The mix always runs with the tracks: a live stream sends it. Only
    // encoded to Opus when the files have it.
    let want_mix = mix && enabled.len() >= 2;
    let mut mixer = None;
    let mut mix_failed = false;
    if !enabled.is_empty() {
        let sinks = sinks.clone();
        match Mix::start(anchor, move |packet| write_audio(&sinks, 0, packet)) {
            Ok(m) => {
                m.set_encoding(want_mix);
                mixer = Some(m);
            }
            Err(e) => {
                log!("running without the mix: {e}");
                mix_failed = want_mix;
            }
        }
    }
    let mut in_files = want_mix && mixer.is_some();
    // Every input exists before any track runs.
    let mut inputs: Vec<_> = enabled
        .iter()
        .map(|_| mixer.as_ref().map(Mix::input))
        .collect();
    // Streams before the tracks: the mix, until it is dropped below.
    let first = Arc::new(AtomicUsize::new(usize::from(in_files)));
    let mut running = Vec::new();
    let mut controls = Vec::new();
    // The microphones speak into it, the other tracks duck under it.
    let voice = Arc::new(VoiceKey::default());
    // An app track holds its app out of the desktop while it captures;
    // held from now, so the desktop starts without it. One that fails to
    // start, or loses its source later, gives it back to the desktop.
    let exclusions = Exclusions::new(Some(Box::new({
        let on_event = on_event.clone();
        move |apps| on_event(Event::DesktopExcept(apps))
    })));
    let holds: Vec<_> = desktop_except(&tracks)
        .iter()
        .map(|app| exclusions.hold(app))
        .collect();
    let mut failed = Vec::new();
    for (input, &i) in inputs.iter_mut().zip(&enabled) {
        let track = &tracks[i];
        let (index, sinks) = (StreamIndex::new(&first, running.len()), sinks.clone());
        let (mut chain, control) =
            vesper_dsp::live(&active_filters(&track.filters), usize::from(CHANNELS));
        let options = Options {
            anchor,
            muted: track.muted,
            volume_db: track.volume_db,
            mix: input.take(),
            // Always there, even empty: filters can be added while it runs.
            process: Some(Box::new(move |buf| {
                chain.process(buf);
                chain.latency()
            })),
            on_state: Some(Box::new({
                let (on_event, name) = (on_event.clone(), track.name.clone());
                move |state| on_event(Event::AudioTrack(name.clone(), state))
            })),
            on_signal: Some(Box::new({
                let on_event = on_event.clone();
                move || on_event(Event::AudioSignal)
            })),
            voice: Some(voice.clone()),
            ducking: track.ducking.settings(),
            speaks: track.ducking.voice,
            exclusions: match &track.source {
                Source::App(_) if !track.leave_desktop => None,
                _ => Some(exclusions.clone()),
            },
        };
        match AudioCapture::start(track.source.clone(), options, move |packet| {
            write_audio(&sinks, index.get(), packet)
        }) {
            Ok(capture) => {
                running.push((i, capture));
                controls.push((i, control));
            }
            Err(e) => {
                log!("running without the {:?} track: {e}", track.name);
                failed.push(track.name.as_str());
            }
        }
    }
    drop(holds);
    // A mix of one track would be a copy of it.
    let dropped = in_files && running.len() < 2;
    if dropped {
        if let Some(m) = &mixer {
            m.set_encoding(false);
        }
        in_files = false;
        // The tracks move up one stream: before any file or replay uses them.
        first.store(0, Ordering::Relaxed);
    }
    if let Some(text) = missing_audio(&failed, mix_failed || dropped) {
        on_event(Event::AudioMissing(text));
    }
    // Started outside the lock (it blocks until PipeWire is ready); the
    // latest mute and volume applied under it, so no change is missed.
    let mut audio = lock(sources);
    for ((i, capture), (_, control)) in running.iter().zip(&mut controls) {
        if let Some(t) = audio.tracks.get(*i) {
            capture.set_muted(t.muted);
            capture.set_volume_db(t.volume_db);
            capture.set_ducking(t.ducking.settings());
            capture.set_speaks(t.ducking.voice);
            control.set(&active_filters(&t.filters));
        }
    }
    (audio.running, audio.mixer, audio.filters) = (running, mixer, controls);
    audio.exclusions = Some(exclusions);
    audio.mix_in_files = in_files;
    // A stream running across the restart goes on with the new mix.
    let tap = audio.stream_tap.clone();
    audio.set_stream_tap(tap);
}

/// One notice for the tracks that could not start, and a lost mix.
pub(super) fn missing_audio(failed: &[&str], no_mix: bool) -> Option<String> {
    match (failed, no_mix) {
        ([], false) => None,
        ([], true) => Some(tr!("Recording without the mix track").to_owned()),
        ([one], _) => Some(tr!("Recording without the audio track {}", one)),
        (many, _) => Some(tr!(
            "Recording without the audio tracks {}",
            many.join(", ")
        )),
    }
}

/// Empties the ring's audio and sizes it for the running streams.
pub(super) fn reset_ring_audio(sources: &AudioSources, sinks: &SharedSinks) {
    let streams = lock(sources).streams();
    if let Some(ring) = lock(sinks).ring.as_mut() {
        ring.audio = vec![VecDeque::new(); streams];
    }
}

/// Stops the captures, then the mix: their last packets go to the sinks, so
/// never call it holding the sinks lock.
pub(super) fn stop_audio(sources: &AudioSources) {
    let (running, mixer) = {
        let mut audio = lock(sources);
        // Its tracks leave one by one: not worth telling.
        if let Some(ex) = audio.exclusions.take() {
            ex.quiet();
        }
        audio.filters.clear();
        audio.mix_in_files = false;
        (std::mem::take(&mut audio.running), audio.mixer.take())
    };
    drop(running);
    drop(mixer);
}
