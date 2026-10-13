#!/usr/bin/env bash
# Fetch the Unity game `cargo xtask test-unity` runs in Wine through DXVK on
# Mesa's lavapipe (stage 23, step 3; docs/WINE.md §9), and make the volume
# it runs from.
#
# The game is Unity Technologies' Spaceship Demo, release 2021.2.0: a
# first-person walk through a spaceship, built with Unity 2021.2.3f1, the
# High Definition Render Pipeline and the VFX Graph, 64-bit, Direct3D 11 by
# default on Windows, on Mono (not IL2CPP), with no anti-cheat. It is
# published by Unity as a release of an open repository and is downloaded
# without an account; the release's Windows build is pinned here by its
# SHA-256 (taken on 2026-10-12; GitHub gives no digest for the asset).
#
# The volume is a Wine tree with the game unpacked at `/unity` and DXVK's
# `d3d11.dll` and `dxgi.dll` beside its executable, from `fetch-dxvk.sh`'s
# directory, so that one disk at `/data` has the X server, Wine, Vulkan on
# lavapipe and the game. The Wine tree is made here, by `fetch-wine.sh`
# under this script's own directory, and not copied from the shared one:
# other sessions write the shared tree, and a copy taken while it was being
# remade on 2026-10-12 held another yserver than its stamp said. The
# packages already in the shared Wine pool are copied to this one first,
# each still checked against its pin, so nothing is downloaded twice.
# The game's Burst debug information (`*_BurstDebugInformation_DoNotShip`,
# 80 MB that the release says not to ship) is left out.
#
# Writes $FERRIX_UNITY_VOLUME/unity.img (default
# ~/.local/share/ferrix/unity/unity.img), about 3.3 GB, and beside it
# yserver.commit, dxvk.version and game.release, what it carries, which
# test-unity compares with the pins. Needs what fetch-wine.sh needs, and
# unzip; no root.
#
# Usage: tools/common/fetch/fetch-unity.sh

set -euo pipefail

GAME_RELEASE=2021.2.0
GAME_SHA256=13111680e87084c035be06d5311ff7c2fa7c543f85443930fffb61d8428c9fd4
url=https://github.com/Unity-Technologies/SpaceshipDemo/releases/download/$GAME_RELEASE/SpaceshipDemo-Release-$GAME_RELEASE-Win64.zip

out=${FERRIX_UNITY_VOLUME:-$HOME/.local/share/ferrix/unity}
shared=${FERRIX_WINE_VOLUME:-$HOME/.local/share/ferrix/wine}
dxvk=${FERRIX_DXVK_DIR:-$HOME/.local/share/ferrix/dxvk}

for tool in curl sha256sum unzip mkfs.btrfs; do
    command -v "$tool" > /dev/null || { echo "fetch-unity: $tool is not installed" >&2; exit 1; }
done

here=$(dirname "$0")
dxvk_pin=$(sed -n 's/^DXVK_VERSION=//p' "$here/fetch-dxvk.sh")
[ "$(cat "$dxvk/dxvk.version" 2> /dev/null)" = "$dxvk_pin" ] || {
    echo "fetch-unity: $dxvk holds no DXVK $dxvk_pin: tools/common/fetch/fetch-dxvk.sh fetches it" >&2
    exit 1
}

mkdir -p "$out/pool"
zip="$out/pool/SpaceshipDemo-Release-$GAME_RELEASE-Win64.zip"
if [ ! -f "$zip" ]; then
    curl -fsSL -o "$zip.part" "$url"
    mv "$zip.part" "$zip"
fi
echo "$GAME_SHA256  $zip" | sha256sum -c --quiet \
    || { echo "fetch-unity: $zip does not match its pinned checksum" >&2; rm -f "$zip"; exit 1; }

# This volume's own Wine tree, which the game is unpacked into.
# fetch-wine.sh's image is not used, so it is deleted to give the disk back.
wine="$out/wine"
mkdir -p "$wine/pool"
if [ -d "$shared/pool" ] && [ "$shared" != "$wine" ]; then
    cp -n "$shared/pool/"*.deb "$wine/pool/" 2> /dev/null || true
fi
FERRIX_WINE_VOLUME="$wine" "$here/fetch-wine.sh"
rm -f "$wine/wine.img"

tree="$wine/tree"
game="$tree/unity"
mkdir -p "$game"
unzip -q "$zip" -d "$game"
rm -rf "$game"/*_BurstDebugInformation_DoNotShip
[ -f "$game/Spaceship.exe" ] && [ -f "$game/UnityPlayer.dll" ] \
    || { echo "fetch-unity: no Spaceship.exe and UnityPlayer.dll in the release" >&2; exit 1; }
cp "$dxvk/d3d11.dll" "$dxvk/dxgi.dll" "$game/"

# Room for DXVK's shader cache, Unity's player log and its settings.
size=$(( $(du -sm "$tree" | cut -f1) + 512 ))
image="$out/unity.img"
rm -f "$out/yserver.commit" "$out/dxvk.version" "$out/game.release" "$image"
truncate -s "${size}M" "$image"
mkfs.btrfs -q --rootdir "$tree" "$image"
cp "$wine/yserver.commit" "$out/yserver.commit"
echo "$dxvk_pin" > "$out/dxvk.version"
echo "$GAME_RELEASE" > "$out/game.release"
echo "unity volume: $image (${size} MiB), Spaceship Demo $GAME_RELEASE, unpacked in $tree"
