# L2: games starting/quitting (sizes alternate) while Vesper records; fd/thread counts, file validity
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
$VESPER --record-game 50 > $L/log/vesper-l2.log 2>&1 &
V=$!
sleep 1
echo "before: $(rss $V)"
gamescope --backend headless -W 1280 -H 720 -- bash $LABROOT/game_loop.sh > $L/log/gs.log 2>&1
echo "games done; during: $(rss $V)"
wait $V
grep -vE "Stats" $L/log/vesper-l2.log | cut -c1-220 | tail -12
f=$(last_file); echo "$f"; info "$f"
ffprobe -v error -show_entries packet=stream_index,pts_time -of csv=p=0 "$f" > $L/log/pts.csv
for s in 0 1; do echo "stream $s: n=$(grep -c "^$s," $L/log/pts.csv) last=$(grep "^$s," $L/log/pts.csv | sort -t, -k2 -g | tail -1)"; done
ffmpeg -v error -i "$f" -f null - 2>&1 | head -3
