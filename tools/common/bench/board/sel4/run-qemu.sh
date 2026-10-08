#!/bin/bash
# Boot a qemu-arm-virt image from build.sh under qemu-system-arm and keep the
# serial log. QEMU has no STM32MP157 machine, so this is the smoke test only.
#
#   ./run-qemu.sh <config> <tag> [root]
#
# Environment:
#   CPU=cortex-a15|cortex-a7   the -cpu (default cortex-a15; must match the build)
#   SECURE=on                  boot in the Secure world (see the README: QEMU reads
#                              DBGDSCR.NS as 0, so a PMU-exporting kernel started
#                              non-secure touches SDER and takes an undefined instruction)
#   VIRT=on                    start in HYP mode (virtualization=on), as U-Boot
#                              may leave the DK1: exercises the elfloader's leave_hyp
#   ICOUNT=1                   -icount shift=0: QEMU's PMU then counts instructions
#                              (event 0x08), and its cycle counter follows them
#   SECS=600                   give up after this many seconds
#
# Writes <root>/logs/<tag>.log and prints it. Stops at the first end marker.
set -euo pipefail
name=${1:?usage: run-qemu.sh <config> <tag> [root]}
tag=${2:?usage: run-qemu.sh <config> <tag> [root]}
root=${3:-$HOME/.local/share/ferrix/board-bench/sel4}
img=$(ls "$root/build/$name/images/"*)
mkdir -p "$root/logs"
log=$root/logs/$tag.log
cpu=${CPU:-cortex-a15}
machine="virt,highmem=off,secure=${SECURE:-off},virtualization=${VIRT:-off},gic-version=2"
extra=()
[ "${ICOUNT:-0}" = 1 ] && extra=(-icount shift=0,align=off)
cmd=(qemu-system-arm -machine "$machine" -cpu "$cpu" -m 1024 -smp 1 -display none
     -monitor none -serial "file:$log" -no-reboot "${extra[@]}" -kernel "$img")
echo "# $(date -Is) $name: ${cmd[*]}" > "$log.meta"
cat "$log.meta"
"${cmd[@]}" < /dev/null &
pid=$!
end='board-bench end sel4rt|All is well in the universe|Test suite failed|halting|Assertion|FAULT|Halting'
for _ in $(seq 1 "$(( ${SECS:-600} * 2 ))"); do
  sleep 0.5
  if grep -Eq "$end" "$log" 2>/dev/null; then sleep 1; break; fi
  kill -0 "$pid" 2>/dev/null || break
done
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true
cat "$log"
