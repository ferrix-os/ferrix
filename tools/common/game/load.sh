#!/bin/sh
# The game path's background load (docs/GAME-PATH.md §2.4, stage 24's R3):
# the same script on the Linux guest and on Ferrix, run as an ordinary
# user's job and started before the game.
#
#   load.sh start      start it and return
#   load.sh stop       stop it
#
# It is three parts:
#   * load-crate.rs, a fixed crate, compiled by rustc in a loop, one loop
#     for every processor `rig cpus` counts;
#   * `rig write`, streaming to a file on the disk and syncing it;
#   * `rig hold`, holding memory until RIG_HOLD percent (80) of it is in use.
#
# Where things are, each with its default:
#   RIG        the rig binary                     rig, on PATH
#   RIG_RUSTC  the compiler                       rustc, on PATH
#   RIG_CRATE  the crate                          load-crate.rs beside this script
#   RIG_WORK   the compilers' output              /tmp/rig-load
#   RIG_DISK   the directory the writer writes to $HOME
#   RIG_HOLD   percent of memory in use           80
#   RIG_WRITE_MIB  MiB the writer writes a pass   1024
#
# Every line it prints for a gate starts with "load:". `rig busy 10` while
# it runs says whether it holds every processor.

rig=${RIG:-rig}
rustc=${RIG_RUSTC:-rustc}
crate=${RIG_CRATE:-$(dirname "$0")/load-crate.rs}
work=${RIG_WORK:-/tmp/rig-load}
disk=${RIG_DISK:-$HOME}
hold=${RIG_HOLD:-80}

case $1 in
start)
	mkdir -p "$work" || exit 1
	rm -f "$work/stop" "$work/pids"
	n=$("$rig" cpus) || exit 1
	i=0
	while [ "$i" -lt "$n" ]; do
		mkdir -p "$work/c$i"
		builds=0
		while [ ! -e "$work/stop" ]; do
			"$rustc" --edition 2021 --crate-type lib -C opt-level=2 -C codegen-units=1 \
				--out-dir "$work/c$i" "$crate" || break
			builds=$((builds + 1))
			echo "compile: loop $i, $builds builds" >"$work/c$i.count"
		done >"$work/c$i.log" 2>&1 &
		echo $! >>"$work/pids"
		i=$((i + 1))
	done
	"$rig" write "$disk" "${RIG_WRITE_MIB:-1024}" >"$work/write.log" 2>&1 &
	echo $! >>"$work/pids"
	"$rig" hold "$hold" >"$work/hold.log" 2>&1 &
	echo $! >>"$work/pids"
	echo "load: $n compile loops, a writer to $disk, memory held at $hold%"
	;;
stop)
	: >"$work/stop"
	# The loops, the writer and the holder end at once; a compiler already
	# running finishes its build and is not started again.
	for pid in $(cat "$work/pids"); do
		kill "$pid" 2>/dev/null
	done
	rm -f "$disk/rig-write.bin"
	echo "load: stopped"
	;;
*)
	echo "usage: load.sh start | stop" >&2
	exit 2
	;;
esac
