# E1: encoder fails to open (LIBVA_DRIVER_NAME=bogus): UI recovery, replay buffer on + a recording request
. $LABROOT/lib.sh
mkdir -p $L/cfg/vesper
printf 'game_source=true\nreplay=true\n' > $L/cfg/vesper/settings
export LIBVA_DRIVER_NAME=bogus
gamescope --backend headless -W 1280 -H 800 -- bash $LABROOT/ui_child.sh > $L/log/gs.log 2>&1 &
GS=$!
sleep 16
V=$(pgrep -x vesper | head -1)
echo "vesper alive: $(kill -0 $V 2>/dev/null && echo yes || echo no)"
grep -vE "Stats|start-pipeline" $L/log/ui.log | cut -c1-250 | tail -12
cat $L/cfg/vesper/settings | grep -E "replay"
kill -TERM $V; sleep 2
echo "after TERM alive: $(kill -0 $V 2>/dev/null && echo yes || echo no)"
kill $GS 2>/dev/null; wait $GS 2>/dev/null
