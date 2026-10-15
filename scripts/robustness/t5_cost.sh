# T5: what streaming adds to a recording, CPU and GPU engines: a 1440p game
# recorded alone, then recorded and streamed (own 1080p60 H.264 encode at
# 6 Mbit/s), then streamed sharing the recording's encode (HEVC, native).
# VESPER_BIN: an optimized build with the dev flags, e.g.
# CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true cargo build --release -p vesper-app.
. $LABROOT/lib.sh
. $LABROOT/audio_up.sh
audio_up
start_game t5 ${SIZE:-2560x1440}
measure() {
  local name=$1
  shift
  "$@" > $L/log/vesper-t5-$name.log 2>&1 &
  local v=$!
  sleep 8
  echo "$name: $(python3 -I $LABROOT/cost.py $v 12)"
  wait $v
}
serve() {
  ffmpeg -y -hide_banner -loglevel error -listen 1 -i rtmp://127.0.0.1:1935/live/test -c copy -f flv $L/live-t5.flv > $L/log/rtmp-t5.log 2>&1 &
  S=$!
  sleep 1
}
measure record ${VESPER_BIN:-$VESPER} --record-game 24
serve
VESPER_STREAM_RECORD=1 measure record+stream-own-h264-1080p60 ${VESPER_BIN:-$VESPER} --stream-game 24 rtmp://127.0.0.1:1935/live/test
wait $S
serve
VESPER_STREAM_RECORD=1 VESPER_STREAM_CODEC=hevc VESPER_STREAM_HEIGHT=0 VESPER_STREAM_KBPS=${SHARED_KBPS:-50000} \
  measure record+stream-shared ${VESPER_BIN:-$VESPER} --stream-game 24 rtmp://127.0.0.1:1935/live/test
wait $S
grep -h "live: " $L/log/vesper-t5-*.log | grep -v connect
stop_game
