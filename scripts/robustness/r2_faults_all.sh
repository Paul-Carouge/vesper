#!/bin/bash
# Runs the r2_fault.sh matrix, one lab per fault; output in $OUT (default: a temp dir).
# usage: r2_faults_all.sh [fault ...]
cd "$(dirname "$(readlink -f "$0")")" || exit 1
OUT=${OUT:-$(mktemp -d)}
mkdir -p "$OUT"
echo "output in $OUT"
FAULTS=${*:-serverdead tls-untrusted tls-plain tls-die pwrestart pwfreeze diskfull gamekill term vstkill vststorm vstfreeze}
for f in $FAULTS; do
  extra=()
  case $f in
    serverdead) extra=(SECS=90 DEAD=50 LOGLINES=60) ;;
    tls-die) extra=(SECS=45) ;;
    pwrestart|pwfreeze) extra=(SECS=40) ;;
    vststorm) extra=(SECS=60) ;;
    vstfreeze) extra=(SECS=40) ;;
    diskfull) extra=(SECS=40) ;;
    *) extra=(SECS=40) ;;
  esac
  TMO=${TMO:-200} ./r2 "f-$f" r2_fault.sh FAULT=$f VST=${VST:-${VESPER_LAB_DATA:-/tmp/vesper-lab-$(id -u)}/gain.vst3} "${extra[@]}" 2>&1 | grep -v fusermount > "$OUT/$f.txt"
  echo "== $f done"
done
