# L1: does the layer ever block the game? vkcube renders N frames (vsync 60): time it
#  - alone, with Vesper recording, with Vesper SIGSTOPped mid-recording, with Vesper SIGKILLed.
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
N=${N:-1200}
run_game() { # run_game <tag>; runs vkcube --c N inside gamescope, echoes elapsed ms
  local t0=$(date +%s%N)
  gamescope --backend headless -W 1280 -H 720 -- env SteamAppId=1 vkcube --wsi xcb --c $N --width 1280 --height 720 > $L/log/game-$1.log 2>&1
  local t1=$(date +%s%N)
  echo "$1: game done in $(( (t1 - t0) / 1000000 )) ms"
}
run_game base
# with Vesper: record, then the case
for mode in rec stop kill; do
  $VESPER --record-game 60 > $L/log/vesper-$mode.log 2>&1 &
  V=$!
  case $mode in
    stop) (sleep 9; kill -STOP $V) & ;;
    kill) (sleep 9; kill -9 $V) & ;;
  esac
  run_game $mode
  kill -CONT $V 2>/dev/null; kill $V 2>/dev/null; wait $V 2>/dev/null
  grep -E "Finished|Stopped|panick" $L/log/vesper-$mode.log | cut -c1-200 | tail -2
  rm -f /tmp/vesper-0/capture.sock
done
