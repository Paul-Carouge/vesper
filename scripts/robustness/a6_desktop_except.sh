# A6: Desktop audio leaves out the apps that have their own track.
# A fake "firefox" plays a 440 Hz tone and another app white noise, both on
# the default sink; a third app plays 1000 Hz on labsink2 (not the default).
#   except: Desktop + firefox tracks, firefox left out of Desktop. Mid-run
#           (seconds of the recording): firefox stops at 3 and is back at 4
#           (its new stream must be linked again); the noise moves to
#           labsink2 at 6 and back at 8; the default sink is labsink2 from
#           10 to 13. Desktop: no 440 Hz ever, noise but not from 6 to 8 nor
#           from 10 to 13, 1000 Hz from 10 to 13.
#           WirePlumber then moves the 1000 Hz stream to the default sink:
#           from 13 on, 1000 Hz is in Desktop (and in plain), as in the monitor.
#   plain:  same tracks, nothing left out (the sink's monitor, as on master).
#   cost:   CPU of the audio threads, 1 left-out app + 5 other streams, both
#           modes (PROBE: an optimized audio_probe for real numbers).
# Bands per second of each track: a6_bands.py.
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
PROBE=${PROBE:-$R/target/debug/examples/audio_probe}
SECS=${SECS:-12}
# node_id <node.name>: the node's id in the lab's graph
node_id() {
  pw-dump | python3 -I -c 'import json,sys; print(next(o["id"] for o in json.load(sys.stdin) if o.get("info",{}).get("props",{}).get("node.name")==sys.argv[1]))' "$1"
}
move_to() { pw-metadata $(node_id $1) target.object $2 Spa:Id > /dev/null; }
default_sink() { pw-metadata 0 default.configured.audio.sink "{\"name\":\"$1\"}" Spa:String:JSON > /dev/null; }
echo "default sink: $(pw-metadata 0 default.audio.sink 2>/dev/null | grep -o "value:'[^']*'")"
# play <pidfile> <lavfi source> <app name> <binary> [pw-cat args...]
play() {
  local pidfile=$1 src=$2 name=$3 bin=$4
  shift 4
  ffmpeg -loglevel quiet -re -f lavfi -i "$src" -f s16le -ar 48000 -ac 2 - 2>/dev/null |
    pw-cat -p --raw --rate 48000 --channels 2 --format s16 \
      -P "{ application.name = \"$name\" application.process.binary = \"$bin\" node.name = \"$name\" }" "$@" - > $L/log/play-$pidfile.log 2>&1 &
  echo $! > $L/run/$pidfile.pid
}
stop_play() { kill $(pid_of $1) 2>/dev/null; }
play firefox "sine=frequency=440:sample_rate=48000" Firefox firefox
play noise "anoisesrc=color=white:amplitude=0.1:sample_rate=48000" Noiser noiser
play other "sine=frequency=1000:sample_rate=48000" Elsewhere elsewhere --target labsink2
sleep 2
probe() { # probe <name> <except> : records SECS s of Desktop, Microphone, Firefox
  VESPER_PROBE_DESKTOP_EXCEPT=$2 RUST_LOG=${RUST_LOG:-warn} timeout 60 $PROBE $SECS Firefox > $L/log/a6-$1.log 2>&1 &
  echo $! > $L/run/probe.pid
}
finish() { # finish <name>
  wait $(pid_of probe); echo "$1: probe exit $?"
  grep -E "packets|rror|WARN" $L/log/a6-$1.log | cut -c1-160
  cp /tmp/vesper-audio-probe.mka $L/log/a6-$1.mka
  python3 -I $LABROOT/a6_bands.py $L/log/a6-$1.mka
}
echo "== except (see the header for the timeline)"
SECS=16 probe except Firefox
sleep 3
stop_play firefox
sleep 1
play firefox "sine=frequency=440:sample_rate=48000" Firefox firefox
sleep 2
move_to Noiser labsink2
sleep 2
move_to Noiser labsink
sleep 2
default_sink labsink2
sleep 3
default_sink labsink
echo "default sink after: $(pw-metadata 0 default.audio.sink 2>/dev/null | grep -o "value:'[^']*'")"
finish except
echo "== plain"
probe plain ""
finish plain
echo "== cost: 1 left-out app + 5 other streams"
for n in 1 2 3 4; do
  play extra$n "anoisesrc=color=pink:amplitude=0.05:sample_rate=48000" Extra$n extra$n
done
sleep 1
for mode in plain except; do
  ex=""; [ $mode = except ] && ex=Firefox
  SECS=24 probe cost-$mode "$ex"
  sleep 6
  echo "cost $mode: $(python3 -I $LABROOT/cost.py $(children_named $(pid_of probe) audio_probe) 15 vesper-audio)"
  wait $(pid_of probe)
done
for p in firefox noise other extra1 extra2 extra3 extra4; do stop_play $p; done
