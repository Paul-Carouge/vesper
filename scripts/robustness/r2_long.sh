# R2 long run: a game recorded (3 tracks, voice chain on the mic, mix) and
# streamed to a local RTMP receiver for LONG_SECS (3600). Samples RSS, threads,
# fds, CPU every 60 s; at the end checks the recording (ffprobe: streams,
# last pts of each track, drift between them) and the received stream.
# Env: LONG_SECS, SIZE (2560x1440), HZ (144), STREAM (own|shared|none).
. $LABROOT/lib.sh
. $LABROOT/audio_up.sh
SECS=${LONG_SECS:-3600}
audio_up
tone() {
  ffmpeg -loglevel quiet -re -f lavfi -i "sine=frequency=$2" -f s16le -ar 48000 -ac 2 - 2>/dev/null | pw-cat -p --raw --rate 48000 --channels 2 --format s16 --target $1 - > /dev/null 2>&1 &
}
ffmpeg -loglevel quiet -re -f lavfi -i "anoisesrc=c=pink:a=0.08:r=48000,aformat=channel_layouts=stereo" -f s16le -ar 48000 -ac 2 - 2>/dev/null | pw-cat -p --raw --rate 48000 --channels 2 --format s16 --target labmic - > /dev/null 2>&1 &
tone labsink2 600
mkdir -p $L/cfg/vesper
VOICE=$'filter=true|high_pass\nfilter=true|noise_suppression\nfilter=true|expander\nfilter=true|compressor\nfilter=true|de_esser\nfilter=true|limiter'
{ printf 'tracks=\ntrack=true|false|0|desktop|Desktop\ntrack=true|false|0|output:labmic|Mic\n%s\n' "$VOICE"
  printf 'track=true|false|0|output:labsink2|Dev2\naudio_mix=true\nreplay=false\n'; } > $L/cfg/vesper/settings
GS_ARGS="-r ${HZ:-144}" start_game long ${SIZE:-2560x1440}
rm -f $L/log/long-samples.log
if [ "${STREAM:-own}" != none ]; then
  ffmpeg -y -hide_banner -loglevel error -listen 1 -i rtmp://127.0.0.1:1935/live/test -c copy -f flv $L/live-long.flv > $L/log/rtmp-long.log 2>&1 &
  SRV=$!
  sleep 1
fi
case ${STREAM:-own} in
  own) VESPER_STREAM_RECORD=1 $VESPER --stream-game $SECS rtmp://127.0.0.1:1935/live/test > $L/log/vesper-long.log 2>&1 & ;;
  shared) VESPER_STREAM_RECORD=1 VESPER_STREAM_CODEC=hevc VESPER_STREAM_HEIGHT=0 VESPER_STREAM_KBPS=50000 $VESPER --stream-game $SECS rtmp://127.0.0.1:1935/live/test > $L/log/vesper-long.log 2>&1 & ;;
  none) $VESPER --record-game $SECS > $L/log/vesper-long.log 2>&1 & ;;
esac
V=$!
sleep 10
t=10
while kill -0 $V 2>/dev/null; do
  cpu=$(python3 -I -c "
import os,sys
f=open('/proc/$V/stat').read().rsplit(')',1)[1].split()
print(int(f[11])+int(f[12]))")
  echo "t=$t cpu_ticks=$cpu $(rss $V)" >> $L/log/long-samples.log
  sleep 60
  t=$((t+60))
done
wait $V
echo "rc=$?"
cat $L/log/long-samples.log | awk 'NR==1||NR%10==0'
tail -1 $L/log/long-samples.log
grep -E "WARN|ERROR|error|dropped: [1-9]|behind|gap" $L/log/vesper-long.log | sort | uniq -c | sort -rn | head -10 | cut -c1-200
grep StreamStats $L/log/vesper-long.log | tail -1 | cut -c1-200
grep "Stats {" $L/log/vesper-long.log | tail -1 | cut -c1-200
f=$(last_file); echo "$f"; info "$f"
ffprobe -v error -show_entries packet=stream_index,pts_time -of csv=p=0 "$f" > $L/log/long-pts.csv
for s in 0 1 2 3 4; do echo "rec stream $s: n=$(grep -c "^$s," $L/log/long-pts.csv) last=$(grep "^$s," $L/log/long-pts.csv | sort -t, -k2 -g | tail -1)"; done
if [ "${STREAM:-own}" != none ]; then
  wait $SRV
  python3 -I $LABROOT/live_check.py $L/live-long.flv h264 1080 60
  ffprobe -v error -show_entries packet=stream_index,pts_time -of csv=p=0 $L/live-long.flv > $L/log/live-pts.csv
  for s in 0 1; do echo "live stream $s: n=$(grep -c "^$s," $L/log/live-pts.csv) last=$(grep "^$s," $L/log/live-pts.csv | sort -t, -k2 -g | tail -1)"; done
fi
stop_game
