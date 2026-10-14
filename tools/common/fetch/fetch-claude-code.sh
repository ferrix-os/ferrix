#!/usr/bin/env bash
# Fetch Anthropic's prebuilt Claude Code and the Debian programs it runs,
# and put them on a btrfs volume: the disk `cargo xtask test-claude-code`
# runs Claude Code from (docs/CLAUDE-CODE.md).
#
# Nothing here is built. `claude` is Claude Code's native release, the one
# https://claude.ai/install.sh installs: a 234 MB glibc program, a
# JavaScript runtime with Claude Code bundled into it, which needs nothing
# of the system's but glibc itself -- libc, libm, libpthread, libdl and
# librt. The glibc and loader are Debian 13's, the same pin
# tools/common/fetch/fetch-chrome.sh takes. Beside them are bash, which
# Claude Code's Bash tool runs every command in and will not do without,
# and ripgrep, which its Grep and Glob tools search with when it is told to
# use the system's.
#
# Two machines: linux-x64 (the default) and linux-arm64, for an AArch64
# Ferrix. Each has a volume of its own, from the same release and the same
# Debian packages built for it.
#
# The volume holds a Debian-shaped tree at its root -- `usr/lib64` and
# `usr/lib/x86_64-linux-gnu`, or `usr/lib/aarch64-linux-gnu`, and
# `usr/bin` -- and Claude Code at `claude-code/claude`. Ferrix mounts it at
# `/data`, and the test's image links the directories glibc names into it,
# as test-chrome's does.
#
# Every download is pinned by its SHA-256: Debian's from trixie's Packages
# file (amd64 on 2026-09-30, arm64 on 2026-10-14), and Claude Code's from
# its release's manifest.json, which publishes one for each platform. After
# unpacking, every library each ELF file on the volume needs is looked for
# on it, so a dependency this list lacks fails here rather than on Ferrix.
#
# Writes $FERRIX_CLAUDE_CODE_VOLUME/claude-code.img (default
# ~/.local/share/ferrix/claude-code/claude-code.img, or
# ~/.local/share/ferrix/claude-code-arm64/claude-code.img for arm64). Needs
# curl, sha256sum, dpkg-deb, readelf and mkfs.btrfs; no root. Nothing it
# downloads is run.
#
# Usage: tools/common/fetch/fetch-claude-code.sh [x64|arm64]

set -euo pipefail

machine=${1:-x64}
debian=${DEBIAN_MIRROR:-https://deb.debian.org/debian}
releases=${CLAUDE_CODE_RELEASES:-https://downloads.claude.ai/claude-code-releases}

# The stable channel's release on 2026-09-30, and each binary's checksum
# from that release's manifest.json.
CLAUDE_CODE_VERSION=2.1.280

# Pool path and SHA-256 of each Debian 13 package, the loader's path in
# the tree, and glibc's library directory.
case $machine in
    x64)
        out=${FERRIX_CLAUDE_CODE_VOLUME:-$HOME/.local/share/ferrix/claude-code}
        CLAUDE_CODE_SHA256=1e08503dbdf3c2cb0d706d32f3408277388d1c76ef108673e8fe42c1b322925b
        loader=usr/lib64/ld-linux-x86-64.so.2
        libdirs=(usr/lib/x86_64-linux-gnu usr/lib64)
        debs=(
            "main/g/glibc/libc6_2.41-12+deb13u4_amd64.deb 967aa62605721081c3eb2a17650611a792aa802d76a6511d1840242623d204c9"
            "main/g/gcc-14/libgcc-s1_14.2.0-19_amd64.deb 3c71917b490d1a17aed43196a2787a256ecf060526cdb20216a74bedc061b150"
            "main/b/bash/bash_5.2.37-2+b10_amd64.deb 2fd7b04f1b7caa29c4e683f5216e6af354a41e3cdcf6e75394d7cda680f9ab82"
            "main/n/ncurses/libtinfo6_6.5+20250216-2_amd64.deb 8b9f6a7983e9418564e48a627518de4c03917b56efe68d7f3e93bd8fffa1cc10"
            "main/r/rust-ripgrep/ripgrep_14.1.1-1+b4_amd64.deb 7e0c32510c264c31335fe3b9ae37ab76dcd22f7d1627a0a08518a5bf28b17ac2"
            "main/p/pcre2/libpcre2-8-0_10.46-1~deb13u2_amd64.deb 1252b96a5bc44bb5db982bef8eb18e54f5047cede2aff641bce4f8e1edb91c3e"
        )
        ;;
    arm64)
        out=${FERRIX_CLAUDE_CODE_VOLUME:-$HOME/.local/share/ferrix/claude-code-arm64}
        CLAUDE_CODE_SHA256=92f2b4fd05d0bdcf7b9a0d4e0ecef4a1e4b368b290cd8fd07cff9a50013f45a2
        loader=usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1
        libdirs=(usr/lib/aarch64-linux-gnu)
        debs=(
            "main/g/glibc/libc6_2.41-12+deb13u4_arm64.deb 8784eda966b189c777a384dac5ce009e8fc9b52d006926c5a013e7fa8aa688cc"
            "main/g/gcc-14/libgcc-s1_14.2.0-19_arm64.deb 1108bc87879833d6d9a145f22a4a15cddb34e065b4b5f4b97bee586adbac2851"
            "main/b/bash/bash_5.2.37-2+b10_arm64.deb 9e2d2b3646b250f53be79b6128d9cff5e99cc5ff4a8c84e58efab59ad9bb0ddc"
            "main/n/ncurses/libtinfo6_6.5+20250216-2_arm64.deb 2a747f7fff3c2347c357387a7133df997410f07ba1b1df1a1461a97bd7fbc2fc"
            "main/r/rust-ripgrep/ripgrep_14.1.1-1+b4_arm64.deb 9b4f347ba2320b22cba217cc4b078536596cc6a1409df001fc560ad3fa396609"
            "main/p/pcre2/libpcre2-8-0_10.46-1~deb13u2_arm64.deb e7d2c997dac145c16457be0fed3d084c98cd030bec7633eec7e5bde6dbb97712"
        )
        ;;
    *)
        echo "fetch-claude-code: the machine is x64 or arm64, not $machine" >&2
        exit 1
        ;;
esac

for tool in curl sha256sum dpkg-deb readelf mkfs.btrfs; do
    command -v "$tool" > /dev/null || { echo "fetch-claude-code: $tool is not installed" >&2; exit 1; }
done

# Download $2/$1 into the pool as $4 (default: $1's last component) unless
# it is there, and check it against $3.
fetch() {
    local path=$1 base=$2 sum=$3 name=${4:-${1##*/}}
    local file="$out/pool/$name"
    if [ ! -f "$file" ]; then
        curl -fsSL -o "$file.part" "$base/$path"
        mv "$file.part" "$file"
    fi
    echo "$sum  $file" | sha256sum -c --quiet \
        || { echo "fetch-claude-code: $file does not match its pinned checksum" >&2; rm -f "$file"; exit 1; }
    printf '%s\n' "$file"
}

mkdir -p "$out/pool"
tree="$out/tree"
rm -rf "$tree"
mkdir -p "$tree"

for entry in "${debs[@]}"; do
    read -r path sum <<< "$entry"
    dpkg-deb -x "$(fetch "$path" "$debian/pool" "$sum")" "$tree"
done

binary=$(fetch "$CLAUDE_CODE_VERSION/linux-$machine/claude" "$releases" "$CLAUDE_CODE_SHA256" \
    "claude-$CLAUDE_CODE_VERSION-linux-$machine")
mkdir -p "$tree/claude-code"
install -m 755 "$binary" "$tree/claude-code/claude"

# Documentation and manuals, which nothing runs.
rm -rf "$tree/usr/share/doc" "$tree/usr/share/man" "$tree/usr/share/lintian"

test -x "$tree/claude-code/claude" \
    || { echo "fetch-claude-code: no claude in the tree" >&2; exit 1; }
test -x "$tree/usr/bin/bash" \
    || { echo "fetch-claude-code: no bash in the tree" >&2; exit 1; }
test -e "$tree/$loader" \
    || { echo "fetch-claude-code: no linker at $loader" >&2; exit 1; }

# Every library an ELF file on the volume needs must be on it.
missing=0
while IFS= read -r -d '' file; do
    head -c 4 "$file" | grep -q $'\x7fELF' || continue
    for needed in $(readelf -d "$file" 2> /dev/null | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
        found=0
        for dir in "${libdirs[@]}"; do
            [ -e "$tree/$dir/$needed" ] && found=1
        done
        if [ "$found" = 0 ]; then
            echo "fetch-claude-code: ${file#"$tree"/} needs $needed, which is not on the volume" >&2
            missing=1
        fi
    done
done < <(find "$tree/claude-code" "$tree/usr/bin" "$tree/${libdirs[0]}" \
    -maxdepth 1 -type f -print0)
[ "$missing" = 0 ] || exit 1

# Room for Claude Code's configuration and what its tools write, and a size
# that does not depend on how mkfs rounds.
size=$(( $(du -sm "$tree" | cut -f1) + 512 ))
image="$out/claude-code.img"
rm -f "$image"
truncate -s "${size}M" "$image"
mkfs.btrfs -q --rootdir "$tree" "$image"
# The tree stays beside the image, so the same files can be tried on the host.
echo "claude-code volume ($machine): $image (${size} MiB), unpacked in $tree"
