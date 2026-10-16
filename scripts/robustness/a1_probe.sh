# A1: audio_probe with a live tone; RUST_LOG shows the audio warnings the app swallows
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
. $LABROOT/audio_up.sh
audio_up
RUST_LOG=${RUST_LOG:-warn} timeout 60 $R/target/debug/examples/audio_probe ${SECS:-12} 2>&1 | cut -c1-220 | tail -${TAILN:-25}
