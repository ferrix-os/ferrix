#!/bin/bash
# Boot the card's zImage and initramfs (build.sh's out/card/bench/linux)
# under QEMU's virt machine with one Cortex-A7: the module loads, lbench runs
# every series and ends with `board-bench end lbench` and a reset, which ends
# QEMU (-no-reboot). A smoke test only: QEMU's cycle counter is its virtual
# clock at 1 GHz, and it counts instructions only under -icount, so no figure
# from it means anything for the board.
#
#   [QEMU_ARGS='-icount shift=0'] smoke-qemu.sh [extra kernel arguments]
#
# The serial log goes to $BB_LINUX_OUT/logs/smoke-qemu-<time>.log. Exits 0
# when the log has `LB done`, `board-bench end lbench` and no `lbench error`.
set -euo pipefail

OUT=${BB_LINUX_OUT:-$HOME/.local/share/ferrix/board-bench/linux}
CARD=$OUT/out/card/bench/linux
QEMU=${QEMU:-qemu-system-arm}
TIMEOUT=${TIMEOUT:-540}
read -r -a qemu_args <<<"${QEMU_ARGS:-}"

mkdir -p "$OUT/logs"
log=$OUT/logs/smoke-qemu-$(date +%Y%m%d-%H%M%S).log
echo "smoke: $($QEMU --version | head -1); QEMU_ARGS='${QEMU_ARGS:-}'; log $log"

status=0
start=$SECONDS
timeout "$TIMEOUT" "$QEMU" -M virt -cpu cortex-a7 -smp 1 -m 512 -nographic -no-reboot \
    "${qemu_args[@]}" -kernel "$CARD/zImage" -initrd "$CARD/initramfs.cpio.gz" \
    -append "console=ttyAMA0 cpufreq.off=1 maxcpus=1 panic=-1 $*" \
    </dev/null >"$log" 2>&1 || status=$?
echo "smoke: qemu exit $status after $((SECONDS - start)) s"

grep -E 'Linux version|pmu-user|^lbench |^LB |^board-bench |reboot:|Kernel panic|Unable to|Internal error' "$log" |
    grep -v '\.cycles\.pct ' || true
if grep -q '^LB done' "$log" && grep -q '^board-bench end lbench' "$log" && ! grep -q '^lbench error' "$log"; then
    echo "smoke: PASS"
else
    echo "smoke: FAIL"
    exit 1
fi
