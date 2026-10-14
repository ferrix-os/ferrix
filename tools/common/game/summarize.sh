#!/bin/bash
# summarize.sh <prefix>: median/min of <prefix>.swap (swap-fps lines) and <prefix>.webgl (webgl-fps
# lines), compositor frames/s from <prefix>.serial (hyprix lines), CPU from <prefix>.cpu0/.cpu1.
p=$1
stat() { sort -n | awk '{a[NR]=$1} END{ if(!NR){print "n=0"; exit} m=(NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2; printf "n=%d median=%s min=%s max=%s\n",NR,m,a[1],a[NR]}'; }
[ -s "$p.swap" ] && { echo "swap-fps:          $(awk '/^swap-fps/{print $2}' "$p.swap" | stat)"; echo "raw motion ev/s:   $(awk '/^swap-fps/{print $8}' "$p.swap" | stat)"; }
[ -s "$p.webgl" ] && echo "webgl-fps:         $(grep -ao 'webgl-fps [0-9.]*' "$p.webgl" | awk '{print $2}' | stat)"
if [ -s "$p.serial" ]; then
  grep -aq 'hyprix: frames' "$p.serial" && echo "hyprix frames/s:   $(grep -a 'hyprix: frames' "$p.serial" | awk '{for(i=1;i<=NF;i++) if($i=="last") print $(i+1)}' | stat)"
  grep -aq 'webgl-fps' "$p.serial" && echo "webgl-fps (serial): $(grep -ao 'webgl-fps [0-9.]*' "$p.serial" | awk '{print $2}' | stat)"
  echo "NVRM lines in window: $(grep -ac NVRM "$p.serial")"
fi
if [ -s "$p.cpu0" ] && [ -s "$p.cpu1" ]; then
  awk 'FNR==1{f++} f==1 && NF==4 {u[$1" "$2]=$3+$4} f==1 && NF==1 {t0=$1} f==2 && NF==1 {t1=$1}
       f==2 && NF==4 && (($1" "$2) in u) {d[$1]+=$3+$4-u[$1" "$2]}
       END{ dt=t1-t0; for(n in d) printf "cpu %-18s %6.1f %% of one core (over %d s)\n", n, d[n]/dt, dt }' "$p.cpu0" "$p.cpu1" | sort
fi
