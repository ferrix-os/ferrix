#!/bin/bash
# Host side: one Teeworlds measurement on the Ferrix card VM.
#   measure-tw.sh <label> <gfx_vsync 0|1> [seconds=30] [settle=20]
# Stops the running client (not the server), starts it with the swap counter,
# lets it settle, then takes <seconds> of swap-fps lines, CPU ticks and the
# compositor's frame lines from the serial log. Leaves the client running.
# Results go under the baseline's directory ($FERRIX_LINUX_BASELINE).
set -u
label=$1 vs=$2 secs=${3:-30} settle=${4:-20}
S=$(cd "$(dirname "$0")" && pwd)
B=${FERRIX_LINUX_BASELINE:-$HOME/.local/share/ferrix/linux-baseline}; R=$B/results/ferrix; mkdir -p "$R"
H=$S/../../nvidia/card.sh
export CARD_AGENT=linux-baseline
. ~/.local/share/ferrix/nvidia/cardvm.env   # LOG = serial log
g() { "$H" ssh "$@" < /dev/null; }
g 'pkill -f "^./teeworlds cl_"; i=0; while pgrep -f "^./teeworlds cl_" >/dev/null && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done; pgrep -f "^./teeworlds cl_" >/dev/null && pkill -9 -f "^./teeworlds cl_"; rm -f /tmp/lb/'"$label"'.log; true'
"$H" exec "/bin/busybox sh /tmp/lb/twrun.sh $vs /tmp/lb/$label.log"
sleep "$settle"
g '/bin/busybox sh /tmp/lb/cpusnap.sh' > "$R/$label.cpu0"
l0=$(wc -l < "$LOG"); s0=$(g "wc -l < /tmp/lb/$label.log")
sleep "$secs"
g '/bin/busybox sh /tmp/lb/cpusnap.sh' > "$R/$label.cpu1"
l1=$(wc -l < "$LOG")
g "cat /tmp/lb/$label.log" | tail -n +$((s0 + 1)) | head -n "$secs" > "$R/$label.swap"
g "head -c 4000 /tmp/lb/$label.log.out" > "$R/$label.client-out"
sed -n "$((l0 + 1)),${l1}p" "$LOG" > "$R/$label.serial"
"$S/../summarize.sh" "$R/$label"
