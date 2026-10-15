# Sourced: samples $VP (recorder) and $GAME once per second for $SECS s,
# or until $VP exits.
engines() { # pid -> summed drm-engine ns over unique clients
    for f in /proc/$1/fdinfo/*; do awk '/^drm-client-id:/{id=$2} /^drm-engine-/{sub(":","",$1); printf "%s %s %s\n", id, $1, $2}' "$f" 2>/dev/null; done \
        | awk '{k=$1" "$2; if ($3>m[k]) m[k]=$3} END{for(k in m){split(k,a," "); s[a[2]]+=m[k]} for(e in s) printf "%s=%s ", e, s[e]}'
}
i=0
while [ $i -lt "$SECS" ]; do
    ts=$(date +%s.%N)
    echo "$ts gpu_busy=$(cat /sys/class/drm/card1/device/gpu_busy_percent)" >> "$OUT/gpu.txt"
    [ -n "$VP" ] && [ -d /proc/$VP ] && {
        snap $VP v >> "$OUT/threads.txt"
        echo "$ts rss_kb=$(awk '/VmRSS/{print $2}' /proc/$VP/status) fds=$(ls /proc/$VP/fd | wc -l) threads=$(ls /proc/$VP/task | wc -l) $(engines $VP)" >> "$OUT/vproc.txt"
    }
    [ -n "$GAME" ] && [ -d /proc/$GAME ] && {
        echo "$ts $(cat /proc/$GAME/task/*/schedstat | awk '{s+=$1} END{print s}') 0 $(engines $GAME)" >> "$OUT/game.txt"
    }
    [ -n "$VP" ] && ! [ -d /proc/$VP ] && break
    sleep 1
    i=$((i+1))
done
