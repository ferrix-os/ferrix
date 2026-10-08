#!/bin/bash
# Gather the card's files from the build host into one local stage
# directory laid out as bootfs gains them, with a SHA256SUMS over every file
# (stage-card.ps1 checks each copy against it).
#
#   gather.sh <sources file> <stage dir> [host]
#
# The sources file has one line per directory to copy:
#   <path on bootfs>  <directory on the host>
# e.g.
#   bench/linux        ~/.local/share/ferrix/board-bench/linux/out/card/bench/linux
#   bench/ferrix/main  ~/.local/share/ferrix/board-bench/ferrix/main/FERRIX
#   EFI/BOOT           ~/.local/share/ferrix/board-bench/ferrix/main/EFI/BOOT
# `#` starts a comment. Each host directory must carry its own SHA256SUMS
# (names relative to it); a file that does not match it after the copy stops
# the gather. The stage directory must not exist yet.
set -euo pipefail

sources=${1:?sources file}
stage=${2:?stage dir}
host=${3:-nazuna-wg}

[ ! -e "$stage" ] || { echo "gather: $stage exists" >&2; exit 1; }
mkdir -p "$stage"

while read -r card dir; do
    case $card in '' | '#'*) continue ;; esac
    mkdir -p "$stage/$card"
    echo "== $card <- $host:$dir"
    # One tar stream per directory: names and bytes as the host has them.
    ssh "$host" "cd $dir && test -f SHA256SUMS && tar -cf - ." </dev/null | tar -xf - -C "$stage/$card"
    (cd "$stage/$card" && sha256sum -c --quiet SHA256SUMS) || {
        echo "gather: $card does not match its SHA256SUMS" >&2
        exit 1
    }
    rm "$stage/$card/SHA256SUMS"
done <"$sources"

(cd "$stage" && find . -type f ! -name SHA256SUMS | sed 's|^\./||' | sort | while read -r f; do
    sha256sum "$f"
done) >"$stage/SHA256SUMS"
cp "$sources" "$stage/../$(basename "$stage").sources"
echo "staged $(wc -l <"$stage/SHA256SUMS") files, $(du -sh "$stage" | cut -f1), in $stage"
