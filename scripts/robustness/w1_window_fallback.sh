# W1: game mode, a Steam game without the layer, replay buffer on, then a
# recording: the game window is asked for once (the lab has no portal, so it
# fails), never again in a loop, and the recording fails cleanly. Then a
# layer game starts and is recorded through the layer, not its window.
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
mkdir -p $L/cfg/vesper
printf 'game_source=true\nreplay=true\nreplay_seconds=10\n' > $L/cfg/vesper/settings
rm -f $L/cfg/vesper/window-tokens
gamescope --backend headless -W 1280 -H 800 -- bash -c '
env SteamAppId=4242 VESPER_CAPTURE_DISABLE=1 vkcube --wsi xcb --width 640 --height 480 > $L/log/cube.log 2>&1 &
sleep 2
echo $$ > $L/run/vesper.pid
exec env VESPER_DEMO=recording VESPER_DEMO_DELAY=4000 $VESPER > $L/log/ui.log 2>&1' > $L/log/gs.log 2>&1 &
GS=$!
sleep 35
V=$(pid_of vesper)
echo "vesper alive: $(kill -0 $V 2>/dev/null && echo yes || echo no)"
echo "window asks (want 1): $(grep -c 'asking for the game window' $L/log/ui.log)"
grep -E "game window|No frame|recording failed" $L/log/ui.log | cut -c1-200
kill -TERM $V
sleep 2
kill $GS 2>/dev/null
wait $GS 2>/dev/null
ls $L/vid/Vesper 2>/dev/null | head
