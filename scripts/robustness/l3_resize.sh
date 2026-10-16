# L3: resize storm of the game window while Vesper records (swapchain recreation loop)
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
start_game l3
$VESPER --record-game 30 > $L/log/vesper-l3.log 2>&1 &
V=$!
sleep 6
echo "before storm: $(rss $V) game: $(rss $(pid_of display-l3))"
DISPLAY=$(game_display l3) python3 -I $LABROOT/xresize.py ${COUNT:-300} 0.03
echo "after storm: $(rss $V) game: $(rss $(pid_of display-l3))"
echo "game alive: $(kill -0 $(pid_of display-l3) 2>/dev/null && echo 1 || echo 0); swapchains created: $(grep -c 'Creating swapchain' $L/log/game-l3.log)"
wait $V
grep -vE "Stats" $L/log/vesper-l3.log | cut -c1-200 | tail -12
f=$(last_file); info "$f"
ffmpeg -v error -i "$f" -f null - 2>&1 | head -3
stop_game
