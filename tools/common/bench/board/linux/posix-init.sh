#!/bin/busybox sh
# /init of Linux's level-4 initramfs (docs/BOARD-BENCH.md, B5): lmbench and
# speedtest1, the same static binaries and run scripts Ferrix's image runs,
# under the same Alpine busybox as /bin/sh. Ends like every board bench:
# `board-bench end posix`, then a reset with no hand at the board.
export PATH=/bin:/sbin
/bin/busybox mkdir -p /proc /sys /tmp /dev /sbin
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null
mount -t tmpfs tmpfs /tmp
D=/opt/posixbench
echo "posix info kernel: $(uname -srvm)"
echo "posix info cmdline: $(cat /proc/cmdline)"
echo "posix info cpus online: $(grep -c ^processor /proc/cpuinfo)"
echo "posix info binsh: $(readlink /bin/sh)"
echo "posix info tmp: $(grep ' /tmp ' /proc/mounts)"
LMB_ROUNDS=${LMB_ROUNDS:-1} LMB_TMP=/tmp sh $D/run-lmbench.sh $D/bin 2>&1
echo "posix lmbench-exit $?"
ST_SIZE=${ST_SIZE:-60} ST_DIR=/tmp sh $D/run-speedtest1.sh $D/bin 2>&1
echo "posix speedtest1-exit $?"
echo "board-bench end posix"
sleep 1
reboot -f
