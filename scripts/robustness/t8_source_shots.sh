# T8: the source card's states, in English and French, in $L/shots as PNG:
# no source, a source, the picker open, Change clicked while recording (the
# notice says why the source stays), and the picker failing (the lab has no
# ScreenCast portal: the source stays, with an error).
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
mkdir -p $L/cfg/vesper $L/shots
printf 'replay=false\n' > $L/cfg/vesper/settings
shot() {
  local name=$1 lang=$2 demo=$3
  gamescope --backend headless -W 1080 -H 1600 -- env LANGUAGE=$lang VESPER_THEME=dark SLINT_SCALE_FACTOR=2 \
    VESPER_WINDOW_SIZE=440x700 VESPER_DEMO="$demo" VESPER_DEMO_DELAY=1500 \
    VESPER_SNAPSHOT=$L/shots/$name-$lang.ppm $VESPER > $L/log/shot-$name-$lang.log 2>&1
  ffmpeg -y -loglevel error -i $L/shots/$name-$lang.ppm $L/shots/$name-$lang.png && rm $L/shots/$name-$lang.ppm && echo "$name-$lang: ok"
}
for lang in en fr; do
  shot none $lang replay-off
  shot screen $lang screen-look
  shot picking $lang screen-look,picking-look
  shot locked $lang screen-look,recording-look,source
  shot failed $lang screen-look,source
done
