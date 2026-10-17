#!/usr/bin/env bash
# Runs the keyring tests (`#[ignore]`d: they need a Secret Service) against
# scripts/fake_secret_service.py, one private session bus per fake mode, with
# a throwaway HOME, XDG directories and no display (see the fake's docstring):
# never the user's keyring. Needs dbus, python-dbus and python-gobject.
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
cargo test -p vesper-app --bin vesper --no-run

# mode "VAR=1 ..." test...: the fake started with VARs, then exactly these tests.
mode() {
    local vars=$1
    shift
    t=$(mktemp -d)
    trap 'rm -rf "$t"' EXIT
    echo "== fake Secret Service ${vars:-(default)}: $*"
    # shellcheck disable=SC2086 # $vars: zero or more VAR=value words.
    if ! env -u WAYLAND_DISPLAY -u DISPLAY -u XAUTHORITY -u XDG_STATE_HOME -u XDG_CONFIG_DIRS \
        -u DBUS_SESSION_BUS_ADDRESS \
        HOME="$t" XDG_RUNTIME_DIR="$t" XDG_CONFIG_HOME="$t/config" XDG_DATA_HOME="$t/data" \
        $vars \
        dbus-run-session -- bash -c '
            python3 -I scripts/fake_secret_service.py &
            fake=$!
            # Wait for the name: a call before it would start a real keyring.
            owned() {
                dbus-send --session --print-reply --dest=org.freedesktop.DBus /org/freedesktop/DBus \
                    org.freedesktop.DBus.NameHasOwner string:org.freedesktop.secrets | grep -q true
            }
            for _ in $(seq 100); do
                owned && break
                sleep 0.1
            done
            owned || { echo "the fake Secret Service did not start" >&2; exit 1; }
            cargo test -p vesper-app --bin vesper -- --ignored --exact --test-threads=1 "$@" | tee "$HOME/out"
            status=${PIPESTATUS[0]}
            kill "$fake"
            # A renamed test would match nothing and pass: check they all ran.
            grep -q "test result: ok. $# passed" "$HOME/out" || { echo "expected $# tests to pass" >&2; exit 1; }
            exit "$status"
        ' bash "$@"; then
        rm -rf "$t"
        return 1
    fi
    rm -rf "$t"
}

mode "" \
    secret::tests::keyring_round_trip \
    secret::tests::keyring_old_key_goes_to_its_service_and_never_overwrites \
    live::tests::keyring_pasted_key_reaches_go_live_and_survives_quit
mode "NO_REPLACE=1" secret::tests::keyring_store_keeps_only_the_new_key
mode "FAIL_GET=1" secret::tests::keyring_saved_but_not_checked_says_so
mode "DISMISS=1 SEED_OLD=old_key" secret::tests::keyring_locked_keeps_the_old_key
mode "HANG=1" secret::tests::a_hung_keyring_gives_up_quickly
mode "DISMISS=1 EXPECT=refused" secret::tests::a_refusing_keyring_is_not_an_absent_one
echo "all keyring tests passed"
