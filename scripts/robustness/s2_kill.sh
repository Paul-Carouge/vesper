# S2: SIGKILL of the recorder mid-recording; the file must stay playable to the last cluster
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
. $LABROOT/audio_up.sh
audio_up
start_game s2
$VESPER --record-game 60 > $L/log/vesper-s2.log 2>&1 &
V=$!
sleep 12
kill -9 $V
wait $V 2>/dev/null
f=$(last_file); echo "$f"; ls -la "$f"
info "$f"
ffprobe -v error -show_entries packet=stream_index,pts_time -of csv=p=0 "$f" > $L/log/s2-pts.csv
for s in 0 1 2 3; do echo "stream $s: n=$(grep -c "^$s," $L/log/s2-pts.csv) last=$(grep "^$s," $L/log/s2-pts.csv | sort -t, -k2 -g | tail -1)"; done
ffmpeg -v error -i "$f" -f null - 2>&1 | head -5; echo "decode rc=$?"
stop_game
