# Runs as gamescope's child: a vkcube "game" and the Vesper UI, side by side.
env SteamAppId=1 vkcube --wsi xcb --width 1280 --height 720 > $L/log/cube.log 2>&1 &
sleep 2
exec env VESPER_DEMO="${DEMO:-recording}" VESPER_DEMO_DELAY="${DELAY:-6000}" $VESPER > $L/log/ui.log 2>&1
