#!/bin/sh
# R3's check inside a guest, the same on the Linux guest and on Ferrix
# (docs/GAME-PATH.md §2.4 and §8.1): the clock, the launch timer on a
# stand-in (and on RIG_GAME, when it names one), a timed sleep's overshoot
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
# The launch timer against a stand-in that reports its first swap 100 ms
# after its constructor: the line must say about 100 ms more for the swap.
"$rig" launch "$rig" fake-game 100 || exit 7
# M6 on a real program, where the guest can show one: RIG_GAME is the
# command line of a GLX program with the frame recorder preloaded (the
# paced client framegl, for a few seconds), run twice, the first after the
# boot (cold) and again (warm). It needs an X server: test-game-rig's
# console has none, and `test-frames` times framegl on its desktop instead.
if [ -n "${RIG_GAME:-}" ]; then
	for launch in cold warm; do
		echo "measure: launch $launch"
		# shellcheck disable=SC2086 # RIG_GAME is a command line
		"$rig" launch $RIG_GAME || exit 8
	done
fi
echo "measure: idle"
"$rig" wake -n 5000 || exit 3
sh "$load" start || exit 4
echo "measure: loaded, warming up for $warm s"
sleep "$warm"
"$rig" busy 10 || exit 5
"$rig" wake -n 10000 || exit 6
sh "$load" stop
echo "measure: done"
