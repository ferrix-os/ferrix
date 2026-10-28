#!/bin/sh
# Regenerate src/spirv/*.spv from the compositor's GLSL.
#
# The shapes hyprix-gpu draws are the GPU painter's, cut by the same
# shaders: src/user/system/linux/compositor/virgl/shaders, written in GLSL
# ES for virgl. Vulkan takes SPIR-V, so each file is rewritten for Vulkan
# GLSL here -- the uniforms `to_clip` and `u[]` become one push-constant
# block, the texture a combined image sampler at set 0, binding 0, and
# `#include "shape.glsl"` the file itself -- and compiled by
# glslangValidator (Debian: glslang-tools), then checked by spirv-val
# (spirv-tools). Run it after changing anything under those shaders, and
# commit what it wrote: the build needs neither tool.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
shaders=$here/../../compositor/virgl/shaders
out=$here/../src/spirv
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

block='layout(push_constant) uniform Constants { vec4 to_clip; vec4 u[4]; } pc;'
for file in "$shaders"/*.vert "$shaders"/*.frag; do
    name=$(basename "$file")
    awk -v block="$block" -v dir="$shaders" '
        /^#version/ { print "#version 450"; print block; next }
        /^precision / { next }
        /^uniform vec4 to_clip;/ { print "#define to_clip pc.to_clip"; next }
        /^uniform vec4 u\[[0-9]+\];/ { print "#define u pc.u"; next }
        /^uniform sampler2D tex;/ { print "layout(set = 0, binding = 0) uniform sampler2D tex;"; next }
        /^#include "/ {
            split($0, parts, "\"")
            while ((getline line < (dir "/" parts[2])) > 0) print line
            close(dir "/" parts[2])
            next
        }
        { print }
    ' "$file" >"$work/$name"
    glslangValidator -V --target-env vulkan1.3 -o "$out/$name.spv" "$work/$name" >/dev/null
    spirv-val --target-env vulkan1.3 "$out/$name.spv"
done
