#!/usr/bin/env bash
# The Intel driver's card0 on the N150 box (docs/INTEL-GPU.md §4.3, M6's
# first step): under `ferrix.efifb ferrix.intelgpu`, K4 lends the firmware
# framebuffer to 00:02.0, and the `intel` driver serves `card0` over the lit
# plane as a copying card, beside `renderD128`. This checks a running Ferrix
# desktop on the box over ssh, as intel-checkpoint.sh does, and judges it:
#
#   tools/common/n150/intel-card-checkpoint.sh [BOX] [BOX_SSH_OPTIONS...]
#
# What proves it:
#   the driver: "intel: card0 published", "intel: card0: the first frame is
#     on the screen" (the compositor's first flush copied), "intel: render
#     node published";
#   /dev/dri: card0 and renderD128;
#   the compositor (hyprix) running on card0;
#   no "intel: card0 stopped" or "no card0" line.
# It boots nothing and writes nothing on the box; the evidence goes to
# ~/.local/share/ferrix/n150-linux/evidence/intel-card-<UTC time>.txt here.
set -euo pipefail
box=${1:-192.168.178.146}
shift || true
ssh_box=(ssh -o BatchMode=yes -o ConnectTimeout=10 "$@" "root@$box")
evidence=$HOME/.local/share/ferrix/n150-linux/evidence
mkdir -p "$evidence"
out=$evidence/intel-card-$(date -u +%Y%m%dT%H%M%SZ).txt
"${ssh_box[@]}" true || { echo "intel-card-checkpoint: the box at $box does not answer ssh" >&2; exit 1; }
script=$(cat << 'EOF'
echo "== the box"
date -u
cat /proc/cmdline 2> /dev/null
echo "== /dev/dri"
ls -l /dev/dri 2>&1
echo "== the compositor"
ps 2> /dev/null | grep -E 'hyprix|yserver' | grep -v grep
echo "== the kernel's display lines"
(dmesg 2> /dev/null || cat /proc/kmsg 2> /dev/null) | grep -i -E 'display|lent|card0|efifb' | tail -n 30
echo "== the driver's lines"
(dmesg 2> /dev/null || cat /proc/kmsg 2> /dev/null) | grep -E 'intel: ' | tail -n 60
EOF
)
{
    echo "intel-card-checkpoint: box $box, from $(hostname), $(date -u)"
    printf '%s\n' "$script" | "${ssh_box[@]}" sh
} > "$out" 2>&1 || true

fails=0
check() { # label, extended regex
    local line
    line=$(grep -a -E -m1 "$2" "$out" || true)
    if [ -n "$line" ]; then echo "  PROVED  $1: $(printf '%s' "$line" | sed 's/^[[:space:]]*//')"
    else echo "  NOT     $1"; fails=$((fails + 1)); fi
}
echo "intel-card-checkpoint: verdict for $out"
check "card0 published by the Intel driver" 'intel: card0 published'
check "the compositor's first frame copied" 'intel: card0: the first frame is on the screen'
check "the render node beside it" 'intel: render node published'
check "/dev/dri/card0" ' card0$'
check "/dev/dri/renderD128" ' renderD128$'
check "hyprix running" '^ *[0-9]+ .*hyprix'
if grep -a -q -E 'intel: (no card0|card0 stopped)' "$out"; then
    grep -a -E 'intel: (no card0|card0 stopped)' "$out" | sed 's/^/  NOT     /'
    fails=$((fails + 1))
fi
echo "intel-card-checkpoint: $([ $fails = 0 ] && echo PASSED || echo "FAILED ($fails not proved)")"
[ $fails = 0 ]
