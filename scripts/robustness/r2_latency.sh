# R2 latency: arrival jitter of a stream at a local receiver (ffprobe as the
# RTMP listener) and burstiness on the wire (r2_wire.py proxy in front of it).
# Env: CODEC (h264|hevc), H (1080|0), KBPS, SECS (50), REC (1), SIZE (2560x1440), HZ (144).
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
. $LABROOT/audio_up.sh
audio_up
mkdir -p $L/cfg/vesper
printf 'tracks=\ntrack=true|false|0|desktop|Desktop\naudio_mix=true\nreplay=false\n' > $L/cfg/vesper/settings
GS_ARGS="-r ${HZ:-144}" start_game lat ${SIZE:-2560x1440}
ffprobe -v error -listen 1 -fflags nobuffer -show_entries packet=stream_index,pts_time,size,flags -of csv rtmp://127.0.0.1:1936/live/test 2> $L/log/probe-lat.err | python3 -I $LABROOT/r2_arrival.py ${SKIP:-8} > $L/lat.out &
P=$!
python3 -I $LABROOT/r2_wire.py 1935 1936 ${SECS:-50} > $L/wire.out 2>&1 &
W=$!
sleep 1
VESPER_STREAM_RECORD=${REC:-1} VESPER_STREAM_CODEC=${CODEC:-h264} VESPER_STREAM_HEIGHT=${H:-1080} VESPER_STREAM_KBPS=${KBPS:-6000} \
  $VESPER --stream-game ${SECS:-50} rtmp://127.0.0.1:1935/live/test > $L/log/vesper-lat.log 2>&1 &
V=$!
wait $V
wait $W
wait $P
echo "== latency CODEC=${CODEC:-h264} H=${H:-1080} REC=${REC:-1}"
cat $L/wire.out $L/lat.out
grep StreamStats $L/log/vesper-lat.log | tail -1 | cut -c1-200
head -3 $L/log/probe-lat.err
stop_game
