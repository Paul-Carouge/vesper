# R2: the UI records AND streams a game; SIGNAL ($SIG, default TERM) arrives
# after 10 s of both. Checks: exit time, recording finalized (all tracks end
# together), the receiver saw a clean end (stream closed) or not, no leftover
# host/ffmpeg children. Env: SIG, WHEN (seconds of recording+live before the signal, 10).
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
. $LABROOT/audio_up.sh
SIG=${SIG:-TERM}
audio_up
mkdir -p $L/cfg/vesper
printf 'game_source=true\nreplay=true\nreplay_seconds=30\nlive_service=custom\nlive_url=rtmp://127.0.0.1:1935/live/test\nlive_height=1080\nlive_fps=60\nlive_kbps=6000\nlive_codec=h264\n' > $L/cfg/vesper/settings
ffmpeg -y -hide_banner -loglevel warning -listen 1 -i rtmp://127.0.0.1:1935/live/test -c copy -f flv $L/live-term.flv > $L/log/rtmp-term.log 2>&1 &
SRV=$!
sleep 1
export DEMO="recording,ctrl+l" DELAY=8000
gamescope --backend headless -W 1280 -H 800 -- bash $LABROOT/ui_child.sh > $L/log/gs.log 2>&1 &
GS=$!
sleep $((8 + ${WHEN:-10} + 4))
V=$(pid_of vesper)
echo "vesper pid $V; $(rss $V)"
grep -E "Started|go-live|live: connected|Stream\(Live" $L/log/ui.log | cut -c1-120 | tail -4
t0=$(date +%s%N)
if [ "$SIG" = GAMEKILL ]; then
  kill_game_client
  echo "game killed; watching 25 s"
  sleep 25
  grep -vE "Stats|start-pipeline|inotify|zbus|randr" $L/log/ui.log | cut -c1-200 | tail -8
  echo "recording still running: $(grep -c 'Finished' $L/log/ui.log) finished events"
  SIG=TERM
  t0=$(date +%s%N)
fi
kill -$SIG $V
for i in $(seq 1 150); do kill -0 $V 2>/dev/null || break; sleep 0.2; done
echo "alive after SIG$SIG: $(kill -0 $V 2>/dev/null && echo yes || echo no), took $(( ($(date +%s%N) - t0) / 1000000 )) ms"
grep -vE "Stats|start-pipeline|inotify|zbus|randr" $L/log/ui.log | cut -c1-200 | tail -10
f=$(last_file); echo "$f"; info "$f"
ffprobe -v error -show_entries packet=stream_index,pts_time -of csv=p=0 "$f" > $L/log/pts.csv
for s in 0 1 2 3; do echo "stream $s: n=$(grep -c "^$s," $L/log/pts.csv) last=$(grep "^$s," $L/log/pts.csv | sort -t, -k2 -g | tail -1)"; done
sleep 1
echo "ffmpeg server still running: $(kill -0 $SRV 2>/dev/null && echo yes || echo no)"
cat $L/log/rtmp-term.log | head -5
ffprobe -v error -show_entries format=duration -of csv=p=0 $L/live-term.flv
echo "stray processes: $(grep -lx vesper-vst-host /proc/[0-9]*/comm 2>/dev/null | wc -l)"
kill $GS 2>/dev/null; wait $GS 2>/dev/null
kill $SRV 2>/dev/null
