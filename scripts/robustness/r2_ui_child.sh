# gamescope's child for r2_ui.sh: an optional vkcube "game", then the Vesper UI.
# gamescope sets DISPLAY here, so lib.sh's guard cannot run: check the rest.
[ -n "${L:-}" ] && [ "$HOME" = "$L/home" ] && [ -z "$(ls -A /run/user 2>/dev/null)" ] || { echo "r2_ui_child.sh: not in the lab" >&2; exit 1; }
if [ "${GAME:-0}" = 1 ]; then
  env SteamAppId=1 vkcube --wsi xcb --width 1280 --height 720 > $L/log/cube.log 2>&1 &
  echo $! > $L/run/cube.pid
  sleep 2
fi
echo $$ > $L/run/vesper.pid
exec env ${SHIM:+LD_PRELOAD=$SHIM} ${DEMO:+VESPER_DEMO=$DEMO VESPER_DEMO_DELAY=${DELAY:-3000}} $VESPER ${ARGS:-} > $L/log/ui.log 2>&1
