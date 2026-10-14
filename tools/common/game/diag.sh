#!/bin/sh
# Diagnostic sampler (not for landing): every 2 s, N samples.
n=${1:-30}
i=0
while [ "$i" -lt "$n" ]; do
	m=$(grep -E '^(MemFree|MemAvailable|Cached|Dirty|Shmem):' /proc/meminfo | tr -s ' ' | tr '\n' ' ')
	echo "diag: s=$i $(cat /proc/uptime) | $m"
	cat /proc/ferrix-seam | sed 's/^/diag: /'
	c=$(cat /tmp/rig-load/c0.count /tmp/rig-load/c1.count /tmp/rig-load/c2.count /tmp/rig-load/c3.count 2>/dev/null | tr '\n' ';')
	echo "diag: counts $c"
	i=$((i + 1))
	sleep 2
done
