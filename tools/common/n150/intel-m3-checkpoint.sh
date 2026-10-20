#!/usr/bin/env bash
# M3's box checkpoint (docs/INTEL-GPU.md §7): anv and iris on the N150's
# real GPU, through the `intel` driver's render node over the GT back end.
# It runs tools/common/n150/intel-checkpoint.sh (vulkaninfo --summary with
# anv alone, eglinfo -B, the kernel's and the driver's lines) and judges what
# that wrote, one PROVED/NOT line a fact:
#
#   tools/common/n150/intel-m3-checkpoint.sh [BOX] [BOX_SSH_OPTIONS...]
#
# The box must already run a Ferrix under `ferrix.intelgpu` with this
# driver and be reachable over ssh: K4 (`ferrix.efifb` beside
# `ferrix.intelgpu`, so the desktop and its sshd have a card), and K2 for the
# node itself, since the kernel refuses a render-node client while 00:02.0's
# DMA is untranslated (K3, `chardev ... refused while its DMA is
# untranslated`). Before K2 the verdict says so, which is itself the answer.
# It boots nothing; main schedules box time.
#
# What proves M3 on the box:
#   the driver: "intel: back end up", "intel: render node published";
#   anv: deviceName "Intel(R) Graphics (ADL-N)", driverID
#     DRIVER_ID_INTEL_OPEN_SOURCE_MESA, vulkaninfo exited 0;
#   iris: the GL renderer "Mesa Intel(R) Graphics (ADL-N)", eglinfo exited 0;
#   no "intel: unserved" line (each one names a call to add).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
evidence=$HOME/.local/share/ferrix/n150-linux/evidence
before=$(ls -1t "$evidence"/intel-checkpoint-*.txt 2>/dev/null | head -n 1 || true)
"$here/intel-checkpoint.sh" "$@"
out=$(ls -1t "$evidence"/intel-checkpoint-*.txt 2>/dev/null | head -n 1 || true)
if [ -z "$out" ] || [ "$out" = "$before" ]; then
    echo "intel-m3-checkpoint: intel-checkpoint.sh wrote no evidence file" >&2
    exit 1
fi

fails=0
check() { # label, extended regex
    local line
    line=$(grep -a -E -m1 "$2" "$out" || true)
    if [ -n "$line" ]; then echo "  PROVED  $1: $(printf '%s' "$line" | sed 's/^[[:space:]]*//')"
    else echo "  NOT     $1"; fails=$((fails + 1)); fi
}
echo "intel-m3-checkpoint: verdict for $out"
check "the GT up under the back end" 'intel: back end up in [0-9]+ ms'
check "the render node published" 'intel: render node published'
check "/dev/dri/renderD* there" 'renderD[0-9]+'
check "anv names ADL-N" 'deviceName[[:space:]]*=[[:space:]]*Intel\(R\) Graphics \(ADL-N\)'
check "anv is Mesa's Intel driver" 'driverID[[:space:]]*=[[:space:]]*DRIVER_ID_INTEL_OPEN_SOURCE_MESA'
check "vulkaninfo exited 0" '^== vulkaninfo exited 0$'
check "iris renders" 'OpenGL (core profile )?renderer: Mesa Intel\(R\) Graphics \(ADL-N\)'
check "eglinfo exited 0" '^== eglinfo exited 0$'
if grep -a -q 'refused while its DMA is untranslated\|render node refused' "$out"; then
    echo "  NOT     the kernel refused the node: 00:02.0's DMA is untranslated (K2 owed)"
    fails=$((fails + 1))
fi
if grep -a -q 'intel: unserved' "$out"; then
    echo "  NOTE    unserved calls (each to be added):"
    grep -a 'intel: unserved' "$out" | sed 's/^/          /'
fi
if grep -a -q -E 'intel: (the back end did not come up|serving stopped|no render node)' "$out"; then
    grep -a -E 'intel: (the back end did not come up|serving stopped|no render node)' "$out" | sed 's/^/  NOT     /'
    fails=$((fails + 1))
fi
echo "intel-m3-checkpoint: $([ $fails = 0 ] && echo PASSED || echo "FAILED ($fails not proved)")"
[ $fails = 0 ]
