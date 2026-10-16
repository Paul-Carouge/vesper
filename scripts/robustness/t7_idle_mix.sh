# T7: CPU of the audio threads with 2 tracks, no mix track, no stream: the
# mix must stay idle. BEFORE and AFTER: two optimized builds with the dev
# flags (see t5_cost.sh) to compare.
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
. $LABROOT/audio_up.sh
audio_up
start_game t7
mkdir -p $L/cfg/vesper
printf 'audio_mix=false\n' > $L/cfg/vesper/settings
for bin in ${BEFORE:-} ${AFTER:-$VESPER}; do
  for run in 1 2; do
    $bin --record-game 26 > $L/log/vesper-t7.log 2>&1 &
    v=$!
    sleep 8
    echo "$(basename $bin) run $run: $(python3 -I $LABROOT/cost.py $v 15 vesper-audio)"
    wait $v
  done
done
stop_game
