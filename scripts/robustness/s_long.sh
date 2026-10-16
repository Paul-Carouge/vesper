# S-long: 31 min game recording with a tone on the desktop track; samples RSS/fds/threads.
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
. $LABROOT/audio_up.sh
SECS=${LONG_SECS:-1900}
audio_up
pw-cli ls Node | grep node.name | tr '\n' ' '; echo
start_game long
$VESPER --record-game $SECS > $L/log/vesper-long.log 2>&1 &
V=$!
sleep 3
t=0
while kill -0 $V 2>/dev/null; do
  echo "t=$t vesper: $(rss $V)" >> $L/log/long-samples.log
  sleep 30
  t=$((t+30))
done
wait $V; echo "rc=$?"
grep -vE "Stats" $L/log/vesper-long.log | cut -c1-300 | tail -8
tail -2 $L/log/vesper-long.log | cut -c1-200
f=$(last_file); echo "$f"; info "$f"
ffprobe -v error -show_entries packet=stream_index,pts_time -of csv=p=0 "$f" > $L/log/long-pts.csv
for s in 0 1 2 3; do echo "stream $s: n=$(grep -c "^$s," $L/log/long-pts.csv) last=$(grep "^$s," $L/log/long-pts.csv | sort -t, -k2 -g | tail -1)"; done
stop_game
