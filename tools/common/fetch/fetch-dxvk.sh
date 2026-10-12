#!/usr/bin/env bash
# Fetch DXVK and build the Direct3D 11 program `cargo xtask test-dxvk` runs
# with it in Wine, on Mesa's lavapipe (stage 23, E2.4; docs/WINE.md §6).
#
# DXVK is its release tarball, pinned by SHA-256 (GitHub's digest for the
# asset, 2026-10-12); only its 64-bit d3d11.dll and dxgi.dll are kept. The
# program is tools/common/test/d3d11-triangle.c, built here with mingw-w64's
# gcc against the import libraries mingw-w64 ships. Lavapipe and the Vulkan
# loader are not fetched here: they are on the Wine volume already, from
# yserver's tree (Debian's mesa-vulkan-drivers and libvulkan1, pinned in
# fetch-yserver.sh), and this script stops if they are not.
#
# The three files go into the image `test-dxvk` builds, not onto a volume;
# the Wine volume (`fetch-wine.sh`) is attached as it is.
#
# Writes $FERRIX_DXVK_DIR (default ~/.local/share/ferrix/dxvk): d3d11.dll,
# dxgi.dll, d3d11-triangle.exe, the d3d11-triangle.c it was built from
# (xtask refuses a program built from another), and dxvk.version, the
# release the DLLs are from.
# Needs curl, sha256sum, tar and x86_64-w64-mingw32-gcc; no root.
#
# Usage: tools/common/fetch/fetch-dxvk.sh

set -euo pipefail

DXVK_VERSION=3.1.1
DXVK_SHA256=40565b4a724aadc4433fa4e010b4b23916d9b1f1baeee64e17186db94f54e608
url=https://github.com/doitsujin/dxvk/releases/download/v$DXVK_VERSION/dxvk-$DXVK_VERSION.tar.gz

out=${FERRIX_DXVK_DIR:-$HOME/.local/share/ferrix/dxvk}
wine=${FERRIX_WINE_VOLUME:-$HOME/.local/share/ferrix/wine}
source="$(cd "$(dirname "$0")/../test" && pwd)/d3d11-triangle.c"

for tool in curl sha256sum tar x86_64-w64-mingw32-gcc; do
    command -v "$tool" > /dev/null || { echo "fetch-dxvk: $tool is not installed" >&2; exit 1; }
done

# What DXVK draws on, from the Wine volume's tree.
for file in usr/share/vulkan/icd.d/lvp_icd.json usr/lib/x86_64-linux-gnu/libvulkan_lvp.so \
    usr/lib/x86_64-linux-gnu/libvulkan.so.1 \
    usr/lib/x86_64-linux-gnu/wine/x86_64-unix/winevulkan.so; do
    [ -e "$wine/tree/$file" ] || {
        echo "fetch-dxvk: $wine/tree/$file is not there: tools/common/fetch/fetch-wine.sh makes it" >&2
        exit 1
    }
done

mkdir -p "$out/pool"
tarball="$out/pool/dxvk-$DXVK_VERSION.tar.gz"
if [ ! -f "$tarball" ]; then
    curl -fsSL -o "$tarball.part" "$url"
    mv "$tarball.part" "$tarball"
fi
echo "$DXVK_SHA256  $tarball" | sha256sum -c --quiet \
    || { echo "fetch-dxvk: $tarball does not match its pinned checksum" >&2; rm -f "$tarball"; exit 1; }

unpacked="$out/pool/dxvk-$DXVK_VERSION"
rm -rf "$unpacked"
tar -xzf "$tarball" -C "$out/pool"
for dll in d3d11.dll dxgi.dll; do
    [ -f "$unpacked/x64/$dll" ] || { echo "fetch-dxvk: no x64/$dll in the release" >&2; exit 1; }
    cp "$unpacked/x64/$dll" "$out/$dll"
done
rm -rf "$unpacked"

x86_64-w64-mingw32-gcc -O2 -o "$out/d3d11-triangle.exe" "$source" \
    -ld3d11 -ldxgi -ld3dcompiler_47 -luuid
cp "$source" "$out/d3d11-triangle.c"
echo "$DXVK_VERSION" > "$out/dxvk.version"
echo "dxvk: $out (DXVK $DXVK_VERSION, d3d11-triangle.exe from ${source##*/})"
