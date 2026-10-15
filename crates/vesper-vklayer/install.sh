#!/bin/sh
# Installs Vesper's game capture Vulkan layer for the current user, or removes
# it with --uninstall. Once installed, the layer loads into every Vulkan app
# (it stays dormant until Vesper records). VESPER_CAPTURE_DISABLE=1 turns it
# off for one launch.
#
#   crates/vesper-vklayer/install.sh [--uninstall | path/to/libvesper_vklayer.so]
#
# Without a path, builds the layer in release mode first.
set -eu

data="${XDG_DATA_HOME:-$HOME/.local/share}"
manifest="$data/vulkan/implicit_layer.d/vesper_capture.json"
lib_dir="$data/vesper"
lib="$lib_dir/libvesper_vklayer.so"

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$manifest" "$lib"
    rmdir "$lib_dir" 2>/dev/null || true
    echo "removed $manifest"
    exit 0
fi

src="${1:-}"
if [ -z "$src" ]; then
    root=$(cd "$(dirname "$0")/../.." && pwd)
    cargo build --release -p vesper-vklayer --manifest-path "$root/Cargo.toml"
    src="${CARGO_TARGET_DIR:-$root/target}/release/libvesper_vklayer.so"
fi

mkdir -p "$lib_dir" "$(dirname "$manifest")"
# Copy, then rename: running games keep the old file mapped.
cp "$src" "$lib.tmp"
mv -f "$lib.tmp" "$lib"
cat >"$manifest" <<EOF
{
    "file_format_version": "1.2.0",
    "layer": {
        "name": "VK_LAYER_VESPER_capture",
        "type": "GLOBAL",
        "library_path": "$lib",
        "api_version": "1.4.0",
        "implementation_version": "1",
        "description": "Vesper game capture (dormant until Vesper records)",
        "disable_environment": {
            "VESPER_CAPTURE_DISABLE": "1"
        }
    }
}
EOF
echo "installed $manifest -> $lib"
