#!/bin/bash
# Host side: one frame record on the Linux baseline guest (docs/GAME-PATH.md §2.4, R1).
#   measure-frames.sh <label> <seconds> [stall ms]
# Builds the recorder and its client with `cargo xtask frames` -- the same two binaries test-frames
# carries into Ferrix --, copies them and frames.sh into the guest's ~/rig, runs frames.sh there,
# fetches the record to results/linux/<label>.rec under the baseline's directory and summarises it.
# GUEST names the ssh wrapper (`<wrapper> ssh <command>`): the baseline's card-ssh.sh by default,
# or its smoke-kvm.sh for the image without the card, with HEADLESS=1 (frames.sh says why). GL is
# passed on too.
set -u
l=$1 secs=$2 stall=${3:-0}
S=$(cd "$(dirname "$0")" && pwd)
T=$(cd "$S/../../../.." && pwd)
B=${FERRIX_LINUX_BASELINE:-$HOME/.local/share/ferrix/linux-baseline}
G=${GUEST:-$B/card-ssh.sh}
R=$B/results/linux; mkdir -p "$R"
F=$T/build/x86_64/frames
(cd "$T" && cargo xtask frames) || exit 1
gs() { "$G" ssh "$@"; }
gs 'mkdir -p rig lb' < /dev/null || exit 1
for f in libframerec.so framegl; do
  gs "cat > rig/$f && chmod +x rig/$f" < "$F/$f" || exit 1
done
gs 'cat > rig/frames.sh' < "$S/frames.sh" || exit 1
gs "GL=${GL:-soft} HEADLESS=${HEADLESS:-0} bash rig/frames.sh \$HOME/rig $l $secs $stall" < /dev/null | tee "$R/$l.out"
gs "cat lb/$l.rec" < /dev/null > "$R/$l.rec"
(cd "$T" && cargo xtask frames --from "$R/$l.rec")
