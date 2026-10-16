# C2: a webcam layer through the whole pipeline (PipeWire, GPU JPEG decode
# or raw YUY2, compositor, encoder) against the fake camera node, with the
# colour checked in the file and the cost printed (1080p30 MJPEG at 1440p).
#   cargo build -p vesper-capture --example camera_lab
#   cargo test -p vesper-app --bin vesper --no-run   # prints the test binary
#   scripts/robustness/go c2 c2_camera_scene.sh <test binary>
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
EX=$BUILD/examples/camera_lab
T=${1:?usage: c2_camera_scene.sh <vesper test binary>}
[ -x "$EX" ] || { echo "build it first: cargo build -p vesper-capture --example camera_lab"; exit 1; }
fake() { # fake <args...> -> pid in F
  "$EX" fake "$@" > "$L/log/fake-$1.log" 2>&1 &
  F=$!
  echo $F > "$L/run/fake.pid"
  sleep 1
}
run() { # run <r,g,b>: the lab test against lab_camera
  VESPER_LAB_CAMERA=lab_camera VESPER_LAB_RGB=$1 "$T" --exact \
    recorder::gpu_tests::a_lab_webcam_is_recorded_with_its_colour --nocapture 2>&1 |
    grep -E "webcam|capture alone|test result|panicked"
}
ffmpeg -v error -f lavfi -i color=c=0xc03020:s=1920x1080 -frames:v 1 -pix_fmt yuvj422p \
  -c:v mjpeg -f image2pipe - > "$L/run/lab.jpg"

echo "== MJPEG 1080p30 (GPU decode), colour c03020"
fake mjpeg "$L/run/lab.jpg" 1920x1080
run 192,48,32
kill $F; wait $F 2>/dev/null

echo "== YUY2 480p30 (raw, BT.601 limited red)"
fake yuy2
run 255,0,0
kill $F; wait $F 2>/dev/null
