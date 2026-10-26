#!/usr/bin/env bash
# Fetch Linux's own seccomp selftest and build it, static against musl, for
# `cargo xtask test-seccomp-selftest` (docs/SECCOMP.md §8.2, S6's second
# part).
#
# `tools/testing/selftests/seccomp/seccomp_bpf.c` and the three headers it
# includes (kselftest's harness, kselftest.h, clone3's), from Linux v6.12,
# each pinned by its SHA-256. They are taken from GitHub's raw files of the
# torvalds/linux tag, the tree kernel.org's tarball holds.
#
# Built with `zig cc` (Zig 0.13.0 from PyPI's `ziglang`, in a virtual
# environment of its own), which carries musl for every target, so one host
# builds all three: x86_64-linux-musl, aarch64-linux-musl and
# arm-linux-musleabihf. tools/common/data/seccomp-selftest/ holds what the
# build adds: prelude.h (musl's answers to the test's glibc and <linux/prctl.h>
# assumptions), sys/capability.h (the four libcap calls the test makes, over
# capget and capset, since no libcap is linked), and init.c, which includes
# the test with no arguments, since Ferrix starts a built-in init as `sh -c`.
#
# Writes $FERRIX_SECCOMP_SELFTEST/<arch>/seccomp_bpf (default
# ~/.local/share/ferrix/seccomp-selftest), where test-seccomp-selftest looks.
#
# Needs curl, sha256sum and python3 with venv.
#
# Usage: tools/common/fetch/fetch-seccomp-selftest.sh

set -euo pipefail

tag=v6.12
base=https://raw.githubusercontent.com/torvalds/linux/$tag/tools/testing/selftests
here=$(cd "$(dirname "$0")/../../.." && pwd)
shim=$here/tools/common/data/seccomp-selftest
out=${FERRIX_SECCOMP_SELFTEST:-$HOME/.local/share/ferrix/seccomp-selftest}

files=(
    "seccomp/seccomp_bpf.c 88d96b85f5d920904d74a9f0ca9846460cc6b9f650f56d30dfb6cf483fa694d3"
    "kselftest_harness.h 38ac12ca1ff9cbcd57c19ae2e4230f426ae4b1ae00cf4713de578dfde4ed6e23"
    "kselftest.h 5d5720da5593490c7260d867904531b48230a4868798dcb4b34df8039a7be609"
    "clone3/clone3_selftests.h 0d2c0e44d826b62645818d44b3515d273fa11729e2d8f76e665c759a4973c4ae"
)

for tool in curl sha256sum python3; do
    command -v "$tool" > /dev/null || { echo "fetch-seccomp-selftest: $tool is not installed" >&2; exit 1; }
done

src=$out/src
mkdir -p "$src/seccomp" "$src/clone3"
for entry in "${files[@]}"; do
    read -r path sum <<< "$entry"
    if [ ! -f "$src/$path" ] || ! echo "$sum  $src/$path" | sha256sum -c --quiet 2> /dev/null; then
        curl -fsSL -o "$src/$path.part" "$base/$path"
        mv "$src/$path.part" "$src/$path"
    fi
    echo "$sum  $src/$path" | sha256sum -c --quiet \
        || { echo "fetch-seccomp-selftest: $path does not match its pinned checksum" >&2; rm -f "$src/$path"; exit 1; }
done

zig=$out/zig
if [ ! -x "$zig/bin/python" ] || ! "$zig/bin/python" -m ziglang version 2> /dev/null | grep -qx 0.13.0; then
    python3 -m venv "$zig"
    "$zig/bin/pip" install --quiet ziglang==0.13.0
fi

for pair in "x86_64 x86_64-linux-musl" "aarch64 aarch64-linux-musl" "armv7a arm-linux-musleabihf"; do
    read -r arch target <<< "$pair"
    mkdir -p "$out/$arch"
    "$zig/bin/python" -m ziglang cc -target "$target" -static -O2 -D_GNU_SOURCE \
        -include "$shim/prelude.h" -I"$shim" -I"$src" \
        -o "$out/$arch/seccomp_bpf" "$shim/init.c" -lpthread
    echo "fetch-seccomp-selftest: $out/$arch/seccomp_bpf"
done
