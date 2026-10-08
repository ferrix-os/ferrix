#!/bin/sh
# run-speedtest1.sh: SQLite's speedtest1, BOARD-BENCH level 4, on an
# in-memory database and on a file database in a RAM-backed directory.
#
#   run-speedtest1.sh [BIN_DIR]
#
# BIN_DIR holds speedtest1 (default: ./bin beside this script). Environment,
# each printed in the header:
#   ST_SIZE    speedtest1 --size (default 60; README.md says how it was
#              chosen for a 650 MHz Cortex-A7)
#   ST_DIR     the RAM-backed directory for the file database (default /tmp)
#   ST_ROUNDS  passes over both modes (default 1)
#   ST_MODES   which of "memdb file" to run (default both)
# Both modes run the default test set ("main") with speedtest1's defaults
# otherwise: journal_mode DELETE, synchronous FULL, the default page cache.
#
# Output, paired across kernels by <tag>:
#   speedtest1 <mode>.<test> <seconds> s round=<r>    one per test, from
#                                                       speedtest1's own lines
#   speedtest1 <mode>.total <seconds> s round=<r>
#   speedtest1 <mode> FAIL status=<s> round=<r>
# and every line speedtest1 printed, as
#   speedtest1-raw <mode> <line>
# Builtins only for the parsing; the external commands are rm and uname.

set -u
set -f

HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${1:-$HERE/bin}
SIZE=${ST_SIZE:-60}
DIR=${ST_DIR:-/tmp}
ROUNDS=${ST_ROUNDS:-1}
MODES=${ST_MODES:-memdb file}
FAILURES=0
ROUND=0
DB=$DIR/speedtest1.db

printf 'speedtest1-info uname=%s\n' "$(uname -a 2>/dev/null)"
printf 'speedtest1-info bin=%s size=%s dir=%s rounds=%s modes=%s\n' "$BIN" "$SIZE" "$DIR" "$ROUNDS" "$MODES"
if [ -r /proc/mounts ]; then
	while read -r dev mnt type rest; do
		[ "$mnt" = "$DIR" ] && printf 'speedtest1-info dir-fs=%s (%s)\n' "$type" "$dev"
	done </proc/mounts
fi

run_mode() {
	mode=$1
	case $mode in
	memdb) set -- --memdb ;;
	file)
		rm -f "$DB" "$DB-journal" "$DB-wal" "$DB-shm"
		set -- "$DB"
		;;
	*) printf 'speedtest1-info unknown mode %s\n' "$mode"; return ;;
	esac
	out=$("$BIN/speedtest1" --size "$SIZE" --testset main "$@" 2>&1)
	status=$?
	total=
	printf '%s\n' "$out" | while IFS= read -r line; do
		[ -n "$line" ] && printf 'speedtest1-raw %s %s\n' "$mode" "$line"
	done
	# "<test> - <description>....... <seconds>s", and "TOTAL....... <s>s".
	tests=$(printf '%s\n' "$out" | while read -r first second rest; do
		[ -n "$rest" ] || continue
		last=
		for w in $rest; do last=$w; done
		case $first in
		[0-9]*) [ "$second" = - ] && printf 'speedtest1 %s.%s %s s round=%s\n' "$mode" "$first" "${last%s}" "$ROUND" ;;
		esac
	done)
	[ -n "$tests" ] && printf '%s\n' "$tests"
	for w in $out; do
		case $w in TOTAL*) total=next ;; *s) [ "$total" = next ] && total=${w%s} ;; esac
	done
	if [ "$status" -ne 0 ] || [ -z "$total" ] || [ "$total" = next ]; then
		printf 'speedtest1 %s FAIL status=%s round=%s\n' "$mode" "$status" "$ROUND"
		FAILURES=$((FAILURES + 1))
	else
		printf 'speedtest1 %s.total %s s round=%s\n' "$mode" "$total" "$ROUND"
	fi
	[ "$mode" = file ] && rm -f "$DB" "$DB-journal" "$DB-wal" "$DB-shm"
}

while [ "$ROUND" -lt "$ROUNDS" ]; do
	ROUND=$((ROUND + 1))
	for mode in $MODES; do
		run_mode "$mode"
	done
done
printf 'speedtest1-done failures=%s rounds=%s\n' "$FAILURES" "$ROUNDS"
