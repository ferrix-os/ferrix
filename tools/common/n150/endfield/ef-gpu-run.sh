#!/bin/busybox sh
# Endfield on the N150's iGPU, started and reported in one place (root).
# The five scripts of tools/common/n150/endfield/ go to /data on the box:
# ef-gpu-run.sh, ef-start-anv.sh, ef-player-anv.sh, ef-fullscreen.sh and
# ef-settings.sh (docs/INTEL-GPU.md, section 10, M5's box run).
#
#   ef-gpu-run.sh start    settings, watcher, GRYPHLINK in the split
#                          (launcher on lavapipe, Endfield.exe on anv), then
#                          `watch` until EF_WAIT seconds have passed
#   ef-gpu-run.sh watch    a report line every EF_EVERY s until EF_WAIT
#   ef-gpu-run.sh report   one full report now
#   ef-gpu-run.sh stop     the watcher stopped; with "stop all" also the
#                          prefix's wineserver (the launcher and the game)
#
# The report says: the launcher's and the game's processes; who holds the
# render node open (in the split every DXVK process opens it once, because
# anv enumerates its device in each; only the game should draw on it); the
# game's window in hyprix; the DXVK device each program chose, from DXVK's
# logs; and the game's frame rate from Wine's fps channel in proton.out.
#
# Env: EF_RES (1280x800; "keep" leaves the game's settings alone), EF_WAIT
# (1200), EF_EVERY (15), EF_FULLSCREEN (1), and EF_VK, EF_GAME_DXVK,
# EF_HUD, EF_PROTON_LOG for the player (ef-player-anv.sh). Output goes to
# stdout and to /home/ferrix/plog/gpu-report.txt.
bb=/bin/busybox
plog=/home/ferrix/plog
pfx=/home/ferrix/cd-dwp/pfx
out=$plog/gpu-report.txt
dxlog=$plog/dxvk
game=Endfield.exe
title=${EF_TITLE:-Endfield}
[ "$($bb id -u)" = 0 ] || { echo "ef-gpu-run.sh: run as root"; exit 1; }

say() { echo "$*"; echo "$*" >> $out; }

# The pids whose program is $1: comm first (Wine names the process after
# the exe), else argv[0]'s last path element (Wine's Windows path).
pids_of() {
    for p in /proc/[0-9]*; do
        c=$($bb cat $p/comm 2>/dev/null) || continue
        if [ "$c" = "$1" ]; then echo ${p#/proc/}; continue; fi
        a=$( { $bb tr '\0' '\n' < $p/cmdline; } 2>/dev/null | $bb head -n 1)
        case "$a" in *\\"$1"|*/"$1"|"$1") echo ${p#/proc/} ;; esac
    done
}

# "pid:comm" of every process that holds a render node open.
render_holders() {
    for p in /proc/[0-9]*; do
        for f in $p/fd/*; do
            case "$($bb readlink $f 2>/dev/null)" in
            /dev/dri/renderD*) echo "${p#/proc/}:$($bb cat $p/comm 2>/dev/null)"; break ;;
            esac
        done
    done | $bb tr '\n' ' '
}

# The device DXVK chose in one log: the "Found device" lines not followed
# by a "Skipping" line.
dxvk_choice() {
    $bb awk '
        /Found config file:/ { conf = 1 }
        /Found device: / { if (d != "") chosen = chosen d "; "; d = $0; sub(/.*Found device: /, "", d); next }
        /Skipping: / { if (d != "") { skipped = skipped d " (" $NF ")" "; "; d = "" } next }
        /^err: / { errs++ }
        END {
            if (d != "") chosen = chosen d
            sub(/; $/, "", chosen); sub(/; $/, "", skipped)
            printf "%s chose [%s]%s%s\n", (conf ? "config read," : "NO config file,"), chosen, (skipped ? " skipped [" skipped "]" : ""), (errs ? " errs " errs : "")
        }' "$1"
}

window_of() {
    XDG_RUNTIME_DIR=/run/user/1000
    [ -d /run/user/1000/hypr ] || XDG_RUNTIME_DIR=/tmp
    XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR hyprctl clients 2>/dev/null | $bb awk -v want="$1" '
        /^Window / { t = $0; sub(/^Window [^ ]+ -> /, "", t); sub(/:$/, "", t); on = (index(t, want) == 1); next }
        on && /^\t(at|size|fullscreen|mapped|visible): / { sub(/^\t/, ""); w[t] = w[t] " " $0 }
        END { for (t in w) printf "[%s%s] ", t, w[t] }'
}

last_fps() {
    $bb grep -a 'trace:fps' $plog/proton.out 2>/dev/null | $bb tail -n 1 | $bb sed 's/.*@ approx/approx/'
}

line() {
    t=$($bb date +%T)
    l=$(pids_of Launcher.exe | $bb tr '\n' ' ')
    g=$(pids_of Games.exe | $bb tr '\n' ' ')
    e=$(pids_of $game | $bb tr '\n' ' ')
    r=$(render_holders)
    gr=no
    for p in $e; do case " $r" in *" $p:"*) gr=yes ;; esac; done
    a=-
    for f in $dxlog/Endfield_dxgi.log $dxlog/Endfield_d3d11.log; do
        [ -s $f ] && { a=$(dxvk_choice $f); break; }
    done
    say "$t load $($bb cut -d' ' -f1-3 /proc/loadavg) | Launcher [$l] Games [$g] Endfield [$e] render-open-by-game $gr | dxvk $a | window $(window_of "$title") | fps $(last_fps)"
}

report() {
    say "=== ef-gpu-run report $($bb date '+%F %T')"
    say "status: $($bb tr '\n' ' ' < $plog/status 2>/dev/null)"
    say "render nodes: $($bb ls -l /dev/dri 2>/dev/null | $bb grep render | $bb tr -s ' ' | $bb tr '\n' ' ')"
    say "held open by: $(render_holders)"
    for name in Launcher.exe Games.exe $game; do
        say "$name: $(pids_of $name | $bb tr '\n' ' ')"
    done
    say "DXVK config ($(ls /tmp/ferrix-run/ef-dxvk.conf 2>/dev/null)):"
    [ -r /tmp/ferrix-run/ef-dxvk.conf ] && while read -r l; do say "    $l"; done < /tmp/ferrix-run/ef-dxvk.conf
    for f in $dxlog/*.log; do
        # The device lines are in the dxgi log (d3d11 and d3d9 log their own).
        $bb grep -q 'Found device: ' "$f" 2>/dev/null || continue
        say "DXVK $($bb basename $f): $(dxvk_choice $f)"
    done
    say "game window: $(window_of "$title")"
    say "other windows: $(window_of UIPlatform)$(window_of GRYPHLINK)"
    n=$($bb grep -a -c 'trace:fps' $plog/proton.out 2>/dev/null)
    say "fps lines: ${n:-0}; last 5:"
    $bb grep -a 'trace:fps' $plog/proton.out 2>/dev/null | $bb tail -n 5 | while read -r l; do say "    $l"; done
    say "proton.out tail:"
    $bb grep -a -v 'trace:fps' $plog/proton.out 2>/dev/null | $bb tail -n 15 | while read -r l; do say "    $l"; done
    say "load: $($bb cat /proc/loadavg)"
}

watch() {
    limit=${EF_WAIT:-1200} every=${EF_EVERY:-15} elapsed=0
    while [ $elapsed -lt $limit ]; do
        line
        $bb sleep $every
        elapsed=$((elapsed + every))
    done
    report
}

wine_running() {
    for f in /proc/[0-9]*/cmdline; do
        a=$( { $bb tr '\0' '\n' < "$f"; } 2>/dev/null | $bb head -n 1)
        case "$a" in *Endfield.exe|*Launcher.exe|*Games.exe|*/wineserver|wineserver) return 0 ;; esac
    done
    return 1
}

case "${1:-report}" in
start)
    if wine_running; then echo "ef-gpu-run.sh: the launcher, the game or a wineserver is running; stop them first"; exit 3; fi
    for f in /data/ef-start-anv.sh /data/ef-player-anv.sh /data/ef-fullscreen.sh; do
        [ -x $f ] || { echo "ef-gpu-run.sh: no $f"; exit 2; }
    done
    mkdir -p $plog && chown 1000:1000 $plog
    [ -s $out ] && mv $out $out.$($bb date +%Y%m%d-%H%M%S)
    say "=== ef-gpu-run start $($bb date '+%F %T') EF_VK=${EF_VK:-split} EF_RES=${EF_RES:-1280x800}"
    res=${EF_RES:-1280x800}
    if [ "$res" != keep ]; then
        if [ -r /data/ef-settings.sh ]; then
            w=${res%x*} h=${res#*x}
            su ferrix -c "WINEPREFIX=$pfx PROTON=/data/dwproton OUT=$plog/settings-before /bin/busybox sh /data/ef-settings.sh dump" > $plog/settings-before.txt 2>&1
            su ferrix -c "WINEPREFIX=$pfx PROTON=/data/dwproton /bin/busybox sh /data/ef-settings.sh set $w $h" 2>&1 | while read -r l; do say "settings: $l"; done
        else
            say "settings: no /data/ef-settings.sh, the game's own"
        fi
    fi
    if [ "${EF_FULLSCREEN:-1}" = 1 ]; then
        $bb setsid /data/ef-fullscreen.sh > /dev/null 2>&1 < /dev/null &
        say "watcher: started (log /tmp/ef-fullscreen.log)"
    fi
    $bb setsid /data/ef-start-anv.sh > /dev/null 2>&1 < /dev/null &
    say "GRYPHLINK: started; start the game in the launcher (Start), then this reports"
    watch
    ;;
watch) watch ;;
report) report ;;
stop)
    /data/ef-fullscreen.sh stop
    if [ "${2:-}" = all ]; then
        su ferrix -c "WINEPREFIX=$pfx /data/dwproton/files/bin/wineserver -k"
        echo "ef-gpu-run.sh: wineserver -k sent"
    fi
    ;;
*) echo "usage: ef-gpu-run.sh start | watch | report | stop [all]"; exit 2 ;;
esac
