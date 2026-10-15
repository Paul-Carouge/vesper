# Helpers for scenarios (sourced). Runs inside run.sh's namespaces.
# start_game <name> : gamescope headless + vkcube; sets GS (gamescope pid).
# $L/run/display-<name> gets gamescope's Xwayland DISPLAY (see game_display).
start_game() {
  local size=${2:-1280x720}
  gamescope --backend headless -W ${size%x*} -H ${size#*x} -- bash -c 'echo "$DISPLAY" > "$0"; exec env SteamAppId=1 vkcube --width "$1" --height "$2"' $L/run/display-$1 ${size%x*} ${size#*x} > $L/log/game-$1.log 2>&1 &
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
