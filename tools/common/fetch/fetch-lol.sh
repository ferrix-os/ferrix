#!/usr/bin/env bash
# Make the volume `cargo xtask test-lol` runs League of Legends' game
# executable from, in Wine through DXVK on Mesa's lavapipe (docs/WINE.md §13,
# milestone L1).
#
# Nothing is downloaded for the game: Riot does not publish it on its own,
# so it is copied from an installation the customer owns, given as
# LOL_GAME_DIR (the install's `Game` folder). Its executable is pinned by
# SHA-256; a patch that changes it makes this script refuse until the pin
# moves. The copy is read only: nothing is written to the installation.
# The volume holds Riot's files and stays on the machines that made it: it
# is never committed, uploaded or shared.
#
# All of the install's 30 GB but a few MB is DATA/FINAL. LOL_FILES, if set,
# names a file of paths relative to `Game/`, one a line, and only those are
# copied (milestone L0's list of what the executable opens); without it the
# whole folder is. LOL_ARGS, if set, is the command line the game is started
# with, written to the volume beside it.
#
# The volume is a Wine tree with the game at `/lol` and DXVK's `d3d11.dll`
# and `dxgi.dll` beside its executable, made the way fetch-unity.sh makes
# its own (a Wine tree of this volume's, from fetch-wine.sh, with the
# shared pool's packages copied in first).
#
# Writes $FERRIX_LOL_VOLUME/lol.img (default ~/.local/share/ferrix/lol/lol.img)
# and beside it yserver.commit, dxvk.version and game.sha256, which test-lol
# compares with the pins. Needs what fetch-wine.sh needs; no root.
#
# Usage: LOL_GAME_DIR=/mnt/c/Riot\ Games/League\ of\ Legends/Game \
#        [LOL_FILES=list] [LOL_ARGS='...'] tools/common/fetch/fetch-lol.sh

set -euo pipefail

# Patch 16.20.8248524 (content-metadata.json), taken on 2026-10-19.
GAME_VERSION=16.20.8248524
GAME_EXE_SHA256=f6fd49d6fb21c6cec23e329ca8a9a7c0809393cb950daf29ca62cf60a08455c6
EXE="League of Legends.exe"

out=${FERRIX_LOL_VOLUME:-$HOME/.local/share/ferrix/lol}
shared=${FERRIX_WINE_VOLUME:-$HOME/.local/share/ferrix/wine}
dxvk=${FERRIX_DXVK_DIR:-$HOME/.local/share/ferrix/dxvk}
source_dir=${LOL_GAME_DIR:?"fetch-lol: set LOL_GAME_DIR to the install's Game folder"}

for tool in sha256sum mkfs.btrfs; do
    command -v "$tool" > /dev/null || { echo "fetch-lol: $tool is not installed" >&2; exit 1; }
done

echo "$GAME_EXE_SHA256  $source_dir/$EXE" | sha256sum -c --quiet \
    || { echo "fetch-lol: $source_dir/$EXE is not patch $GAME_VERSION's executable (pinned by SHA-256)" >&2; exit 1; }

here=$(dirname "$0")
dxvk_pin=$(sed -n 's/^DXVK_VERSION=//p' "$here/fetch-dxvk.sh")
[ "$(cat "$dxvk/dxvk.version" 2> /dev/null)" = "$dxvk_pin" ] || {
    echo "fetch-lol: $dxvk holds no DXVK $dxvk_pin: tools/common/fetch/fetch-dxvk.sh fetches it" >&2
    exit 1
}

# This volume's own Wine tree, which the game is copied into.
wine="$out/wine"
mkdir -p "$wine/pool"
if [ -d "$shared/pool" ] && [ "$shared" != "$wine" ]; then
    cp -n "$shared/pool/"*.deb "$wine/pool/" 2> /dev/null || true
fi
FERRIX_WINE_VOLUME="$wine" "$here/fetch-wine.sh"
rm -f "$wine/wine.img"

tree="$wine/tree"
game="$tree/lol"
rm -rf "$game"
mkdir -p "$game"
if [ -n "${LOL_FILES:-}" ]; then
    # One path a line, spaces and all; read before the cd, so a relative
    # LOL_FILES still names the right file.
    tr -d '\r' < "$LOL_FILES" | sed '/^$/d' \
        | (cd "$source_dir" && xargs -d '\n' cp --parents -t "$game" --)
    # The executable is copied whatever the list says.
    cp "$source_dir/$EXE" "$game/"
else
    cp -r "$source_dir/." "$game/"
fi
echo "$GAME_EXE_SHA256  $game/$EXE" | sha256sum -c --quiet \
    || { echo "fetch-lol: the copied executable does not match its pin" >&2; exit 1; }
cp "$dxvk/d3d11.dll" "$dxvk/dxgi.dll" "$game/"
printf '%s\n' "${LOL_ARGS:-}" > "$game/ferrix-args.txt"

# Room for DXVK's shader cache and the game's logs.
size=$(( $(du -sm "$tree" | cut -f1) + 512 ))
image="$out/lol.img"
rm -f "$out/yserver.commit" "$out/dxvk.version" "$out/game.sha256" "$image"
truncate -s "${size}M" "$image"
mkfs.btrfs -q --rootdir "$tree" "$image"
cp "$wine/yserver.commit" "$out/yserver.commit"
echo "$dxvk_pin" > "$out/dxvk.version"
echo "$GAME_EXE_SHA256" > "$out/game.sha256"
echo "lol volume: $image (${size} MiB), League of Legends $GAME_VERSION, in $tree"
