# T3: the uplink is slower than the stream (tc on the lab's loopback): the
# stream drops video until keyframes and reports it; the recording and the
# encode ticks must not suffer. Env: RATE (3mbit), KBPS (6000).
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
. $LABROOT/audio_up.sh
audio_up
start_game t3
# A real link MTU, so tbf can shape it (loopback packets are 64 KB).
ip link set lo mtu 1500
ffmpeg -y -hide_banner -loglevel warning -listen 1 -i rtmp://127.0.0.1:1935/live/test -c copy -f flv $L/live-t3.flv > $L/log/rtmp-t3.log 2>&1 &
S=$!
sleep 1
VESPER_STREAM_RECORD=1 VESPER_STREAM_KBPS=${KBPS:-6000} $VESPER --stream-game 30 rtmp://127.0.0.1:1935/live/test > $L/log/vesper-t3.log 2>&1 &
V=$!
sleep 8
tc qdisc add dev lo root tbf rate ${RATE:-3mbit} burst 64kb latency 300ms && echo "loopback limited to ${RATE:-3mbit}"
sleep 14
tc qdisc del dev lo root && echo "limit lifted"
wait $V
wait $S
grep -E "live|Stream\(" $L/log/vesper-t3.log | grep -v StreamStats | cut -c1-200
grep StreamStats $L/log/vesper-t3.log | awk 'NR % 3 == 0' | cut -c1-120
echo "--- recording ticks (encode never late: dropped stays 0)"
grep -E "Stats \{" $L/log/vesper-t3.log | grep -v StreamStats | tail -1 | cut -c1-200
f=$(last_file); info "$f"
ffprobe -v error -select_streams v -show_entries packet=pts_time -of csv=p=0 "$f" | python3 -I -c '
import sys
t = [float(x) for x in sys.stdin if x.strip()]
g = max(b - a for a, b in zip(t, t[1:]))
print(f"recording: {len(t)} frames over {t[-1] - t[0]:.2f} s, largest gap {g * 1000:.0f} ms")'
echo "--- received stream"
python3 -I $LABROOT/live_check.py $L/live-t3.flv h264 720 0 lossy
stop_game
