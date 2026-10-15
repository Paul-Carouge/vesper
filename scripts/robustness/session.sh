#!/bin/bash
# usage: session.sh <scenario script>; runs it inside a private dbus + pipewire.
. "$(dirname "$(readlink -f "$0")")/env.sh"
pipewire > $L/log/pipewire.log 2>&1 &
PW=$!
sleep 1
wireplumber > $L/log/wp.log 2>&1 &
WP=$!
sleep 1
export PW WP
bash "$@"
rc=$?
kill $WP $PW 2>/dev/null
wait 2>/dev/null
exit $rc
