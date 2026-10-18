# R2 startup: time from exec to the window's first frame (the debug build's
# "first frame at" log, see dev.rs), and how long a second launch takes to
# hand over to the first and exit; under headless gamescope with a fake tray
# and the lab's D-Bus session (the portal is D-Bus activated there, as at
# session start). Env: RUNS (5), REPLAY (false).
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
mkdir -p $L/cfg/vesper
printf 'replay=%s\n' ${REPLAY:-false} > $L/cfg/vesper/settings
python3 -I $LABROOT/r2_fake_tray.py > $L/log/tray.log 2>&1 &
TRAY=$!
sleep 1
ms() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%d", (b - a) * 1000 }'; }
for i in $(seq ${RUNS:-5}); do
  log=$L/log/startup-$i.log
  # gamescope sets DISPLAY in its child, so lib.sh's guard cannot run there.
  gamescope --backend headless -W 1280 -H 800 -- bash -c '
    [ "$HOME" = "$L/home" ] && [ -z "$(ls -A /run/user 2>/dev/null)" ] || exit 1
    date +%s.%N > $L/run/exec-at
    $VESPER > $0 2>&1 &
    echo $! > $L/run/vesper.pid
    for _ in $(seq 150); do grep -q "first frame at" $0 && break; sleep 0.1; done
    a=$(date +%s.%N)
    $VESPER > $0.second 2>&1
    echo "$a $(date +%s.%N)" > $L/run/second
    wait' $log > $L/log/gs-$i.log 2>&1 &
  GS=$!
  for _ in $(seq 200); do [ -s $L/run/second ] && break; sleep 0.1; done
  t0=$(cat $L/run/exec-at)
  t1=$(grep -m1 -o "first frame at [0-9.]*" $log | cut -d' ' -f4)
  read -r s0 s1 < $L/run/second
  echo "run $i: first frame after ${t1:+$(ms $t0 $t1)} ms; second launch exits after ${s1:+$(ms $s0 $s1)} ms"
  kill -TERM $(pid_of vesper) 2>/dev/null
  wait $GS 2>/dev/null
  rm -f $L/run/second $L/run/exec-at
done
kill $TRAY 2>/dev/null
