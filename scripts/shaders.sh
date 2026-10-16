#!/bin/sh
# Compiles the GLSL compute shaders to the SPIR-V committed next to them, so
# building Vesper (and CI) needs no shader compiler. Run after editing a
# .comp file and commit both. Needs glslc (shaderc) or glslangValidator
# (glslang); spirv-val is used when installed.
set -eu
cd "$(dirname "$0")/.."
for src in crates/*/shaders/*.comp; do
    out="${src%.comp}.spv"
    if command -v glslc >/dev/null; then
        glslc --target-env=vulkan1.1 -O -o "$out" "$src"
    elif command -v glslangValidator >/dev/null; then
        glslangValidator --target-env vulkan1.1 -o "$out" "$src"
    else
        echo "install glslc (shaderc) or glslangValidator (glslang)" >&2
        exit 1
    fi
    if command -v spirv-val >/dev/null; then
        spirv-val --target-env vulkan1.1 "$out"
    fi
    echo "$out"
done
