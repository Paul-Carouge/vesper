# S2b: SIGKILL mid-recording leaves a marker; the next launch of the UI
# remuxes the file in place (cues, duration) without losing a packet
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
. $LABROOT/audio_up.sh
M=$HOME/.local/state/vesper/recordings-in-progress
rm -rf "$M"
audio_up
start_game s2b
$VESPER --record-game 4 > $L/log/vesper-s2b-clean.log 2>&1
echo "markers after a clean finish: $(ls $M | wc -l)"
$VESPER --record-game 60 > $L/log/vesper-s2b.log 2>&1 &
V=$!
sleep 12
kill -9 $V
wait $V 2>/dev/null
stop_game
f=$(last_file); echo "$f"
echo "markers after the kill: $(ls $M | wc -l) -> $(tail -c +1 $M/* | tail -1)"
cues() { python3 -c 'import sys; d=open(sys.argv[1],"rb").read(); print("cues" if b"\x1c\x53\xbb\x6b" in d[-4<<20:] else "no cues")' "$1"; }
echo "before: $(cues "$f")"; info "$f"
ffprobe -v error -show_entries packet=stream_index,pts_time,size,flags -of csv=p=0 "$f" > $L/log/s2b-before.csv
# The UI launches (gamescope headless), in French, and snapshots its window
# once the recovery notice shows (in $L/shots/s2b-notice.png), then quits.
mkdir -p $L/cfg/vesper $L/shots
printf 'replay=false\n' > $L/cfg/vesper/settings
gamescope --backend headless -W 1100 -H 1760 -- env LANGUAGE=fr SLINT_SCALE_FACTOR=2 \
  VESPER_WINDOW_SIZE=440x780 VESPER_DEMO=none VESPER_DEMO_DELAY=2500 \
  VESPER_SNAPSHOT=$L/shots/s2b-notice.ppm $VESPER > $L/log/ui.log 2>&1
ffmpeg -y -loglevel error -i $L/shots/s2b-notice.ppm $L/shots/s2b-notice.png && echo "snapshot: $L/shots/s2b-notice.png"
echo "markers after the launch: $(ls $M | wc -l)"
grep -E "interrupted recording" $L/log/ui.log
ls "$L/vid/Vesper"
echo "after: $(cues "$f")"; info "$f"
ffprobe -v error -show_entries packet=stream_index,pts_time,size,flags -of csv=p=0 "$f" > $L/log/s2b-after.csv
cmp -s $L/log/s2b-before.csv $L/log/s2b-after.csv && echo "packets identical ($(wc -l < $L/log/s2b-after.csv))" || { echo "PACKETS DIFFER"; diff $L/log/s2b-before.csv $L/log/s2b-after.csv | head; }
ffprobe -v error -select_streams v:0 -show_entries stream=color_range,color_space,color_transfer,color_primaries,chroma_location -of compact "$f"
ffprobe -v error -show_entries stream=index:stream_tags=title:stream_disposition=default -of compact "$f"
ffmpeg -v error -i "$f" -f null - 2>&1 | head -5; echo "decode rc=$?"
# A seek in the middle lands at once: the cues are used.
TIMEFORMAT="seek+1 frame: %Rs"; time ffmpeg -v error -ss 4 -i "$f" -frames:v 1 -f null -
