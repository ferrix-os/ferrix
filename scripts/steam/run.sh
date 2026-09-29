# `cargo xtask run-steam` and `test-steam-window` (docs/STEAM.md): started by
# hyprix's exec-once, as root. yserver on :0 as a Wayland client of hyprix,
# a lease for eth0, then client.sh as uid 1000; a watcher prints
# "steam-window: login window" once hyprix lists Steam's sign-in window.
export PATH=/usr/local/bin:/bin:/data/usr/bin HOME=/data/home USER=root LANG=C.UTF-8
echo "steam-window: start"
YSERVER_BACKEND=wayland YSERVER_ALLOW_SOFTWARE_VULKAN=1 RUST_LOG=${YRUST_LOG:-info} \
    /data/yserver/yserver :0 -nolisten tcp > /tmp/yserver.log 2>&1 &
waited=0
while [ ! -S /tmp/.X11-unix/X0 ] && [ $waited -lt 120 ]; do sleep 1; waited=$((waited + 1)); done
echo "steam-window: yserver's socket after ${waited}s"
if ! udhcpc -i eth0 -n -q -t 5 -T 2 > /dev/null 2>&1; then
    echo "steam-window: no lease for eth0"
    echo "steam-window: end"
    exit 1
fi
chown -R 1000:1000 /data/home /data/steam
(
    seen=
    while :; do
        clients=$(/bin/hyprctl clients 2>/dev/null)
        titles=$(echo "$clients" | grep -i 'title' | tr '\n' ' ')
        if [ "$titles" != "$seen" ]; then
            echo "steam-window: windows: $titles"
            seen=$titles
        fi
        case "$clients" in
            *'Sign in to Steam'*) echo "steam-window: login window"; break ;;
            *'Unexpected Transport Error'*)
                # The client refused its web helper's websocket, and stays up
                # showing so: say why now, not at an exit that never comes.
                # Its transport log names the pids it checked, and lsof, run
                # as the client runs it, whom it finds on the port.
                tail -n 40 /data/steam/logs/transport_client.txt 2>/dev/null \
                    | sed 's/^/steam-window: transport_client.txt: /'
                su -s /bin/sh ferrix -c '/usr/bin/lsof -P -F upnR -i TCP@127.0.0.1' 2>&1 \
                    | head -n 40 | sed 's/^/steam-window: lsof: /'
                ls -l /proc/*/fd 2>/dev/null | grep socket | head -n 20 \
                    | sed 's/^/steam-window: fd: /'
                break ;;
        esac
        sleep 3
    done
) &
su -p -s /bin/sh ferrix -c '/bin/busybox sh /steam/client.sh' 2>&1
for log in bootstrap_log.txt console_log.txt transport_client.txt webhelper.txt; do
    [ -f /data/steam/logs/$log ] && tail -n 40 /data/steam/logs/$log | sed "s/^/steam-window: $log: /"
done
grep -iE 'error|panic' /tmp/yserver.log | tail -n 40 | sed 's/^/steam-window: yserver: /'
echo "steam-window: end"
