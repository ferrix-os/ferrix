#!/bin/bash
# build.sh <arch> <out>: B5's binaries and run scripts, checked against their
# SHA256SUMS, and posix-init.sh beside them. MEASUREMENT ONLY.
set -eu
SRC=${POSIXBENCH_SRC:-$HOME/.local/share/ferrix/board-bench/posix}
out=$2
(cd "$SRC" && sha256sum --quiet -c SHA256SUMS)
rm -rf "$out/bundle"
mkdir -p "$out/bundle"
cp -a "$SRC/bin" "$SRC/run-lmbench.sh" "$SRC/run-speedtest1.sh" "$out/bundle/"
cp "$(dirname "$0")/posix-init.sh" "$out/bundle/posix-init.sh"
chmod 755 "$out/bundle/posix-init.sh"
