#!/usr/bin/env bash
# Stage 20's exit: every test, booting only what Ferrix compiled.
#
#     tools/common/test/selfhost-matrix.sh record DIR    # the matrix, writing DIR/plan
#     cargo xtask test-selfhost --accel kvm --plan DIR
#                                              # Ferrix makes every build in it
#     tools/common/test/selfhost-matrix.sh replay DIR    # the matrix again, every build
#                                              # answered from DIR/store
#
# `plan DIR` writes the same plan as `record DIR` without booting anything
# (FERRIX_BUILDS=plan:DIR): minutes, no QEMU, and none of the volumes the
# boots need, which is what lets a scheduled CI job make one. Every row then
# ends at its first boot, so a row for `--arch all` runs once per
# architecture, and a row's exit says nothing.
#
# Each row is a test command of the gate matrix, run with
# FERRIX_BUILDS=record:DIR or replay:DIR/store (tools/common/xtask/src/builds.rs). Both
# runs use a home of their own, DIR/home, so that the programs the script
# builds install -- busybox, uutils and the ports, which a replay puts back
# from the store -- never reach the ones other checkouts on this machine use.
# That home links everything in ~/.local/share/ferrix but those three, whose
# source archives alone are copied; Cargo and rustup keep their own homes.
#
# The summary is DIR/<mode>/summary, a row's log beside it. Judge a run by
# `grep "exit [1-9]"` in the summary, and by FERRIX-PANIC in the logs.
set -u
mode=${1:?record or replay}
mkdir -p "${2:?the plan directory}" && dir=$(cd "$2" && pwd) || exit 2
case $mode in
    record) builds=record:$dir ;;
    plan) builds=plan:$dir ;;
    replay) builds=replay:$dir/store ;;
    *) echo "selfhost-matrix: $mode is none of record, plan and replay" >&2; exit 2 ;;
esac
cd "$(dirname "$0")/../../.." || exit 2

# A build's key names what it runs and reads, not the tree it compiles, so a
# plan and its store belong to the tree they were recorded on: its hash, so
# that squashing the commits above it changes nothing, and what is modified.
tree="$(git rev-parse 'HEAD^{tree}') $(git status --porcelain --untracked-files=no | sha256sum | cut -c1-16)"
if [ "$mode" = record ] || [ "$mode" = plan ]; then
    echo "$tree" > "$dir/tree"
elif [ "$(cat "$dir/tree" 2>/dev/null)" != "$tree" ]; then
    echo "selfhost-matrix: $dir was recorded on $(cat "$dir/tree" 2>/dev/null), this is $tree" >&2
    exit 2
fi

real=$HOME/.local/share/ferrix
home=$dir/home
data=$home/.local/share/ferrix
if [ ! -d "$data" ]; then
    mkdir -p "$data"
    for entry in "$real"/*; do
        case ${entry##*/} in
            busybox | uutils | ports) ;;
            *) ln -s "$entry" "$data/" ;;
        esac
    done
    mkdir -p "$data/busybox"
    for entry in "$real"/busybox/*; do
        [ "${entry##*/}" = ferrousli ] || ln -s "$entry" "$data/busybox/"
    done
    for script in busybox uutils ports; do
        mkdir -p "$data/$script/ferrousli/src"
        cp -a "$real/$script/ferrousli/src/." "$data/$script/ferrousli/src/"
    done
fi
export CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}
export RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}
export FERRIX_RUSTC_SYSROOT=${FERRIX_RUSTC_SYSROOT:-$real/rustc}
export HOME=$home
export FERRIX_BUILDS=$builds

log=$dir/$mode
mkdir -p "$log"
: > "$log/summary"
echo "== $mode $(git log --oneline -1) $(date +%F' '%T)" >> "$log/summary"
run() {
    local name=$1
    shift
    if [ "$mode" = plan ] && [[ " $* " == *" --arch all "* ]]; then
        local arch argument
        for arch in x86_64 aarch64 armv7a; do
            local each=()
            for argument in "$@"; do
                [ "$argument" = all ] && [ "${each[-1]:-}" = --arch ] && argument=$arch
                each+=("$argument")
            done
            run "$name-$arch" "${each[@]}"
        done
        return
    fi
    echo "== $name: start $(date +%T)" >> "$log/summary"
    "$@" > "$log/$name.log" 2>&1 < /dev/null
    echo "== $name: exit $? $(date +%T)" >> "$log/summary"
}

musl=$data/busybox/{arch}/bin/busybox.static
ferrousli=$data/busybox/ferrousli/x86_64/bin/busybox.static
debian=$data/busybox/debian
run ports cargo xtask ports
run busybox cargo xtask busybox
run uutils cargo xtask uutils
# Every app's package, which test-foot, test-init (sshdt) and test-vkgears
# boot: the matrix's home starts with none.
run build-apps cargo xtask build-apps --arch all
run build-release cargo xtask build --arch all --release
run boot-x86_64 cargo xtask test-boot --arch x86_64 --timeout 600
run boot-aarch64 cargo xtask test-boot --arch aarch64 --timeout 600
run boot-armv7a cargo xtask test-boot --arch armv7a --timeout 600
run boot-armv7a-smp2 cargo xtask test-boot --arch armv7a --smp 2 --timeout 600
run boot-x86_64-kvm cargo xtask test-boot --arch x86_64 --accel kvm --timeout 600
run shell-zinc cargo xtask test-shell --arch all --timeout 600
run shell-ferrousli cargo xtask test-shell --arch x86_64 --init "$ferrousli" --timeout 600
run shell-musl cargo xtask test-shell --arch x86_64 --init "$musl" --timeout 600
run shell-glibc cargo xtask test-shell --arch x86_64 --init /usr/bin/busybox --timeout 600
run shell-debian-dynamic cargo xtask test-shell --arch all --init "$debian/{arch}/busybox" \
    --interpreter "$debian/{arch}/ld.so" --library "$debian/{arch}/libc.so.6" \
    --library "$debian/{arch}/libresolv.so.2" --timeout 600
# The same program on ferrousli's own loader and libc.so.6, which this tree
# builds; on x86-64, whose C library is linked with the tree's cc (the Arm
# ones need standard libraries the sysroot does not carry).
run shell-debian-ferrousli cargo xtask test-shell --arch x86_64 --init "$debian/{arch}/busybox" \
    --interpreter ferrousli --library ferrousli --timeout 600
run vfs-ferrousli cargo xtask test-vfs --arch x86_64 --init "$ferrousli" --timeout 600
run vfs-musl cargo xtask test-vfs --arch x86_64 --init "$musl" --timeout 600
run net-ferrousli cargo xtask test-net --arch x86_64 --init "$ferrousli" --timeout 600
run net-musl cargo xtask test-net --arch all --init "$musl" --timeout 600
run display cargo xtask test-display --arch all --timeout 600
run threads cargo xtask test-threads --arch all --timeout 600
run jobs cargo xtask test-jobs --timeout 600
run compositor cargo xtask test-compositor --arch x86_64 --timeout 600
run compositor-aarch64 cargo xtask test-compositor --arch aarch64 --timeout 900
run foot cargo xtask test-foot --arch x86_64 --timeout 900
run restart cargo xtask test-restart --timeout 600
run sysfs cargo xtask test-sysfs --timeout 600
run video cargo xtask test-video --arch x86_64 --timeout 600
run input cargo xtask test-input --arch x86_64 --timeout 600
run seat cargo xtask test-seat --arch x86_64 --timeout 600
run pty cargo xtask test-pty --arch x86_64 --timeout 600
run btrfs cargo xtask test-btrfs --arch all --timeout 600
run powerfail cargo xtask test-powerfail --arch all --seeds 8 --timeout 600
# The gates added since the matrix was first run (2026-09-24), each as its
# own landings run it, with its own timeout.
run init cargo xtask test-init --arch all
run auth cargo xtask test-auth --arch all
run procfs cargo xtask test-procfs --arch all
run kaslr cargo xtask test-kaslr --arch x86_64
run adb cargo xtask test-adb --init "$musl"
run audio cargo xtask test-audio --arch x86_64
run pkg cargo xtask test-pkg --arch x86_64
run apps cargo xtask test-apps
run install cargo xtask test-install
run bwrap cargo xtask test-bwrap
run clipboard cargo xtask test-clipboard
run badapple cargo xtask test-badapple
run vkgears cargo xtask test-vkgears
run yserver cargo xtask test-yserver
run xwindow cargo xtask test-xwindow
run chrome cargo xtask test-chrome
run claude-code cargo xtask test-claude-code
run steamcmd cargo xtask test-steamcmd
run steam-store cargo xtask test-steam-store
run steam-window cargo xtask test-steam-window
run rustc-kvm cargo xtask test-rustc --accel kvm
run rustc-tcg cargo xtask test-rustc --release
run selfhost cargo xtask test-selfhost --accel kvm
echo "== DONE $(date +%F' '%T)" >> "$log/summary"
