# Usage (inside ns.sh): run_obs.sh TAG W H REC_FPS GAME_LIMIT SECONDS
# OBS Studio + obs-vkcapture recording vkcube, HEVC VAAPI CQP 28, all config
# under the scratch HOME.
. $AUDIT_TOOLS/env.sh
TAG=$1 WID=$2 HEI=$3 FPS=$4 LIMIT=$5 SECS=$6
OUT=$S/runs/$TAG
rm -rf "$OUT"; mkdir -p "$OUT" $S/logs "$HOME/obsout"
O=$XDG_CONFIG_HOME/obs-studio
rm -rf "$O"; mkdir -p "$O/basic/profiles/Audit" "$O/basic/scenes"
for f in global.ini user.ini; do cat > "$O/$f" <<EOF
[General]
FirstRun=true
LastVersion=536870912
EnableAutoUpdates=false
ConfirmOnExit=false
[Basic]
Profile=Audit
ProfileDir=Audit
SceneCollection=Audit
SceneCollectionFile=Audit
[BasicWindow]
SysTrayEnabled=true
SysTrayWhenStarted=true
EOF
done
cat > "$O/basic/profiles/Audit/basic.ini" <<EOF
[General]
Name=Audit
[Output]
Mode=Advanced
[AdvOut]
RecType=Standard
RecFilePath=$HOME/obsout
RecFormat2=mkv
RecEncoder=hevc_ffmpeg_vaapi_tex
RecTracks=1
RecRescale=false
[Video]
BaseCX=$WID
BaseCY=$HEI
OutputCX=$WID
OutputCY=$HEI
FPSType=1
FPSInt=$FPS
ScaleType=bicubic
ColorFormat=NV12
ColorSpace=709
ColorRange=Partial
[Audio]
SampleRate=48000
ChannelSetup=Stereo
EOF
cat > "$O/basic/profiles/Audit/recordEncoder.json" <<EOF
{"rate_control":"CQP","qp":28,"keyint_sec":2,"bf":0,"vaapi_device":"/dev/dri/renderD128"}
EOF
cat > "$O/basic/scenes/Audit.json" <<EOF
{"name":"Audit","current_scene":"Scene","current_program_scene":"Scene",
 "scene_order":[{"name":"Scene"}],
 "DesktopAudioDevice1":{"id":"pulse_output_capture","versioned_id":"pulse_output_capture","name":"Desktop Audio","uuid":"5b1d0a2e-0000-4000-8000-000000000003","settings":{"device_id":"default"},"mixers":255,"volume":1.0},
 "sources":[
  {"id":"scene","versioned_id":"scene","name":"Scene","uuid":"5b1d0a2e-0000-4000-8000-000000000001","mixers":0,
   "settings":{"id_counter":1,"items":[{"name":"Game","source_uuid":"5b1d0a2e-0000-4000-8000-000000000002","visible":true,"locked":false,"id":1,"pos":{"x":0.0,"y":0.0},"rot":0.0,"scale":{"x":1.0,"y":1.0},"align":5,"bounds_type":0,"bounds_align":0,"bounds":{"x":0.0,"y":0.0},"crop_left":0,"crop_top":0,"crop_right":0,"crop_bottom":0}]}},
  {"id":"vkcapture-source","versioned_id":"vkcapture-source","name":"Game","uuid":"5b1d0a2e-0000-4000-8000-000000000002","settings":{},"mixers":0}
 ]}
EOF
start_kwin $WID $HEI
MH="autostart_log=4,log_duration=$((SECS-8)),output_folder=$OUT"
[ "$LIMIT" != 0 ] && MH="$MH,fps_limit=$LIMIT"
OBS_VKCAPTURE=1 MANGOHUD=1 MANGOHUD_CONFIG="$MH" WAYLAND_DISPLAY=$NESTED \
    vkcube --wsi wayland --width $WID --height $HEI --present_mode 1 > "$OUT/vkcube.log" 2>&1 &
GAME=$!
sleep 1
DRI_PRIME=1002:7550 QT_QPA_PLATFORM=wayland WAYLAND_DISPLAY=$NESTED obs --startrecording --minimize-to-tray --disable-updater --disable-shutdown-check --multi > "$OUT/obs.log" 2>&1 &
VP=$!
. $AUDIT_TOOLS/sample.sh
kill -INT $VP; i=0
while [ -d /proc/$VP ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done
kill $VP 2>/dev/null; wait $VP
kill $GAME; wait $GAME
stop_kwin
for f in "$HOME"/obsout/*.mkv; do
    [ -f "$f" ] || continue
    ffprobe -v error -count_packets -select_streams v:0 -show_entries stream=nb_read_packets,width,height,avg_frame_rate:format=duration,size -of compact "$f" > "$OUT/probe.txt" 2>&1
    rm -f "$f"
done
cp -r "$O/logs" "$OUT/obslogs" 2>/dev/null
cleanup_ns
echo done
