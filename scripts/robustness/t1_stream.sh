# T1: live stream of a game to a local server (ffmpeg as the listener) while
# recording; checks the received stream and the recording.
# Env: PROTO (rtmp, or srt), SECS (20), CODEC (h264), H (1080; the game is
# 1280x720), FPS (60), KBPS (6000).
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
. $LABROOT/audio_up.sh
audio_up
start_game t1
if [ "${PROTO:-rtmp}" = srt ]; then
  URL=srt://127.0.0.1:9000
  OUT=$L/live1.ts
  ffmpeg -y -hide_banner -loglevel warning -i "$URL?mode=listener" -c copy -f mpegts $OUT > $L/log/srt1.log 2>&1 &
else
  URL=rtmp://127.0.0.1:1935/live/test
  OUT=$L/live1.flv
  ffmpeg -y -hide_banner -loglevel warning -listen 1 -i $URL -c copy -f flv $OUT > $L/log/rtmp1.log 2>&1 &
fi
S1=$!
sleep 1
VESPER_STREAM_RECORD=1 VESPER_STREAM_CODEC=${CODEC:-h264} VESPER_STREAM_HEIGHT=${H:-1080} VESPER_STREAM_FPS=${FPS:-60} VESPER_STREAM_KBPS=${KBPS:-6000} \
  $VESPER --stream-game ${SECS:-20} $URL > $L/log/vesper-t1.log 2>&1 &
V=$!
sleep 4
echo "vesper while live: $(rss $V)"
wait $V
wait $S1
grep -vE "Stats \{|StreamStats" $L/log/vesper-t1.log | cut -c1-200 | tail -14
grep StreamStats $L/log/vesper-t1.log | tail -2 | cut -c1-200
echo "--- received stream"
case ${CODEC:-h264} in av1) WANT=av1 ;; hevc) WANT=hevc ;; *) WANT=h264 ;; esac
python3 -I $LABROOT/live_check.py $OUT $WANT 720 ${FPS:-60}
echo "--- recording"
f=$(last_file); info "$f"
stop_game
