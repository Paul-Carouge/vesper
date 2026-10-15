# A5: PipeWire daemon frozen (SIGSTOP) when a recording starts and while it runs
. $LABROOT/lib.sh
mkdir -p $L/cfg/vesper
printf 'game_source=true\nreplay=false\n' > $L/cfg/vesper/settings
gamescope --backend headless -W 1280 -H 800 -- bash $LABROOT/ui_child.sh > $L/log/gs.log 2>&1 &
GS=$!
sleep 5
kill -STOP $PW
echo "pipewire frozen at t=5 (the demo presses record at t~8)"
sleep 12
V=$(pgrep -x vesper | head -1)
echo "vesper alive: $(kill -0 $V 2>/dev/null && echo yes || echo no); $(grep -c 'recording to' $L/log/ui.log) recording started"
grep -E "recording to|track|Could not" $L/log/ui.log | cut -c1-200 | tail -5
sleep 6
echo "SIGTERM"
t0=$(date +%s%N)
kill -TERM $V
for i in $(seq 1 100); do kill -0 $V 2>/dev/null || break; sleep 0.1; done
echo "vesper alive: $(kill -0 $V 2>/dev/null && echo yes || echo no) after $(( ($(date +%s%N) - t0) / 1000000 )) ms"
f=$(last_file); echo "$f"; [ -n "$f" ] && info "$f"
kill -CONT $PW
kill -9 $V 2>/dev/null
kill $GS 2>/dev/null; wait $GS 2>/dev/null
