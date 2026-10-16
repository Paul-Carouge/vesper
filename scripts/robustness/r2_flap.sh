# R2 flap: a track on an explicit device that is the default sink (output:labsink, running):
# the capture stream reaches Streaming, then WirePlumber answers "defined target not found",
# over and over (backoff reset by pw.rs: streamed). Prints the CPU of the loop.
# Env: TRACK (output:labsink), DEFAULT (name of the sink made default first; empty: leave).
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
. $LABROOT/audio_up.sh
audio_up
pw-dump | jq -r '.[] | select(.type=="PipeWire:Interface:Node") | [.id, .info.state, .info.props["media.class"], .info.props["node.name"]] | @tsv'
wpctl status 2>&1 | grep -iE "sink|\*" | head
if [ -n "$DEFAULT" ]; then
  id=$(pw-dump | jq -r ".[] | select(.type==\"PipeWire:Interface:Node\") | select(.info.props[\"node.name\"]==\"$DEFAULT\") | .id")
  pw-metadata -n default 0 default.configured.audio.sink "{ \"name\": \"$DEFAULT\" }" > /dev/null
  sleep 1
  echo "default now: $(wpctl status | grep -E '^ │  \*' | head -2 | tr -s ' ')"
fi
mkdir -p $L/cfg/vesper
printf 'tracks=\ntrack=true|false|0|%s|T\naudio_mix=false\nreplay=false\n' "${TRACK:-output:labsink}" > $L/cfg/vesper/settings
start_game dbg
$VESPER --record-game 16 > $L/log/dbg.log 2>&1 &
V=$!
sleep 6
python3 -I $LABROOT/r2_sample.py $V 8 "flap"
wait $V
echo "reconnects: $(grep -c 'Reconnecting' $L/log/dbg.log), running: $(grep -c 'Running' $L/log/dbg.log)"
grep -E "AudioTrack" $L/log/dbg.log | head -3 | cut -c1-200
stop_game
