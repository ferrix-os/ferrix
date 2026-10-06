#!/bin/bash
# po7-linux: alternate Linux (default, mitigations=off), Ferrix main
# (bench-ipc, mitigations on and off) and seL4 (matched, matched w/o refill),
# rounds $1..$2, each run only when the 1-minute load is <= 4.
B=$HOME/.local/share/ferrix/linux-bench
L=$HOME/.local/share/ferrix/logs/po7-linux
W=/home/sebastian/Documents/projects/os/ferrix/.claude/worktrees/po7-linux
S=$HOME/.local/share/ferrix/sel4
INIT="$HOME/.local/share/ferrix/busybox/{arch}/bin/busybox.static"
quiet() {
  while awk '{exit !($1 > 4.0)}' /proc/loadavg; do
    echo "# $(date -Is) waiting: load $(cut -d' ' -f1-3 /proc/loadavg)"; sleep 30
  done
}
ferrix() { # tag, on|off
  quiet
  local log=$L/$1.log
  echo "# $(date -Is) ferrix main $(git -C $W rev-parse --short=9 HEAD) mitigations=$2 loadavg=$(cut -d' ' -f1-3 /proc/loadavg)" > $log.meta
  ( cd $W && CARGO_TARGET_DIR=$W/target timeout 1800 cargo xtask bench-ipc --release --accel kvm --smp 1 --mitigations $2 --init "$INIT" ) > $log 2>&1
  echo "# end status=$? loadavg=$(cut -d' ' -f1-3 /proc/loadavg)" >> $log.meta
  echo "R $1 $(grep -m1 'x86_64: host load' $log | sed 's/.*x86_64: //')"
  grep 'x86_64: ipc-bench \(floor\|domain-call\|call\)' $log | sed "s/^/R $1 /"
}
for r in $(seq "$1" "$2"); do
  quiet; echo "R r$r-linux-default load $(cut -d' ' -f1 /proc/loadavg)"; $B/run.sh r$r-linux-default default | grep '^LB name' | grep -v 'rep=[1234] ' | sed "s/^/R r$r-linux-default /"
  ferrix r$r-ferrix-on on
  quiet; echo "R r$r-linux-off load $(cut -d' ' -f1 /proc/loadavg)"; $B/run.sh r$r-linux-off off | grep '^LB name' | grep -v 'rep=[1234] ' | sed "s/^/R r$r-linux-off /"
  ferrix r$r-ferrix-off off
  for b in matched-nopcid norsb-nopcid; do
    quiet; echo "R r$r-sel4-$b load $(cut -d' ' -f1 /proc/loadavg)"
    $S/run.sh $b nopcid po7linux-r$r-$b 11 > /dev/null 2>&1
    cp $S/logs/po7linux-r$r-$b.log $L/r$r-sel4-$b.log; cp $S/logs/po7linux-r$r-$b.log.meta $L/r$r-sel4-$b.log.meta
    grep -a 'call-replyrecv-roundtrip' $L/r$r-sel4-$b.log | sed 's/\x1b\[[0-9;]*m//g' | sed "s/^/R r$r-sel4-$b /"
  done
done
echo "R series done $(date -Is)"
