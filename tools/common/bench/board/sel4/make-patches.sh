#!/bin/bash
# Write patches/*.patch from a checkout fetch.sh made and someone then edited:
# the inverse of fetch.sh's apply step. Each patch is `git diff` of its
# repository against the pin, new files included (by name, so fetch.sh's copy
# of board-bench.h stays out), after a header that says what it is.
#
#   ./make-patches.sh [root]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=${1:-$HOME/.local/share/ferrix/board-bench/sel4}
src=$root/src
mkdir -p "$here/patches"

make_patch() { # repo patch-name header paths...
  local repo=$1 name=$2 header=$3
  shift 3
  git -C "$src/$repo" add -N -- "$@"
  { printf '%s\n\n' "$header"; git -C "$src/$repo" diff --no-color -- "$@"; } > "$here/patches/$name"
  git -C "$src/$repo" reset -q -- "$@"
  echo "patches/$name: $(grep -c '^diff --git' "$here/patches/$name") files"
}

make_patch kernel seL4-stm32mp1.patch \
"seL4 c6ce4d2a: the STM32MP1 platform (STM32MP157, two Cortex-A7, GIC-400,
generic timer) for the STM32MP157D-DK1, and SCTLR/ACTLR printed at boot by
debug kernels; on stm32mp1 the PMU export skips the Secure-only SDER probe
(seL4 runs non-secure under OP-TEE). Kernel files GPL-2.0-only; libsel4's
constants.h BSD-2-Clause; the device tree is Linux v7.2.9's stm32mp157a-dk1,
GPL-2.0-only (gen-dts.py)." \
  src/plat/stm32mp1 libsel4/sel4_plat_include/stm32mp1 tools/dts/stm32mp157a-dk1.dts \
  src/arch/arm/kernel/boot.c src/arch/arm/armv/armv7-a/user_access.c

make_patch tools/seL4 seL4_tools-stm32mp1.patch \
"seL4_tools f1f63d93: the elfloader prints the mode it was entered in, and
SCTLR and ACTLR as entered (AArch32). GPL-2.0-only. The STM32MP1's UART is the
elfloader's existing st,stm32h7-uart driver; its image is the default ELF." \
  elfloader-tool/src/arch-arm/sys_boot.c

make_patch projects/util_libs util_libs-stm32mp1.patch \
"util_libs 8dd23f73: libplatsupport for stm32mp1: the U(S)ARTs (UART4 the
console, the STM32MP2's polled driver) and the generic timer's ltimer (CNTP,
PPI 30) when the kernel exports the physical counter and timer. BSD-2-Clause." \
  libplatsupport/CMakeLists.txt libplatsupport/plat_include/stm32mp1 \
  libplatsupport/src/plat/stm32mp1

make_patch projects/seL4_libs seL4_libs-stm32mp1.patch \
"seL4_libs 262a34dc: libsel4bench's Cortex-A7 PMU events (sel4bench had
none, so no Cortex-A7 platform could build it). BSD-2-Clause." \
  libsel4bench/arch_include/arm/cpu/cortex-a7 libsel4bench/src/arch/arm/cpu/cortex-a7

make_patch projects/sel4bench sel4bench-stm32mp1-sel4rt.patch \
"sel4bench 18f9d5f0: sel4rt, the matched round trip timed with Ferrix's
board-bench.h (fetch.sh copies it to apps/ipc/src/), run first in the IPC app;
the run's last line, and on stm32mp1 a reset through RCC MPSYSRST; SMP
on stm32mp1 is two cores, as on the STM32MP2.
apps/ipc BSD-2-Clause, apps/sel4bench/src/main.c GPL-2.0-only." \
  apps/ipc/CMakeLists.txt apps/ipc/src/main.c apps/ipc/src/sel4rt.c apps/ipc/src/sel4rt.h \
  apps/sel4bench/src/main.c settings.cmake
