#!/bin/bash
# Linux 7.2.9 for the STM32MP157D-DK1, and Linux's side of the board IPC
# bench (docs/BOARD-BENCH.md, item B1).
#
#   build.sh [step ...]
#
# Steps, in order (no argument runs fetch to collect):
#   fetch    download linux-$VER.tar.xz, check it against kernel.org's
#            signed sha256sums.asc and the tarball's own signature
#   config   unpack, then multi_v7_defconfig, unmodified
#   kernel   zImage, the device trees and modules_prepare; not the defconfig's
#            own modules, which the bench does not load (rerun to continue a
#            stopped make)
#   module   pmu-user.ko, out of tree against this build
#   lbench   the static bench, /init of the initramfs, built -marm
#   cpio     initramfs.cpio.gz: /init and /pmu-user.ko
#   collect  the card's files into $OUT/out/card/bench/linux, and the record
#            (versions, hashes, sizes, the CPU clock points) into $OUT/out
#   clean    remove the unpacked source and every build directory, keeping
#            dl/ and out/ (run it only after collect); gen_init_cpio is kept
#            in tools/ for repack
#   repack   after clean: lbench and the initramfs again, the kernel and
#            device tree as built; the card's initramfs and out/ updated,
#            and a line for it appended to versions.txt
#
# Everything is written under $BB_LINUX_OUT (default
# ~/.local/share/ferrix/board-bench/linux); nothing is written next to this
# script. Needs curl, gpg, xz, gzip, dtc, fdtget, the kernel's host build
# tools (make, flex, bison, bc, ...) and arm-linux-gnueabihf-gcc (15.2 was
# used). JOBS sets make's parallelism (default 8).
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
common=$(cd "$here/../common" && pwd)

VER=7.2.9
URL=https://cdn.kernel.org/pub/linux/kernel/v${VER%%.*}.x
# The keys kernel.org signs with (https://www.kernel.org/signature.html): the
# checksum autosigner for sha256sums.asc, Greg Kroah-Hartman for stable tars.
FPR_AUTOSIGNER=B8868C80BA62A1FFFAF5FDA9632D3A06589DA6B1
FPR_GREGKH=647F28654894E3BD457199BE38DBBDC86092693E

OUT=${BB_LINUX_OUT:-$HOME/.local/share/ferrix/board-bench/linux}
JOBS=${JOBS:-8}
CROSS=arm-linux-gnueabihf-
# ARM, not gcc's default Thumb-2, until Ferrix's user mode is known (B3).
CFLAGS=(-O2 -static -marm -mcpu=cortex-a7 -Wall -Wextra)
# Mainline has no stm32mp157d-dk1.dts. The D-DK1 is the A-DK1 with the 800 MHz
# part fused in; the firmware (TF-A, OP-TEE, U-Boot) is the a-dk1 build, and
# OP-TEE leaves the RCC non-secure, which is what this (non-SCMI) tree expects.
DTB=st/stm32mp157a-dk1.dtb
# Where the card's files go on its bootfs partition (boot-linux.cmd).
CARD=bench/linux

SRC=$OUT/src/linux-$VER
BUILD=$OUT/build
KMAKE=(make -C "$SRC" O="$BUILD" ARCH=arm CROSS_COMPILE="$CROSS" -j"$JOBS")

die() {
    echo "build.sh: $*" >&2
    exit 1
}

fetch() {
    mkdir -p "$OUT/dl" "$OUT/out"
    cd "$OUT/dl"
    for f in sha256sums.asc "linux-$VER.tar.xz" "linux-$VER.tar.sign"; do
        if [ ! -s "$f" ]; then
            curl -sSfL -o "$f.part" "$URL/$f"
            mv "$f.part" "$f"
        fi
    done

    # A keyring of our own, so the user's is never touched.
    export GNUPGHOME=$OUT/gnupg
    mkdir -p "$GNUPGHOME"
    chmod 700 "$GNUPGHOME"
    if ! gpg --batch --list-keys "$FPR_AUTOSIGNER" "$FPR_GREGKH" >/dev/null 2>&1; then
        gpg --batch --quiet --auto-key-locate clear,nodefault,wkd \
            --locate-keys autosigner@kernel.org gregkh@kernel.org >/dev/null
    fi

    # sha256sums.asc: a valid signature by the autosigner, and only the signed
    # text is used.
    local st
    st=$(gpg --batch --status-fd 1 --output sha256sums.txt --yes --decrypt sha256sums.asc 2>/dev/null) ||
        die "sha256sums.asc: signature check failed"
    grep -q "^\[GNUPG:\] VALIDSIG .* $FPR_AUTOSIGNER\$" <<<"$st" ||
        die "sha256sums.asc is not signed by $FPR_AUTOSIGNER"
    local want got
    want=$(awk -v f="linux-$VER.tar.xz" '$2 == f { print $1 }' sha256sums.txt)
    [ -n "$want" ] || die "sha256sums.asc has no line for linux-$VER.tar.xz"
    got=$(sha256sum "linux-$VER.tar.xz" | cut -d' ' -f1)
    [ "$want" = "$got" ] || die "linux-$VER.tar.xz: sha256 $got, kernel.org says $want"

    # The tarball's own signature is over the uncompressed tar.
    st=$(xz -dc "linux-$VER.tar.xz" | gpg --batch --status-fd 1 --verify "linux-$VER.tar.sign" - 2>/dev/null) ||
        die "linux-$VER.tar.sign: signature check failed"
    grep -q "^\[GNUPG:\] VALIDSIG .* $FPR_GREGKH\$" <<<"$st" ||
        die "linux-$VER.tar is not signed by $FPR_GREGKH"

    {
        echo "linux-$VER.tar.xz sha256 $got"
        echo "  matches $URL/sha256sums.asc, signed by $FPR_AUTOSIGNER"
        echo "  linux-$VER.tar signed by $FPR_GREGKH (linux-$VER.tar.sign)"
    } | tee "$OUT/out/source.txt"
}

config() {
    if [ ! -f "$OUT/src/.linux-$VER.unpacked" ]; then
        mkdir -p "$OUT/src"
        tar -C "$OUT/src" -xJf "$OUT/dl/linux-$VER.tar.xz"
        touch "$OUT/src/.linux-$VER.unpacked"
    fi
    "${KMAKE[@]}" multi_v7_defconfig
    sha256sum "$BUILD/.config"
}

kernel() {
    "${KMAKE[@]}" zImage dtbs modules_prepare
    ls -l "$BUILD/arch/arm/boot/zImage" "$BUILD/arch/arm/boot/dts/$DTB"
}

module() {
    mkdir -p "$OUT/module"
    cp "$here/pmu-user/pmu-user.c" "$here/pmu-user/Kbuild" "$OUT/module/"
    # Without `make modules` there is no Module.symvers; vmlinux's exports are
    # in vmlinux.symvers, which modpost checks the module against.
    local syms=()
    [ -f "$BUILD/Module.symvers" ] || syms=(KBUILD_EXTRA_SYMBOLS="$BUILD/vmlinux.symvers")
    make -C "$BUILD" ARCH=arm CROSS_COMPILE="$CROSS" M="$OUT/module" "${syms[@]}" modules
    modinfo "$OUT/module/pmu-user.ko" | grep -E '^(name|vermagic|license):'
}

lbench() {
    mkdir -p "$OUT/lbench"
    "${CROSS}gcc" "${CFLAGS[@]}" -I"$common" -o "$OUT/lbench/lbench" "$here/lbench.c"
    # ARM or Thumb-2: the mode these flags compile in, and main's symbol (bit
    # 0 set is a Thumb function). The timed calls trap through lbench's own
    # svc stubs, in main's mode; the C library's syscall() is set-up only.
    local mode main libc
    if "${CROSS}gcc" "${CFLAGS[@]}" -dM -E - </dev/null | grep -q '__thumb2__'; then
        mode=thumb2
    else
        mode=arm
    fi
    main=$("${CROSS}readelf" -s "$OUT/lbench/lbench" | awk '$8 == "main" { print $2 }')
    libc=$("${CROSS}readelf" -s "$OUT/lbench/lbench" | awk '$8 == "syscall" { print $2 }')
    echo "lbench: ${CFLAGS[*]} compiles as $mode; main at 0x$main (timed calls: own svc stubs), libc's syscall at 0x$libc (set-up only)" |
        tee "$OUT/lbench/mode.txt"
}

# The build's gen_init_cpio, or the copy clean keeps.
gen_init_cpio() {
    if [ -x "$BUILD/usr/gen_init_cpio" ]; then
        echo "$BUILD/usr/gen_init_cpio"
    elif [ -x "$OUT/tools/gen_init_cpio" ]; then
        echo "$OUT/tools/gen_init_cpio"
    else
        die "no gen_init_cpio: run kernel, or a clean that kept it"
    fi
}

# The module as built, or the copy collect put in out/.
module_ko() {
    if [ -f "$OUT/module/pmu-user.ko" ]; then
        echo "$OUT/module/pmu-user.ko"
    else
        echo "$OUT/out/pmu-user.ko"
    fi
}

cpio() {
    mkdir -p "$OUT/out"
    local list=$OUT/out/initramfs.list
    cat >"$list" <<EOF
dir /dev 0755 0 0
nod /dev/console 0600 0 0 c 5 1
dir /proc 0755 0 0
dir /sys 0755 0 0
file /init $OUT/lbench/lbench 0755 0 0
file /pmu-user.ko $(module_ko) 0644 0 0
EOF
    "$(gen_init_cpio)" -t 0 "$list" | gzip -9n >"$OUT/out/initramfs.cpio.gz"
    ls -l "$OUT/out/initramfs.cpio.gz"
}

collect() {
    local card=$OUT/out/card/$CARD dtb
    dtb=$(basename "$DTB")
    mkdir -p "$card"
    cp "$BUILD/arch/arm/boot/zImage" "$BUILD/arch/arm/boot/dts/$DTB" "$OUT/out/initramfs.cpio.gz" "$card/"
    cd "$OUT/out"
    cp "$BUILD/.config" config
    cp "$OUT/module/pmu-user.ko" "$OUT/lbench/lbench" "$OUT/lbench/mode.txt" .
    cp "$here/boot-linux.cmd" .
    dtc -I dtb -O dts -q -o "${dtb%.dtb}.dts" "$card/$dtb"

    # The CPU clock points: each CPU's clock-frequency and OPP table, and every
    # operating-points-v2 table in the tree.
    {
        echo "# CPU clock points in $DTB"
        local cpu table opp tables=0
        for cpu in $(fdtget -l "$card/$dtb" /cpus); do
            printf '/cpus/%s clock-frequency=%s operating-points-v2=%s\n' "$cpu" \
                "$(fdtget -t u "$card/$dtb" "/cpus/$cpu" clock-frequency 2>/dev/null || echo -)" \
                "$(fdtget "$card/$dtb" "/cpus/$cpu" operating-points-v2 2>/dev/null || echo none)"
        done
        for table in $(fdtget -l "$card/$dtb" /); do
            [ "$(fdtget "$card/$dtb" "/$table" compatible 2>/dev/null)" = operating-points-v2 ] || continue
            tables=$((tables + 1))
            echo "/$table"
            for opp in $(fdtget -l "$card/$dtb" "/$table"); do
                printf '  %s: hz=%s uV=%s supported-hw=%s\n' "$opp" \
                    "$(fdtget -t u "$card/$dtb" "/$table/$opp" opp-hz | awk '{ print $1 * 4294967296 + $2 }')" \
                    "$(fdtget -t u "$card/$dtb" "/$table/$opp" opp-microvolt)" \
                    "$(fdtget -t x "$card/$dtb" "/$table/$opp" opp-supported-hw 2>/dev/null || echo -)"
            done
        done
        [ "$tables" -gt 0 ] || echo "no operating-points-v2 table: no OPPs, so no cpufreq"
    } >opps.txt

    {
        echo "# Linux $VER for the STM32MP157D-DK1, built $(date -u +%Y-%m-%dT%H:%MZ)"
        cat source.txt
        echo "gcc: $("${CROSS}gcc" --version | head -1)"
        echo "kernelrelease: $(make -s -C "$SRC" O="$BUILD" ARCH=arm CROSS_COMPILE="$CROSS" kernelrelease)"
        echo "config: multi_v7_defconfig, unmodified"
        grep '^CONFIG_CC_VERSION_TEXT=' config
        cat mode.txt
        echo
        echo "# The card's files: bootfs (mmc 0:4) /$CARD/"
        (cd card && find "$CARD" -type f | sort | while read -r f; do
            printf '%s  %9d  %s\n' "$(sha256sum "$f" | cut -d' ' -f1)" "$(stat -c %s "$f")" "$f"
        done)
        echo
        echo "# The rest"
        sha256sum config pmu-user.ko lbench initramfs.list boot-linux.cmd
        echo
        cat opps.txt
    } >versions.txt
    cat versions.txt
}

clean() {
    [ -f "$OUT/out/versions.txt" ] || die "clean: run collect first"
    mkdir -p "$OUT/tools"
    [ ! -x "$BUILD/usr/gen_init_cpio" ] || cp "$BUILD/usr/gen_init_cpio" "$OUT/tools/"
    rm -rf "$OUT/build" "$OUT/module" "$OUT/lbench" "$SRC"
    rm -f "$OUT/src/.linux-$VER.unpacked"
    rmdir "$OUT/src" 2>/dev/null || true
    du -sh "$OUT"
}

repack() {
    [ -f "$OUT/out/versions.txt" ] || die "repack: run collect first"
    lbench
    cpio
    cp "$OUT/out/initramfs.cpio.gz" "$OUT/out/card/$CARD/"
    cp "$OUT/lbench/lbench" "$OUT/lbench/mode.txt" "$OUT/out/"
    {
        echo
        echo "# repacked $(date -u +%Y-%m-%dT%H:%MZ): lbench and the initramfs only"
        cat "$OUT/lbench/mode.txt"
        (cd "$OUT/out" && sha256sum lbench "card/$CARD/initramfs.cpio.gz")
    } >>"$OUT/out/versions.txt"
    rm -rf "$OUT/lbench"
    tail -5 "$OUT/out/versions.txt"
}

steps=("$@")
[ ${#steps[@]} -gt 0 ] || steps=(fetch config kernel module lbench cpio collect)
for step in "${steps[@]}"; do
    case $step in
    fetch | config | kernel | module | lbench | cpio | collect | clean | repack)
        echo "== $step"
        (cd "$OUT" 2>/dev/null || true; "$step")
        ;;
    *) die "unknown step $step" ;;
    esac
done
