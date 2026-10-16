# S1: disk full mid-recording (4 MiB tmpfs as the output folder)
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
mkdir -p $L/small
mount -t tmpfs -o size=4m tmpfs $L/small
echo "XDG_VIDEOS_DIR=\"$L/small\"" > $L/cfg/user-dirs.dirs
start_game s1
$VESPER --record-game 20 > $L/log/vesper-s1.log 2>&1 &
V=$!
for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22; do
  kill -0 $V 2>/dev/null || break
  sleep 1
done
echo "vesper alive after 22s: $(kill -0 $V 2>/dev/null && echo yes || echo no)"
wait $V; echo "rc=$?"
grep -vE "Stats" $L/log/vesper-s1.log | cut -c1-300 | tail -12
grep -c audio $L/log/vesper-s1.log
df -h $L/small | tail -1
f=$(last_file $L/small); echo "$f"; info "$f"
ls -la $L/small/Vesper
stop_game
