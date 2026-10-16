# R2 UI idle cost: the real window under headless gamescope, with a fake tray.
# Env: REPLAY (true|false), HIDE_AT (seconds after launch to click the tray:
# hides the window; empty: stays shown), GAME (1: a vkcube alongside, the
# replay buffer then really encodes), WARM (12), WIN (20), SHIM, TRAY (0: no
# tray at all), MIX (true), MUTED (false), DEMO (VESPER_DEMO intents, e.g.
# settings,desktop-devices to open the device list: NodeWatch runs), CHURN (1:
# short-lived audio streams come and go on the graph).
. $LABROOT/lib.sh
. $LABROOT/audio_up.sh
audio_up
WARM=${WARM:-12}
WIN=${WIN:-20}
mkdir -p $L/cfg/vesper
printf 'replay=%s\nreplay_seconds=60\ngame_source=true\ntracks=\ntrack=true|%s|0|desktop|Desktop\ntrack=true|%s|0|output:labsink2|Dev2\naudio_mix=%s\n' \
  ${REPLAY:-false} ${MUTED:-false} ${MUTED:-false} ${MIX:-true} > $L/cfg/vesper/settings
if [ "${TRAY:-1}" = 1 ]; then
  python3 -I $LABROOT/r2_fake_tray.py ${HIDE_AT:-} > $L/log/tray.log 2>&1 &
  sleep 1
fi
export GAME SHIM ARGS DEMO
export SHIM_OUT=$L/shim.out
gamescope --backend headless -W 1280 -H 800 -r 60 -- bash $LABROOT/r2_ui_child.sh > $L/log/gs.log 2>&1 &
GS=$!
if [ "${CHURN:-0}" = 1 ]; then
  ( while :; do
      ffmpeg -loglevel quiet -f lavfi -i "sine=frequency=500:duration=0.3" -f s16le -ar 48000 -ac 2 - 2>/dev/null | pw-cat -p --raw --rate 48000 --channels 2 --format s16 --target labsink - > /dev/null 2>&1
      sleep 0.2
    done ) &
  CH=$!
fi
sleep 4
V=$(pgrep -x vesper | head -1)
echo "vesper pid $V; tray: $(tr '\n' ' ' < $L/log/tray.log)"
sleep $WARM
python3 -I $LABROOT/r2_sample.py $V $WIN "ui replay=${REPLAY:-false} hide_at=${HIDE_AT:-never} game=${GAME:-0} tray=${TRAY:-1}"
echo "tray: $(tr '\n' ' ' < $L/log/tray.log)"
grep -vE "Stats|inotify" $L/log/ui.log | cut -c1-160 | tail -4
kill -TERM $V
[ -n "$CH" ] && kill $CH
wait $GS 2>/dev/null
