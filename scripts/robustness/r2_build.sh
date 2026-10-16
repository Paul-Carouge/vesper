#!/bin/bash
# The build the r2_* scenarios run: optimized, with the debug flags the lab needs
# (--record-game, --stream-game, VESPER_DEMO) via debug assertions. Output: target-da/release.
cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1
CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true CARGO_TARGET_DIR=$PWD/target-da exec cargo build --release --workspace
