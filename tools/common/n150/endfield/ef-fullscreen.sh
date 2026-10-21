#!/bin/busybox sh
# Keep Endfield's windows (the game and its UIPlatform sign-in pages)
# full-screen on the N150 desktop: the game opens each page as a new window
# and resizes its own, and hyprix then tiles it small again. Every 2 s,
# unless one of them is full-screen already, the newest is focused and made
# full-screen (2026-10-18; 2026-10-21: one instance, by address, the focus
# checked first, titles from EF_FS_TITLES, a log).
# hyprix's socket: /run/user/1000 since the desktop is ferrix's session
# (sessiond), /tmp before. Runs as root or as ferrix.
#   ef-fullscreen.sh          the loop (ef-gpu-run.sh starts it)
#   ef-fullscreen.sh once     one pass, then exit
#   ef-fullscreen.sh stop     end a running loop
bb=/bin/busybox
pidfile=/tmp/ef-fullscreen.pid
log=/tmp/ef-fullscreen.log
titles=${EF_FS_TITLES:-Endfield UIPlatform}
export XDG_RUNTIME_DIR=/run/user/1000
[ -d /run/user/1000/hypr ] || XDG_RUNTIME_DIR=/tmp
hc=$(command -v hyprctl) || hc=/bin/hyprctl
case "${1:-loop}" in
stop)
    p=$($bb cat $pidfile 2>/dev/null)
    [ -n "$p" ] && $bb kill "$p" 2>/dev/null && echo "ef-fullscreen: stopped $p"
    $bb rm -f $pidfile
    exit 0 ;;
loop)
    p=$($bb cat $pidfile 2>/dev/null)
    if [ -n "$p" ] && [ -d /proc/$p ] && $bb grep -q ef-fullscreen /proc/$p/cmdline 2>/dev/null; then
        echo "ef-fullscreen: already running as $p"; exit 0
    fi
    echo $$ > $pidfile ;;
once) ;;
*) echo "usage: ef-fullscreen.sh [loop|once|stop]"; exit 2 ;;
esac
pass() {
    # hyprix keeps one full-screen window per workspace: making a second one
    # full-screen ends the first's. So nothing is done while any window
    # named in $titles is full-screen; otherwise the newest of them (the
    # highest address) is made full-screen, by address.
    "$hc" clients 2>/dev/null | $bb awk -v want=" $titles " '
        /^Window / { a = $2; t = $0; sub(/^Window [^ ]+ -> /, "", t); sub(/:$/, "", t); next }
        /^	fullscreen: / {
            if (!index(want, " " t " ")) next
            if ($2 != "0") full = 1
            else if (length(a) > length(best) || (length(a) == length(best) && a > best)) { best = a; name = t }
        }
        END { if (!full && best != "") print best, name }
    ' | while read -r a t; do
        "$hc" dispatch focuswindow "address:0x$a" > /dev/null 2>&1
        # fullscreen toggles the focused window: only if the focus went there.
        if "$hc" activewindow 2>/dev/null | $bb grep -q "^Window $a "; then
            "$hc" dispatch fullscreen 1 > /dev/null 2>&1
            echo "$($bb date +%T) full-screen: $a $t" >> $log
        else
            echo "$($bb date +%T) not focused, left: $a $t" >> $log
        fi
    done
}
if [ "${1:-loop}" = once ]; then pass; exit 0; fi
trap '$bb rm -f $pidfile; exit 0' INT TERM
while :; do
    pass
    $bb sleep 2
done
