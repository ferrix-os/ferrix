#!/usr/bin/env bash
# Fetch the two shell trees an image carries beside zinc, pinned, for a
# build that has to be the same on every machine -- a release above all:
#
#   * oh-my-zsh at commit 964ddf78, as GitHub's archive of that commit;
#   * zsh 5.9.2's function and completion tree, from zsh.org's tarball
#     (whose MD5 zsh.org's MD5SUM lists), laid out as an installed
#     /usr/share/zsh/functions is: Functions' directories, and Completion.
#
# `cargo xtask omz` and `zsh-functions` install whatever a person points
# them at, which is right for a desktop of their own and wrong for one that
# leaves this machine: what tree a release carries would be whatever the
# build machine had. This writes both under <dir> and prints the two
# variables xtask reads them from (crate::omz):
#
#     eval "$(tools/common/fetch/fetch-shell-trees.sh <dir>)"
#
# Downloads are kept in <dir>/src and refused if their checksums differ.
# tools/common/release/pinned.py reads the pins below for the release's
# licence inventory and sources.

set -euo pipefail

dir=${1:?usage: fetch-shell-trees.sh <dir>}
mkdir -p "$dir/src"
dir=$(cd "$dir" && pwd)
src=$dir/src

OMZ_COMMIT=964ddf789b022b6958b27d02d7fa54f552de21e9
OMZ_TARBALL=ohmyzsh-$OMZ_COMMIT.tar.gz
OMZ_URL=https://github.com/ohmyzsh/ohmyzsh/archive/$OMZ_COMMIT.tar.gz
OMZ_SHA256=89ccb57b3328f0df303d557bef54cc7d0efb6f22f016552943ef3e15273e0588
ZSH_VERSION=5.9.2
ZSH_TARBALL=zsh-$ZSH_VERSION.tar.xz
ZSH_URL=https://www.zsh.org/pub/$ZSH_TARBALL
ZSH_SHA256=36fa734374b44783582cec09bcd67822e2f992c779ec1624ab5596df078d2f81

fetch() { # url file checksum-command checksum
    local url=$1 file=$2 tool=$3 sum=$4
    if [ ! -f "$file" ]; then
        curl -fsSL --max-time 300 -o "$file.part" "$url"
        mv "$file.part" "$file"
    fi
    local got
    got=$("$tool" "$file" | cut -d' ' -f1)
    if [ "$got" != "$sum" ]; then
        echo "${0##*/}: $file does not match its pinned $tool" >&2
        echo "  expected $sum" >&2
        echo "  got      $got" >&2
        exit 1
    fi
}

fetch "$OMZ_URL" "$src/$OMZ_TARBALL" sha256sum "$OMZ_SHA256"
fetch "$ZSH_URL" "$src/$ZSH_TARBALL" sha256sum "$ZSH_SHA256"

rm -rf "$dir/oh-my-zsh" "$dir/zsh-functions" "$dir/zsh-$ZSH_VERSION"
mkdir -p "$dir/oh-my-zsh" "$dir/zsh-functions"
tar -xzf "$src/$OMZ_TARBALL" -C "$dir/oh-my-zsh" --strip-components=1
tar -xJf "$src/$ZSH_TARBALL" -C "$dir"
cp -R "$dir/zsh-$ZSH_VERSION/Functions/." "$dir/zsh-functions/"
cp -R "$dir/zsh-$ZSH_VERSION/Completion" "$dir/zsh-functions/Completion"
rm -rf "$dir/zsh-$ZSH_VERSION"
[ -f "$dir/zsh-functions/Misc/is-at-least" ] || { echo "${0##*/}: zsh $ZSH_VERSION has no Functions/Misc/is-at-least" >&2; exit 1; }
echo "export FERRIX_OMZ='$dir/oh-my-zsh' FERRIX_ZSH_FUNCTIONS='$dir/zsh-functions'"
