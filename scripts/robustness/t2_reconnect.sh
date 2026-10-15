# T2: the RTMP server dies mid-stream and comes back 6 s later. The
# recording must go on untouched; the stream reconnects on its own.
. $LABROOT/lib.sh
. $LABROOT/audio_up.sh
audio_up
start_game t2
serve() {
  ffmpeg -y -hide_banner -loglevel warning -listen 1 -i rtmp://127.0.0.1:1935/live/test -c copy -f flv $L/live-$1.flv > $L/log/rtmp-$1.log 2>&1 &
  S=$!
}
serve a
sleep 1
VESPER_STREAM_RECORD=1 $VESPER --stream-game 40 rtmp://127.0.0.1:1935/live/test > $L/log/vesper-t2.log 2>&1 &
V=$!
sleep 12
echo "killing the server (pid $S)"
kill -9 $S
wait $S 2>/dev/null
sleep 6
serve b
echo "server back"
wait $V
wait $S
grep -E "live|Stream\(" $L/log/vesper-t2.log | cut -c1-200
echo "--- first connection"
python3 -I $LABROOT/live_check.py $L/live-a.flv h264 720 60 | tail -4
echo "--- after the reconnection"
python3 -I $LABROOT/live_check.py $L/live-b.flv h264 720 60
echo "--- recording (no gap expected)"
f=$(last_file); info "$f"
ffprobe -v error -select_streams v -show_entries packet=pts_time -of csv=p=0 "$f" | python3 -I -c '
import sys
t = [float(x) for x in sys.stdin if x.strip()]
g = max(b - a for a, b in zip(t, t[1:]))
print(f"recording: {len(t)} frames over {t[-1] - t[0]:.2f} s, largest gap {g * 1000:.0f} ms")'
grep -E "dropped: [1-9]" $L/log/vesper-t2.log | grep -v StreamStats | head -2
stop_game
