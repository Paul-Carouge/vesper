# R1: replay buffer (30 s) while the game crashes: how much of the saved clip is the game, how much a frozen frame?
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
mkdir -p $L/cfg/vesper
printf 'game_source=true\nreplay=true\nreplay_seconds=30\n' > $L/cfg/vesper/settings
# The demo presses Ctrl+S ${SAVE_AFTER:-45} s after Vesper started; the game dies ~13 s after it started.
gamescope --backend headless -W 1280 -H 800 -- env DEMO=ctrl+s DELAY=$(( ${SAVE_AFTER:-45} * 1000 )) bash $LABROOT/ui_child.sh > $L/log/gs.log 2>&1 &
GS=$!
sleep 15
echo "== game crashes (kill -9) at ~t=15"
kill_game_client
sleep $(( ${SAVE_AFTER:-45} + 4 - 13 ))
V=$(pid_of vesper)
grep -E "save-replay|saved replay|replay" $L/log/ui.log | cut -c1-200
f=$(ls -t $L/vid/Vesper/*.mkv 2>/dev/null | head -1); echo "$f"
[ -n "$f" ] && info "$f"
# freezedetect: intervals where the picture does not change
[ -n "$f" ] && ffmpeg -v info -i "$f" -map 0:v -vf "freezedetect=n=-60dB:d=0.5" -an -f null - 2>&1 | grep -E "freeze_(start|duration)" | cut -c1-120
kill -TERM $V; sleep 1
kill $GS 2>/dev/null; wait $GS 2>/dev/null
