# L5: does the OpenGL interposer ever block the game? L1 for vesper-gl: an
# uncapped GLX app runs SECS seconds (frames counted) alone, with Vesper
# recording, with Vesper SIGSTOPped mid-recording, with Vesper SIGKILLed.
. $LABROOT/lib.sh
SECS=${SECS:-20}
mkdir -p $L/apps
gcc -O2 -o $L/apps/gltest64 $LABROOT/apps/gltest.c -lX11 -lGL -lEGL -lm || exit 1
run_game() {
  gamescope --backend headless -W 1280 -H 720 -- env SteamAppId=1 vblank_mode=0 \
    $VESPER_GL $L/apps/gltest64 glx ${SECS}s > $L/log/game-$1.log 2>&1
  echo "$1: $(grep -E '^frames' $L/log/game-$1.log)"
}
run_game base
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
