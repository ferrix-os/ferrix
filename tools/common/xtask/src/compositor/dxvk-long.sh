export PATH=/bin:/data/usr/bin HOME=/tmp/wine XDG_RUNTIME_DIR=/tmp
export WINEPREFIX=/tmp/wine/prefix WINESERVER=/usr/lib/wine/wineserver64
export WINEDEBUG="err+all,fixme-all"
export VK_ICD_FILENAMES=/intel/intel_icd.json MESA_VK_WSI_DEBUG=sw MESA_SHADER_CACHE_DISABLE=1
echo "dxvk: start"
waited=0
while [ ! -S /tmp/.X11-unix/X0 ] && [ $waited -lt 120 ]; do sleep 1; waited=$((waited + 1)); done
mkdir -p /tmp/wine
display=$DISPLAY
unset DISPLAY
/usr/lib/wine/wine64 wineboot -i > /tmp/wine/wineboot.log 2>&1
echo "dxvk: wineboot exited $?"
/usr/lib/wine/wineserver64 -w
/usr/lib/wine/wine64 reg add 'HKCU\Software\Wine\X11 Driver' /v UseXRandR /d N /f > /tmp/wine/reg.log 2>&1
/usr/lib/wine/wineserver64 -w
export DISPLAY=$display
echo "dxvk: window"
threads() {
    for p in /proc/[0-9]*; do
        c=$(cat $p/comm 2>/dev/null)
        case "$c" in
            *.exe|wine*)
                for t in $p/task/*; do
                    echo "dxvk: thread $(basename $p) $c / $(cat $t/comm 2>/dev/null) $(sed 's/.*) //' $t/stat 2>/dev/null | cut -d' ' -f1)"
                done ;;
        esac
    done
}
run() {
    name=$1 limit=$2 config=$3
    shift 3
    echo "dxvk: $name starts, limit ${limit}s, DXVK_CONFIG=$config"
    ( cd /long && DXVK_CONFIG="$config" WINEDLLOVERRIDES="d3d11,dxgi,d3d9=n" DXVK_LOG_LEVEL=info DXVK_LOG_PATH=none \
        /usr/lib/wine/wine64 "$@" > /tmp/$name.txt 2>&1 ) &
    pid=$!
    lines=0 still=0 t=0
    while kill -0 $pid 2>/dev/null; do
        sleep 10
        t=$((t + 10))
        n=$(grep -c . /tmp/$name.txt)
        if [ "$n" = "$lines" ]; then still=$((still + 10)); else still=0; lines=$n; fi
        if [ $((t % 60)) = 0 ]; then
            echo "dxvk: $name t=${t}s $(grep -E 't=|frames=' /tmp/$name.txt | tail -n 1)"
        fi
        if [ $still -ge 120 ] || [ $t -ge $limit ]; then
            echo "dxvk: $name STUCK after ${t}s, no output for ${still}s"
            threads
            ls -l /proc/[0-9]*/fd 2>/dev/null | grep -c renderD | sed "s/^/dxvk: $name render fds: /"
            /usr/lib/wine/wineserver64 -k
            kill -9 $pid 2>/dev/null
            break
        fi
    done
    wait $pid
    echo "dxvk: $name exited $? after ${t}s"
    grep -v '^info:    ' /tmp/$name.txt | grep -v 'lock_display\|winediag\|wineusb\|winebth\|:fixme:' | tail -n 30 | sed "s/^/dxvk: $name: /"
    /usr/lib/wine/wineserver64 -w
}
L=LONGSECS
run d3d9-default $((L + 300)) "dxgi.syncInterval = 0; d3d9.presentInterval = 0" /long/d3d9-spin.exe $L
run scene-present $((L + 300)) "dxgi.syncInterval = 0" /long/d3d11-scene.exe 640 360 $L present 2
run scene-offscreen $((L + 300)) "" /long/d3d11-scene.exe 640 360 $L offscreen 2
run d3d9-fifo 400 "" /long/d3d9-spin.exe 120
echo "dxvk: end"
