# T4: what the user reads when the stream cannot start: a server that hangs
# up after the handshake (a refused key), nothing listening (a wrong URL),
# and an unknown host (retried: it may be the network). The recording goes
# on in every case.
. $LABROOT/lib.sh
start_game t4
python3 -I $LABROOT/rtmp_reject.py 1935 1 > $L/log/reject.log 2>&1 &
S=$!
sleep 1
for url in rtmp://127.0.0.1:1935/live/wrong-key rtmp://127.0.0.1:1936/live/test rtmp://no-such-host.invalid/live/test; do
  echo "--- $url"
  VESPER_STREAM_RECORD=1 $VESPER --stream-game 8 $url > $L/log/vesper-t4.log 2>&1
  grep -E "live:|Stream\(|Finished" $L/log/vesper-t4.log | cut -c1-200
done
kill $S 2>/dev/null
wait $S 2>/dev/null
stop_game
