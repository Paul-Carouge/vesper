# F1: hostile clients on the live socket while a real game is recorded; and idle clients hogging the slots
. $LABROOT/lib.sh
start_game f1
$VESPER --record-game 40 > $L/log/vesper-f1.log 2>&1 &
V=$!
sleep 6
echo "== garbage + churn for 12 s: $(rss $V)"
python3 -I $LABROOT/flood.py garbage 12 &
P1=$!
python3 -I $LABROOT/flood.py churn 12 &
P2=$!
wait $P1 $P2
echo "after: $(rss $V)"
echo "== 40 idle clients hold the slots (a real game connecting now?)"
python3 -I $LABROOT/flood.py idle 8 30 &
sleep 4
echo "start a second game while 40 idle clients are connected"
gamescope --backend headless -W 800 -H 600 -- env SteamAppId=2 vkcube --wsi xcb --c 300 --width 800 --height 600 > /dev/null 2>&1 &
sleep 12
echo "== vesper log"
wait $V
grep -vE "Stats" $L/log/vesper-f1.log | cut -c1-200 | tail -6
f=$(last_file); info "$f"
ffmpeg -v error -i "$f" -f null - 2>&1 | head -3
stop_game
