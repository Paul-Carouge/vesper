# A10: audio capture under small quanta and CPU load. Plays the a9 sources,
# forces the graph QUANTUM (default 1024), optionally runs STRESS busy loops
# (games load every core), and records TRACKS for SECS with audio_probe
# (EXCEPT left out of the desktop, as a9). Reports the audio lost (silence
# the timeline inserted, past the startup offset of each track), buffers
# dropped, latencies, the probe's CPU / wakeups / mallocs per thread group
# (r2_sample.py, alloc shim when run through r2), the scheduling policy of
# its threads (1 = SCHED_FIFO) and pw-top's xrun counts (ERR).
#   r2 a10 a10_audio_rt.sh QUANTUM=64 STRESS=24
# PROBE: an optimized audio_probe (cargo build --release -p vesper-audio
# --example audio_probe).
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
PROBE=${PROBE:-$R/target/release/examples/audio_probe}
SECS=${SECS:-24}
QUANTUM=${QUANTUM:-1024}
STRESS=${STRESS:-0}
TRACKS=${TRACKS:-desktop output:labsink2 output:labmic app:gameapp}
EXCEPT=${EXCEPT-gameapp}
play() { # play <name> <lavfi source> [pw-cat args...]
  local name=$1 src=$2
  shift 2
  ffmpeg -loglevel quiet -re -f lavfi -i "$src" -f s16le -ar 48000 -ac 2 - 2>/dev/null |
    pw-cat -p --raw --rate 48000 --channels 2 --format s16 \
      -P "{ application.name = \"$name\" application.process.binary = \"$name\" node.name = \"$name\" }" "$@" - > $L/log/play-$name.log 2>&1 &
  echo $! > $L/run/$name.pid
}
pw-metadata -n settings 0 clock.force-quantum $QUANTUM > /dev/null
play gameapp "sine=frequency=440:sample_rate=48000"
play other "sine=frequency=1000:sample_rate=48000" --target labsink2
play voice "anoisesrc=color=pink:amplitude=0.1:sample_rate=48000" --target labmic
stress=()
for _ in $(seq 1 $STRESS); do
  bash -c 'while :; do :; done' &
  stress+=($!)
done
sleep 2
[ -n "${SHIM:-}" ] && PRE=$SHIM && export SHIM_OUT=$L/shim.out
echo "== QUANTUM=$QUANTUM STRESS=$STRESS TRACKS=$TRACKS EXCEPT=$EXCEPT PROBE=$PROBE"
LD_PRELOAD=${PRE:-} VESPER_PROBE_TRACKS="$TRACKS" VESPER_PROBE_DESKTOP_EXCEPT="$EXCEPT" \
  NO_COLOR=1 RUST_LOG=${RUST_LOG:-info} timeout 90 $PROBE $SECS > $L/log/a10.log 2>&1 &
echo $! > $L/run/probe.pid
sleep 6
p=$(children_named $(pid_of probe) "$(basename $PROBE | cut -c1-15)")
if [ -n "$p" ]; then
  python3 -I $LABROOT/r2_sample.py $p $((SECS - 10)) probe | grep -v "^GPU"
  echo "-- thread policies (comm policy)"
  for t in /proc/$p/task/*; do echo "$(cat $t/comm) $(awk '{print $41}' $t/stat)"; done | sort | uniq -c
  echo "-- pw-top ERR"
  timeout 5 pw-top -b -n 2 2>/dev/null | awk '$NF ~ /audio_probe|vesper|lab|gameapp|other|voice/' | tail -12
fi
wait $(pid_of probe); echo "probe exit $?"
kill "${stress[@]}" 2>/dev/null
# The first gap of each track is its startup offset.
awk '/timeline gap/ { for (i = 1; i <= NF; i++) if ($i == "gap:") n = $(i + 1); all += n; events++ }
     END { printf "gap frames %d in %d events (startup included)\n", all, events }' $L/log/a10.log
grep -c "dropped\|overflow" $L/log/a10.log | sed 's/^/drop lines: /'
grep -E "latency|packets|dropped|overflow|rror" $L/log/a10.log | cut -c1-170
for n in gameapp other voice; do kill $(pid_of $n) 2>/dev/null; done
