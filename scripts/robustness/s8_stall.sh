# S8: Vesper itself stalls (SIGSTOP 8 s, like a suspended laptop or a swapping machine) mid-recording
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
. $LABROOT/audio_up.sh
audio_up
mkdir -p $L/cfg/vesper
printf 'tracks=\ntrack=true|false|0|desktop|Desktop\naudio_mix=true\nreplay=false\n' > $L/cfg/vesper/settings
start_game s8
$VESPER --record-game 30 > $L/log/vesper-s8.log 2>&1 &
V=$!
sleep 10
echo "== SIGSTOP vesper for 8 s"
kill -STOP $V
sleep 8
kill -CONT $V
wait $V
grep -vE "Stats" $L/log/vesper-s8.log | cut -c1-200 | tail -4
f=$(last_file); info "$f"
ffprobe -v error -show_entries packet=stream_index,pts_time -of csv=p=0 "$f" > $L/log/pts.csv
for s in 0 1 2; do echo "stream $s: n=$(grep -c "^$s," $L/log/pts.csv) last=$(grep "^$s," $L/log/pts.csv | sort -t, -k2 -g | tail -1)"; done
echo "largest video pts gap (s):"
grep "^0," $L/log/pts.csv | cut -d, -f2 | sort -g | awk 'NR>1{d=$1-p; if (d>m) m=d} {p=$1} END{print m}'
echo "largest audio pts gap (s):"
grep "^1," $L/log/pts.csv | cut -d, -f2 | sort -g | awk 'NR>1{d=$1-p; if (d>m) m=d} {p=$1} END{print m}'
ffmpeg -v error -i "$f" -f null - 2>&1 | head -3
stop_game
