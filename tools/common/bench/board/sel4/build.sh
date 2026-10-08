#!/bin/bash
# Build seL4 images for the DK1 (stm32mp1) and the QEMU smoke machine.
#
#   ./build.sh <config> [root]       root defaults to ~/.local/share/ferrix/board-bench/sel4
#
# configs:
#   test-dk1     sel4test, debug, printing, stm32mp1, one core
#   bench-dk1    sel4bench (IPC app with sel4rt first), release, stm32mp1, one core
#   bench-dk1-smp  as bench-dk1 with both cores (KernelMaxNumNodes 2): the second table
#   test-qemu    sel4test, debug, qemu-arm-virt, Cortex-A15 in AArch32 (simulation tests)
#   bench-qemu   sel4bench as bench-dk1, qemu-arm-virt, Cortex-A15
#
# Writes <root>/build/<config>/ (images/, config-summary.txt, config-diff.txt)
# after ./fetch.sh has made <root>/src. config-diff.txt lists every
# configuration value that differs from the same project configured with only
# the platform given: a baseline in <root>/build/<config>.defaults.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
name=${1:?usage: build.sh <config> [root]}
root=${2:-$HOME/.local/share/ferrix/board-bench/sel4}
src=$root/src
. "$root/venv/bin/activate"
export CMAKE_POLICY_VERSION_MINIMUM=3.5
# arm-none-eabi-, not arm-linux-gnueabihf-: seL4 builds user code soft-float
# (-mfloat-abi=soft: its -march=armv7ve names no FPU, so its hard-float probe
# fails), and gnueabihf's crtbegin.o and libgcc.a are hard-float only, which
# ld refuses to link with soft-float objects. arm-none-eabi's thumb/v7-a/nofp
# multilib is soft-float. seL4's own flags compile all of its C with -marm.
common=(-G Ninja -DCROSS_COMPILER_PREFIX=arm-none-eabi- -DSEL4_CACHE_DIR="$root/cache")
dk1=(-DPLATFORM=stm32mp1)
kdk1=(-DKernelPlatform=stm32mp1 -DKernelSel4Arch=aarch32)
qemu=(-DPLATFORM=qemu-arm-virt -DKernelSel4Arch=aarch32 -DARM_CPU=cortex-a15)
kqemu=(-DKernelPlatform=qemu-arm-virt -DKernelSel4Arch=aarch32 -DARM_CPU=cortex-a15)
# sel4test: debug kernel with printing, every test the build includes (regex
# .*), no halt on a failure, so the log lists each test's result. No user-level
# timer, so no timer tests: the only one the DK1 could have is the generic
# timer's physical half (CNTP, PPI 30; the kernel owns the virtual one), and
# whether the non-secure world may use CNTP depends on CNTHCTL, which OP-TEE
# sets and the non-secure world cannot read; a trapped access would end the
# run. The port has that ltimer: EXTRA="-DKernelArmExportPCNTUser=ON
# -DKernelArmExportPTMRUser=ON" turns it on.
test=(-DRELEASE=OFF -DVERIFICATION=OFF -DSMP=OFF -DMCS=OFF -DDOMAINS=OFF -DBAMBOO=OFF
      -DLibSel4TestPrinterRegex=.* -DLibSel4TestPrinterHaltOnTestFailure=OFF)
# sel4bench: release (no printing), fast path, one core, no hypervisor, the IPC
# app only; the PMU and the virtual counter readable from user mode
# (board-bench.h); 1 MiB stacks for sel4rt's client (its samples live there),
# as on x86-64. Plain serial lines (no ANSI colour per client).
# AllowUnstableOverhead, as sel4bench sets it on x86-64: an unsteady
# counter-read overhead must not abort the run before its end line. sel4rt
# prints the sources: the kernel's commit and the patches' hash.
patches_hash=$({ cat "$here"/patches/*.patch 2>/dev/null || true; } | sha256sum | cut -c1-12)
sources="seL4-$(git -C "$src/kernel" rev-parse --short=12 HEAD)+patches-$patches_hash"
bench=(-DRELEASE=ON -DFASTPATH=ON -DSMP=OFF -DKernelMaxNumNodes=1 -DMCS=OFF
       -DIPC=ON -DIRQUSER=OFF -DSCHED=OFF -DSIGNAL=OFF -DMAPPING=OFF -DSYNC=OFF
       -DHARDWARE=OFF -DFAULT=OFF -DVCPU=OFF -DARM_HYP=OFF -DKernelArmHypervisorSupport=OFF
       -DKernelArmExportPMUUser=ON -DKernelArmExportVCNTUser=ON
       -DLibSel4UtilsStackSize=1048576 -DAllowUnstableOverhead=ON
       -DLibSel4SerialServerColoredOutput=OFF
       -DSel4rtSources="$sources")

case "$name" in
  test-dk1)      project=sel4test;  plat=("${dk1[@]}"); kplat=("${kdk1[@]}");     args=("${test[@]}") ;;
  bench-dk1)     project=sel4bench; plat=("${dk1[@]}"); kplat=("${kdk1[@]}");     args=("${bench[@]}") ;;
  bench-dk1-smp) project=sel4bench; plat=("${dk1[@]}"); kplat=("${kdk1[@]}");     args=("${bench[@]}" -DSMP=ON) ;;
  test-qemu)     project=sel4test;  plat=("${qemu[@]}"); kplat=("${kqemu[@]}");    args=("${test[@]}") ;;
  bench-qemu)    project=sel4bench; plat=("${qemu[@]}"); kplat=("${kqemu[@]}");    args=("${bench[@]}") ;;
  *) echo "unknown config $name" >&2; exit 2 ;;
esac

configure() { # dir args...
  local dir=$1; shift
  rm -rf "$dir"; mkdir -p "$dir"
  (cd "$dir" && cmake "${common[@]}" "$@" -C "$src/projects/$project/settings.cmake" \
     "$src/projects/$project" > configure.log 2>&1) || { tail -40 "$dir/configure.log"; exit 1; }
}

# OUT names another build directory and EXTRA adds cmake arguments, for
# diagnosis only (e.g. OUT=bench-qemu-dbg EXTRA="-DRELEASE=OFF").
b=$root/build/${OUT:-$name}
read -r -a extra <<< "${EXTRA:-}"
configure "$b.defaults" "${plat[@]}"
configure "$b" "${plat[@]}" "${args[@]}" "${extra[@]}"
(cd "$b" && nice -n 10 ninja -j8 > build.log 2>&1) || { tail -60 "$b/build.log"; exit 1; }

python3 "$here/config-diff.py" "$b.defaults" "$b" > "$b/config-diff.txt"
rm -rf "$b.defaults"   # nazuna's disk is nearly full: the baseline is only for the diff
# The kernel configured alone for the same platform: its own defaults.
rm -rf "$b.kdefaults"; mkdir -p "$b.kdefaults"
(cd "$b.kdefaults" && cmake -G Ninja -DCMAKE_TOOLCHAIN_FILE="$src/kernel/gcc.cmake" \
   -DCROSS_COMPILER_PREFIX=arm-none-eabi- "${kplat[@]}" "$src/kernel" > configure.log 2>&1) \
  || { tail -40 "$b.kdefaults/configure.log"; exit 1; }
python3 "$here/config-diff.py" --kernel "$b.kdefaults" "$b" > "$b/kernel-config-diff.txt"
rm -rf "$b.kdefaults"
grep -E "^(Kernel|LibSel4|LibPlatSupport|Sel4test|Sel4bench|App|Elfloader|CMAKE_BUILD_TYPE|CROSS_COMPILER_PREFIX)[A-Za-z0-9_]*:[A-Z]+=" \
  "$b/CMakeCache.txt" | grep -v ":INTERNAL=" > "$b/config-summary.txt"
cat "$b/config-diff.txt" "$b/kernel-config-diff.txt"
for f in "$b"/images/*; do
  echo "image $f $(stat -c %s "$f") bytes"
  arm-none-eabi-readelf -lW "$f" | grep -E "Entry point|LOAD"
done
