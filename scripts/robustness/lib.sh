# Helpers for scenarios (sourced). Runs inside run.sh's namespaces.
#
# Guard: scenarios run built binaries ($VESPER, test binaries), so only inside
# run.sh's lab (private user namespace, /run/user hidden, no display, the lab's
# HOME). Never run a scenario directly: go through `go`, `r2` or `run.sh`.
lab_abort() { echo "lib.sh: not in the lab ($1), refusing to run" >&2; exit 1; }
[ -z "${WAYLAND_DISPLAY:-}${DISPLAY:-}" ] || lab_abort "WAYLAND_DISPLAY/DISPLAY set"
[ -z "$(ls -A /run/user 2>/dev/null)" ] || lab_abort "/run/user visible"
read -r _ns_in _ns_out _ < /proc/self/uid_map
[ "$_ns_in $_ns_out" != "0 0" ] || lab_abort "no user namespace"
[ "$HOME" != "$(getent passwd "$_ns_out" | cut -d: -f6)" ] || lab_abort "HOME is the real home"
[ -n "${L:-}" ] && [ "$HOME" = "$L/home" ] || lab_abort "HOME is not the lab's"
#
# start_game <name> : gamescope headless + vkcube; sets GS (gamescope pid).
# $L/run/display-<name> gets gamescope's Xwayland DISPLAY (see game_display).
start_game() {
  local size=${2:-1280x720}
  gamescope --backend headless -W ${size%x*} -H ${size#*x} ${GS_ARGS:-} -- bash -c 'echo "$DISPLAY" > "$0"; exec env SteamAppId=1 vkcube --width "$1" --height "$2"' $L/run/display-$1 ${size%x*} ${size#*x} > $L/log/game-$1.log 2>&1 &
  GS=$!
  sleep 5
}
# game_display <name>: the DISPLAY gamescope gave its game (never a hardcoded one)
game_display() { cat $L/run/display-$1; }
# kill the vkcube client only (the game crash)
kill_game_client() { kill -9 $(pgrep -x vkcube) 2>/dev/null; }
stop_game() { kill $GS 2>/dev/null; wait $GS 2>/dev/null; }
# info <file>: streams and packet counts
info() {
  ffprobe -v error -count_packets -show_entries stream=codec_type,codec_name,nb_read_packets,start_time -of compact "$1" 2>&1 | grep -v "^\[" | tr '\n' ' '; echo
  ffprobe -v error -show_entries format=duration,size -of compact "$1" 2>&1 | tr '\n' ' '; echo
}
# last_file: newest mkv in $L/vid (or dir $1)
last_file() { ls -t ${1:-$L/vid}/Vesper/*.mkv 2>/dev/null | head -1; }
# rss <pid>
rss() { grep -E "VmRSS|Threads" /proc/$1/status | tr '\n' ' '; echo "fds=$(ls /proc/$1/fd | wc -l)"; }
