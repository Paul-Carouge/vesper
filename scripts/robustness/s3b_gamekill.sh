# S3b: UI recording a game; the game is killed -9 mid-recording
. $LABROOT/lib.sh
. $LABROOT/audio_up.sh
audio_up
mkdir -p $L/cfg/vesper
printf 'game_source=true\nreplay=false\n' > $L/cfg/vesper/settings
gamescope --backend headless -W 1280 -H 800 -- bash $LABROOT/ui_child.sh > $L/log/gs.log 2>&1 &
GS=$!
sleep 15
V=$(pgrep -x vesper | head -1)
echo "vesper pid $V; rec started: $(grep -c 'Started' $L/log/ui.log)"
kill -9 $(pgrep -x vkcube)
echo "killed the game"
sleep 8
grep -vE "Stats|start-pipeline" $L/log/ui.log | cut -c1-250 | tail -8
echo "vesper alive: $(kill -0 $V 2>/dev/null && echo yes || echo no)"
f=$(last_file); echo "$f"; info "$f"
ffprobe -v error -show_entries packet=stream_index,pts_time -of csv=p=0 "$f" > $L/log/pts.csv
for s in 0 1 2 3; do echo "stream $s: n=$(grep -c "^$s," $L/log/pts.csv) last=$(grep "^$s," $L/log/pts.csv | sort -t, -k2 -g | tail -1)"; done
kill -TERM $V; sleep 2
kill $GS 2>/dev/null; wait $GS 2>/dev/null
