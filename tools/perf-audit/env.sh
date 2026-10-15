# Sourced by inner scripts: redirected dirs and the nested compositor.
S=$AUDIT_DIR
W=$AUDIT_REPO
export HOME=$S/home
export XDG_CONFIG_HOME=$HOME/.config XDG_DATA_HOME=$HOME/.local/share XDG_CACHE_HOME=$HOME/.cache
mkdir -p "$XDG_CONFIG_HOME/vesper" "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$HOME/out"
echo "XDG_VIDEOS_DIR=\"$HOME/out\"" > "$XDG_CONFIG_HOME/user-dirs.dirs"
unset DISPLAY
NESTED=wayland-vesper-audit

start_kwin() { # width height
    kwin_wayland --virtual --width "$1" --height "$2" --socket $NESTED > "$S/logs/kwin.log" 2>&1 &
    KWIN=$!
    i=0
    while [ ! -S "$XDG_RUNTIME_DIR/$NESTED" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done
}
stop_kwin() { kill $KWIN 2>/dev/null; wait $KWIN 2>/dev/null; }

# Layer for vkcube: only through VK_ADD_IMPLICIT_LAYER_PATH, never installed.
layer_env() {
    mkdir -p "$S/layer"
    cat > "$S/layer/vesper_capture.json" <<EOF
{
    "file_format_version": "1.2.0",
    "layer": {
        "name": "VK_LAYER_VESPER_capture",
        "type": "GLOBAL",
        "library_path": "$W/target/release/libvesper_vklayer.so",
        "api_version": "1.4.0",
        "implementation_version": "1",
        "description": "Vesper game capture (audit)",
        "disable_environment": { "VESPER_CAPTURE_DISABLE": "1" }
    }
}
EOF
    export VK_ADD_IMPLICIT_LAYER_PATH=$S/layer
}

# /proc sampler: per-thread utime+stime (ticks), ctx switches, RSS, fds.
snap() { # pid label
    for t in /proc/$1/task/*; do
        tid=${t##*/}
        printf '%s %s %s %s ' "$2" "$(date +%s.%N)" "$tid" "$(tr ' ' '_' < $t/comm)"
        printf '%s 0 ' "$(cut -d' ' -f1 $t/schedstat)"
        grep -E 'voluntary_ctxt' $t/status | awk '{printf "%s ", $2}'
        echo
    done
}

# Kills every other process of this user namespace (dbus-activated portals...).
cleanup_ns() {
    me=$(readlink /proc/self/ns/user)
    for p in /proc/[0-9]*; do
        pid=${p#/proc/}
        [ "$pid" = $$ ] && continue
        [ "$(readlink $p/ns/user 2>/dev/null)" = "$me" ] || continue
        case "$(cat $p/comm 2>/dev/null)" in sh|dbus-run-sessio|unshare|dbus-daemon) continue;; esac
        kill $pid 2>/dev/null
    done
}
