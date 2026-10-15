# Usage (inside ns.sh): run_game.sh TAG W H REC_FPS GAME_LIMIT SECONDS MODE [BIN]
# MODE: rec (layer + vesper --record-game), dormant (layer, no vesper),
#       nolayer (no layer), recshim (rec with the syscall shim)
. $AUDIT_TOOLS/env.sh
TAG=$1 WID=$2 HEI=$3 FPS=$4 LIMIT=$5 SECS=$6 MODE=$7
BIN=${8:-$W/target-audit/release/vesper}
OUT=$S/runs/$TAG
rm -rf "$OUT"; mkdir -p "$OUT" $S/logs
cat > "$XDG_CONFIG_HOME/vesper/settings" <<EOF
tracks=
track=true|false|0|desktop|Desktop
track=true|false|0|output:easyeffects_sink|Speakers
audio_mix=true
EOF
start_kwin $WID $HEI
[ "$MODE" != nolayer ] && layer_env
MH="autostart_log=4,log_duration=$((SECS-8)),output_folder=$OUT"
[ "$LIMIT" != 0 ] && MH="$MH,fps_limit=$LIMIT"
SteamAppId=1 MANGOHUD=1 MANGOHUD_CONFIG="$MH" WAYLAND_DISPLAY=$NESTED \
    vkcube --wsi wayland --width $WID --height $HEI --present_mode 1 > "$OUT/vkcube.log" 2>&1 &
GAME=$!
sleep 1
VP=
case $MODE in
rec) VESPER_FPS=$FPS "$BIN" --record-game $SECS > "$OUT/vesper.log" 2>&1 & VP=$! ;;
recshim) SHIM_BT=$OUT/bt.txt SHIM_OUT=$OUT/shim.txt LD_PRELOAD=$AUDIT_TOOLS/sysshim.so VESPER_FPS=$FPS "$BIN" --record-game $SECS > "$OUT/vesper.log" 2>&1 & VP=$! ;;
esac
. $AUDIT_TOOLS/sample.sh
[ -n "$VP" ] && { wait $VP; echo "vesper exit $?" >> "$OUT/vesper.log"; }
kill $GAME; wait $GAME
stop_kwin
for f in "$HOME"/out/Vesper/*.mkv; do
    [ -f "$f" ] || continue
    ffprobe -v error -count_packets -select_streams v:0 -show_entries stream=nb_read_packets,width,height,avg_frame_rate:format=duration,size -of compact "$f" > "$OUT/probe.txt" 2>&1
    rm -f "$f"
done
cleanup_ns
echo done
