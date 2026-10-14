#!/usr/bin/env bash
# Fetch iPXE's source for `cargo xtask netboot-stick` and `test-netboot`
# (docs/NETBOOT.md).
#
# iPXE is its release tarball from GitHub, tag v2.0.0 (commit 12798ec29aa8),
# pinned by SHA-256 (taken 2026-10-14). Only the tarball is kept: xtask
# checks it against the same digest, unpacks it into its own target
# directory and builds `bin-x86_64-efi/snp.efi` there with the stick's
# script embedded, so nothing is ever built in this directory.
#
# Writes $FERRIX_NETBOOT_DIR (default ~/.local/share/ferrix/netboot):
# pool/ipxe-2.0.0.tar.gz.
# Needs curl and sha256sum; no root. Building it later needs make, gcc and
# perl.
#
# Usage: tools/common/fetch/fetch-ipxe.sh

set -euo pipefail

IPXE_VERSION=2.0.0
IPXE_SHA256=9ed6d029be901a0ccc87cb2e5f9c774620f30f84ebdd507c6dd3e1e6229b7bd5
url=https://github.com/ipxe/ipxe/archive/refs/tags/v$IPXE_VERSION.tar.gz

out=${FERRIX_NETBOOT_DIR:-$HOME/.local/share/ferrix/netboot}

for tool in curl sha256sum; do
    command -v "$tool" > /dev/null || { echo "fetch-ipxe: $tool is not installed" >&2; exit 1; }
done

mkdir -p "$out/pool"
tarball="$out/pool/ipxe-$IPXE_VERSION.tar.gz"
if [ ! -f "$tarball" ]; then
    curl -fsSL -o "$tarball.part" "$url"
    mv "$tarball.part" "$tarball"
fi
echo "$IPXE_SHA256  $tarball" | sha256sum -c --quiet \
    || { echo "fetch-ipxe: $tarball does not match its pinned checksum" >&2; rm -f "$tarball"; exit 1; }
echo "ipxe: $tarball (iPXE $IPXE_VERSION)"
