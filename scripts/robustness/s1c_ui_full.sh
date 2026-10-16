# S1c: UI, disk full mid-recording with the replay buffer on; then a replay save onto the full disk
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
mkdir -p $L/small $L/cfg/vesper
mount -t tmpfs -o size=3m tmpfs $L/small
printf 'game_source=true\nreplay=true\nreplay_seconds=30\n' > $L/cfg/vesper/settings
# the UI's output folder comes from XDG_VIDEOS_DIR
echo "XDG_VIDEOS_DIR=\"$L/small\"" > $L/cfg/user-dirs.dirs
gamescope --backend headless -W 1280 -H 800 -- env DEMO=recording,ctrl+s DELAY=8000 bash $LABROOT/ui_child.sh > $L/log/gs.log 2>&1 &
GS=$!
sleep 30
V=$(pid_of vesper)
echo "vesper alive: $(kill -0 $V 2>/dev/null && echo yes || echo no)"
grep -vE "Stats|start-pipeline" $L/log/ui.log | cut -c1-250 | tail -12
df -h $L/small | tail -1
ls -la $L/small/Vesper
kill -TERM $V; sleep 1
kill $GS 2>/dev/null; wait $GS 2>/dev/null
