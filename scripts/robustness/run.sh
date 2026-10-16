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
# Everything runs in a private user+mount+network+pid namespace (uid 0 inside,
# so the capture socket is /tmp/vesper-0, never the real /tmp/vesper-<uid>;
# no network but loopback, so the session's abstract X11 and D-Bus sockets are
# out of reach) on a private /tmp (X locks, sockets), with its
# own D-Bus session, PipeWire and WirePlumber (ALSA/V4L2/BlueZ monitors
# disabled: no real device is ever opened), its own XDG directories under
# $VESPER_LAB_DATA (default /tmp/vesper-lab-<uid>) and a headless gamescope as
# compositor. Games are vkcube with SteamAppId=1 and the layer from
# target/debug via VK_ADD_IMPLICIT_LAYER_PATH. Everything inside dies when the
# scenario ends. Scenarios: s* shutdown/storage, a* audio, e* encoder, f* hostile
# clients, l* layer and game churn, p* prefs/instances.
HERE=$(dirname "$(readlink -f "$0")")
DATA=${VESPER_LAB_DATA:-/tmp/vesper-lab-$(id -u)}
mkdir -p "$DATA"
export HERE DATA
# Inside: the real /tmp stays reachable on /mnt only to bind back the lab data
# and the checkout when they live under /tmp; everything else gets a fresh /tmp.
exec unshare -Urmnpf --mount-proc --kill-child bash -c '
set -e
ip link set lo up
mount --bind /tmp /mnt
mount -t tmpfs tmpfs /tmp
mkdir -m 1777 /tmp/.X11-unix
mkdir -m 0700 /tmp/vesper-0
for p in "$DATA" "$(readlink -f "$HERE/../..")"; do
  case $p in /tmp/*) mkdir -p "$p"; mount --bind "/mnt${p#/tmp}" "$p" ;; esac
done
# The real session'"'"'s sockets (Wayland, PipeWire, D-Bus, kwallet) are out of
# reach, and the bus gets the lab'"'"'s HOME, XDG directories and no display
# before it starts: services it activates (portal, kwalletd, permission
# store) would otherwise use the real compositor, wallet and files.
mount -t tmpfs tmpfs /run/user
. "$HERE/env.sh"
set +e
dbus-run-session -- bash "$HERE/session.sh" "$@"' x "$@"
