#!/bin/busybox sh
# zero-copy-soak.sh DIR [SECONDS]: on the N150, as root, with the desktop and
# yserver up. Runs tools/common/test/d3d11-triangle.c's program (DIR holds
# d3d11-triangle.exe, d3d11.dll and dxgi.dll as fetch-dxvk.sh makes them) in
# a Proton prefix (ZC_PREFIX, default Endfield's, /home/ferrix/cd-dwp/pfx,
# which must not be in use: a fresh prefix made by Proton's wine alone does
# not start the program), DXVK on anv, presenting through yserver's
# zero-copy path, for SECONDS (default 120). Half way it maximizes the
# window, as Yoru's title bar button does, and at three quarters restores it.
#
# The program presents far faster than hyprix repaints, so yserver replaces
# owed presents in the pass they were owed in, hands others to hyprix, and
# gets buffers back by wl_buffer.release: every path a buffer can take back
# to the client. It passes when the program said a present count every
# second of the run (no gap of 5 s), presented at least 20 frames a second,
# followed the maximize (a `buffers` line at the maximized size), and ended
# by itself. It fails on a stall, which is what a buffer never made idle
# looks like (2026-10-28: an idle retired before its completion drained left
# the idle fence unfired and Endfield waiting for ever), and on a swap chain
# that cannot be made (`Present failed`; the same day, lavapipe could not
# import anv's 1920x974 buffers).
#
# Prints one verdict line, "zero-copy-soak: PASSED ..." or
# "zero-copy-soak: FAILED ...", and exits 0 or 1.
set -u
dir=${1:?usage: zero-copy-soak.sh DIR [SECONDS]}
seconds=${2:-120}
prefix=${ZC_PREFIX:-/home/ferrix/cd-dwp/pfx}
bb=/bin/busybox
hyprctl() { env XDG_RUNTIME_DIR=/run/user/1000 HYPRLAND_INSTANCE_SIGNATURE=ferrix /bin/hyprctl "$@"; }
for file in d3d11-triangle.exe d3d11.dll dxgi.dll; do
    [ -f "$dir/$file" ] || { echo "zero-copy-soak: FAILED: no $dir/$file"; exit 1; }
done
$bb ps -o args | $bb grep -a -q '[/]data/yserver/yserver' \
    || { echo "zero-copy-soak: FAILED: no yserver running"; exit 1; }
$bb ps -o args | $bb grep -a -q '[w]ineserver' \
    && { echo "zero-copy-soak: FAILED: a wineserver is running (the game?); end it first"; exit 1; }
out=$dir/soak.out
$bb chown -R 1000:1000 "$dir"
cat > "$dir/run.sh" <<EOF
#!/bin/busybox sh
export HOME=/home/ferrix USER=ferrix DISPLAY=:0 XDG_RUNTIME_DIR=/tmp/ferrix-run
export WINEPREFIX=$prefix PATH=/data/dwproton/files/bin:/usr/bin:/bin
export WINEDEBUG=-all VK_ICD_FILENAMES=/data/usr/share/vulkan/icd.d/intel_icd.json
export DXVK_LOG_LEVEL=warn DXVK_LOG_PATH=none WINEDLLOVERRIDES="d3d11,dxgi=n"
# Not paced to the refresh, as the box runs Endfield (ef-player-anv.sh):
# presents come faster than hyprix repaints, several in one pass of yserver.
export DXVK_CONFIG="dxgi.syncInterval = 0"
cd $dir
exec wine $dir/d3d11-triangle.exe ff8020 $seconds
EOF
$bb chmod 755 "$dir/run.sh"
$bb chown 1000:1000 "$dir/run.sh"
: > "$out"
$bb chown 1000:1000 "$out"
su ferrix -c "$bb setsid $dir/run.sh > $out 2>&1 < /dev/null &"

# The program's first present, after the prefix is made (a minute or two
# the first time).
waited=0
until $bb grep -q 'presenting for' "$out" || [ $waited -ge 300 ]; do
    $bb sleep 1
    waited=$((waited + 1))
done
$bb grep -q 'presenting for' "$out" || {
    echo "zero-copy-soak: FAILED: the program never presented ($($bb tail -1 "$out"))"
    exit 1
}
start=$($bb date +%s)
last=0 last_at=$start maximized= state=windowed gap=0
while :; do
    $bb sleep 1
    now=$($bb date +%s)
    t=$((now - start))
    count=$($bb grep -a -o 'presented=[0-9]*' "$out" | $bb tail -1 | $bb cut -d= -f2)
    count=${count:-0}
    if [ "$count" -gt "$last" ]; then
        last=$count last_at=$now
    elif [ $((now - last_at)) -gt "$gap" ]; then
        gap=$((now - last_at))
    fi
    if [ "$state" = windowed ] && [ "$t" -ge $((seconds / 2)) ]; then
        hyprctl dispatch focuswindow title:d3d11-triangle > /dev/null
        hyprctl dispatch snapwindow max > /dev/null
        state=maximized
        $bb sleep 3
        maximized=$(hyprctl clients | $bb grep -A6 '> d3d11-triangle' | $bb sed -n 's/.*size: \([0-9]*\),\([0-9]*\)/\1x\2/p')
    elif [ "$state" = maximized ] && [ "$t" -ge $((seconds * 3 / 4)) ]; then
        hyprctl dispatch focuswindow title:d3d11-triangle > /dev/null
        hyprctl dispatch snapwindow restore > /dev/null
        state=restored
    fi
    $bb grep -q 'presented=.*done\|Present failed\|failed:' "$out" && break
    [ "$t" -ge $((seconds + 60)) ] && break
done
said=$($bb grep -a -E 'Present failed|failed:' "$out" | $bb head -1)
rate=$((last / seconds))
if [ -n "$said" ]; then
    echo "zero-copy-soak: FAILED: $said"
    exit 1
elif ! $bb grep -q 'presented=.*done' "$out"; then
    echo "zero-copy-soak: FAILED: the program stalled at $last presents and never ended (longest gap ${gap} s)"
    exit 1
elif [ "$gap" -ge 5 ] || [ "$rate" -lt 20 ]; then
    echo "zero-copy-soak: FAILED: $last presents in $seconds s, $rate a second, longest gap ${gap} s"
    exit 1
elif [ -z "$maximized" ] || ! $bb grep -q "buffers $maximized" "$out"; then
    echo "zero-copy-soak: FAILED: maximized to '${maximized}', but the program's buffers are $($bb grep -a -o 'buffers [0-9x]*' "$out" | $bb tr '\n' ' ')"
    exit 1
fi
echo "zero-copy-soak: PASSED: $last presents in $seconds s ($rate a second), longest gap ${gap} s, followed the maximize to $maximized"
