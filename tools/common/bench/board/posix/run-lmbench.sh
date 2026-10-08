#!/bin/sh
# run-lmbench.sh: BOARD-BENCH level 4's lmbench set, with fixed arguments,
# for Linux 7.2.9 and Ferrix on the DK1 alike (README.md says what each
# image runs).
#
#   run-lmbench.sh [BIN_DIR]
#
# BIN_DIR holds the programs build.sh installs (default: ./bin beside this
# script). Environment, each printed in the header:
#   LMB_ROUNDS  passes over the whole set (default 1)
#   LMB_TMP     a RAM-backed directory for the scratch files (default /tmp)
#   LMB_ENOUGH  microseconds per timed sample, lmbench's ENOUGH (default
#               100000); fixed so both kernels time alike rather than each
#               choosing its own interval
#   LMB_ONLY    a space-separated list of programs to run (default all)
# Every program runs -P 1 -N 11: one process, the median of 11 samples,
# lmbench's own default count, given explicitly.
#
# Output, one line per result, paired across kernels by <tag>:
#   lmbench <tag> <value> <unit> round=<r>
#   lmbench <tag> FAIL status=<s> round=<r>
# and every line a program printed, as
#   lmbench-raw <tag> <line>
# Builtins only for the parsing, so busybox ash, dash or bash run it; the
# external commands are cp, chmod, dd, rm and uname.

set -u
set -f

HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${1:-$HERE/bin}
ROUNDS=${LMB_ROUNDS:-1}
TMP=${LMB_TMP:-/tmp}
ENOUGH=${LMB_ENOUGH:-100000}
ONLY=${LMB_ONLY:-}
export ENOUGH
# lmbench's own overhead corrections: measured by each program at start, as
# lmbench does when they are not given.
unset LOOP_O TIMING_O LMBENCH_SCHED 2>/dev/null || true

FILE=$TMP/XXX
STAT=$TMP/lmbench-stat
REPS="-P 1 -N 11"
FAILURES=0
ROUND=0
INVALID=

wanted() {
	[ -z "$ONLY" ] && return 0
	for w in $ONLY; do
		[ "$w" = "$1" ] && return 0
	done
	return 1
}

raw() {
	tag=$1
	shift
	printf '%s\n' "$*" | while IFS= read -r line; do
		[ -n "$line" ] && printf 'lmbench-raw %s %s\n' "$tag" "$line"
	done
}

# The word before $2 in the text $1, or nothing.
word_before() {
	prev=
	for w in $1; do
		if [ "$w" = "$2" ]; then
			printf '%s' "$prev"
			return
		fi
		prev=$w
	done
}

# The last word of the text $1.
last_word() {
	last=
	for w in $1; do
		last=$w
	done
	printf '%s' "$last"
}

# run_one <tag> <kind> <program> <args...>: run, keep the raw lines, print
# the result. kind: us (a "... N microseconds" line), mbs (a "N MB/sec"
# line), last-us / last-mbs (the last word of the output, in us or MB/s).
run_one() {
	tag=$1
	kind=$2
	prog=$3
	shift 3
	out=$("$BIN/$prog" "$@" 2>&1)
	status=$?
	raw "$tag" "$out"
	case $kind in
	us) value=$(word_before "$out" microseconds); unit=us ;;
	mbs) value=$(word_before "$out" MB/sec); unit=MB/s ;;
	last-us) value=$(last_word "$out"); unit=us ;;
	last-mbs) value=$(last_word "$out"); unit=MB/s ;;
	esac
	if [ -n "$INVALID" ]; then
		# The program gave a figure, but a check says it timed a failure.
		printf 'lmbench %s FAIL status=%s round=%s\n' "$tag" "$INVALID" "$ROUND"
		FAILURES=$((FAILURES + 1))
	elif [ "$status" -ne 0 ] || [ -z "$value" ]; then
		printf 'lmbench %s FAIL status=%s round=%s\n' "$tag" "$status" "$ROUND"
		FAILURES=$((FAILURES + 1))
	else
		printf 'lmbench %s %s %s round=%s\n' "$tag" "$value" "$unit" "$ROUND"
	fi
}

# lat_ctx prints a header and then "<processes> <us>" per process count.
run_ctx() {
	tag=$1
	shift
	out=$("$BIN/lat_ctx" $REPS "$@" 2>&1)
	status=$?
	raw "$tag" "$out"
	value=
	prev=
	for w in $out; do
		[ "$prev" = 2 ] && value=$w
		prev=$w
	done
	if [ "$status" -ne 0 ] || [ -z "$value" ]; then
		printf 'lmbench %s FAIL status=%s round=%s\n' "$tag" "$status" "$ROUND"
		FAILURES=$((FAILURES + 1))
	else
		printf 'lmbench %s %s us round=%s\n' "$tag" "$value" "$ROUND"
	fi
}

# lat_mem_rd prints "<range MB> <ns>" for every range up to its length: one
# result per range, tagged with it.
run_mem_rd() {
	len=$1
	stride=$2
	tag=lat_mem_rd.s$stride
	out=$("$BIN/lat_mem_rd" $REPS "$len" "$stride" 2>&1)
	status=$?
	raw "$tag" "$out"
	n=0
	printf '%s\n' "$out" | {
		while read -r range ns rest; do
			case $range in
			[0-9]*.[0-9]*)
				[ -n "$ns" ] && printf 'lmbench %s.%s %s ns round=%s\n' "$tag" "$range" "$ns" "$ROUND"
				;;
			esac
		done
	}
	for w in $out; do
		case $w in [0-9]*.[0-9]*) n=$((n + 1)) ;; esac
	done
	if [ "$status" -ne 0 ] || [ "$n" -eq 0 ]; then
		printf 'lmbench %s FAIL status=%s round=%s\n' "$tag" "$status" "$ROUND"
		FAILURES=$((FAILURES + 1))
	fi
}

info() {
	printf 'lmbench-info %s\n' "$*"
}

info "uname=$(uname -a 2>/dev/null)"
info "bin=$BIN rounds=$ROUNDS tmp=$TMP ENOUGH=$ENOUGH reps='$REPS' only='$ONLY'"
if [ -r /proc/mounts ]; then
	while read -r dev mnt type rest; do
		[ "$mnt" = "$TMP" ] && info "tmp-fs=$type ($dev)"
	done </proc/mounts
fi

# The scratch files: lat_proc execs /tmp/hello, as lmbench's scripts/lmbench
# copies it there; lat_pagefault and lat_mmap map an 8 MB file (lmbench's
# default MB=8); lat_syscall stats and opens a small file.
cp "$BIN/hello" /tmp/hello && chmod 755 /tmp/hello || info "could not copy hello to /tmp/hello"
dd if=/dev/zero of="$FILE" bs=65536 count=128 2>/dev/null || info "could not write $FILE"
: >"$STAT"

while [ "$ROUND" -lt "$ROUNDS" ]; do
	ROUND=$((ROUND + 1))
	if wanted lat_syscall; then
		for what in null read write; do
			run_one lat_syscall.$what us lat_syscall $REPS $what
		done
		for what in stat fstat open; do
			run_one lat_syscall.$what us lat_syscall $REPS $what "$STAT"
		done
	fi
	wanted lat_pipe && run_one lat_pipe us lat_pipe $REPS
	wanted lat_unix && run_one lat_unix us lat_unix $REPS
	if wanted lat_ctx; then
		run_ctx lat_ctx.s0.p2 -s 0 2
		run_ctx lat_ctx.s16.p2 -s 16 2
	fi
	if wanted lat_proc; then
		# lat_proc never reads its child's exit status, so a failed exec
		# would still give a figure; exec-check makes its calls once and
		# says whether hello ran.
		checks=$("$BIN/exec-check" 2>&1)
		raw exec-check "$checks"
		for what in fork exec shell; do
			case $what:$checks in
			exec:*"execve ok"* | shell:*"shell ok"* | fork:*) INVALID= ;;
			*) INVALID=exec-check ;;
			esac
			run_one lat_proc.$what us lat_proc $REPS $what
		done
		INVALID=
	fi
	if wanted lat_sig; then
		for what in install catch; do
			run_one lat_sig.$what us lat_sig $REPS $what
		done
	fi
	wanted lat_pagefault && run_one lat_pagefault us lat_pagefault $REPS "$FILE"
	if wanted lat_mmap; then
		# lat_mmap refuses (silently, status 1) a size under 320 KB.
		for size in 512k 2m 8m; do
			run_one lat_mmap.$size last-us lat_mmap $REPS $size "$FILE"
		done
	fi
	wanted bw_pipe && run_one bw_pipe mbs bw_pipe $REPS
	wanted bw_unix && run_one bw_unix mbs bw_unix $REPS
	# Ranges up to 32 MB at the Cortex-A7's 64-byte line: L1 (32 KB), L2
	# (256 KB on the STM32MP157) and DRAM, all from one run.
	wanted lat_mem_rd && run_mem_rd 32 64
	if wanted bw_mem; then
		for size in 16k 128k 8m; do
			for what in rd wr cp; do
				run_one bw_mem.$what.$size last-mbs bw_mem $REPS $size $what
			done
		done
	fi
done

rm -f /tmp/hello "$FILE" "$STAT"
printf 'lmbench-done failures=%s rounds=%s\n' "$FAILURES" "$ROUNDS"
