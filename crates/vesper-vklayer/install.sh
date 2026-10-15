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

# The path goes into JSON unescaped: accept only plain absolute paths.
case "$lib" in
/*) ;;
*) echo "install path must be absolute: $lib" >&2; exit 1 ;;
esac
case "$lib" in
*'"'* | *'\'*) echo "install path must not contain quotes or backslashes: $lib" >&2; exit 1 ;;
esac

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$manifest" "$manifest.tmp" "$lib" "$lib.tmp"
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
# Written aside, then renamed: the loader never sees a partial manifest.
cat >"$manifest.tmp" <<EOF
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
mv -f "$manifest.tmp" "$manifest"
echo "installed $manifest -> $lib"
