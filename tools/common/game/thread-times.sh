#!/bin/sh
# thread-times.sh -- a per-thread sample of running programs' processor time.
#
# Usage: thread-times.sh PATTERN [SECONDS] [ROUNDS]
#
# Every process whose command line matches PATTERN (an awk extended regular
# expression, e.g. 'Endfield|GRYPHLINK|wine') has each of its threads read
# twice from /proc, SECONDS apart (default 5), ROUNDS times (default 1).
# For each thread the round prints, busiest first:
#
#   usr%  sys%   share of one processor in user mode and in the kernel
#   vol/s inv/s  voluntary and involuntary switches a second
#   pid tid      and the thread's name (comm)
#
# then one line per process and the machine's own split from /proc/stat.
# utime and stime are /proc/<pid>/task/<tid>/stat's fields 14 and 15 in
# clock ticks of 1/100 s; the switch counts are status's last two lines.
#
# On Ferrix (docs/THREAD-TIMES.md) stime is counted at every entry from user
# mode and return, and switches by why the task left the processor. Run the
# same script on the Linux baseline for the comparison. Only POSIX sh, awk,
# cat and sleep: busybox is enough. Reading /proc/*/task/*/stat of another
# user's process needs nothing special, as on Linux.
set -u
PATTERN=${1:?usage: thread-times.sh PATTERN [SECONDS] [ROUNDS]}
SECONDS_APART=${2:-5}
ROUNDS=${3:-1}
HZ=100

# One snapshot: a line per thread, "pid tid utime stime vol invol comm",
# and one "cpu user nice system idle iowait irq softirq" line.
snapshot() {
    for d in /proc/[0-9]*; do
        pid=${d#/proc/}
        [ -r "$d/cmdline" ] || continue
        cmd=$(tr '\0' ' ' < "$d/cmdline" 2>/dev/null)
        [ -n "$cmd" ] || cmd=$(cat "$d/comm" 2>/dev/null)
        printf '%s\n' "$cmd" | awk -v p="$PATTERN" '$0 ~ p && $0 !~ /thread-times[.]sh/ { found = 1 } END { exit !found }' || continue
        for t in "$d"/task/[0-9]*; do
            tid=${t##*/}
            stat=$(cat "$t/stat" 2>/dev/null) || continue
            status=$(cat "$t/status" 2>/dev/null) || continue
            printf '%s\n%s\n' "$stat" "$status" | awk -v pid="$pid" -v tid="$tid" '
                NR == 1 {
                    lp = index($0, "("); rp = 0
                    for (i = length($0); i > 0; i--) if (substr($0, i, 1) == ")") { rp = i; break }
                    comm = substr($0, lp + 1, rp - lp - 1)
                    gsub(/ /, "_", comm)
                    n = split(substr($0, rp + 2), f, " ")
                    utime = f[12]; stime = f[13]
                    next
                }
                /^voluntary_ctxt_switches:/ { vol = $2 }
                /^nonvoluntary_ctxt_switches:/ { inv = $2 }
                END { printf "T %s %s %s %s %s %s %s\n", pid, tid, utime, stime, vol + 0, inv + 0, comm }'
        done
    done
    awk '$1 == "cpu" { print "C", $2, $3, $4, $5, $6, $7, $8 }' /proc/stat
}

round=1
while [ "$round" -le "$ROUNDS" ]; do
    a=$(snapshot)
    sleep "$SECONDS_APART"
    b=$(snapshot)
    printf '%s\n--\n%s\n' "$a" "$b" | awk -v secs="$SECONDS_APART" -v hz="$HZ" -v round="$round" '
        $0 == "--" { second = 1; next }
        $1 == "T" && !second { key = $2 " " $3; u0[key] = $4; s0[key] = $5; v0[key] = $6; i0[key] = $7; next }
        $1 == "T" && second {
            key = $2 " " $3
            if (!(key in u0)) next
            du = ($4 - u0[key]) * 100 / (hz * secs); ds = ($5 - s0[key]) * 100 / (hz * secs)
            dv = ($6 - v0[key]) / secs; di = ($7 - i0[key]) / secs
            n++; line[n] = sprintf("%6.1f %6.1f %8.1f %8.1f  %7s %7s  %s", du, ds, dv, di, $2, $3, $8)
            busy[n] = du + ds
            pu[$2] += du; ps[$2] += ds; pv[$2] += dv; pi[$2] += di; pn[$2]++
            next
        }
        $1 == "C" && !second { for (i = 2; i <= 8; i++) c0[i] = $i; next }
        $1 == "C" && second { for (i = 2; i <= 8; i++) c1[i] = $i; next }
        END {
            printf "== round %d, %d s: per thread, busiest first (%% of one processor)\n", round, secs
            printf "%6s %6s %8s %8s  %7s %7s  %s\n", "usr%", "sys%", "vol/s", "inv/s", "pid", "tid", "comm"
            for (i = 1; i <= n; i++) order[i] = i
            for (i = 2; i <= n; i++) {
                j = i; while (j > 1 && busy[order[j - 1]] < busy[order[j]]) { t = order[j]; order[j] = order[j - 1]; order[j - 1] = t; j-- }
            }
            for (i = 1; i <= n; i++) print line[order[i]]
            printf "-- per process\n"
            for (p in pn) printf "%6.1f %6.1f %8.1f %8.1f  %7s %7d threads\n", pu[p], ps[p], pv[p], pi[p], p, pn[p]
            total = 0; for (i = 2; i <= 8; i++) total += c1[i] - c0[i]
            if (total > 0)
                printf "-- machine: user %.1f%% nice %.1f%% system %.1f%% idle %.1f%% iowait %.1f%% irq %.1f%% softirq %.1f%% of all processors\n",
                    (c1[2] - c0[2]) * 100 / total, (c1[3] - c0[3]) * 100 / total, (c1[4] - c0[4]) * 100 / total,
                    (c1[5] - c0[5]) * 100 / total, (c1[6] - c0[6]) * 100 / total, (c1[7] - c0[7]) * 100 / total,
                    (c1[8] - c0[8]) * 100 / total
        }'
    round=$((round + 1))
done
