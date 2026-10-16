# P2: hostile settings files at launch: huge, binary, a directory, unreadable (mode 000 is ignored by root: skipped)
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
mkdir -p $L/cfg/vesper
launch() { # launch <label>: time from start to the first "start-pipeline"/"no tray" log line, peak RSS
  rm -f $L/log/p2.log $L/run/vesper.pid
  local t0=$(date +%s%N)
  gamescope --backend headless -W 800 -H 600 -- bash -c "echo \$\$ > $L/run/vesper.pid; exec $VESPER > $L/log/p2.log 2>&1" > /dev/null 2>&1 &
  local GS=$!
  local ok=no peak=0
  for i in $(seq 1 150); do
    if grep -q -E "no tray icon|start-pipeline|single instance" $L/log/p2.log 2>/dev/null; then ok=yes; break; fi
    sleep 0.2
  done
  local V=$(pid_of vesper)
  [ -n "$V" ] && peak=$(grep VmHWM /proc/$V/status | tr -s ' ' | cut -d' ' -f2)
  echo "$1: up=$ok in $(( ($(date +%s%N) - t0) / 1000000 )) ms, peak RSS ${peak} kB"
  [ -n "$V" ] && kill -TERM $V
  sleep 1
  kill $GS 2>/dev/null; wait $GS 2>/dev/null
}
rm -f $L/cfg/vesper/settings
launch "no file"
head -c 300000000 /dev/urandom | base64 -w 80 > $L/cfg/vesper/settings
ls -la $L/cfg/vesper/settings | cut -d' ' -f5
launch "400 MB of base64 junk"
head -c 200000000 /dev/urandom > $L/cfg/vesper/settings
launch "200 MB of raw binary"
rm -f $L/cfg/vesper/settings
seq 1 400000 | sed 's/^/allow_game=g/' > $L/cfg/vesper/settings
launch "400k distinct allow_game lines (6 MB)"
rm -f $L/cfg/vesper/settings
mkdir $L/cfg/vesper/settings
launch "settings is a directory"
rmdir $L/cfg/vesper/settings
echo "output_folder=/proc/nothing" > $L/cfg/vesper/settings
launch "output folder under /proc"
