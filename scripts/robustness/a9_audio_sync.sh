# A9: track alignment, mix and live (AAC) latency, CPU of the audio threads.
# A "gameapp" plays 440 Hz on the default sink (labsink), "other" 1000 Hz on
# labsink2, noise on labmic. audio_probe records TRACKS (its
# VESPER_PROBE_TRACKS) for SECS with EXCEPT left out of the desktop, as the
# app does for apps with their own track. Each track's latency is how long
# after its pts its packets come out: tracks of one moment must match
# (monitor, app and desktop-except alike), and the mix and AAC tap must not
# lag by a stalled track's 0.5 s.
#   TRACKS="desktop app:gameapp output:labsink2"   all three within ~1 ms
#   TRACKS="output:labsink2 app:nosuchapp" EXCEPT=nosuchapp AAC=1
#                                                  no stall in the mix
# PROBE: an optimized audio_probe (cargo build --release -p vesper-audio
# --example audio_probe).
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
PROBE=${PROBE:-$R/target/release/examples/audio_probe}
SECS=${SECS:-12}
TRACKS=${TRACKS:-desktop app:gameapp output:labsink2}
EXCEPT=${EXCEPT-gameapp}
play() { # play <name> <lavfi source> [pw-cat args...]
  local name=$1 src=$2
  shift 2
  ffmpeg -loglevel quiet -re -f lavfi -i "$src" -f s16le -ar 48000 -ac 2 - 2>/dev/null |
    pw-cat -p --raw --rate 48000 --channels 2 --format s16 \
      -P "{ application.name = \"$name\" application.process.binary = \"$name\" node.name = \"$name\" }" "$@" - > $L/log/play-$name.log 2>&1 &
  echo $! > $L/run/$name.pid
}
play gameapp "sine=frequency=440:sample_rate=48000" ${GAME_SINK:+--target $GAME_SINK}
play other "sine=frequency=1000:sample_rate=48000" --target labsink2
play voice "anoisesrc=color=pink:amplitude=0.1:sample_rate=48000" --target labmic
sleep 2
echo "== TRACKS=$TRACKS EXCEPT=$EXCEPT AAC=${AAC:-}"
VESPER_PROBE_TRACKS="$TRACKS" VESPER_PROBE_DESKTOP_EXCEPT="$EXCEPT" VESPER_PROBE_AAC=${AAC:-} \
  NO_COLOR=1 RUST_LOG=${RUST_LOG:-info} timeout 60 $PROBE $SECS > $L/log/a9.log 2>&1 &
echo $! > $L/run/probe.pid
sleep 4
p=$(children_named $(pid_of probe) "$(basename $PROBE | cut -c1-15)")
[ -n "$p" ] && python3 -I $LABROOT/r2_sample.py $p $((SECS - 6)) probe | grep -v "^GPU"
wait $(pid_of probe); echo "probe exit $?"
grep -E "latency|packets|timing|defined target|WARN|rror" $L/log/a9.log | cut -c1-170
grep -E "defined target" $L/log/wp.log | cut -c1-170 | head -5
for n in gameapp other voice; do kill $(pid_of $n) 2>/dev/null; done
