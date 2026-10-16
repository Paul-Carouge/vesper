# A3: the PipeWire daemon restarts mid-recording (a user restarting the audio stack)
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
. $LABROOT/audio_up.sh
mkdir -p $L/cfg/vesper
cat > $L/cfg/vesper/settings <<'EOF'
tracks=
track=true|false|0|desktop|Desktop
track=true|false|0|output:labsink2|Dev2
audio_mix=true
replay=false
EOF
audio_up
ffmpeg -loglevel quiet -re -f lavfi -i "sine=frequency=880" -f s16le -ar 48000 -ac 2 - 2>/dev/null | pw-cat -p --raw --rate 48000 --channels 2 --format s16 --target labsink2 - > /dev/null 2>&1 &
start_game a3
$VESPER --record-game 40 > $L/log/vesper-a3.log 2>&1 &
V=$!
sleep 14
echo "== t~12: kill pipewire"
kill $PW
sleep 5
echo "vesper alive: $(kill -0 $V 2>/dev/null && echo yes || echo no): $(rss $V)"
echo "== t~17: restart pipewire + wireplumber + tones"
kill $WP 2>/dev/null
pipewire > $L/log/pipewire2.log 2>&1 &
PW=$!
sleep 1
wireplumber > $L/log/wp2.log 2>&1 &
sleep 2
ffmpeg -loglevel quiet -re -f lavfi -i "sine=frequency=440" -f s16le -ar 48000 -ac 2 - 2>/dev/null | pw-cat -p --raw --rate 48000 --channels 2 --format s16 --target labsink - > /dev/null 2>&1 &
ffmpeg -loglevel quiet -re -f lavfi -i "sine=frequency=880" -f s16le -ar 48000 -ac 2 - 2>/dev/null | pw-cat -p --raw --rate 48000 --channels 2 --format s16 --target labsink2 - > /dev/null 2>&1 &
wait $V
echo "vesper rc=$?"
grep -vE "Stats" $L/log/vesper-a3.log | cut -c1-200 | tail -5
f=$(last_file); info "$f"
for a in 1 2 3; do
  echo "audio stream $a (0=mix):"
  ffmpeg -v error -i "$f" -map 0:a:$((a-1)) -af "asetnsamples=96000,astats=metadata=1:reset=1,ametadata=print:key=lavfi.astats.Overall.RMS_level:file=-" -f null - 2>&1 | grep RMS | sed 's/.*=//' | tr '\n' ' '
  echo
done
stop_game
