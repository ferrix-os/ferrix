#!/bin/busybox sh
# ferrix.init= of Ferrix's level-4 card (docs/BOARD-BENCH.md, B5), the twin
# of Linux's tools/common/bench/board/linux/posix-init.sh: lmbench and
# speedtest1, the same static binaries and run scripts, under the same Alpine
# busybox as /bin/sh. `#!/bin/busybox sh`, not `#!/bin/sh`: on a Ferrix image
# with zinc, /bin/sh is zinc. Ends like every board bench: `board-bench end
# posix`, then a reset with no hand at the board. MEASUREMENT ONLY.
export PATH=/bin:/sbin
/bin/busybox mkdir -p /proc /sys /tmp /dev /sbin
/bin/busybox --install -s /bin 2>/dev/null
# Ferrix's kernel mounts these itself; Linux's init mounts them. Only what is
# not mounted yet, so neither is mounted twice.
grep -qs ' /proc ' /proc/mounts || mount -t proc proc /proc
grep -qs ' /sys ' /proc/mounts || mount -t sysfs sysfs /sys
grep -qs ' /dev ' /proc/mounts || mount -t devtmpfs devtmpfs /dev 2>/dev/null
grep -qs ' /tmp ' /proc/mounts || mount -t tmpfs tmpfs /tmp
# ST_SIZE= and LMB_ROUNDS= on the command line, as Linux hands them to init.
for word in $(cat /proc/cmdline); do
	case $word in
	ST_SIZE=*|LMB_ROUNDS=*) export "$word" ;;
	esac
done
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
