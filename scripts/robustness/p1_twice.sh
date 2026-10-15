# P1: two instances, with and without the session bus (single-instance guard)
. $LABROOT/lib.sh
mkdir -p $L/cfg/vesper
printf 'game_source=true\nreplay=true\n' > $L/cfg/vesper/settings
cat > $L/two.sh <<'EOF'
$VESPER > $L/log/ui-a.log 2>&1 &
A=$!
sleep 6
$VESPER > $L/log/ui-b.log 2>&1 &
B=$!
sleep 6
echo "A alive: $(kill -0 $A 2>/dev/null && echo yes || echo no); B alive: $(kill -0 $B 2>/dev/null && echo yes || echo no)"
grep -E "already|single|capturing" $L/log/ui-a.log $L/log/ui-b.log | cut -c1-220
kill -TERM $A $B 2>/dev/null
sleep 2
EOF
echo "=== with the session bus"
gamescope --backend headless -W 800 -H 600 -- bash $L/two.sh 2>&1 | grep -v -E "Gamescope|gamescope"
echo "=== without a session bus"
DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent gamescope --backend headless -W 800 -H 600 -- bash $L/two.sh 2>&1 | grep -v -E "Gamescope|gamescope"
