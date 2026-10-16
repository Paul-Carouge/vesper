# R2 memory: the UI with a 2 min replay buffer and a live stream to a local
# receiver, for MEM_SECS (1200); RSS/threads/fds every 30 s. The stream is
# started by the Ctrl+L intent once the game is up.
# Env: MEM_SECS, SHIM (unset: none), REPLAY_S (120), SAVE (1: Ctrl+S at the end).
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
. $LABROOT/audio_up.sh
audio_up
mkdir -p $L/cfg/vesper
printf 'replay=true\nreplay_seconds=%s\ngame_source=true\nlive_service=custom\nlive_url=rtmp://127.0.0.1:1935/live/test\nlive_height=1080\nlive_fps=60\nlive_kbps=6000\nlive_codec=h264\ntracks=\ntrack=true|false|0|desktop|Desktop\naudio_mix=true\n' ${REPLAY_S:-120} > $L/cfg/vesper/settings
ffmpeg -y -hide_banner -loglevel error -listen 1 -i rtmp://127.0.0.1:1935/live/test -c copy -f null - > $L/log/rtmp-mem.log 2>&1 &
SRV=$!
sleep 1
export GAME=1 SHIM
export DEMO="ctrl+l"
export DELAY=${DELAY:-12000}
gamescope --backend headless -W 1280 -H 800 -r 60 -- bash $LABROOT/r2_ui_child.sh > $L/log/gs.log 2>&1 &
GS=$!
sleep 6
V=$(pid_of vesper)
t=6
end=$((${MEM_SECS:-1200}))
rm -f $L/log/mem-samples.log
while [ $t -lt $end ] && kill -0 $V 2>/dev/null; do
  echo "t=$t $(rss $V)" >> $L/log/mem-samples.log
  sleep 30
  t=$((t+30))
done
if [ "${SAVE:-1}" = 1 ]; then
  echo "== replay save at t=$t: $(rss $V)"
fi
cat $L/log/mem-samples.log | awk 'NR<=3||NR%4==0'
tail -1 $L/log/mem-samples.log
grep -E "live:|Stream|WARN|ERROR|dropped" $L/log/ui.log | grep -v "Stats {" | sort | uniq -c | sort -rn | head -12 | cut -c1-200
grep -c StreamStats $L/log/ui.log
kill -TERM $V
wait $GS 2>/dev/null
kill $SRV 2>/dev/null
