# L4: games the Vulkan layer alone does not reach: a 32-bit Vulkan app
# (i686 layer) and OpenGL apps through vesper-gl (GLX linked, GLX loaded with
# dlopen + dlsym like SDL/GLFW, EGL; 64-bit and 32-bit). For each: run time
# per frame with nothing loaded, dormant (no Vesper), and while Vesper
# records it; then ffprobe the recording and save its middle frame as a PNG
# in $L/shots/. Needs gcc with lib32 (multilib) and the i686 debug builds:
#   cargo build -p vesper-vklayer -p vesper-glcapture --target i686-unknown-linux-gnu
# CASES="gl64-glx gl32-egl ..." picks some; SECS (default 8) per run; SIZE
# (default 1280x720) of the OpenGL windows.
. $LABROOT/lib.sh
SECS=${SECS:-8}
SIZE=${SIZE:-1280x720}
B=$L/apps
mkdir -p $B $L/shots
gcc -O2 -o $B/gltest64 $LABROOT/apps/gltest.c -lX11 -lGL -lEGL -lm || exit 1
gcc -m32 -O2 -o $B/gltest32 $LABROOT/apps/gltest.c -L/usr/lib32 -lX11 -lGL -lEGL -lm || exit 1
gcc -m32 -O2 -o $B/vktest32 $LABROOT/apps/vktest.c -L/usr/lib32 -lvulkan -lxcb || exit 1
gcc -O2 -o $B/vktest64 $LABROOT/apps/vktest.c -lvulkan -lxcb || exit 1

# game <case> <tag>: one run in a headless gamescope; prints its frame times.
game() {
  local wrap=() cmd
  case $1 in
    vk32) cmd=($B/vktest32 ${SECS}s) ;;
    vk64) cmd=($B/vktest64 ${SECS}s) ;;
    gl64-*) cmd=($B/gltest64 ${1#gl64-} ${SECS}s $SIZE) ;;
    gl32-*) cmd=($B/gltest32 ${1#gl32-} ${SECS}s $SIZE) ;;
  esac
  case $1:$2 in
    gl*:none) ;;
    gl*) wrap=($VESPER_GL) ;;
    vk*:none) wrap=(env VESPER_CAPTURE_DISABLE=1) ;;
  esac
  gamescope --backend headless -W ${SIZE%x*} -H ${SIZE#*x} -- env SteamAppId=1 vblank_mode=0 VESPER_LAYER_DEBUG=1 \
    "${wrap[@]}" "${cmd[@]}" > $L/log/game-$1-$2.log 2>&1
  echo "  $2: $(grep -E '^frames' $L/log/game-$1-$2.log)"
  grep -E "vesper layer|ERROR" $L/log/game-$1-$2.log | sort | uniq -c | head -5
}

for c in ${CASES:-vk32 gl64-glx gl64-glx-dlopen gl64-egl gl32-glx gl32-glx-dlopen gl32-egl}; do
  echo "== $c"
  game $c none
  game $c dormant
  rm -f $L/vid/Vesper/*.mkv
  $VESPER --record-game $((SECS - 2)) > $L/log/vesper-$c.log 2>&1 &
  V=$!
  game $c rec
  wait $V
  grep -E "Finished|panick|rror" $L/log/vesper-$c.log | cut -c1-200 | tail -2
  f=$(last_file)
  if [ -n "$f" ]; then
    info "$f"
    ffmpeg -v error -y -ss $(( (SECS - 2) / 2 )) -i "$f" -frames:v 1 $L/shots/$c.png && echo "  frame: $L/shots/$c.png"
  else
    echo "  no recording"
  fi
  rm -f /tmp/vesper-0/capture.sock
done
