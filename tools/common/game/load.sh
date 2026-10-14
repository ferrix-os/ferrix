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
#   RIG_PARTS  the parts started                  compile write hold
#
# The script uses no glob: zinc, Ferrix's shell, stops on one that matches
# nothing, as zsh does.
#
# Every line it prints for a gate starts with "load:". `rig busy 10` while
# it runs says whether it holds every processor.

rig=${RIG:-rig}
rustc=${RIG_RUSTC:-rustc}
crate=${RIG_CRATE:-$(dirname "$0")/load-crate.rs}
work=${RIG_WORK:-/tmp/rig-load}
disk=${RIG_DISK:-$HOME}
hold=${RIG_HOLD:-80}

parts=" ${RIG_PARTS:-compile write hold} "

case $1 in
start)
	rm -rf "$work"
	mkdir -p "$work" || exit 1
	: >"$work/pids"
	n=0
	case $parts in *" compile "*) n=$("$rig" cpus) || exit 1 ;; esac
	echo "$n" >"$work/loops"
	i=0
	while [ "$i" -lt "$n" ]; do
		mkdir -p "$work/c$i"
		echo "compile: loop $i, 0 builds" >"$work/c$i.count"
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
	case $parts in *" write "*)
		"$rig" write "$disk" "${RIG_WRITE_MIB:-1024}" >"$work/write.log" 2>&1 &
		echo $! >>"$work/pids"
		;;
	esac
	case $parts in *" hold "*)
		"$rig" hold "$hold" >"$work/hold.log" 2>&1 &
		echo $! >>"$work/pids"
		;;
	esac
	echo "load: started:$parts($n compile loops, the writer to $disk, memory held at $hold%)"
	;;
stop)
	: >"$work/stop"
	# The loops, the writer and the holder end at once; a compiler already
	# running finishes its build and is not started again.
	for pid in $(cat "$work/pids"); do
		kill "$pid" 2>/dev/null
	done
	rm -f "$disk/rig-write.bin"
	# What each part did, and a compiler's complaint if a loop stopped on one.
	n=$(cat "$work/loops")
	i=0
	while [ "$i" -lt "$n" ]; do
		cat "$work/c$i.count" "$work/c$i.log"
		i=$((i + 1))
	done
	for log in hold write; do
		[ -e "$work/$log.log" ] && cat "$work/$log.log"
	done
	echo "load: stopped"
	;;
*)
	echo "usage: load.sh start | stop" >&2
	exit 2
	;;
esac
