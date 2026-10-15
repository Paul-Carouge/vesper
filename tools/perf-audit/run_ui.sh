# Usage (inside ns.sh): run_ui.sh TAG MODE SECONDS [FPS]
# MODE: nogame (UI, replay on, game source, no game), replay (with vkcube),
#       record (with vkcube, recording starts 6 s in), hidden (--background, no tray)
. $AUDIT_TOOLS/env.sh
TAG=$1 MODE=$2 SECS=$3 FPS=${4:-60}
BIN=$W/target-audit/release/vesper
OUT=$S/runs/$TAG
rm -rf "$OUT"; mkdir -p "$OUT" $S/logs
cat > "$XDG_CONFIG_HOME/vesper/settings" <<EOF
fps=$FPS
replay=true
replay_seconds=60
game_source=true
tracks=
track=true|${MUTED:-false}|0|desktop|Desktop
track=true|${MUTED:-false}|0|output:easyeffects_sink|Speakers
audio_mix=true
EOF
start_kwin 2560 1440
GAME=
if [ "$MODE" != nogame ]; then
    layer_env
    SteamAppId=1 WAYLAND_DISPLAY=$NESTED MANGOHUD=1 MANGOHUD_CONFIG="no_display,fps_limit=144" \
        vkcube --wsi wayland --width 2560 --height 1440 --present_mode 1 > "$OUT/vkcube.log" 2>&1 &
    GAME=$!
    sleep 1
fi
DEMO=
[ "$MODE" = record ] && DEMO="VESPER_DEMO=recording VESPER_DEMO_DELAY=6000"
env $DEMO VESPER_T0=$(date +%s%N) WAYLAND_DISPLAY=$NESTED "$BIN" > "$OUT/vesper.log" 2>&1 &
VP=$!
. $AUDIT_TOOLS/sample.sh
kill -TERM $VP; wait $VP
echo "vesper exit $?" >> "$OUT/vesper.log"
[ -n "$GAME" ] && { kill $GAME; wait $GAME; }
stop_kwin
for f in "$HOME"/out/Vesper/*.mkv; do
    [ -f "$f" ] || continue
    ffprobe -v error -count_packets -select_streams v:0 -show_entries stream=nb_read_packets,width,height,avg_frame_rate:format=duration,size -of compact "$f" > "$OUT/probe.txt" 2>&1
    rm -f "$f"
done
cleanup_ns
echo done
