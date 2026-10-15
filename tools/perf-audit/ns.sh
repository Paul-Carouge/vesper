#!/bin/sh
# Performance audit harness (measurement only, never part of a release).
#
# Runs Vesper, vkcube (+ MangoHud frame-time logs) and optionally OBS inside a
# nested headless KWin, with a private /tmp (so a running Vesper keeps
# /tmp/vesper-<uid>), a private D-Bus session without activatable services,
# HOME/XDG dirs redirected to $AUDIT_DIR/home, and the layer loaded only
# through VK_ADD_IMPLICIT_LAYER_PATH. Nothing is installed or written in the
# real home.
#
#   cargo build --release                       # layer
#   CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true CARGO_TARGET_DIR=target-audit \
#       cargo build --release -p vesper-app --features perf-audit
#   gcc -O2 -shared -fPIC -o tools/perf-audit/sysshim.so tools/perf-audit/sysshim.c -ldl
#   export AUDIT_DIR=/tmp/vesper-audit AUDIT_TOOLS=$PWD/tools/perf-audit AUDIT_REPO=$PWD
#   unshare -r -m sh $AUDIT_TOOLS/ns.sh $AUDIT_TOOLS/batch1.sh
#   python3 -I $AUDIT_TOOLS/analyze.py $AUDIT_DIR/runs/*
#
# The uid mapping assumes uid/gid 1000.
S=$AUDIT_DIR
mkdir -p "$S"
mount --bind "$S" /mnt || exit 1
mount -t tmpfs tmpfs /tmp || exit 1
mkdir -p "$S"
mount --bind /mnt "$S" || exit 1
exec unshare --user --map-user=1000 --map-group=1000 dbus-run-session --config-file=$AUDIT_TOOLS/bus.conf -- sh "$@"
