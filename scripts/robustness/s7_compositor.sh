# S7c: the compositor dies (SIGKILL gamescope) while Vesper's UI records a game (vkcube is a client too)
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
. $LABROOT/audio_up.sh
audio_up
mkdir -p $L/cfg/vesper
printf 'game_source=true\nreplay=false\n' > $L/cfg/vesper/settings
gamescope --backend headless -W 1280 -H 800 -- bash $LABROOT/ui_child.sh > $L/log/gs.log 2>&1 &
GS=$!
sleep 16
V=$(pid_of vesper)
echo "vesper pid $V rec: $(grep -c 'recording to' $L/log/ui.log)"
kill -9 $GS
sleep 5
echo "vesper alive after compositor death: $(kill -0 $V 2>/dev/null && echo yes || echo no)"
grep -vE "Stats|start-pipeline" $L/log/ui.log | cut -c1-250 | tail -6
f=$(last_file); echo "$f"; info "$f"
ffmpeg -v error -i "$f" -f null - 2>&1 | head -3
