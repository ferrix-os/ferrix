#!/bin/sh
# R3's check inside a guest, the same on the Linux guest and on Ferrix
# (docs/GAME-PATH.md §2.4 and §8.1): the clock, a timed sleep's overshoot
# idle, then the background load, every processor's busy share under it,
# and the overshoot again under it.
#
#   measure.sh [WARM-UP SECONDS]       (30)
#
# It takes the rig's binary, the compiler and the disk from load.sh's
# variables, and the load script and crate from beside itself unless
# RIG_LOAD names the script.

here=${RIG_HERE:-$(dirname "$0")}
load=${RIG_LOAD:-$here/load.sh}
rig=${RIG:-rig}
warm=${1:-30}

source=/sys/devices/system/clocksource/clocksource0/current_clocksource
if [ -r "$source" ]; then
	echo "measure: clocksource $(cat "$source")"
else
	echo "measure: clocksource not in sysfs (Ferrix prints it at boot: the kernel's clock line)"
fi
echo "measure: idle"
"$rig" wake -n 5000 || exit 3
sh "$load" start || exit 4
echo "measure: loaded, warming up for $warm s"
sleep "$warm"
"$rig" busy 10 || exit 5
"$rig" wake -n 10000 || exit 6
cat "${RIG_WORK:-/tmp/rig-load}/hold.log" "${RIG_WORK:-/tmp/rig-load}/write.log"
sh "$load" stop
echo "measure: done"
