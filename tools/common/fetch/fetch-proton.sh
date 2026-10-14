#!/usr/bin/env bash
# Fetch GloriousEggroll's Proton (GE-Proton11-7, the latest prebuilt Proton)
# and put it on a btrfs volume beside Debian's Wine, with the 32-bit Debian
# libraries its 32-bit half runs on: the disk Endfield's launcher runs in
# Proton's Wine from (docs/WINE.md §10).
#
# Why Proton: Debian's Wine 10.0 draws the launcher's Qt translucent dialogs
# (its install dialog) blank, on Linux as on Ferrix. Valve's own Proton 11
# ships as source only; GE-Proton is built from it and released as a
# tarball, which is used as it is, unpacked at `/proton` (`files/bin/wine`,
# `files/lib/wine/...`, and the libraries it bundles in
# `files/lib/{x86_64,i386}-linux-gnu`). Its `proton` script, which Steam
# runs inside its own runtime, is not used; what it sets for Wine
# (WINEDLLPATH and LD_LIBRARY_PATH into `files/lib`) is set by whoever
# starts `files/bin/wine`.
#
# This build is Wine's old 32/64 layout, not its new WoW64 mode: a 32-bit
# Unix side (`files/lib/wine/i386-unix`, a 32-bit ELF `wine` and its `.so`
# modules) runs 32-bit Windows programs, and without it the 64-bit side
# does not make a prefix either (wineboot: "could not load kernel32.dll,
# c0000135"). That half is a 32-bit glibc program, so Debian 13's i386
# libraries it needs to make a prefix and draw a window through X are
# here, in Debian's multiarch places (`/usr/lib/i386-linux-gnu`, and
# `/lib/ld-linux.so.2` through the merged /usr as Debian has it): glibc,
# libgcc and libstdc++, zlib and zstd, Xlib with xcb, Xau and Xdmcp, the X
# extension libraries winex11 opens by name, and FreeType and fontconfig
# with what they load. The list is what running wineboot, the 32-bit
# `cmd` and a 32-bit window asked for on Linux (2026-10-13), not every
# NEEDED of Proton's modules: audio, USB, smart cards, Wayland and the
# media libraries Proton bundles are loaded only when a program asks, and
# are not here, as on the Wine volume.
#
# Proton's 64-bit side finds everything it needs to start on the Wine
# volume: its modules' NEEDED that neither the volume nor Proton has are
# the same on-demand ones (ALSA, PulseAudio, pcsclite, libusb, Wayland,
# VA-API, gnutls for its bundled FFmpeg), so no amd64 package is added.
#
# The volume is the Wine volume's tree (`fetch-wine.sh`, run into a
# directory of this volume's own when it is missing or carries another
# yserver than the pin's; the packages already in the shared Wine volume's
# pool are copied, not downloaded again) with Proton and the i386
# libraries added. The shared Wine tree is not used: other sessions bind it
# as a root for bubblewrap.
#
# The tarball is pinned by the SHA-512 GloriousEggroll publishes beside it
# (GE-Proton11-7-x86_64.sha512sum); a copy already downloaded is used when
# FERRIX_PROTON_TARBALL names it and it matches. Every Debian package is
# pinned by its SHA-256, taken from trixie's, trixie-updates' and
# trixie-security's i386 Packages files on 2026-10-13.
#
# Writes $FERRIX_PROTON_VOLUME/proton.img (default
# ~/.local/share/ferrix/proton/proton.img), about 5.7 GB with 2 GiB of room
# for a prefix, with yserver.commit and proton.version beside it. Needs
# curl, sha256sum, sha512sum, tar, dpkg-deb, readelf, mkfs.btrfs and
# fetch-wine.sh's tools; no root.
#
# Usage: tools/common/fetch/fetch-proton.sh

set -euo pipefail

debian=${DEBIAN_MIRROR:-https://deb.debian.org/debian}
security=${DEBIAN_SECURITY_MIRROR:-https://security.debian.org/debian-security}
out=${FERRIX_PROTON_VOLUME:-$HOME/.local/share/ferrix/proton}
shared=$HOME/.local/share/ferrix/wine
yserver=${FERRIX_YSERVER_VOLUME:-$HOME/.local/share/ferrix/yserver}

PROTON_VERSION=GE-Proton11-7
proton_url="https://github.com/GloriousEggroll/proton-ge-custom/releases/download/$PROTON_VERSION/$PROTON_VERSION-x86_64.tar.gz"
proton_sum=7db87e9787e20c35cbdac26018431d5794626b626e4067b050684e45a88cc2ca229d7d263519eafb2e168cde5bef57611065d159d3685aaec152ccb9abe3073f

# Debian's Open Sans, for Segoe UI, which Windows programs name and no
# prefix has: Qt and Chromium then fall back to Times New Roman, and the
# launcher's dialogs come out serif where Windows draws them in Segoe UI.
# Open Sans is close to it in shape, and hinted: Microsoft's own open
# stand-in, Selawik, carries no TrueType instructions, so Wine's FreeType
# auto-hints it and rounds each glyph's advance to a pixel, and the
# spacing came out uneven ("N OTIFICATION"). The boot points Wine's font
# replacements for the Segoe UI family at it (docs/WINE.md §10). Pinned
# by SHA-256 from trixie's Packages, 2026-10-13.
open_sans_deb="pool/main/f/fonts-open-sans/fonts-open-sans_1.11-2_all.deb"
open_sans_sum=ed8c4258af6c23e5105fa18940544f060f3c9664fbe881f25130fe0c65bc1485

# Pool path and SHA-256 of each i386 package, from the main archive.
debs=(
    "pool/main/b/brotli/libbrotli1_1.1.0-2+b7_i386.deb 696b5206497da474e9480c0a4700d0835ee34f90ee0e505091a9402e70fc1a71"
    "pool/main/b/bzip2/libbz2-1.0_1.0.8-6_i386.deb c23dee450dcee574135cbe004d463be3f413cdc017b22d8273c5ce0a18a7de10"
    "pool/main/g/glibc/libc6_2.41-12+deb13u4_i386.deb 67bb4b54cf4bb6a380449dfa1852cd9135659f85360caa0e41d312f4b30e573a"
    "pool/main/e/expat/libexpat1_2.8.3-1~deb13u1_i386.deb d0396d41674a2fb1946af2b131630534a2cf09d160c8ea13bc5bbaa7416f071a"
    "pool/main/f/fontconfig/libfontconfig1_2.15.0-2.3_i386.deb d24faaee293c8f76fd68dea181ac927ad8fb8ec18fcdf7e5bf6fe847678f227b"
    "pool/main/f/freetype/libfreetype6_2.13.3+dfsg-1+deb13u1_i386.deb fbe01c5a6e2654ed7c8fbb08b39c9b20cf19bd24554088d11762c330f7af1dd2"
    "pool/main/g/gcc-14/libgcc-s1_14.2.0-19_i386.deb a4c71fd856d2a48a7505a087b4186e3cca23f94603c05e3fb7c799b27e72f761"
    "pool/main/g/gcc-14/libstdc++6_14.2.0-19_i386.deb b6020260b92a97ac33ae58a73b16f3ab31fed7632e9b861f7cd5fc393facd6ed"
    "pool/main/libx/libx11/libx11-6_1.8.12-1_i386.deb 0a4ee67fffb3299953a9bd298c33e4f66d1b7f2f112eb83eb32c8e4351927741"
    "pool/main/libx/libxau/libxau6_1.0.11-1_i386.deb 1645415b2ea8b64268f87944b733b9f9a2f243f89eb73555120eaf459fa06a57"
    "pool/main/libx/libxcb/libxcb1_1.17.0-2+b1_i386.deb a4c421cf27ef4f66b32f650e3ac8bbd12bc8ec28b70a3be5dcc56dc3453e8d3e"
    "pool/main/libx/libxcomposite/libxcomposite1_0.4.6-1_i386.deb 74993ed9b8652244e862dbad8224b72a1fd634ee326abee8c7f7d3b3800e4a9c"
    "pool/main/libx/libxcursor/libxcursor1_1.2.3-1_i386.deb 730d69855b63ad950143bd3b1a4629b25633eaa691148e9fde1dfe38495da809"
    "pool/main/libx/libxdmcp/libxdmcp6_1.1.5-1_i386.deb 06b3a5d82fd69b1ff66e4e68fd4cf55f616864a29f456609a1f92b275b6126f7"
    "pool/main/libx/libxext/libxext6_1.3.4-1+b3_i386.deb 9e15d0b7f325c70081acb33e21bf019a1bb7c9f0df262ece49bef91d072705a1"
    "pool/main/libx/libxfixes/libxfixes3_6.0.0-2+b4_i386.deb 5ab7a33e21647a5398d422eff7426595c1bcbb8a58574c0cc8aa1a4f0a44c6cc"
    "pool/main/libx/libxi/libxi6_1.8.2-1_i386.deb 5a423e684387b8595cd9de0496279cef5a2bf54afd6a5c165d92b6e692e5a0fe"
    "pool/main/libx/libxinerama/libxinerama1_1.1.4-3+b4_i386.deb 53618c19fc1f95d814e57a67b81d079ef6afef35f9fdc017193b3834b602ed00"
    "pool/main/libx/libxrandr/libxrandr2_1.5.4-1+b3_i386.deb b3940d28cd51c5fb295737085bcca24064f3288a3b98cea3f9f92a4dfa757c72"
    "pool/main/libx/libxrender/libxrender1_0.9.12-1_i386.deb c1666fddcb0ded151fbe3a2569264761b4e8a18e862bcc2bce156aa0f06d3473"
    "pool/main/libx/libxxf86vm/libxxf86vm1_1.1.4-1+b4_i386.deb b3f612262c335fde6536d4fa62df37640a5e376c29e8e484bfce70f7ee6eabd0"
    "pool/main/libz/libzstd/libzstd1_1.5.7+dfsg-1_i386.deb 305f0ab5f16552c44906b3f3f6e9ad3eb33f6f857c3347fdf3bd987abd6989a9"
    "pool/main/z/zlib/zlib1g_1.3.dfsg+really1.3.1-1+b1_i386.deb 8950c79b1ade51f53b2f37863aec532cfaca3dff315e28f0c5b53902425c41ed"
)

# The same, from trixie-security.
security_debs=(
    "pool/updates/main/libp/libpng1.6/libpng16-16t64_1.6.48-1+deb13u6_i386.deb 087f92b31fa6c4ff047e539c944a3f97432d15b6b8e5ef4127a7e13b72643847"
)

# Wine's Schannel, which a Windows program's own HTTPS goes through
# (WinHTTP, WinINet): GnuTLS and what it loads, and the CA certificates
# crypt32 checks a server's chain against. Without them Wine says "secure
# connections will not be available", and the launcher's download SDK,
# which fetches the game that way, ends at once (nCode 206) where the
# launcher's own libcurl and its web view, which carry their own TLS,
# reach the same servers. amd64 only: the downloader is the 64-bit
# Games.exe. The closure is what trixie's Packages give libgnutls30t64
# beyond what the Wine volume has, 2026-10-14; the certificates are
# fetch-steam.sh's pin.
tls_debs=(
    "pool/main/g/gmp/libgmp10_6.3.0+dfsg-3_amd64.deb d0d0265eb01770f17afd0f7c8c0622f80479dcfbbe13653a0debeec61464e622"
    "pool/main/g/gnutls28/libgnutls30t64_3.8.9-3+deb13u4_amd64.deb 18a8bdfd91c7e3bcb01719d55a2b56849c7160b34f1f52b1c4fdfdd41bf1352b"
    "pool/main/n/nettle/libhogweed6t64_3.10.1-1_amd64.deb b059ed155115ac09da295322de48ee1ed58ef5fa45edcc9e12ff0e94636ef25f"
    "pool/main/libi/libidn2/libidn2-0_2.3.8-2_amd64.deb 90b039bcdc4578f8e1c4935adf8dbb525e36a164deefdbbb8c45bac347d48278"
    "pool/main/n/nettle/libnettle8t64_3.10.1-1_amd64.deb 1b03d4a9cd9c8143ba50fe6396f36937e901a317d492d179f4596862c1731cfe"
    "pool/main/p/p11-kit/libp11-kit0_0.25.5-3_amd64.deb 784bf2063e166c8bc851a32623b74ebd85c499043a9d57c5bbd64fa63447f45a"
    "pool/main/libt/libtasn1-6/libtasn1-6_4.20.0-2+deb13u1_amd64.deb 23fec6e06583ce2bad9b2c04c9b485e90440e259b1abf8677cd80d3ce60831ad"
    "pool/main/libu/libunistring/libunistring5_1.3-2_amd64.deb 6dd3490bef06ea1096f32d10766b7e016cf579bfae8451d1b7df15ce05b8aa46"
    "pool/main/c/ca-certificates/ca-certificates_20250419_all.deb ef590f89563aa4b46c8260d49d1cea0fc1b181d19e8df3782694706adf05c184"
)

for tool in curl sha256sum sha512sum tar dpkg-deb readelf mkfs.btrfs; do
    command -v "$tool" > /dev/null || { echo "fetch-proton: $tool is not installed" >&2; exit 1; }
done

# Download $2/$1 into the pool unless it is there, and check it against $3.
fetch() {
    local file="$out/pool/${1##*/}" base=$2 sum=$3
    if [ ! -f "$file" ]; then
        curl -fsSL -o "$file.part" "$base/$1"
        mv "$file.part" "$file"
    fi
    echo "$sum  $file" | sha256sum -c --quiet \
        || { echo "fetch-proton: $file does not match its pinned checksum" >&2; rm -f "$file"; exit 1; }
    printf '%s\n' "$file"
}

# This volume's own Wine tree, made again when it is missing or carries
# another yserver than fetch-yserver.sh pins.
wine="$out/wine"
pinned=$(sed -n 's/^YSERVER_COMMIT=//p' "$(dirname "$0")/fetch-yserver.sh")
if [ ! -d "$wine/tree" ] || [ "$(cat "$wine/yserver.commit" 2> /dev/null)" != "$pinned" ]; then
    mkdir -p "$wine/pool"
    [ ! -d "$shared/pool" ] || cp --update=none "$shared/pool/"*.deb "$wine/pool/" 2> /dev/null || true
    FERRIX_WINE_VOLUME="$wine" FERRIX_YSERVER_VOLUME="$yserver" "$(dirname "$0")/fetch-wine.sh"
    rm -rf "$wine/wine.img" "$wine/root"
fi

mkdir -p "$out/pool"

# Proton's tarball, checked against its SHA-512.
tarball="$out/pool/$PROTON_VERSION-x86_64.tar.gz"
if [ ! -f "$tarball" ]; then
    if [ -n "${FERRIX_PROTON_TARBALL:-}" ] \
        && echo "$proton_sum  $FERRIX_PROTON_TARBALL" | sha512sum -c --quiet 2> /dev/null; then
        ln "$FERRIX_PROTON_TARBALL" "$tarball" 2> /dev/null || cp "$FERRIX_PROTON_TARBALL" "$tarball"
    else
        curl -fsSL -o "$tarball.part" "$proton_url"
        mv "$tarball.part" "$tarball"
    fi
fi
echo "$proton_sum  $tarball" | sha512sum -c --quiet \
    || { echo "fetch-proton: $tarball does not match its pinned checksum" >&2; rm -f "$tarball"; exit 1; }

# The i386 libraries, unpacked apart, and only their libraries kept.
i386="$out/i386"
rm -rf "$i386"
mkdir -p "$i386"
for entry in "${debs[@]}"; do
    read -r deb sum <<< "$entry"
    dpkg-deb -x "$(fetch "$deb" "$debian" "$sum")" "$i386"
done
for entry in "${security_debs[@]}"; do
    read -r deb sum <<< "$entry"
    dpkg-deb -x "$(fetch "$deb" "$security" "$sum")" "$i386"
done
test -e "$i386/usr/lib/i386-linux-gnu/ld-linux.so.2" \
    || { echo "fetch-proton: no ld-linux.so.2 in the i386 libc6" >&2; exit 1; }

# The volume: the Wine tree, the i386 libraries where Debian puts them, and
# Proton at /proton.
tree="$out/tree"
rm -rf "$tree"
cp -a "$wine/tree" "$tree"
cp -a "$i386/usr/lib/i386-linux-gnu" "$tree/usr/lib/"
cp -a "$i386/usr/lib/ld-linux.so.2" "$tree/usr/lib/ld-linux.so.2"
mkdir -p "$tree/etc/ld.so.conf.d"
cp -a "$i386/etc/ld.so.conf.d/." "$tree/etc/ld.so.conf.d/"
rm -rf "$i386"
for entry in "${tls_debs[@]}"; do
    read -r deb sum <<< "$entry"
    dpkg-deb -x "$(fetch "$deb" "$debian" "$sum")" "$tree"
done
# The bundle `update-ca-certificates` would write, which crypt32 reads.
mkdir -p "$tree/etc/ssl/certs"
find "$tree/usr/share/ca-certificates" -name '*.crt' -print0 | sort -z \
    | xargs -0 cat > "$tree/etc/ssl/certs/ca-certificates.crt"
test -s "$tree/etc/ssl/certs/ca-certificates.crt" \
    || { echo "fetch-proton: the certificate bundle is empty" >&2; exit 1; }
test -e "$tree/usr/lib/x86_64-linux-gnu/libgnutls.so.30" \
    || { echo "fetch-proton: no libgnutls.so.30 on the volume" >&2; exit 1; }
mkdir -p "$tree/proton"
tar -xzf "$tarball" -C "$tree/proton" --strip-components=1
dpkg-deb -x "$(fetch "$open_sans_deb" "$debian" "$open_sans_sum")" "$out/open-sans"
mkdir -p "$tree/usr/share/fonts/truetype/open-sans"
cp "$out/open-sans/usr/share/fonts/truetype/open-sans/OpenSans-"{Regular,Semibold,Light,Bold}.ttf \
    "$tree/usr/share/fonts/truetype/open-sans/"
rm -rf "$out/open-sans"
test -x "$tree/proton/files/bin/wine" \
    || { echo "fetch-proton: no files/bin/wine in $PROTON_VERSION" >&2; exit 1; }

# Every library Proton's loaders and the Unix modules that start a prefix
# and draw a window need must be on it, for both halves.
missing=0
check() {
    local multi=$1 bundled=$2 file needed
    shift 2
    for file in "$@"; do
        for needed in $(readelf -d "$file" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
            if [ ! -e "$tree/usr/lib/$multi/$needed" ] && [ ! -e "$tree/usr/lib/$needed" ] \
                && [ ! -e "$bundled/$needed" ] && [ ! -e "$(dirname "$file")/$needed" ]; then
                echo "fetch-proton: ${file#"$tree"/} needs $needed, which is not on the volume" >&2
                missing=1
            fi
        done
    done
}
lib="$tree/proton/files/lib"
check x86_64-linux-gnu "$lib/x86_64-linux-gnu" "$tree/proton/files/bin/"{wine,wineserver} \
    "$lib/wine/x86_64-unix/"{wine,ntdll.so,win32u.so,winex11.so,ws2_32.so,opengl32.so,winevulkan.so}
check i386-linux-gnu "$lib/i386-linux-gnu" \
    "$lib/wine/i386-unix/"{wine,ntdll.so,win32u.so,winex11.so,ws2_32.so}
[ "$missing" = 0 ] || exit 1

# Room for a prefix, as on the Wine volume (fetch-wine.sh says why 2 GiB).
size=$(( $(du -sm "$tree" | cut -f1) + 2048 ))
image="$out/proton.img"
rm -f "$out/yserver.commit" "$out/proton.version" "$image"
truncate -s "${size}M" "$image"
mkfs.btrfs -q --rootdir "$tree" "$image"
cp "$wine/yserver.commit" "$out/yserver.commit"
echo "$PROTON_VERSION" > "$out/proton.version"
echo "proton volume: $image (${size} MiB), $PROTON_VERSION, unpacked in $tree"
