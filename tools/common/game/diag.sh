#!/bin/sh
# Diagnostic sampler (not for landing): every 2 s, N samples.
n=${1:-30}
i=0
while [ "$i" -lt "$n" ]; do
	free=$(grep MemFree /proc/meminfo)
	echo "diag: s=$i $(cat /proc/uptime) | $free"
	cat /proc/ferrix-seam | sed 's/^/diag: /'
	line=""
	for d in /proc/[0-9]*; do
		s=$(cat "$d/stat" 2>/dev/null) || continue
		case "$s" in *"(rustc)"*|*"(rig)"*) ;; *) continue ;; esac
		set -- $s
		rss=$(grep VmRSS "$d/status" 2>/dev/null | tr -s ' \t' ' ' | cut -d' ' -f2)
		line="$line $1:$2:$3:u${14}:s${15}:mj${12}:r$rss"
	done
	echo "diag: procs$line"
	c=$(cat /tmp/rig-load/c0.count /tmp/rig-load/c1.count /tmp/rig-load/c2.count /tmp/rig-load/c3.count 2>/dev/null | tr '\n' ';')
	echo "diag: counts $c"
	i=$((i + 1))
	sleep 2
done
