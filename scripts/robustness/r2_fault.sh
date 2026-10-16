# R2 faults while recording AND streaming (--stream-game, dev entry point).
# FAULT = serverdead | tls-untrusted | tls-plain | tls-die | pwrestart |
#   pwfreeze | diskfull | gamekill | vstkill | vststorm | vstfreeze | term | none
# Env: DEAD (50 s the server stays down), SECS (50), STREAM (own|shared), VST
# (a .vst3 bundle for the vst faults).  Prints the stream/recording state and
# what the recording looks like afterwards (frames, gaps, per-track spans).
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
. $LABROOT/audio_up.sh
FAULT=${FAULT:-none}
SECS=${SECS:-50}
audio_up
tone() {
  ffmpeg -loglevel quiet -re -f lavfi -i "sine=frequency=$2" -f s16le -ar 48000 -ac 2 - 2>/dev/null | pw-cat -p --raw --rate 48000 --channels 2 --format s16 --target $1 - > /dev/null 2>&1 &
}
ffmpeg -loglevel quiet -re -f lavfi -i "anoisesrc=c=pink:a=0.08:r=48000,aformat=channel_layouts=stereo" -f s16le -ar 48000 -ac 2 - 2>/dev/null | pw-cat -p --raw --rate 48000 --channels 2 --format s16 --target labmic - > /dev/null 2>&1 &
tone labsink2 600
mkdir -p $L/cfg/vesper
VOICE=$'filter=true|high_pass\nfilter=true|noise_suppression\nfilter=true|expander\nfilter=true|compressor\nfilter=true|de_esser\nfilter=true|limiter'
{ printf 'tracks=\ntrack=true|false|0|desktop|Desktop\ntrack=true|false|0|output:labmic|Mic\n%s\n' "$VOICE"
  case $FAULT in vst*) printf 'filter=true|vst3:id=1\n' ;; esac
  printf 'track=true|false|0|output:labsink2|Dev2\naudio_mix=true\nreplay=false\n'; } > $L/cfg/vesper/settings
case $FAULT in vst*)
  mkdir -p $HOME/.vst3 $L/cfg/vesper/vst3
  cp -r $VST $HOME/.vst3/
  printf 'path=%s\ncid=%s\nname=Gain\ncomponent=\ncontroller=\n' $HOME/.vst3/$(basename $VST) "${VST_CID:-6e33225254224a00aa69301af318797d}" > $L/cfg/vesper/vst3/1.conf ;;
esac
if [ $FAULT = diskfull ]; then
  mkdir -p $L/small
  mount -t tmpfs -o size=${FULLMB:-6}m tmpfs $L/small
  echo "XDG_VIDEOS_DIR=\"$L/small\"" > $L/cfg/user-dirs.dirs
fi
start_game flt
URL=rtmp://127.0.0.1:1935/live/test
serve() { # serve <tag>
  ffmpeg -y -hide_banner -loglevel warning -listen 1 -i rtmp://127.0.0.1:1935/live/test -c copy -f flv $L/live-$1.flv > $L/log/rtmp-$1.log 2>&1 &
  SRV=$!
}
serve_loop() {
  ( n=0; while :; do n=$((n+1)); ffmpeg -y -hide_banner -loglevel warning -listen 1 -i rtmp://127.0.0.1:1935/live/test -c copy -f flv $L/live-loop$n.flv > $L/log/rtmp-loop$n.log 2>&1; sleep 0.2; done ) &
  LOOP=$!
}
case $FAULT in
  tls-*)
    D=$L/tls; mkdir -p $D
    openssl req -x509 -newkey rsa:2048 -nodes -keyout $D/ca.key -out $D/ca.pem -subj "/CN=lab-ca" -days 2 -addext "basicConstraints=critical,CA:TRUE" > /dev/null 2>&1
    openssl req -newkey rsa:2048 -nodes -keyout $D/s.key -out $D/s.csr -subj "/CN=127.0.0.1" > /dev/null 2>&1
    printf 'subjectAltName=IP:127.0.0.1\n' > $D/san.cnf
    openssl x509 -req -in $D/s.csr -CA $D/ca.pem -CAkey $D/ca.key -CAcreateserial -out $D/s.pem -days 2 -extfile $D/san.cnf > /dev/null 2>&1
    serve_loop
    URL=rtmps://127.0.0.1:1936/live/test
    case $FAULT in
      tls-die) python3 -I $LABROOT/r2_tlsproxy.py 1936 1935 $D/s.pem $D/s.key ${DIE:-14} > $L/log/tlsproxy.log 2>&1 & ;;
      *) python3 -I $LABROOT/r2_tlsproxy.py 1936 1935 $D/s.pem $D/s.key > $L/log/tlsproxy.log 2>&1 & ;;
    esac
    PX=$!
    [ $FAULT = tls-plain ] && URL=rtmps://127.0.0.1:1935/live/test
    if [ $FAULT = tls-die ]; then
      # GnuTLS trusts the p11-kit anchors: add our CA in this private mount namespace only.
      mount -t tmpfs tmpfs /etc/ca-certificates/trust-source/anchors && cp $D/ca.pem /etc/ca-certificates/trust-source/anchors/lab-ca.crt
    fi
    sleep 1 ;;
  *) serve a; sleep 1 ;;
esac
case ${STREAM:-own} in
  shared) export VESPER_STREAM_CODEC=hevc VESPER_STREAM_HEIGHT=0 VESPER_STREAM_KBPS=50000 ;;
esac
VESPER_STREAM_RECORD=1 $VESPER --stream-game $SECS $URL > $L/log/vesper-flt.log 2>&1 &
V=$!
sleep 12
host() { children_named $V vesper-vst-host; }
echo "t=12 fault $FAULT: $(rss $V)"
case $FAULT in
  serverdead)
    kill -9 $SRV; wait $SRV 2>/dev/null
    echo "server killed"; sleep ${DEAD:-50}
    serve b; echo "server back" ;;
  pwrestart)
    kill $PW; sleep 6
    echo "vesper alive: $(kill -0 $V 2>/dev/null && echo yes || echo no)"
    kill $WP 2>/dev/null
    pipewire > $L/log/pipewire2.log 2>&1 &
    PW=$!
    sleep 1
    wireplumber > $L/log/wp2.log 2>&1 &
    sleep 2
    tone labsink 440; tone labsink2 600 ;;
  pwfreeze)
    kill -STOP $PW; sleep 8; kill -CONT $PW; echo "pipewire frozen 8 s" ;;
  gamekill)
    kill_game_client; echo "game killed" ;;
  vstkill)
    echo "hosts: $(host | tr '\n' ' ')"; kill -9 $(host); sleep 3; echo "after kill: $(host | tr '\n' ' ')" ;;
  vststorm)
    for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
      h=$(host | head -1); [ -n "$h" ] && kill -9 $h
      sleep 1.5
    done
    echo "hosts now: $(host | tr '\n' ' '), vesper: $(rss $V)" ;;
  vstfreeze)
    h=$(host | head -1); kill -STOP $h; echo "host $h stopped"; sleep 10
    echo "vesper: $(rss $V)"; kill -CONT $h; sleep 10 ;;
  term)
    t0=$(date +%s%N); kill -TERM $V
    for i in $(seq 1 100); do kill -0 $V 2>/dev/null || break; sleep 0.2; done
    echo "alive after TERM: $(kill -0 $V 2>/dev/null && echo yes || echo no), took $(( ($(date +%s%N) - t0) / 1000000 )) ms" ;;
esac
wait $V; echo "vesper rc=$?"
kill $PX $LOOP 2>/dev/null
echo "--- vesper log"
grep -E "live:|Stream\(|Stopped|Finished|Error|error|WARN|ERROR|Failed|failed" $L/log/vesper-flt.log | grep -v "StreamStats\|zbus\|randr" | sed -E 's/^[0-9T:.Z-]+ +//' | uniq -c | head -${LOGLINES:-30} | cut -c1-220
echo "--- tail"
grep -v "Stats" $L/log/vesper-flt.log | tail -3 | cut -c1-220
f=$(last_file $( [ $FAULT = diskfull ] && echo $L/small )); echo "$f"
info "$f"
ffprobe -v error -select_streams v -show_entries packet=pts_time -of csv=p=0 "$f" | python3 -I -c '
import sys
t = [float(x) for x in sys.stdin if x.strip()]
g = max(b - a for a, b in zip(t, t[1:])) if len(t) > 1 else 0
print(f"recording: {len(t)} frames over {t[-1] - t[0]:.2f} s, largest gap {g * 1000:.0f} ms")' 2>&1 | tail -2
ffprobe -v error -show_entries packet=stream_index,pts_time -of csv=p=0 "$f" > $L/log/pts.csv 2>/dev/null
for s in 1 2 3 4; do echo "audio stream $s: n=$(grep -c "^$s," $L/log/pts.csv) last=$(grep "^$s," $L/log/pts.csv | sort -t, -k2 -g | tail -1)"; done
for x in $L/live-*.flv; do [ -f $x ] && echo "$(basename $x): $(ffprobe -v error -show_entries format=duration -of csv=p=0 $x 2>&1 | head -1) s"; done
if [ $FAULT = pwrestart ] || [ $FAULT = vstkill ] || [ $FAULT = vststorm ] || [ $FAULT = vstfreeze ]; then
  for a in 1 2 3 4; do
    echo "audio $a RMS per 5 s (dB):"
    ffmpeg -v error -i "$f" -map 0:a:$((a-1)) -af "asetnsamples=240000,astats=metadata=1:reset=1,ametadata=print:key=lavfi.astats.Overall.RMS_level:file=-" -f null - 2>&1 | grep RMS | sed 's/.*=//' | tr '\n' ' '
    echo
  done
fi
stop_game
