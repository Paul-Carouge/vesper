# T6: window snapshots of the live stream UI with demo data (no capture, no
# key), in $L/shots as PNG. Env: SIZE (440x780 logical), SCALE (2).
. $LABROOT/lib.sh
mkdir -p $L/cfg/vesper $L/shots
# No replay buffer: it would open the (missing) portal picker.
printf 'replay=false\n' > $L/cfg/vesper/settings
shot() {
  local name=$1 theme=$2 demo=$3
  gamescope --backend headless -W 1200 -H 2000 -- env VESPER_THEME=$theme SLINT_SCALE_FACTOR=${SCALE:-2} \
    VESPER_WINDOW_SIZE=${SIZE:-440x780} VESPER_DEMO="$demo" VESPER_DEMO_DELAY=1500 \
    VESPER_SNAPSHOT=$L/shots/$name.ppm $VESPER > $L/log/shot-$name.log 2>&1
  ffmpeg -y -loglevel error -i $L/shots/$name.ppm $L/shots/$name.png && rm $L/shots/$name.ppm && echo "$name: $(file -b $L/shots/$name.png | cut -d, -f2)"
}
shot live-idle-dark dark screen-look,replay-on
shot live-on-air-dark dark screen-look,replay-on,live-look
shot live-on-air-light light screen-look,replay-on,live-look,recording-look
shot live-reconnecting-dark dark screen-look,replay-on,live-reconnecting-look
shot live-dropping-light light screen-look,replay-on,live-dropping-look
shot live-settings-dark dark screen-look,settings,live-key-look,scroll:${SCROLL:-560}
shot live-settings-light light screen-look,settings,scroll:${SCROLL:-560}
