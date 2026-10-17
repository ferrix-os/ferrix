#!/usr/bin/env bash
# Fetch Arknights: Endfield's Windows launcher (GRYPHLINK) from Gryphline and
# put it, installed, on a btrfs volume beside Debian's Wine: the disk
# `cargo xtask test-endfield-launcher` starts it from (docs/WINE.md §8,
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
# With FERRIX_ENDFIELD_WINE=proton the tree is fetch-proton.sh's instead
# (GE-Proton at `/proton` beside Debian's Wine, docs/WINE.md §10), made
# again when it is missing or holds another Proton or yserver than the
# pins, and the volume goes to ~/.local/share/ferrix/endfield-launcher-proton
# with proton.version beside it: the disk `cargo xtask test-endfield-proton`
# starts the launcher from.
#
# Usage: [FERRIX_ENDFIELD_WINE=proton] tools/common/fetch/fetch-endfield-launcher.sh

set -euo pipefail

LAUNCHER_VERSION=1.6.0.1607
LAUNCHER_DIRECTORY=1.6.0
installer_url="https://launcher.hg-cdn.com/TiaytKBUIEdoEwRT/launcher/6/6/endfield/1.6.0_s2nHVJbcPQrJnjb8/JOvlaZ9A/GRYPHLINK_v${LAUNCHER_VERSION}_6_6_endfield.exe"
installer_sum=b4b06cdb45c88c43fc3952f9810332f2709c3d7250a93016e9a4dc2c5ebaca73
sevenzip_url=https://www.7-zip.org/a/7z2501-linux-x64.tar.xz
sevenzip_sum=4ca3b7c6f2f67866b92622818b58233dc70367be2f36b498eb0bdeaaa44b53f4

wine_kind=${FERRIX_ENDFIELD_WINE:-debian}
case $wine_kind in
    debian) out=${FERRIX_ENDFIELD_VOLUME:-$HOME/.local/share/ferrix/endfield-launcher} ;;
    proton) out=${FERRIX_ENDFIELD_VOLUME:-$HOME/.local/share/ferrix/endfield-launcher-proton} ;;
    *) echo "fetch-endfield-launcher: FERRIX_ENDFIELD_WINE is debian or proton, not $wine_kind" >&2; exit 1 ;;
esac
proton=${FERRIX_PROTON_VOLUME:-$HOME/.local/share/ferrix/proton}
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
if [ "$wine_kind" = proton ]; then
    # fetch-proton.sh's tree, made again when it holds another Proton or
    # another yserver than the pins.
    wine="$proton"
    proton_pinned=$(sed -n 's/^PROTON_VERSION=//p' "$(dirname "$0")/fetch-proton.sh")
    if [ ! -d "$proton/tree" ] || [ "$(cat "$proton/proton.version" 2> /dev/null)" != "$proton_pinned" ] \
        || [ "$(cat "$proton/yserver.commit" 2> /dev/null)" != "$pinned" ]; then
        "$(dirname "$0")/fetch-proton.sh"
    fi
elif [ ! -d "$wine/tree" ] || [ "$(cat "$wine/yserver.commit" 2> /dev/null)" != "$pinned" ]     || ! cmp -s "$wine/tree/yserver/yserver" "$yserver/tree/yserver/yserver"; then
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

# With Proton, `gryphlink`: the launcher on a desktop with yserver, as
# `run-compositor --everything` has (docs/WINE.md §10). It makes a prefix
# under the user's home the first time, as test-endfield-proton's boot
# does -- Proton's fonts linked in before wineboot, Open Sans for the Segoe
# UI family -- and starts the launcher in it. busybox's sh, since /bin/sh
# is zinc, which stops on a glob that matches nothing.
if [ "$wine_kind" = proton ]; then
    mkdir -p "$tree/usr/bin"
    cat > "$tree/usr/bin/gryphlink" << 'GRYPHLINK'
#!/bin/busybox sh
# Arknights: Endfield's launcher (GRYPHLINK) in GE-Proton's Wine, on the X
# server of the desktop it is started from (docs/WINE.md §10).
export DISPLAY="${DISPLAY:-:0}"
export WINEPREFIX="${WINEPREFIX:-$HOME/.local/share/gryphlink/prefix}"
export WINESERVER=/data/proton/files/bin/wineserver
export LD_LIBRARY_PATH=/data/proton/files/lib/x86_64-linux-gnu:/data/proton/files/lib/i386-linux-gnu
export WINEDLLPATH=/data/proton/files/lib/vkd3d:/data/proton/files/lib/wine
export WINEDEBUG="${WINEDEBUG:-err+all,fixme-all}"
# fsync (docs/WINE.md §11): every wait a futex word, not a request to
# wineserver. Without it the launcher's waits kept wineserver busy on the
# N150 box while it downloaded (2026-10-17).
export WINEFSYNC="${WINEFSYNC:-1}"
wine=/data/proton/files/bin/wine
if [ ! -e "$WINEPREFIX/system.reg" ]; then
    mkdir -p "$WINEPREFIX/drive_c/windows/Fonts"
    for f in /data/proton/files/share/fonts/*.tt[fc] /data/proton/files/share/wine/fonts/*.tt[fc] \
        /data/usr/share/fonts/truetype/open-sans/*.ttf; do
        [ -e "$f" ] && ln -sf "$f" "$WINEPREFIX/drive_c/windows/Fonts/"
    done
    "$wine" wineboot -i
    "$WINESERVER" -w
    for pair in "Segoe UI=Open Sans" "Segoe UI Semibold=Open Sans Semibold" "Segoe UI Light=Open Sans Light" \
        "Segoe UI Semilight=Open Sans Light" "Segoe UI Bold=Open Sans"; do
        "$wine" reg add 'HKCU\Software\Wine\Fonts\Replacements' /v "${pair%%=*}" /d "${pair#*=}" /f > /dev/null 2>&1
    done
    "$WINESERVER" -w
    ln -s /data/GRYPHLINK "$WINEPREFIX/drive_c/GRYPHLINK"
fi
cd "$WINEPREFIX/drive_c/GRYPHLINK" || exit 1
exec "$wine" 'C:\GRYPHLINK\Launcher.exe' "$@"
GRYPHLINK
    chmod 755 "$tree/usr/bin/gryphlink"
fi

# Room for what the launcher writes beside itself (its logs, its web
# engine's cache) under QEMU's snapshot; the prefix is made on tmpfs.
size=$(( $(du -sm "$tree" | cut -f1) + 1024 ))
image="$out/endfield.img"
rm -f "$out/yserver.commit" "$out/launcher.version" "$out/proton.version" "$image"
truncate -s "${size}M" "$image"
mkfs.btrfs -q --rootdir "$tree" "$image"
cp "$wine/yserver.commit" "$out/yserver.commit"
[ "$wine_kind" != proton ] || cp "$proton/proton.version" "$out/proton.version"
echo "$LAUNCHER_VERSION" > "$out/launcher.version"
echo "endfield launcher volume: $image (${size} MiB), GRYPHLINK $LAUNCHER_VERSION, unpacked in $tree"
