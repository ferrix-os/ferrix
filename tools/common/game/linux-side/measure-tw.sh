#!/bin/bash
# Host side: one Teeworlds measurement on the linux-3060 domain.
#   measure-tw.sh <label> <nvidia|soft> <gfx_vsync 0|1> [seconds=30] [settle=20]
# card-ssh.sh and the results are the baseline's ($FERRIX_LINUX_BASELINE).
set -u
l=$1 mode=$2 vs=$3 secs=${4:-30} settle=${5:-20}
S=$(cd "$(dirname "$0")" && pwd)
B=${FERRIX_LINUX_BASELINE:-$HOME/.local/share/ferrix/linux-baseline}; R=$B/results/linux; mkdir -p "$R"
gs() { "$B/card-ssh.sh" ssh "$@" < /dev/null; }
gs "pkill -f '^./teeworlds cl_'; sleep 1; lb/twrun.sh $mode $vs /home/ferrix/lb/$l.log"
sleep "$settle"
gs 'bash lb/cpusnap.sh' > "$R/$l.cpu0"; s0=$(gs "wc -l < lb/$l.log")
sleep "$secs"
gs 'bash lb/cpusnap.sh' > "$R/$l.cpu1"
gs "cat lb/$l.log" | tail -n +$((s0 + 1)) | head -n "$secs" > "$R/$l.swap"
gs "cat lb/$l.log.gl; head -c 3000 lb/$l.log.out" > "$R/$l.client-out"
echo "== $l: $(head -1 "$R/$l.client-out")"
"$S/../summarize.sh" "$R/$l"
gs "nvidia-smi --query-gpu=utilization.gpu,temperature.gpu --format=csv,noheader"
