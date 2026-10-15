# A2: device unplug (destroyed null sink used as a track), app exit/restart, mid-recording.
. $LABROOT/lib.sh
tone() { # tone <target> <freq> <appname> -> pid in TP
  ffmpeg -loglevel quiet -re -f lavfi -i "sine=frequency=$2" -f s16le -ar 48000 -ac 2 - 2>/dev/null | pw-cat -p --raw --rate 48000 --channels 2 --format s16 --target $1 -P "{ application.name=$3 }" - > /dev/null 2>&1 &
  TP=$!
}
mkdir -p $L/cfg/vesper
cat > $L/cfg/vesper/settings <<'EOF'
tracks=
track=true|false|0|desktop|Desktop
track=true|false|0|output:labsink2|Dev2
track=true|false|0|app:LabApp|App
audio_mix=true
replay=false
EOF
tone labsink 440 Base
tone labsink2 880 Dev
DEV=$TP
start_game a2
$VESPER --record-game 44 > $L/log/vesper-a2.log 2>&1 &
V=$!
sleep 6   # recording starts after the game connects (~1 s)
tone labsink 1320 LabApp
APP=$TP
sleep 10
echo "== t=10: destroy labsink2, kill the app"
id=$(pw-cli ls Node | grep -B40 'node.name = "labsink2"' | grep -oE 'id [0-9]+' | tail -1 | grep -oE '[0-9]+')
pw-cli destroy $id
kill $APP 2>/dev/null; pkill -P $DEV 2>/dev/null
sleep 10
echo "== t=20: sink2 back, app back"
mkfifo $L/cmd; pw-cli < $L/cmd > $L/log/pwcli.log 2>&1 &
exec 3> $L/cmd
echo "create-node adapter { factory.name=support.null-audio-sink node.name=labsink2 media.class=Audio/Sink object.linked=true audio.position=[FL FR] }" >&3
sleep 2
tone labsink2 880 Dev
tone labsink 1320 LabApp
sleep 6
echo "== graph after replug"
pw-link -l
pw-dump | jq -r '.[] | select(.type=="PipeWire:Interface:Node") | [.id, .info.state, .info.props["node.name"], .info.props["target.object"]] | @tsv'
sleep 19
wait $V
grep -vE "Stats" $L/log/vesper-a2.log | cut -c1-200 | tail -6
f=$(last_file); info "$f"
# RMS per 4 s window per track (ffmpeg astats)
for a in 1 2 3; do
  echo "audio track $a:"
  ffmpeg -v error -i "$f" -map 0:a:$((a)) -af "asetnsamples=192000,astats=metadata=1:reset=1,ametadata=print:key=lavfi.astats.Overall.RMS_level:file=-" -f null - 2>&1 | grep RMS | sed 's/.*=//' | tr '\n' ' '
  echo
done
stop_game
