# Time from `vesper --record-game` launch to its first events, game already running.
. $AUDIT_TOOLS/env.sh
OUT=$S/runs/F1_first_frame
rm -rf "$OUT"; mkdir -p "$OUT"
cat > "$XDG_CONFIG_HOME/vesper/settings" <<EOF
tracks=
track=true|false|0|desktop|Desktop
track=true|false|0|output:easyeffects_sink|Speakers
audio_mix=true
EOF
start_kwin 3840 2160
layer_env
SteamAppId=1 WAYLAND_DISPLAY=$NESTED vkcube --wsi wayland --width 3840 --height 2160 --present_mode 1 > /dev/null 2>&1 &
GAME=$!
sleep 3
for n in 1 2 3; do
    T0=$(date +%s.%N)
    "$W/target-audit/release/vesper" --record-game 3 2>&1 | python3 -I $AUDIT_TOOLS/tstamp.py $T0 | grep -E "listening|waiting|capturing|encoding|recording to|Started|Finished" > "$OUT/run$n.txt"
    sleep 2
done
kill $GAME; wait $GAME
stop_kwin
rm -f "$HOME"/out/Vesper/*.mkv
cleanup_ns
echo done
