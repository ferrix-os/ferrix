# Steam on the `cargo xtask run-compositor --everything` desktop
# (docs/STEAM.md): started by hyprix's exec-once, as root. The desktop's own
# script starts yserver on :0 and the network's lease; this waits for the
# X server, gives the volume's home and Steam's tree to uid 1000, and runs
# client.sh as that user, as `run-steam` does. The client installs itself
# from the bootstrap on the first start, which takes minutes, and then opens
# its sign-in window. Its output is in /tmp/steam.log.
export PATH=/bin:/data/usr/bin HOME=/data/home USER=root LANG=C.UTF-8
waited=0
while [ ! -S /tmp/.X11-unix/X0 ] && [ $waited -lt 120 ]; do sleep 1; waited=$((waited + 1)); done
if [ ! -S /tmp/.X11-unix/X0 ]; then
    echo "steam-desktop: no X server on :0 after ${waited}s; Steam is not started" > /tmp/steam.log
    exit 1
fi
chown -R 1000:1000 /data/home /data/steam
su -p -s /bin/sh ferrix -c '/bin/busybox sh /steam/client.sh' > /tmp/steam.log 2>&1
