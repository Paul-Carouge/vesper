# C1: webcam source against fake camera nodes (the lab has no real device):
# MJPEG on the GPU, raw YUY2, unplug mid-stream, a missing camera.
#   cargo build -p vesper-capture --example camera_lab
#   scripts/robustness/go c1 c1_camera.sh
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
EX=$BUILD/examples/camera_lab
[ -x "$EX" ] || { echo "build it first: cargo build -p vesper-capture --example camera_lab"; exit 1; }
fake() { # fake <args...> -> pid in F
  "$EX" fake "$@" > "$L/log/fake-$1.log" 2>&1 &
  F=$!
  echo $F > "$L/run/fake.pid"
  sleep 1
}
ffmpeg -v error -f lavfi -i color=c=0xc03020:s=1280x720 -frames:v 1 -pix_fmt yuvj422p \
  -c:v mjpeg -f image2pipe - > "$L/run/lab.jpg"

echo "== no camera yet"
"$EX" list
"$EX" open lab_camera 2

echo "== MJPEG 720p30 (GPU decode)"
fake mjpeg "$L/run/lab.jpg"
"$EX" list
"$EX" open lab_camera 3
kill $F; wait $F 2>/dev/null

echo "== YUY2 480p30 (raw, BT.601 limited red: Y 81 U 90 V 240)"
fake yuy2
"$EX" open lab_camera 3

echo "== unplugged after 2 s"
"$EX" open lab_camera 8 &
O=$!
sleep 2
kill $F; wait $F 2>/dev/null
wait $O
