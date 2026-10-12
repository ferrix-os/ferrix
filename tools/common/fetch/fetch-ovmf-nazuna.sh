#!/usr/bin/env bash
# Build an x86-64 OVMF for QEMU's q35 machine whose firmware-programmed
# platform addresses match nazuna (ASUS ProArt B650-CREATOR, AMD AM5), and
# install it beside -- never over -- the OVMF other boots use.
#
# Why: a kernel booted under QEMU should find the platform where it finds
# it on the real PC, so the guest and the board exercise the same paths.
# On nazuna the PCI Express ECAM is at 0xE0000000 (256 MiB, buses 0-255)
# inside the 32-bit PCI MMIO window 0x80000000-0xF6FFFFFF, low RAM ends at
# 0x80000000, and the FADT puts PM1a_EVT at 0x800, PM1a_CNT at 0x804,
# PM_TMR at 0x808 and GPE0 at 0x820.
#
# What differs from stock EDK2 OVMF (OvmfPkg/OvmfPkgX64.dsc, X64,
# -D FD_SIZE_4MB, no Secure Boot, no SMM -- like Ubuntu's OVMF_CODE_4M.fd):
#   - ICH9 LPC PMBASE 0x600 -> 0x800 (ICH9_PMBASE_VALUE in
#     OvmfPkg/Include/OvmfPlatforms.h). It is the one constant every user
#     derives from: PlatformInitLib writes it to LPC PMBASE, AcpiTimerLib
#     reads the PM timer at +8, ResetSystemLib shuts down through +4.
#     QEMU's FADT follows the PMBASE the firmware programs, so the guest
#     sees 0x800/0x804/0x808/0x820. 0x800 stays below the q35 PCI I/O
#     aperture (0x6000-0xFFFF) that PlatformInitLib asserts against.
#   - None for the ECAM: stock OVMF already programs MCH PCIEXBAR from
#     PcdPciExpressBaseAddress = 0xE0000000 and sizes the 32-bit PCI
#     aperture as [top of low RAM, 0xE0000000), so it never overlaps the
#     ECAM. (0xB0000000 is QEMU's and SeaBIOS's default, not OVMF's.) The
#     script checks that value is still there, so a later tag that moves
#     it fails here instead of producing a different platform.
#
# Writes $FERRIX_OVMF_NAZUNA_DIR (default ~/.local/share/ferrix/ovmf-nazuna):
# OVMF_CODE.fd and OVMF_VARS.fd (the empty variable store template), swapped
# in only once the new build is complete. Source and build are in
# $FERRIX_OVMF_NAZUNA_WORK (default ~/ovmf-nazuna-build). Never uses sudo:
# when nasm or iasl is not on PATH, it builds the pinned release below from
# source into $FERRIX_TOOLS_DIR (default ~/.local/share/ferrix/tools) and
# puts its bin/ first on PATH for the EDK2 build.
#
# Debian/Ubuntu build dependencies (nasm and acpica-tools optional, see above):
#   sudo apt-get install build-essential flex bison uuid-dev python3 git \
#     nasm acpica-tools
#
# Usage: [FERRIX_OVMF_JOBS=n] tools/common/fetch/fetch-ovmf-nazuna.sh

set -euo pipefail

tag=edk2-stable202608
commit=2970e5699ba6267f3384ffab20f96647578aebc8
url=https://github.com/tianocore/edk2
pmbase=0x0800
ecam=0xE0000000

nasm_version=2.16.03
nasm_sha256=1412a1c760bbd05db026b6c0d1657affd6631cd0a63cddb6f73cc6d4aa616148
nasm_url=https://www.nasm.us/pub/nasm/releasebuilds/$nasm_version/nasm-$nasm_version.tar.xz
acpica_version=20260408
acpica_sha256=ddc5d3e0f54030e2348484fff681861a161efb4e388e20631209574e7884ad39
acpica_url=https://github.com/acpica/acpica/archive/refs/tags/$acpica_version.tar.gz

out=${FERRIX_OVMF_NAZUNA_DIR:-$HOME/.local/share/ferrix/ovmf-nazuna}
work=${FERRIX_OVMF_NAZUNA_WORK:-$HOME/ovmf-nazuna-build}
tools=${FERRIX_TOOLS_DIR:-$HOME/.local/share/ferrix/tools}
jobs=${FERRIX_OVMF_JOBS:-8}

missing=
for tool in gcc g++ make flex bison python3 git curl; do
    command -v "$tool" >/dev/null || missing="$missing $tool"
done
[ -f /usr/include/uuid/uuid.h ] || missing="$missing uuid-dev"
if [ -n "$missing" ]; then
    echo "ovmf: missing build dependencies:$missing" >&2
    echo "ovmf:   sudo apt-get install build-essential flex bison uuid-dev python3 git curl" >&2
    exit 1
fi
mkdir -p "$work"

# fetch <name> <url> <sha256>: the tarball in a directory of its own,
# checked against the pinned sha256, unpacked fresh into $work/<name>-src.
fetch() {
    local name=$1 from=$2 sum=$3
    local dl=$work/download-$name
    local tarball=$dl/${from##*/}
    mkdir -p "$dl"
    if [ ! -f "$tarball" ] || ! echo "$sum  $tarball" | sha256sum -c --status; then
        echo "ovmf: fetching $from"
        curl -fL --retry 3 -o "$tarball.part" "$from"
        mv "$tarball.part" "$tarball"
    fi
    if ! echo "$sum  $tarball" | sha256sum -c --status; then
        echo "ovmf: $tarball does not have the pinned sha256 $sum" >&2
        exit 1
    fi
    rm -rf "$work/$name-src"
    mkdir -p "$work/$name-src"
    tar -xf "$tarball" -C "$work/$name-src" --strip-components=1
}

export PATH=$tools/bin:$PATH
if ! command -v nasm >/dev/null; then
    fetch nasm "$nasm_url" "$nasm_sha256"
    (
        cd "$work/nasm-src"
        { ./configure --prefix="$tools" && make -j "$jobs" && make install; } \
            >"$work/nasm-build.log" 2>&1 ||
            { tail -n 30 "$work/nasm-build.log" >&2; exit 1; }
    )
    case $("$tools/bin/nasm" -v 2>&1 || true) in *"version $nasm_version "*) ;; *) false ;; esac || {
        echo "ovmf: $tools/bin/nasm is not version $nasm_version" >&2
        exit 1
    }
fi
if ! command -v iasl >/dev/null; then
    fetch acpica "$acpica_url" "$acpica_sha256"
    make -C "$work/acpica-src" -j "$jobs" iasl >"$work/acpica-build.log" 2>&1 ||
        { tail -n 30 "$work/acpica-build.log" >&2; exit 1; }
    mkdir -p "$tools/bin"
    cp "$work/acpica-src/generate/unix/bin/iasl" "$tools/bin/iasl.new"
    mv "$tools/bin/iasl.new" "$tools/bin/iasl"
    case $("$tools/bin/iasl" -v 2>&1 || true) in *"version $acpica_version"*) ;; *) false ;; esac || {
        echo "ovmf: $tools/bin/iasl is not version $acpica_version" >&2
        exit 1
    }
fi
echo "ovmf: $(command -v nasm): $(nasm -v)"
echo "ovmf: $(command -v iasl): $(iasl -v 2>&1 | grep -o 'version [0-9]*' || true)"

# A fresh tree every time, so the changes apply to exactly the tag.
mkdir -p "$work"
src=$work/edk2
rm -rf "$src"
git -c advice.detachedHead=false clone -q --depth 1 --branch "$tag" \
    --recurse-submodules --shallow-submodules "$url" "$src"
got=$(git -C "$src" rev-parse HEAD)
if [ "$got" != "$commit" ]; then
    echo "ovmf: $tag is $got, not the pinned $commit" >&2
    exit 1
fi

# Each change must have taken. EDK2's sources have CRLF line endings,
# which the edit keeps.
cr=$(printf '\r')
platforms=$src/OvmfPkg/Include/OvmfPlatforms.h
sed -i "s/^#define ICH9_PMBASE_VALUE  0x0600\\($cr\\?\\)\$/#define ICH9_PMBASE_VALUE  $pmbase\\1/" "$platforms"
grep -q "^#define ICH9_PMBASE_VALUE  $pmbase$cr\\?\$" "$platforms" || {
    echo "ovmf: ICH9_PMBASE_VALUE was not changed to $pmbase in $platforms" >&2
    exit 1
}
echo "ovmf: ICH9_PMBASE_VALUE = $pmbase"
dsc=$src/OvmfPkg/OvmfPkgX64.dsc
grep -q "^  gEfiMdePkgTokenSpaceGuid.PcdPciExpressBaseAddress|$ecam$cr\\?\$" "$dsc" || {
    echo "ovmf: $dsc no longer sets PcdPciExpressBaseAddress to $ecam" >&2
    exit 1
}
echo "ovmf: PcdPciExpressBaseAddress = $ecam (stock)"

# edksetup.sh reads unset variables.
(
    cd "$src"
    set +u
    export WORKSPACE=$src
    . ./edksetup.sh >/dev/null
    make -C BaseTools -j "$jobs" >"$work/basetools.log" 2>&1 ||
        { tail -n 30 "$work/basetools.log" >&2; exit 1; }
    build -a X64 -t GCC -b RELEASE -p OvmfPkg/OvmfPkgX64.dsc \
        -D FD_SIZE_4MB -n "$jobs" >"$work/build.log" 2>&1 ||
        { tail -n 30 "$work/build.log" >&2; exit 1; }
)
fv=$src/Build/OvmfX64/RELEASE_GCC/FV
for f in OVMF_CODE.fd OVMF_VARS.fd; do
    [ -s "$fv/$f" ] || { echo "ovmf: the build made no $fv/$f" >&2; exit 1; }
done

# Stage beside the old install and swap, so a QEMU another session is
# running from $out never reads a half-written image.
stage=$out.stage
rm -rf "$stage"
mkdir -p "$stage"
cp "$fv/OVMF_CODE.fd" "$fv/OVMF_VARS.fd" "$stage/"
echo "$tag $commit ICH9_PMBASE_VALUE=$pmbase PcdPciExpressBaseAddress=$ecam" >"$stage/SOURCE"
rm -rf "$out.old"
if [ -e "$out" ]; then mv "$out" "$out.old"; fi
mv "$stage" "$out"
rm -rf "$out.old"

( cd "$out" && sha256sum OVMF_CODE.fd OVMF_VARS.fd )
echo "ovmf: $out"
