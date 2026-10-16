# Runs as gamescope's child: a vkcube "game" and the Vesper UI, side by side.
# gamescope sets DISPLAY here, so lib.sh's guard cannot run: check the rest.
[ -n "${L:-}" ] && [ "$HOME" = "$L/home" ] && [ -z "$(ls -A /run/user 2>/dev/null)" ] || { echo "ui_child.sh: not in the lab" >&2; exit 1; }
env SteamAppId=1 vkcube --wsi xcb --width 1280 --height 720 > $L/log/cube.log 2>&1 &
echo $! > $L/run/cube.pid
sleep 2
echo $$ > $L/run/vesper.pid
exec env VESPER_DEMO="${DEMO:-recording}" VESPER_DEMO_DELAY="${DELAY:-6000}" $VESPER > $L/log/ui.log 2>&1
