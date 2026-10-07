#!/usr/bin/env bash
# Build QEMU for Windows with the fixes in tools/common/data/qemu/, and install it
# where xtask looks before PATH (tools/common/xtask/src/paths.rs, `own_qemu`).
#
# The released Windows build (Stefan Weil's 11.1.0, from the winget package)
# jumps to NULL the first time a guest resets virtio-gpu-gl-pci -- OVMF does
# at ExitBootServices -- so every `--gl` boot on Windows, `run-compositor
# --everything` among them, dies as the loader hands over (docs/GPU.md
# §3.12). The fix is QEMU's own reset doing its display work in the main
# thread; until a release has it, this is the QEMU a Windows host boots with.
#
# It is QEMU v11.1.0 with the patches applied, built the way Weil's build is:
# MSYS2's MINGW64 toolchain and libraries, x86-64 only, with GTK, SDL, epoxy,
# virglrenderer and WHPX. spice-protocol, which qemu-vdagent (the clipboard)
# needs, has no MSYS2 package and is built from its release tag. The DLLs the
# binary loads, and the ANGLE libraries epoxy opens at run time, are copied
# beside it, so the directory runs on its own.
#
# Writes $FERRIX_QEMU_DIR (default ~/.local/share/ferrix/qemu in the Windows
# home); the source and the build are in $FERRIX_QEMU_WORK (default the same
# path with `-build`), about 3 GiB. Needs MSYS2 at $MSYS2_ROOT (default C:/msys64); installs the
# packages it needs there with pacman. Run from Git Bash or MSYS2.
#
# Usage: tools/common/fetch/fetch-qemu-windows.sh

set -euo pipefail

version=v11.1.0
spice_protocol=v0.14.5
here=$(cd "$(dirname "$0")/../../.." && pwd)

# Everything below runs in MSYS2's MINGW64 shell, whose compiler and
# libraries are the ones the binary is linked against. The directories are
# settled first, from the Windows home xtask looks in (MSYS2's own $HOME is
# another), and passed to that shell as arguments: its login does not keep
# this shell's environment.
if [ "${1:-}" != --in-msys2 ]; then
    msys=${MSYS2_ROOT:-C:/msys64}
    if [ ! -x "$msys/usr/bin/bash.exe" ]; then
        echo "no MSYS2 at $msys: install it (https://www.msys2.org) or set MSYS2_ROOT" >&2
        exit 1
    fi
    home=${USERPROFILE:-$HOME}
    out=$(cygpath -m "${FERRIX_QEMU_DIR:-$home/.local/share/ferrix/qemu}")
    work=$(cygpath -m "${FERRIX_QEMU_WORK:-$out-build}")
    script=$(cygpath -m "$here/tools/common/fetch/$(basename "$0")")
    MSYSTEM=MINGW64 CHERE_INVOKING=1 exec "$msys/usr/bin/bash.exe" -lc \
        'exec bash "$0" --in-msys2 "$1" "$2" "$3"' "$script" "$out" "$work" \
        "${FERRIX_QEMU_JOBS:-}"
fi

out=$(cygpath -u "$2")
work=$(cygpath -u "$3")
FERRIX_QEMU_JOBS=${4:-}
mkdir -p "$out" "$work"

pacman -S --needed --noconfirm --disable-download-timeout \
    git diffutils make bison flex \
    mingw-w64-x86_64-gcc mingw-w64-x86_64-meson mingw-w64-x86_64-ninja \
    mingw-w64-x86_64-pkgconf mingw-w64-x86_64-python mingw-w64-x86_64-python-distlib \
    mingw-w64-x86_64-glib2 mingw-w64-x86_64-pixman mingw-w64-x86_64-SDL2 \
    mingw-w64-x86_64-gtk3 mingw-w64-x86_64-libepoxy mingw-w64-x86_64-virglrenderer \
    mingw-w64-x86_64-angleproject mingw-w64-x86_64-dtc mingw-w64-x86_64-capstone

if ! pkg-config --exists spice-protocol; then
    rm -rf "$work/spice-protocol"
    git clone -q --depth 1 --branch "$spice_protocol" \
        https://gitlab.freedesktop.org/spice/spice-protocol.git "$work/spice-protocol"
    meson setup "$work/spice-protocol/build" "$work/spice-protocol" --prefix=/mingw64 >/dev/null
    ninja -C "$work/spice-protocol/build" install >/dev/null
fi

src=$work/qemu
if [ ! -d "$src" ]; then
    git clone -q --depth 1 --branch "$version" https://gitlab.com/qemu-project/qemu.git "$src"
fi
# The virtio-gpu-gl fix, and 0002, VT-d's compatibility-format block,
# which stage 10's self-check requires of every x86-64 boot (FX-1002,
# docs/NVIDIA.md section 12.3): without it the first boot of every test
# panics. 0002 was written against 10.2.1 (series-10.2.1); its qtest hunk
# does not apply to 11.1.0 and the tests are not built here, so that file
# is left out.
for patch in "$here"/tools/common/data/qemu/0001-*.patch \
    "$here"/tools/common/data/qemu/0002-*.patch; do
    if git -C "$src" apply --reverse --check \
        --exclude=tests/qtest/intel-iommu-test.c "$patch" 2>/dev/null; then
        echo "qemu: $(basename "$patch") already applied"
    else
        git -C "$src" apply --exclude=tests/qtest/intel-iommu-test.c "$patch"
        echo "qemu: applied $(basename "$patch")"
    fi
done

rm -rf "$src/build"
(
    cd "$src"
    ./configure --target-list=x86_64-softmmu --enable-gtk --enable-sdl \
        --enable-opengl --enable-virglrenderer --enable-whpx --disable-docs \
        --disable-werror --enable-install-blobs --prefix="$out" >/dev/null
)
# FERRIX_QEMU_JOBS caps the build, for a machine someone is using
# (ninja's default is every processor).
ninja -C "$src/build" ${FERRIX_QEMU_JOBS:+-j "$FERRIX_QEMU_JOBS"} >/dev/null
ninja -C "$src/build" ${FERRIX_QEMU_JOBS:+-j "$FERRIX_QEMU_JOBS"} install >/dev/null

# The DLLs it links against, and theirs in turn, until nothing new appears;
# epoxy opens ANGLE's EGL and GLES by name, so those two go in by hand.
cp -u /mingw64/bin/libEGL.dll /mingw64/bin/libGLESv2.dll "$out/"
while :; do
    before=$(ls "$out"/*.dll | wc -l)
    for binary in "$out"/qemu-system-x86_64.exe "$out"/*.dll; do
        ldd "$binary" | awk '$3 ~ /^\/mingw64\// { print $3 }'
    done | sort -u | while read -r dll; do cp -u "$dll" "$out/"; done
    [ "$(ls "$out"/*.dll | wc -l)" = "$before" ] && break
done

"$out/qemu-system-x86_64.exe" --version | head -1
echo "qemu: $(cygpath -w "$out")"
