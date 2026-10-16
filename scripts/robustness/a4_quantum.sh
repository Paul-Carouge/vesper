# A4: graph quantum changes while capturing (audio_probe with tracing on)
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
. $LABROOT/audio_up.sh
audio_up
(
  sleep 4;  echo "q=256";  pw-metadata -n settings 0 clock.force-quantum 256 > /dev/null
  sleep 4;  echo "q=4096"; pw-metadata -n settings 0 clock.force-quantum 4096 > /dev/null
  sleep 4;  echo "q=32";   pw-metadata -n settings 0 clock.force-quantum 32 > /dev/null
  sleep 4;  echo "q=1024 / rate 44100"; pw-metadata -n settings 0 clock.force-quantum 1024 > /dev/null; pw-metadata -n settings 0 clock.force-rate 44100 > /dev/null
  sleep 4;  echo "reset"; pw-metadata -n settings 0 clock.force-quantum 0 > /dev/null; pw-metadata -n settings 0 clock.force-rate 0 > /dev/null
) &
RUST_LOG=warn timeout 60 $R/target/debug/examples/audio_probe 24 2>&1 | grep -v "^peak" | grep -v -E "gap: 1[12][0-9]{3} frames" | sed 's/\x1b\[[0-9;]*m//g' | cut -c1-200 | tail -30
