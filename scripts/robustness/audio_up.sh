# Creates a null sink "labsink" and plays a 440 Hz tone into it. Sets TONE (pid of the player pipeline).
audio_up() {
  pw-cli create-node adapter '{ factory.name=support.null-audio-sink node.name=labsink node.description=labsink media.class=Audio/Sink object.linked=true audio.position=[FL FR] }' > $L/log/sink.log 2>&1
  sleep 1
  ffmpeg -loglevel quiet -re -f lavfi -i "sine=frequency=440" -f s16le -ar 48000 -ac 2 - 2>/dev/null | pw-cat -p --raw --rate 48000 --channels 2 --format s16 --target labsink - > $L/log/tone.log 2>&1 &
  TONE=$!
  sleep 1
}
