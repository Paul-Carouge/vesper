# R2 perf: cost of an optimized build (BUILD=<dir with debug assertions>) per
# thread group. CASE: rec | rec3 | rec3-vst | stream-own | stream-shared | ui-*.
# Env: SIZE (2560x1440), HZ (144), WARM (10), WIN (20), SHIM (allocshim .so),
# VST (a .vst3 bundle, only for rec3-vst), SECS (total, default WARM+WIN+8).
# Needs a build with the dev flags: see r2_build.sh.
. $LABROOT/lib.sh
. $LABROOT/audio_up.sh
CASE=${CASE:-rec}
WARM=${WARM:-10}
WIN=${WIN:-20}
SECS=${SECS:-$((WARM + WIN + 8))}
audio_up
# A virtual microphone and a second sink with tones.
tone() { # tone <target> <freq>
  ffmpeg -loglevel quiet -re -f lavfi -i "sine=frequency=$2" -f s16le -ar 48000 -ac 2 - 2>/dev/null | pw-cat -p --raw --rate 48000 --channels 2 --format s16 --target $1 - > /dev/null 2>&1 &
}
ffmpeg -loglevel quiet -re -f lavfi -i "anoisesrc=c=pink:a=0.08:r=48000,aformat=channel_layouts=stereo" -f s16le -ar 48000 -ac 2 - 2>/dev/null | pw-cat -p --raw --rate 48000 --channels 2 --format s16 --target labmic - > /dev/null 2>&1 &
tone labsink2 600
mkdir -p $L/cfg/vesper
VOICE=$'filter=true|high_pass\nfilter=true|noise_suppression\nfilter=true|expander\nfilter=true|compressor\nfilter=true|de_esser\nfilter=true|limiter'
case $CASE in
  rec) printf 'tracks=\ntrack=true|false|0|desktop|Desktop\naudio_mix=false\nreplay=false\n' ;;
  *) printf 'tracks=\ntrack=true|false|0|desktop|Desktop\ntrack=true|false|0|output:labmic|Mic\n%s\n' "$VOICE"
     printf 'track=true|false|0|output:labsink2|Dev2\naudio_mix=true\nreplay=false\n' ;;
esac > $L/cfg/vesper/settings
if [ "$CASE" = rec3-vst ]; then
  mkdir -p $HOME/.vst3 $L/cfg/vesper/vst3
  cp -r $VST $HOME/.vst3/
  printf 'path=%s\ncid=%s\nname=Gain\ncomponent=\ncontroller=\n' $HOME/.vst3/$(basename $VST) "${VST_CID:-6e33225254224a00aa69301af318797d}" > $L/cfg/vesper/vst3/1.conf
  # the gain plugin goes after the voice chain on the mic
  sed -i 's/^filter=true|limiter$/filter=true|limiter\nfilter=true|vst3:id=1/' $L/cfg/vesper/settings
fi
cat $L/cfg/vesper/settings | tr '\n' ' '; echo
GS_ARGS="-r ${HZ:-144}" start_game perf ${SIZE:-2560x1440}
serve() {
  ffmpeg -y -hide_banner -loglevel error -listen 1 -i rtmp://127.0.0.1:1935/live/test -c copy -f flv $L/live-perf.flv > $L/log/rtmp-perf.log 2>&1 &
  SRV=$!
  sleep 1
}
export SHIM_OUT=$L/shim.out
PRE=
[ -n "$SHIM" ] && PRE="LD_PRELOAD=$SHIM"
case $CASE in
  stream-own) serve; env $PRE VESPER_STREAM_RECORD=1 $VESPER --stream-game $SECS rtmp://127.0.0.1:1935/live/test > $L/log/vesper-perf.log 2>&1 & ;;
  stream-shared) serve; env $PRE VESPER_STREAM_RECORD=1 VESPER_STREAM_CODEC=hevc VESPER_STREAM_HEIGHT=0 VESPER_STREAM_KBPS=50000 $VESPER --stream-game $SECS rtmp://127.0.0.1:1935/live/test > $L/log/vesper-perf.log 2>&1 & ;;
  *) env $PRE $VESPER --record-game $SECS > $L/log/vesper-perf.log 2>&1 & ;;
esac
V=$!
sleep $WARM
echo "vesper $V: $(rss $V)"
HOST=$(pgrep -P $V -x vesper-vst-host | head -1)
[ -n "$HOST" ] && python3 -I $LABROOT/r2_sample.py $HOST $WIN "vst host $HOST" > $L/host-sample.txt &
python3 -I $LABROOT/r2_sample.py $V $WIN "$CASE ${SIZE:-2560x1440}@${HZ:-144}"
sleep 2
[ -n "$HOST" ] && cat $L/host-sample.txt
wait $V
grep -E "StreamStats|Stats \{" $L/log/vesper-perf.log | tail -2 | cut -c1-400
grep -vE "Stats" $L/log/vesper-perf.log | cut -c1-200 | tail -5
f=$(last_file); info "$f"
stop_game
