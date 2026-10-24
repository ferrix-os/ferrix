#!/usr/bin/env bash
# Build the x86-64 QEMU every Ferrix x86-64 boot runs, with the patches in
# tools/common/data/qemu/series-10.2.1, and install it where xtask looks
# before PATH (tools/common/xtask/src/paths.rs, `own_qemu`).
#
# Why: interrupt remapping on VT-d (docs/NVIDIA.md §12.3, N0g). Released
# QEMU ignores GCMD.CFI -- GSTS.CFIS always reads 0 -- and passes every
# compatibility-format interrupt through, so a kernel that turns them off
# and reads CFIS back is told they are blocked while they are still
# delivered. The patch (0002) implements CFI/CFIS and blocks those messages
# with fault reason 0x25. Its own qtest runs here after every build. xtask
# refuses an x86-64 boot on a QEMU whose `--version` lacks `(ferrix-cfi)`.
# 0007 makes amd-iommu's event log readable as the specification lays it
# out -- the address, the whole requester ID, RW, and refused interrupts
# logged -- for the AMD-Vi row (docs/AMD-VI.md section 1.6).
#
# It is QEMU v10.2.1, the version Ubuntu's package has, from the release
# tarball pinned by sha256, configured for x86-64 only, with the display,
# sound and network features the gates use from Ubuntu's build: GTK,
# OpenGL, virglrenderer, VNC, spice-protocol (for qemu-vdagent), PulseAudio,
# PipeWire, ALSA, slirp and the TCG plugins. libslirp is built from QEMU's
# own wrap (pinned to a commit) when the host has no libslirp-dev. AArch64
# and ARMv7-A keep the QEMU on PATH.
#
# Writes $FERRIX_QEMU_DIR (default ~/.local/share/ferrix/qemu) -- bin/ and
# share/ -- replacing what was there only once the new build has passed;
# the tarball, the source and the build are in $FERRIX_QEMU_WORK (default
# the same path with `-build`), about 1.5 GiB. Needs a C toolchain, ninja,
# python3-venv, flex, bison, git and the -dev packages below. Never uses
# sudo: the line it prints for the `ferrix-3060` libvirt domain is for the
# user to run.
#
# Debian/Ubuntu build dependencies:
#   sudo apt-get install build-essential ninja-build python3-venv flex bison \
#     git pkg-config libglib2.0-dev libpixman-1-dev libgtk-3-dev libepoxy-dev \
#     libgbm-dev libvirglrenderer-dev libspice-protocol-dev libpulse-dev \
#     libpipewire-0.3-dev libasound2-dev libzstd-dev
#
# Usage: [FERRIX_QEMU_JOBS=n] tools/common/fetch/fetch-qemu-linux.sh

set -euo pipefail

version=10.2.1
sha256=a3717477d8e2c84d630bfffbc20f6cd3293eb45aa1e6dac6d0cc27689991c9e1
url=https://download.qemu.org/qemu-$version.tar.xz
pkgversion=ferrix-cfi
# Where the `ferrix-3060` domain's <emulator> points: libvirt's QEMU runs
# under an AppArmor profile that does not let it execute from a home
# directory. Never /usr/local/bin, which holds a root-installed 9.2.4.
system_dir=/usr/local/lib/ferrix/qemu

here=$(cd "$(dirname "$0")/../../.." && pwd)
data=$here/tools/common/data/qemu
out=${FERRIX_QEMU_DIR:-$HOME/.local/share/ferrix/qemu}
work=${FERRIX_QEMU_WORK:-$out-build}
mkdir -p "$work"

tarball=$work/qemu-$version.tar.xz
if [ ! -f "$tarball" ] || ! echo "$sha256  $tarball" | sha256sum -c --status; then
    echo "qemu: fetching $url"
    curl -fL --retry 3 -o "$tarball.part" "$url"
    mv "$tarball.part" "$tarball"
fi
if ! echo "$sha256  $tarball" | sha256sum -c --status; then
    echo "qemu: $tarball does not have the pinned sha256 $sha256" >&2
    exit 1
fi

# A fresh tree every time, so the patches apply to exactly the release.
src=$work/qemu-$version
rm -rf "$src"
tar -xf "$tarball" -C "$work"
grep -v '^\s*\(#\|$\)' "$data/series-$version" | while read -r patch; do
    patch -d "$src" -p1 --fuzz=0 --quiet --no-backup-if-mismatch < "$data/$patch"
    echo "qemu: applied $patch"
done

jobs=${FERRIX_QEMU_JOBS:-$(nproc)}
(
    cd "$src"
    ./configure --prefix="$out" --target-list=x86_64-softmmu \
        --with-pkgversion="$pkgversion" \
        --enable-kvm --enable-tcg --enable-plugins \
        --enable-gtk --enable-opengl --enable-virglrenderer --enable-vnc \
        --enable-spice-protocol --enable-slirp \
        --enable-pa --enable-pipewire --enable-alsa \
        --disable-docs --disable-user --disable-werror >"$work/configure.log" 2>&1 ||
        { tail -n 30 "$work/configure.log" >&2; exit 1; }
)
ninja -C "$src/build" -j "$jobs" qemu-system-x86_64 >"$work/build.log" 2>&1 ||
    { tail -n 30 "$work/build.log" >&2; exit 1; }

# The patch's own test: CFIS follows CFI, and with remapping on a
# compatibility-format I/O APIC interrupt faults 0x25 until CFI is set.
ninja -C "$src/build" -j "$jobs" tests/qtest/intel-iommu-test >>"$work/build.log" 2>&1 ||
    { tail -n 30 "$work/build.log" >&2; exit 1; }
QTEST_QEMU_BINARY="$src/build/qemu-system-x86_64" \
    "$src/build/tests/qtest/intel-iommu-test" --tap -k
echo "qemu: intel-iommu qtest passed"

# Install beside the old build and swap, so a QEMU another session is
# running from $out is never half overwritten.
stage=$work/stage
rm -rf "$stage"
DESTDIR="$stage" ninja -C "$src/build" install >>"$work/build.log" 2>&1 ||
    { tail -n 30 "$work/build.log" >&2; exit 1; }
"$stage$out/bin/qemu-system-x86_64" --version | grep -q "($pkgversion)" || {
    echo "qemu: the build's --version does not say ($pkgversion)" >&2
    exit 1
}
rm -rf "$out.old"
if [ -e "$out" ]; then mv "$out" "$out.old"; fi
mkdir -p "$(dirname "$out")"
mv "$stage$out" "$out"
rm -rf "$out.old" "$stage"

"$out/bin/qemu-system-x86_64" --version | head -1
echo "qemu: $out/bin/qemu-system-x86_64"
echo
echo "For the ferrix-3060 libvirt domain, install the same build as root (this script never does):"
echo "  sudo sh -c 'rm -rf $system_dir && install -d -m 755 $system_dir && cp -r --no-preserve=ownership $out/. $system_dir/'"
