# R2 pace: what the layer's Pacer costs and saves while recording a game
# faster than the recording rate. Wakeups of Vesper's capture server and of
# the game's IPC thread (one per copied frame), then repeated frames in the
# file (consecutive identical framemd5). Env: SIZE (2560x1440), HZ (144),
# WARM (8), WIN (15), SECS (WARM+WIN+5).
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
. $LABROOT/audio_up.sh
WARM=${WARM:-8}
WIN=${WIN:-15}
SECS=${SECS:-$((WARM + WIN + 5))}
audio_up
mkdir -p $L/cfg/vesper
printf 'tracks=\ntrack=true|false|0|desktop|Desktop\naudio_mix=false\nreplay=false\n' > $L/cfg/vesper/settings
GS_ARGS="-r ${HZ:-144}" start_game pace ${SIZE:-2560x1440}
GAME=$(pid_of display-pace)
$VESPER --record-game $SECS > $L/log/vesper-pace.log 2>&1 &
V=$!
sleep $WARM
python3 -I $LABROOT/r2_sample.py $GAME $WIN "game (vkcube) $GAME" > $L/game-sample.txt &
python3 -I $LABROOT/r2_sample.py $V $WIN "vesper ${SIZE:-2560x1440}@${HZ:-144}"
sleep 1
grep -E "TOTAL|vesper-capture|==" $L/game-sample.txt
wait $V
grep -E "Stats \{" $L/log/vesper-pace.log | tail -1 | cut -c1-300
f=$(last_file); info "$f"
ffmpeg -v error -i "$f" -map 0:v -f framemd5 - 2>/dev/null | awk -F', *' '
  /^#/ { next }
  { n++; if ($NF == prev) rep++; prev = $NF }
  END { printf "frames %d, repeated %d\n", n, rep }'
stop_game
