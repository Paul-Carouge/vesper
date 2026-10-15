#!/bin/bash
# Failure-injection lab for Vesper (audit harness, never touches the real session).
#
#   scripts/robustness/go <lab-name> <scenario>.sh [args]    # e.g.: go s1 s1_full.sh
#   scripts/robustness/run.sh <scenario>.sh [args]           # same, unfiltered, no timeout
#
# Needs: cargo build --workspace (debug: --record-game and VESPER_DEMO exist
# only there), unshare with user namespaces, gamescope, vkcube, pipewire,
# wireplumber, dbus-run-session, ffmpeg/ffprobe, a Vulkan GPU with VAAPI.
#
# Everything runs in a private user+mount+pid namespace (uid 0 inside, so the
# capture socket is /tmp/vesper-0, never the real /tmp/vesper-<uid>), with its
# own D-Bus session, PipeWire and WirePlumber (ALSA/V4L2/BlueZ monitors
# disabled: no real device is ever opened), its own XDG directories under
# $VESPER_LAB_DATA (default /tmp/vesper-lab-<uid>) and a headless gamescope as
# compositor. Games are vkcube with SteamAppId=1 and the layer from
# target/debug via VK_ADD_IMPLICIT_LAYER_PATH. Everything inside dies when the
# scenario ends. Scenarios: s* shutdown/storage, a* audio, e* encoder, f* hostile
# clients, l* layer and game churn, p* prefs/instances.
HERE=$(dirname "$(readlink -f "$0")")
export HERE
mkdir -p /tmp/vesper-0
chmod 700 /tmp/vesper-0
exec unshare -Urmpf --mount-proc --kill-child bash -c 'mount -t tmpfs tmpfs /tmp/.X11-unix && mount -t tmpfs -o mode=0700 tmpfs /tmp/vesper-0 && dbus-run-session -- bash "$HERE/session.sh" "$@"' x "$@"
