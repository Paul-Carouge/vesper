# S7: SIGTERM at launch, and SIGTERM right after a replay save was requested
. $LABROOT/lib.sh
. $LABROOT/audio_up.sh
audio_up
mkdir -p $L/cfg/vesper
cat > $L/launch.sh <<'EOF'
$VESPER > $L/log/ui-launch.log 2>&1 &
V=$!
sleep ${EARLY:-0.4}
kill -TERM $V
t0=$(date +%s%N)
for i in $(seq 1 100); do kill -0 $V 2>/dev/null || break; sleep 0.1; done
wait $V; rc=$?
echo "EARLY=${EARLY:-0.4}: exit rc=$rc after $(( ($(date +%s%N) - t0) / 1000000 )) ms"
EOF
printf 'game_source=true\nreplay=true\nreplay_seconds=60\n' > $L/cfg/vesper/settings
echo "=== SIGTERM right after a replay save"
rm -f $L/vid/Vesper/*
gamescope --backend headless -W 1280 -H 800 -- env DEMO=ctrl+s DELAY=22000 bash $LABROOT/ui_child.sh > $L/log/gs.log 2>&1 &
GS=$!
sleep ${KILLAT:-24.3}
V=$(pgrep -x vesper | head -1)
kill -TERM $V
t0=$(date +%s%N)
for i in $(seq 1 100); do kill -0 $V 2>/dev/null || break; sleep 0.1; done
echo "gone after $(( ($(date +%s%N) - t0) / 1000000 )) ms"
grep -E "save-replay|saved|replay|signal" $L/log/ui.log | cut -c1-220
f=$(ls -t $L/vid/Vesper/*.mkv 2>/dev/null | head -1); echo "$f"; [ -n "$f" ] && info "$f"
[ -n "$f" ] && ffmpeg -v error -i "$f" -f null - 2>&1 | head -3
kill $GS 2>/dev/null; wait $GS 2>/dev/null
