# R3 slow disk while recording AND streaming (--stream-game, dev entry point):
# probe/slowdisk.c (LD_PRELOAD) stalls every write to the *.mkv for SLOW_MS
# (800) every SLOW_EVERY_S (5; 0: once) from SLOW_AFTER_S (12). Env: SECS (40),
# STREAM (own|shared). Prints the stalls, the frames the pipeline dropped, the
# audio buffers dropped, the largest gap of the recording and of the received
# live stream, and how the recording ended (a disk that stays stalled ends it
# with "Disk too slow", finalized up to there).
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
. $LABROOT/audio_up.sh
SECS=${SECS:-40}
SO=$L/slowdisk.so
gcc -O2 -shared -fPIC -o $SO $LABROOT/probe/slowdisk.c -ldl -lpthread || exit 1
audio_up
tone() {
  ffmpeg -loglevel quiet -re -f lavfi -i "sine=frequency=$2" -f s16le -ar 48000 -ac 2 - 2>/dev/null | pw-cat -p --raw --rate 48000 --channels 2 --format s16 --target $1 - > /dev/null 2>&1 &
}
ffmpeg -loglevel quiet -re -f lavfi -i "anoisesrc=c=pink:a=0.08:r=48000,aformat=channel_layouts=stereo" -f s16le -ar 48000 -ac 2 - 2>/dev/null | pw-cat -p --raw --rate 48000 --channels 2 --format s16 --target labmic - > /dev/null 2>&1 &
tone labsink2 600
mkdir -p $L/cfg/vesper
printf 'tracks=\ntrack=true|false|0|desktop|Desktop\ntrack=true|false|0|output:labmic|Mic\ntrack=true|false|0|output:labsink2|Dev2\naudio_mix=true\nreplay=false\n' > $L/cfg/vesper/settings
start_game slow
ffmpeg -y -hide_banner -loglevel warning -listen 1 -i rtmp://127.0.0.1:1935/live/test -c copy -f flv $L/live-slow.flv > $L/log/rtmp-slow.log 2>&1 &
sleep 1
case ${STREAM:-own} in
  shared) export VESPER_STREAM_CODEC=hevc VESPER_STREAM_HEIGHT=0 VESPER_STREAM_KBPS=50000 ;;
esac
LD_PRELOAD=$SO SLOW_MS=${SLOW_MS:-800} SLOW_EVERY_S=${SLOW_EVERY_S:-5} SLOW_AFTER_S=${SLOW_AFTER_S:-12} \
  VESPER_STREAM_RECORD=1 $VESPER --stream-game $SECS rtmp://127.0.0.1:1935/live/test > $L/log/vesper-slow.log 2>&1 &
V=$!
sleep 10
echo "t=10: $(rss $V)"
wait $V; echo "vesper rc=$?"
log=$L/log/vesper-slow.log
echo "stalls: $(grep -c '^slowdisk: stalled' $log) ($(grep -m1 '^slowdisk' $log))"
echo "dropped (last Stats): $(grep "\] Stats {" $log | grep -o "dropped: [0-9]*" | tail -1)"
echo "audio dropped (encoder behind): $(grep -cE 'audio buffer dropped|audio encoder is behind' $log)"
grep -E "Finished|Notice|Error|ERROR|WARN" $log | grep -vE 'audio buffer dropped|audio encoder is behind' | sed -E 's/^[0-9T:.Z-]+ +//' | uniq -c | head -10 | cut -c1-220
gaps() { # gaps <file> <label>: video frames and largest pts gap
  ffprobe -v error -select_streams v -show_entries packet=pts_time -of csv=p=0 "$1" | python3 -I -c '
import sys
t = sorted(float(x) for x in sys.stdin if x.strip() and x.strip() != "N/A")
g = max(b - a for a, b in zip(t, t[1:])) if len(t) > 1 else 0
print(f"{sys.argv[1]}: {len(t)} frames over {t[-1] - t[0] if t else 0:.2f} s, largest gap {g * 1000:.0f} ms")' "$2"
}
f=$(last_file); echo "$f"
info "$f"
gaps "$f" recording
gaps $L/live-slow.flv live
stop_game
