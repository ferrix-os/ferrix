#!/usr/bin/env bash
# Build the decoder `run-compositor --nvidia`'s moving wallpaper plays
# through: a minimal FFmpeg whose one AV1 decoder is the GPU's, over Vulkan
# Video (VK_KHR_video_decode_av1, which NVIDIA's driver has on Ampere).
#
# `pattern --video --decoder <it>` starts it on the wallpaper's IVF file and
# reads raw `bgr0` frames from its standard output (docs/NVIDIA.md §4.6),
# so the rav1d decode on the guest's processors goes away.
#
# What is built is FFmpeg 7.1.2, pinned by its SHA-256, configured with
# --disable-everything and then only: the native `av1` decoder with the
# `av1_vulkan` hwaccel (and no software AV1 decoder, so a GPU that cannot
# decode is an error, not a quiet fallback to the processor), the `ivf`
# demuxer, the `rawvideo` muxer and encoder, `file` and `pipe`, and the
# filters a download and a pixel-format change need. No assembly (no nasm
# here), no network, no autodetected libraries: the result needs only
# glibc's libc and libm, and dlopens libvulkan.so.1, which NVIDIA's volume
# carries with NVIDIA's ICD. It is linked statically against its own
# libraries (LGPL 2.1 or later, as configured: no --enable-gpl); the pinned
# source tarball is what it is built from.
#
# Built with the host's compiler against its Vulkan headers, glibc-dynamic,
# and refused if it asks for a glibc newer than Debian 13's 2.41, the one
# the volume carries.
#
# Writes $FERRIX_NVIDIA/ffmpeg-vulkan/ (default
# ~/.local/share/ferrix/nvidia/ffmpeg-vulkan): the tarball, the source, and
# `ffmpeg`, which xtask's NVIDIA volume carries as /usr/bin/ffmpeg-vulkan.
#
# Usage: tools/common/fetch/fetch-ffmpeg-vulkan.sh

set -euo pipefail

VERSION=7.1.2
URL=https://ffmpeg.org/releases/ffmpeg-$VERSION.tar.xz
SHA256=089bc60fb59d6aecc5d994ff530fd0dcb3ee39aa55867849a2bbc4e555f9c304

out=${FERRIX_NVIDIA:-$HOME/.local/share/ferrix/nvidia}/ffmpeg-vulkan
mkdir -p "$out"
tarball=$out/ffmpeg-$VERSION.tar.xz
if [ ! -f "$tarball" ]; then
    curl -fsSL -o "$tarball.part" "$URL"
    mv "$tarball.part" "$tarball"
fi
echo "$SHA256  $tarball" | sha256sum -c --quiet \
    || { echo "fetch-ffmpeg-vulkan: $tarball does not match its pinned checksum" >&2; rm -f "$tarball"; exit 1; }

stamp="$SHA256 script=$(sha256sum < "$0" | cut -d' ' -f1)"
if [ -x "$out/ffmpeg" ] && [ "$(cat "$out/ffmpeg.version" 2> /dev/null)" = "$stamp" ]; then
    echo "fetch-ffmpeg-vulkan: $out/ffmpeg is up to date"
    exit 0
fi

src=$out/ffmpeg-$VERSION
rm -rf "$src"
tar -xJf "$tarball" -C "$out"
(
    cd "$src"
    # Two optional device extensions FFmpeg turns on when the driver names
    # them, and that nvrm cannot serve yet (seen on the RTX 3060, 2026-10-10):
    # with VK_NV_optical_flow vkCreateDevice fails (INITIALIZATION_FAILED),
    # and VK_EXT_external_memory_host needs os_lock_user_pages (N1e), without
    # which every downloaded frame fails with OUT_OF_DEVICE_MEMORY. Decoding
    # needs neither.
    for extension in VK_NV_OPTICAL_FLOW_EXTENSION_NAME VK_EXT_EXTERNAL_MEMORY_HOST_EXTENSION_NAME; do
        grep -q "{ $extension," libavutil/hwcontext_vulkan.c \
            || { echo "fetch-ffmpeg-vulkan: $extension is not in FFmpeg's optional list any more" >&2; exit 1; }
        sed -i "/{ $extension,/d" libavutil/hwcontext_vulkan.c
    done
    ./configure --disable-everything --disable-autodetect --disable-x86asm --disable-doc \
        --disable-network --disable-debug --enable-static --disable-shared \
        --disable-ffprobe --disable-ffplay \
        --enable-vulkan --enable-decoder=av1 --enable-hwaccel=av1_vulkan \
        --enable-demuxer=ivf --enable-parser=av1 \
        --enable-bsf=av1_frame_split,av1_frame_merge,extract_extradata \
        --enable-muxer=rawvideo --enable-encoder=rawvideo --enable-protocol=file,pipe \
        --enable-filter=hwdownload,format,scale,null,buffer,buffersink,copy \
        --enable-swscale > "$out/configure.log" 2>&1 \
        || { echo "fetch-ffmpeg-vulkan: configure failed; see $out/configure.log" >&2; exit 1; }
    jobs=$(nproc)
    [ "$jobs" -gt 8 ] && jobs=8
    make -j"$jobs" ffmpeg > "$out/make.log" 2>&1 \
        || { echo "fetch-ffmpeg-vulkan: make failed; see $out/make.log" >&2; exit 1; }
)
newest=$(objdump -T "$src/ffmpeg" | grep -o 'GLIBC_[0-9.]*' | sort -V | tail -n 1)
if [ "$(printf '%s\nGLIBC_2.41\n' "$newest" | sort -V | tail -n 1)" != GLIBC_2.41 ]; then
    echo "fetch-ffmpeg-vulkan: the build asks for $newest, newer than the volume's glibc 2.41" >&2
    exit 1
fi
cp "$src/ffmpeg" "$out/ffmpeg"
echo "$stamp" > "$out/ffmpeg.version"
echo "fetch-ffmpeg-vulkan: $out/ffmpeg (FFmpeg $VERSION, AV1 on Vulkan Video only, needs $newest)"
