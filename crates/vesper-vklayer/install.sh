#!/bin/sh
# Installs Vesper's game capture Vulkan layer for the current user, or removes
# it with --uninstall. Once installed, the layer loads into every Vulkan app
# (it stays dormant until Vesper records). VESPER_CAPTURE_DISABLE=1 turns it
# off for one launch.
#
#   crates/vesper-vklayer/install.sh [--uninstall | path/to/libvesper_vklayer.so [path/to/32-bit/libvesper_vklayer.so]]
#
# Without a path, builds the layer in release mode first, and its 32-bit
# build too when the i686 Rust target is installed
# (`rustup target add i686-unknown-linux-gnu`, plus lib32-glibc and
# lib32-gcc-libs on Arch) for 32-bit games.
set -eu

data="${XDG_DATA_HOME:-$HOME/.local/share}"
manifest_dir="$data/vulkan/implicit_layer.d"
lib_dir="$data/vesper"
lib="$lib_dir/libvesper_vklayer.so"
lib32="$lib_dir/libvesper_vklayer32.so"

# The path goes into JSON unescaped: accept only plain absolute paths.
case "$lib" in
/*) ;;
*) echo "install path must be absolute: $lib" >&2; exit 1 ;;
esac
case "$lib" in
*'"'* | *'\'*) echo "install path must not contain quotes or backslashes: $lib" >&2; exit 1 ;;
esac

if [ "${1:-}" = "--uninstall" ]; then
    for m in vesper_capture_64 vesper_capture_32 vesper_capture; do
        rm -f "$manifest_dir/$m.json" "$manifest_dir/$m.json.tmp"
    done
    rm -f "$lib" "$lib.tmp" "$lib32" "$lib32.tmp"
    rmdir "$lib_dir" 2>/dev/null || true
    echo "removed the layer from $manifest_dir"
    exit 0
fi

src="${1:-}"
src32="${2:-}"
if [ -z "$src" ]; then
    root=$(cd "$(dirname "$0")/../.." && pwd)
    target="${CARGO_TARGET_DIR:-$root/target}"
    cargo build --release -p vesper-vklayer --manifest-path "$root/Cargo.toml"
    src="$target/release/libvesper_vklayer.so"
    if rustup target list --installed 2>/dev/null | grep -qx i686-unknown-linux-gnu; then
        # Without lib32-glibc / lib32-gcc-libs the link fails: 64-bit only.
        if cargo build --release -p vesper-vklayer --target i686-unknown-linux-gnu --manifest-path "$root/Cargo.toml"; then
            src32="$target/i686-unknown-linux-gnu/release/libvesper_vklayer.so"
        else
            echo "32-bit build failed (lib32-glibc, lib32-gcc-libs?): no 32-bit layer" >&2
        fi
    else
        echo "i686 Rust target not installed: no 32-bit layer" >&2
    fi
fi

# install_one <source> <dest> <manifest name> <layer name> <arch>
install_one() {
    mkdir -p "$lib_dir" "$manifest_dir"
    # Copy, then rename: running games keep the old file mapped.
    cp "$1" "$2.tmp"
    mv -f "$2.tmp" "$2"
    # Written aside, then renamed: the loader never sees a partial manifest.
    cat >"$manifest_dir/$3.json.tmp" <<EOF
{
    "file_format_version": "1.2.1",
    "layer": {
        "name": "$4",
        "type": "GLOBAL",
        "library_path": "$2",
        "library_arch": "$5",
        "api_version": "1.4.0",
        "implementation_version": "1",
        "description": "Vesper game capture (dormant until Vesper records)",
        "disable_environment": {
            "VESPER_CAPTURE_DISABLE": "1"
        }
    }
}
EOF
    mv -f "$manifest_dir/$3.json.tmp" "$manifest_dir/$3.json"
    echo "installed $manifest_dir/$3.json -> $2"
}

install_one "$src" "$lib" vesper_capture_64 VK_LAYER_VESPER_capture 64
# Manifest of older versions: it would load the layer twice.
rm -f "$manifest_dir/vesper_capture.json"
if [ -n "$src32" ]; then
    install_one "$src32" "$lib32" vesper_capture_32 VK_LAYER_VESPER_capture_32 32
else
    rm -f "$manifest_dir/vesper_capture_32.json" "$lib32"
fi
