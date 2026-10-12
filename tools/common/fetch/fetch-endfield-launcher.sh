#!/usr/bin/env bash
# Fetch Arknights: Endfield's Windows launcher (GRYPHLINK) from Gryphline and
# put it, installed, on a btrfs volume beside Debian's Wine: the disk
# `cargo xtask test-endfield-launcher` starts it from (docs/WINE.md §7,
# stage 23's step 4).
#
# The installer is the one endfield.gryphline.com links as its PC download
# (`launcher.gryphline.com/launcher/get_latest_launcher?appcode=...`, which
# redirects to Hypergryph's CDN, launcher.hg-cdn.com). It is a 32-bit NSIS
# installer; what it installs is 64-bit (a Qt 5.15 program with
# QtWebEngine, Chromium 87). The 32-bit installer cannot run in Debian's
# Wine on Ferrix until 32-bit Windows programs do (stage 23's E1.6), so it
# is not run: 7-Zip unpacks its payload on the host, and the files are laid
# out as the installer lays them out, `GRYPHLINK/Launcher.exe` beside the
# versioned directory `GRYPHLINK/1.6.0/` it starts `Games.exe` from.
#
# Nothing is logged into and nothing of the game's data is downloaded.
#
# The installer and 7-Zip's Linux build (from 7-zip.org) are pinned by
# their SHA-256, taken when they were first downloaded on 2026-10-12. The
# installer's URL names its version; Gryphline replaces the file when the
# launcher moves on, and then the pin below is moved with it.
#
# Writes $FERRIX_ENDFIELD_VOLUME/endfield.img (default
# ~/.local/share/ferrix/endfield-launcher/endfield.img): the Wine volume's
# tree with `/GRYPHLINK` added, and room for what the launcher writes. The
# tree is this volume's own, made by fetch-wine.sh under `wine/` here when
# it is missing or carries another yserver than the pin's; the packages
# already in the shared Wine volume's pool are copied, not downloaded
# again. (The shared tree is not used: other sessions bind it as a root
# for bubblewrap, and one held an older yserver than its stamp said on
# 2026-10-12.) yserver.commit and launcher.version beside it say what it
# carries. Needs curl, sha256sum, tar, xz, od, cmp and fetch-wine.sh's tools;
# no root.
#
# Usage: tools/common/fetch/fetch-endfield-launcher.sh

set -euo pipefail

LAUNCHER_VERSION=1.6.0.1607
LAUNCHER_DIRECTORY=1.6.0
installer_url="https://launcher.hg-cdn.com/TiaytKBUIEdoEwRT/launcher/6/6/endfield/1.6.0_s2nHVJbcPQrJnjb8/JOvlaZ9A/GRYPHLINK_v${LAUNCHER_VERSION}_6_6_endfield.exe"
installer_sum=b4b06cdb45c88c43fc3952f9810332f2709c3d7250a93016e9a4dc2c5ebaca73
sevenzip_url=https://www.7-zip.org/a/7z2501-linux-x64.tar.xz
sevenzip_sum=4ca3b7c6f2f67866b92622818b58233dc70367be2f36b498eb0bdeaaa44b53f4

out=${FERRIX_ENDFIELD_VOLUME:-$HOME/.local/share/ferrix/endfield-launcher}
shared=$HOME/.local/share/ferrix/wine
yserver=${FERRIX_YSERVER_VOLUME:-$HOME/.local/share/ferrix/yserver}

for tool in curl sha256sum tar xz od cmp mkfs.btrfs; do
    command -v "$tool" > /dev/null || { echo "fetch-endfield-launcher: $tool is not installed" >&2; exit 1; }
done

# Download $1 into the pool as $2 unless it is there, and check it against $3.
fetch() {
    local url=$1 file="$out/pool/$2" sum=$3
    if [ ! -f "$file" ]; then
        curl -fsSL -o "$file.part" "$url"
        mv "$file.part" "$file"
    fi
    echo "$sum  $file" | sha256sum -c --quiet \
        || { echo "fetch-endfield-launcher: $file does not match its pinned checksum" >&2; rm -f "$file"; exit 1; }
    printf '%s\n' "$file"
}

# The PE machine of $1: 8664 for x86-64, 014c for i386.
machine() {
    local pe
    pe=$(od -An -tu4 -j60 -N4 "$1" | tr -d ' ')
    od -An -tx2 -j$((pe + 4)) -N2 "$1" | tr -d ' '
}

# This volume's own Wine tree, made again when it is missing or carries
# another yserver than fetch-yserver.sh pins, or than yserver's tree holds.
wine="$out/wine"
pinned=$(sed -n 's/^YSERVER_COMMIT=//p' "$(dirname "$0")/fetch-yserver.sh")
if [ ! -d "$wine/tree" ] || [ "$(cat "$wine/yserver.commit" 2> /dev/null)" != "$pinned" ]     || ! cmp -s "$wine/tree/yserver/yserver" "$yserver/tree/yserver/yserver"; then
    mkdir -p "$wine/pool"
    [ ! -d "$shared/pool" ] || cp -n "$shared/pool/"*.deb "$wine/pool/" 2> /dev/null || true
    FERRIX_WINE_VOLUME="$wine" "$(dirname "$0")/fetch-wine.sh"
    rm -f "$wine/wine.img"
fi

mkdir -p "$out/pool"
installer=$(fetch "$installer_url" "GRYPHLINK_v${LAUNCHER_VERSION}.exe" "$installer_sum")
archive=$(fetch "$sevenzip_url" 7z2501-linux-x64.tar.xz "$sevenzip_sum")
tools="$out/7zip"
rm -rf "$tools"
mkdir -p "$tools"
tar -xJf "$archive" -C "$tools" 7zz

# The installer's payload, laid out as the installer lays it out.
unpacked="$out/unpacked"
rm -rf "$unpacked"
mkdir -p "$unpacked"
"$tools/7zz" x -y -bd -o"$unpacked" "$installer" > /dev/null
payload="$unpacked/\$0/$LAUNCHER_DIRECTORY"
for program in Launcher.exe Games.exe QtWebEngineProcess.exe; do
    [ -f "$payload/$program" ] \
        || { echo "fetch-endfield-launcher: the installer has no $LAUNCHER_DIRECTORY/$program" >&2; exit 1; }
    [ "$(machine "$payload/$program")" = 8664 ] \
        || { echo "fetch-endfield-launcher: $program is not a 64-bit program" >&2; exit 1; }
done

tree="$out/tree"
rm -rf "$tree"
cp -a "$wine/tree" "$tree"
mkdir -p "$tree/GRYPHLINK"
cp -a "$payload" "$tree/GRYPHLINK/$LAUNCHER_DIRECTORY"
cp -a "$payload/Launcher.exe" "$tree/GRYPHLINK/Launcher.exe"
rm -rf "$unpacked"

# Room for what the launcher writes beside itself (its logs, its web
# engine's cache) under QEMU's snapshot; the prefix is made on tmpfs.
size=$(( $(du -sm "$tree" | cut -f1) + 1024 ))
image="$out/endfield.img"
rm -f "$out/yserver.commit" "$out/launcher.version" "$image"
truncate -s "${size}M" "$image"
mkfs.btrfs -q --rootdir "$tree" "$image"
cp "$wine/yserver.commit" "$out/yserver.commit"
echo "$LAUNCHER_VERSION" > "$out/launcher.version"
echo "endfield launcher volume: $image (${size} MiB), GRYPHLINK $LAUNCHER_VERSION, unpacked in $tree"
