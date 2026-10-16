# T6: window snapshots of the live stream and scene UI with demo data (no
# capture, no key, no webcam), in $L/shots as PNG. Env: SIZE (440x780
# logical), SCALE (2), ONLY (a pattern: only the shots it matches).
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
mkdir -p $L/cfg/vesper $L/shots
# The scenes' demo logo.
cp $R/crates/vesper-app/data/icons/hicolor/256x256/apps/io.github.PaulCarouge.Vesper.png $L/logo.png
# No replay buffer: it would open the (missing) portal picker.
printf 'replay=false\n' > $L/cfg/vesper/settings
shot() {
  local name=$1 theme=$2 demo=$3
  case $name in ${ONLY:-*}) ;; *) return ;; esac
  local size=${SIZE:-440x780} scale=${SCALE:-2}
  # Room for the whole window.
  gamescope --backend headless -W $(( ${size%x*} * scale + 200 )) -H $(( ${size#*x} * scale + 200 )) -- env VESPER_THEME=$theme SLINT_SCALE_FACTOR=$scale \
    VESPER_WINDOW_SIZE=$size VESPER_DEMO="$demo" VESPER_DEMO_DELAY=1500 \
    VESPER_DEMO_IMAGE=$L/logo.png VESPER_SNAPSHOT=$L/shots/$name.ppm $VESPER > $L/log/shot-$name.log 2>&1
  ffmpeg -y -loglevel error -i $L/shots/$name.ppm $L/shots/$name.png && rm $L/shots/$name.ppm && echo "$name: $(file -b $L/shots/$name.png | cut -d, -f2)"
}
shot live-idle-dark dark screen-look,replay-on
shot live-on-air-dark dark screen-look,replay-on,live-look
shot live-on-air-light light screen-look,replay-on,live-look,recording-look
shot live-reconnecting-dark dark screen-look,replay-on,live-reconnecting-look
shot live-dropping-light light screen-look,replay-on,live-dropping-look
shot live-settings-dark dark screen-look,settings,live-key-look,scroll:${SCROLL:-560}
shot live-settings-light light screen-look,settings,scroll:${SCROLL:-560}
shot settings-end-dark dark screen-look,settings,scroll-end
shot settings-end-light light screen-look,settings,scroll-end
shot scenes-main-dark dark screen-look,replay-on,scenes-look,camera-look
shot scenes-main-light light screen-look,replay-on,scenes-look,recording-look
shot scenes-editor-dark dark scenes-look,scene-editor,scene-layer:2
shot scenes-editor-light light scenes-look,scene-editor,scene-layer:1
shot scenes-editor-props-dark dark scenes-look,scene-editor,scene-layer:2,scroll:${SCROLL:-700}
shot scenes-editor-empty-light light scene-editor
SIZE=360x700 shot scenes-editor-narrow-dark dark scenes-look,scene-editor,scene-layer:1
SIZE=1280x800 SCALE=1 shot scenes-editor-wide-dark dark scenes-look,scene-editor,scene-layer:2
SIZE=1280x800 SCALE=1 shot scenes-editor-wide-light light scenes-look,scene-editor,scene-layer:1
shot scenes-editor-typing-dark dark scenes-look,scene-editor,scene-layer:2,scroll:${SCROLL:-700},click:200:290,type:XYZ
# French strings are longer: the narrowest window with the main card and the settings.
SIZE=360x700 shot main-narrow-light light screen-look,replay-on,live-look,recording-look
SIZE=360x700 shot settings-narrow-dark dark screen-look,settings
SIZE=360x700 shot settings-narrow-end-light light screen-look,settings,scroll-end
SIZE=360x700 shot profiles-narrow-dark dark screen-look,profiles
SIZE=360x700 shot consent-narrow-light light game-look,consent-look
SIZE=360x700 shot filters-narrow-dark dark screen-look,filters-look,scroll-end
# Live language switch: picks English in the settings (at 360x700, after scroll-end).
SIZE=360x700 shot language-switch-light light screen-look,settings,scroll-end,click:196:408
# Audio level meters: stereo and mono bars, peak hold, a latched clip, a muted track.
shot meters-dark dark screen-look,replay-on,tracks-look,scroll-end
shot meters-light light screen-look,replay-on,tracks-look,scroll-end
SIZE=360x700 shot meters-narrow-dark dark screen-look,replay-on,tracks-look,scroll-end
