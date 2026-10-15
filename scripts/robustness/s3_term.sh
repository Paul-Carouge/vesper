# S3/S7: UI recording a game; send $SIG ($1, default TERM) to vesper after ~8 s of recording.
. $LABROOT/lib.sh
. $LABROOT/audio_up.sh
SIG=${1:-TERM}
audio_up
mkdir -p $L/cfg/vesper
printf 'game_source=true\nreplay=false\n' > $L/cfg/vesper/settings
gamescope --backend headless -W 1280 -H 800 -- bash $LABROOT/ui_child.sh > $L/log/gs.log 2>&1 &
GS=$!
sleep 14
V=$(pgrep -x vesper | head -1)
echo "vesper pid $V"; grep -E "start-recording|Started|recording" $L/log/ui.log | cut -c1-200 | tail -3
sleep 8
echo "sending SIG$SIG"
t0=$(date +%s%N)
kill -$SIG $V
for i in $(seq 1 100); do kill -0 $V 2>/dev/null || break; sleep 0.2; done
t1=$(date +%s%N)
echo "vesper alive after signal: $(kill -0 $V 2>/dev/null && echo yes || echo no) took $(( (t1 - t0) / 1000000 )) ms"
grep -vE "Stats|start-pipeline" $L/log/ui.log | cut -c1-250 | tail -12
f=$(last_file); echo "$f"; info "$f"
ffprobe -v error -show_entries packet=stream_index,pts_time -of csv=p=0 "$f" > $L/log/pts.csv
for s in 0 1 2 3; do echo "stream $s: n=$(grep -c "^$s," $L/log/pts.csv) last=$(grep "^$s," $L/log/pts.csv | sort -t, -k2 -g | tail -1)"; done
kill $GS 2>/dev/null; wait $GS 2>/dev/null
