# Child of gamescope: games coming and going, with different sizes.
# gamescope sets DISPLAY here, so lib.sh's guard cannot run: check the rest.
[ -n "${L:-}" ] && [ "$HOME" = "$L/home" ] && [ -z "$(ls -A /run/user 2>/dev/null)" ] || { echo "game_loop.sh: not in the lab" >&2; exit 1; }
for i in 1 2 3 4 5 6 7 8; do
  if [ $((i % 2)) = 0 ]; then W=800; H=600; else W=1280; H=720; fi
  env SteamAppId=1 vkcube --wsi xcb --c $((120 + i * 20)) --width $W --height $H > /dev/null 2>&1
  if [ $((i % 3)) = 0 ]; then sleep 1; fi
done
