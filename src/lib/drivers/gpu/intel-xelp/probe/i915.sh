#!/usr/bin/env bash
# Print the i915 uAPI's numbers and layouts from Linux v7.0's UAPI headers
# (docs/INTEL-GPU.md §2.1, Appendix A).
#
# Needs gcc, the system's linux/types.h (linux-libc-dev), and the v7.0 tag's
# include/uapi: by default the sparse checkout docs/INTEL-GPU.md §10 names,
# ~/.local/share/ferrix/intel-gpu-ref/linux, or the tree given as $1. Writes
# i915-64.txt beside this script, which the crate's tests read. Record the
# tree's commit in the commit that changes the file.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
linux="${1:-$HOME/.local/share/ferrix/intel-gpu-ref/linux}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# The tree's drm/ and linux/ headers ahead of the system's, so every number
# is v7.0's even where the host's headers are older.
gcc -O0 -Wall -Werror -D__user= -I"$linux/include/uapi" -o "$work/i915-64" "$here/i915.c"
"$work/i915-64" > "$here/i915-64.txt"
git -C "$linux" log -1 --format='%h %s' 2>/dev/null || true
