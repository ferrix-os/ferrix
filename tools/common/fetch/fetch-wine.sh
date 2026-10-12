#!/usr/bin/env bash
# Fetch Debian 13's 64-bit Wine and put it on a btrfs volume beside yserver:
# the disk `cargo xtask test-wine` starts `wine` from (docs/WINE.md).
#
# Nothing here is built. Wine is Debian's `wine64`, `libwine` and `wine`
# (10.0), unpacked as they are, with what its Unix libraries load to start
# a prefix and draw a window through X: glibc, libunwind, FreeType and
# fontconfig, Xlib and the X extension libraries winex11 opens by name, the
# zlib its PE side takes from `libz-mingw-w64`, and Wine's and Liberation's
# fonts. `libwine` depends on far more (FFmpeg, GStreamer, PulseAudio, ...);
# each of those is a library one of Wine's modules opens when a program
# asks for it, and none is here yet. Debian's `strace` is on the volume too.
#
# The volume is yserver's tree (`fetch-yserver.sh`, which this runs when the
# tree is missing or was built from another commit than the one it pins)
# with Wine's unpacked over it, so one disk at `/data` has the X server, its
# clients and Wine, all on the same Debian's libraries.
#
# 32-bit Windows programs run inside the 64-bit Wine (its new WoW64 mode),
# which loads their DLLs from `i386-windows` beside its own `x86_64-windows`.
# Debian builds those PE files only for its i386 `libwine`, so that
# package's `i386-windows` is copied in and nothing else of it: its Unix
# half is a 32-bit Wine's, which this volume has no use for.
#
# Every package is pinned by its SHA-256, taken from trixie's,
# trixie-updates' and trixie-security's Packages files on 2026-10-10 (the
# i386 `libwine` on 2026-10-12).
#
# Writes $FERRIX_WINE_VOLUME/wine.img (default
# ~/.local/share/ferrix/wine/wine.img), about 3.7 GB with 2 GiB of room for
# a prefix, and yserver.commit
# beside it, the commit of the yserver it carries. Needs curl,
# sha256sum, dpkg-deb, readelf and mkfs.btrfs; no root.
#
# Usage: tools/common/fetch/fetch-wine.sh

set -euo pipefail

debian=${DEBIAN_MIRROR:-https://deb.debian.org/debian}
security=${DEBIAN_SECURITY_MIRROR:-https://security.debian.org/debian-security}
out=${FERRIX_WINE_VOLUME:-$HOME/.local/share/ferrix/wine}
yserver=${FERRIX_YSERVER_VOLUME:-$HOME/.local/share/ferrix/yserver}

# Pool path and SHA-256 of each package, from the main archive.
debs=(
    "pool/main/d/debconf/debconf_1.5.91_all.deb 1ddc7e6c5925f748eefb7ca811d98e049f18c158bb21fbc7e122262ce6011e49"
    "pool/main/f/fontconfig/fontconfig-config_2.15.0-2.3_amd64.deb 0475c00d02660c07a15085051818625331bb502e053242106aaaa1f2ddb41225"
    "pool/main/f/fonts-dejavu/fonts-dejavu-core_2.37-8_all.deb 86635b3d25b3655fc11cb3ecc3af59f0bf19643b02b94f2de48bd10253cdba12"
    "pool/main/f/fonts-dejavu/fonts-dejavu-mono_2.37-8_all.deb 3003e98a5debfdeadc7040a7f715fe9fe6fb67f68deacf6049b54e30f07fc014"
    "pool/main/f/fonts-liberation/fonts-liberation_2.1.5-3_all.deb 3d5c47480067b9615f6bf9b0eda6eefcab9d837af609827015445e8e759b5b8c"
    "pool/main/w/wine/fonts-wine_10.0~repack-6_all.deb 23ea26a992ed7b4577334441c14b2e4b0d95966fbcf87167adfb57f73b29d8d4"
    "pool/main/g/gcc-14/gcc-14-base_14.2.0-19_amd64.deb 5b6825de4263824b78c4c51f6476414f3b4e89c2ab63e81dc8b9b5501e867cf6"
    "pool/main/b/brotli/libbrotli1_1.1.0-2+b7_amd64.deb 0fb79f88db210afbd69282ab9649e525f393ec6950ca34da1a6b359250b8d7db"
    "pool/main/b/bzip2/libbz2-1.0_1.0.8-6_amd64.deb cba4cda04244b5e481bb15524bc3c983a7d1b6f330013b9b381706a2fcb65310"
    "pool/main/g/glibc/libc6_2.41-12+deb13u4_amd64.deb 967aa62605721081c3eb2a17650611a792aa802d76a6511d1840242623d204c9"
    "pool/main/f/fontconfig/libfontconfig1_2.15.0-2.3_amd64.deb 7ae91ec59857cc8e375eb2bdd371a4a69daea38f7787d06d474a847867d852b1"
    "pool/main/g/gcc-14/libgcc-s1_14.2.0-19_amd64.deb 3c71917b490d1a17aed43196a2787a256ecf060526cdb20216a74bedc061b150"
    "pool/main/libu/libunwind/libunwind8_1.8.1-0.1_amd64.deb db21a86dd05c93413f0ef36282a9c64d32410d240f332273a53be1408bec1f62"
    "pool/main/w/wine/libwine_10.0~repack-6_amd64.deb 702c99d2fbaef2a4966667e0cdabd477ed1754c1b3b5d4476606eaf5fd6e1444"
    "pool/main/libx/libx11/libx11-6_1.8.12-1_amd64.deb b5a3fd3bf8c8fd0364bfb9bea00dcba7fc301229bd02dded084632d31f5b0fb3"
    "pool/main/libx/libx11/libx11-data_1.8.12-1_all.deb c54f87069888f80ba4da586da6147d74c7598ccdd8b90906dbc4271fa414c738"
    "pool/main/libx/libxau/libxau6_1.0.11-1_amd64.deb 689a9f0e0ba3e2c65431f864871e303ee904de69dd28abfc462663fae030227f"
    "pool/main/libx/libxcb/libxcb1_1.17.0-2+b1_amd64.deb 5c222a72d11b866447da31693254f738430726e3e065a384e82687b2fd2f978b"
    "pool/main/libx/libxcomposite/libxcomposite1_0.4.6-1_amd64.deb 20e3c1d9b2135f0c8c4246a9fd26a51a57d5850c3b36ecc173c45d6be7328af3"
    "pool/main/libx/libxcursor/libxcursor1_1.2.3-1_amd64.deb bb0dbf58b804faaa74ed9efdae61c4b14bb0b4855680798f63e5ca6a4954976b"
    "pool/main/libx/libxdmcp/libxdmcp6_1.1.5-1_amd64.deb 0740dc760916b2008b45417a42a8fd7dd5de370fb57d31373f15034cda8acf0b"
    "pool/main/libx/libxext/libxext6_1.3.4-1+b3_amd64.deb fc618ec40465e5ce48622606299cb47833efc3fb235ba15543b81f850722f443"
    "pool/main/libx/libxfixes/libxfixes3_6.0.0-2+b4_amd64.deb 3fdd95d86d8e9d63e11f52070935c8f9f912c36aa3b17338d969f7685f3ed4fb"
    "pool/main/libx/libxi/libxi6_1.8.2-1_amd64.deb 093d0903f35bb7a9f6815180ee040e6951fecf9b66c128cd72f064710210606e"
    "pool/main/libx/libxinerama/libxinerama1_1.1.4-3+b4_amd64.deb 49d4e1628960407b8be2880a42428e7629801715a8a5697d42c0b0ca0a479911"
    "pool/main/libx/libxrandr/libxrandr2_1.5.4-1+b3_amd64.deb 11e3490de93a8bbee3daba719cb8e1325a26fb3c125525c34bdcb7deb05eb9b2"
    "pool/main/libx/libxrender/libxrender1_0.9.12-1_amd64.deb 9d042dfd5e613be1e02e6ddd0c5c4adef19c5eb08f6db838c2eba672c496dca4"
    "pool/main/libx/libxxf86vm/libxxf86vm1_1.1.4-1+b4_amd64.deb f9f8487e536293e7ecf91fe7feb14e97e02a3aa652e207c661442dce775d3b14"
    "pool/main/libz/libz-mingw-w64/libz-mingw-w64_1.3.1+dfsg-2_all.deb c4e6d6bfabe8b6d12c797736a3d762d83513b49f488d1aaf61d4526387a2dd3e"
    "pool/main/s/strace/strace_6.13+ds-1_amd64.deb cae290b67cf835350a2cde58ca332256fba475784b3e02a9a8677d73eaf47511"
    "pool/main/w/wine/wine_10.0~repack-6_all.deb fac371d73c676c7d32b7cb619c4531856c268f0b28a2719816b5881e405cfd67"
    "pool/main/w/wine/wine64_10.0~repack-6_amd64.deb 4cdf8319d873c13210815c1aac18859a56ea21b0b2d35a3388dab881464ed488"
    "pool/main/z/zlib/zlib1g_1.3.dfsg+really1.3.1-1+b1_amd64.deb 015be740d6236ad114582dea500c1d907f29e16d6db00566ca32fb68d71ac90d"
)

# The i386 `libwine`, whose `i386-windows` alone is used.
wow64_deb="pool/main/w/wine/libwine_10.0~repack-6_i386.deb fef4802a6553a17eaa40003333a86fc697440d1d21c5824f2dbd1c444f01527b"

# The same, from trixie-security.
security_debs=(
    "pool/updates/main/e/expat/libexpat1_2.8.3-1~deb13u1_amd64.deb 38abe0e710a07688e9c149d74536e67cfee0364bdb64dd6d644c32a1cfad389f"
    "pool/updates/main/f/freetype/libfreetype6_2.13.3+dfsg-1+deb13u1_amd64.deb e4947f3291528f03d574f2b01d5c5fc45c58c47480f32888a5cdf0f231ab584e"
    "pool/updates/main/x/xz-utils/liblzma5_5.8.1-1+deb13u2_amd64.deb b38acab30f295bf1066a6632e31cbd0a1980f2924b38040cda131d5dea5384ec"
    "pool/updates/main/libp/libpng1.6/libpng16-16t64_1.6.48-1+deb13u6_amd64.deb 0f8034d1c2547760430c81e8d87e5d6acd533a3493c80cf76dbecd0343b6773c"
)

for tool in curl sha256sum dpkg-deb readelf mkfs.btrfs; do
    command -v "$tool" > /dev/null || { echo "fetch-wine: $tool is not installed" >&2; exit 1; }
done

# Download $2/$1 into the pool unless it is there, and check it against $3.
fetch() {
    local path=$1 base=$2 sum=$3
    local file="$out/pool/${path##*/}"
    if [ ! -f "$file" ]; then
        curl -fsSL -o "$file.part" "$base/$path"
        mv "$file.part" "$file"
    fi
    echo "$sum  $file" | sha256sum -c --quiet \
        || { echo "fetch-wine: $file does not match its pinned checksum" >&2; rm -f "$file"; exit 1; }
    printf '%s\n' "$file"
}

# yserver's tree, made again when it is missing or holds another commit
# than the one fetch-yserver.sh pins.
pinned=$(sed -n 's/^YSERVER_COMMIT=//p' "$(dirname "$0")/fetch-yserver.sh")
if [ ! -d "$yserver/tree" ] || [ "$(cat "$yserver/yserver.commit" 2> /dev/null)" != "$pinned" ]; then
    "$(dirname "$0")/fetch-yserver.sh"
fi

mkdir -p "$out/pool"
root="$out/root"
rm -rf "$root"
mkdir -p "$root"

# Debian 13's merged /usr, which an installed system has and unpacking does
# not make.
for merged in bin lib lib64 sbin; do
    mkdir -p "$root/usr/$merged"
    ln -s "usr/$merged" "$root/$merged"
done

for entry in "${debs[@]}"; do
    read -r path sum <<< "$entry"
    dpkg-deb -x "$(fetch "$path" "$debian" "$sum")" "$root"
done
for entry in "${security_debs[@]}"; do
    read -r path sum <<< "$entry"
    dpkg-deb -x "$(fetch "$path" "$security" "$sum")" "$root"
done
rm -rf "$root/usr/share/doc" "$root/usr/share/man" "$root/usr/share/locale" \
    "$root/usr/share/lintian"

read -r path sum <<< "$wow64_deb"
wow64="$out/wow64"
rm -rf "$wow64"
mkdir -p "$wow64"
dpkg-deb -x "$(fetch "$path" "$debian" "$sum")" "$wow64"
cp -a "$wow64/usr/lib/i386-linux-gnu/wine/i386-windows/." \
    "$root/usr/lib/x86_64-linux-gnu/wine/i386-windows/"
rm -rf "$wow64"
test -f "$root/usr/lib/x86_64-linux-gnu/wine/i386-windows/ntdll.dll" \
    || { echo "fetch-wine: no 32-bit ntdll.dll in the i386 libwine" >&2; exit 1; }

# What libwine's postinst does, which unpacking does not: zlib's DLLs from
# libz-mingw-w64, each marked as one of Wine's builtins (the marker is at
# offset 64, in the DOS stub), beside Wine's own.
for pair in i686:i386 x86_64:x86_64; do
    in="$root/usr/${pair%:*}-w64-mingw32/lib/zlib1.dll"
    marked="$root/usr/lib/x86_64-linux-gnu/wine/${pair#*:}-windows/zlib1.dll"
    {
        head -c 64 "$in"
        printf 'Wine builtin DLL\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0'
        tail -c +97 "$in"
    } > "$marked"
done

test -x "$root/usr/lib/wine/wine64" \
    || { echo "fetch-wine: no wine64 loader in the packages" >&2; exit 1; }

# The volume: yserver's tree with Wine's over it.
tree="$out/tree"
rm -rf "$tree"
cp -a "$yserver/tree" "$tree"
cp -a "$root/usr/." "$tree/usr/"
[ ! -d "$root/etc" ] || cp -a "$root/etc/." "$tree/etc/"

# Every library Wine's loader and its Unix libraries need must be on it.
missing=0
unix="$tree/usr/lib/x86_64-linux-gnu/wine/x86_64-unix"
for file in "$tree/usr/lib/wine/wine64" "$tree/usr/lib/wine/wineserver64" \
    "$unix/ntdll.so" "$unix/win32u.so" "$unix/winex11.so"; do
    for needed in $(readelf -d "$file" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
        if [ ! -e "$tree/usr/lib/x86_64-linux-gnu/$needed" ] \
            && [ ! -e "$tree/usr/lib64/$needed" ] && [ ! -e "$unix/$needed" ]; then
            echo "fetch-wine: ${file#"$tree"/} needs $needed, which is not on the volume" >&2
            missing=1
        fi
    done
done
[ "$missing" = 0 ] || exit 1

# Room for a prefix on the volume and logs. wineboot's prefix is 768 MB of
# files (du), and took 1.1 GB of the volume on Ferrix: small files, two
# copies of the metadata. With 1024 MiB more than the tree the volume had
# 683 MB free and wineboot ran out at 684 MB of prefix (2026-10-12); with
# 2048 it made the prefix and left 627 MB.
size=$(( $(du -sm "$tree" | cut -f1) + 2048 ))
image="$out/wine.img"
rm -f "$out/yserver.commit" "$image"
truncate -s "${size}M" "$image"
mkfs.btrfs -q --rootdir "$tree" "$image"
# The yserver the volume carries, which cargo xtask test-wine compares with
# the pin, as test-yserver does with yserver's own volume.
cp "$yserver/yserver.commit" "$out/yserver.commit"
echo "wine volume: $image (${size} MiB), unpacked in $tree"
