# Steam on the `cargo xtask run-compositor --everything` desktop
# (docs/STEAM.md): started by hyprix's exec-once -- as root, or as ferrix
# where sessiond starts the session -- and again by fuzzel's Steam entry. The desktop's own
# script starts yserver on :0 and the network's lease; this waits for the
# X server, gives the volume's home and Steam's tree to uid 1000, and runs
# client.sh as that user, as `run-steam` does. The client installs itself
# from the bootstrap on the first start, which takes minutes, and then opens
# its sign-in window. Its output goes to the serial console, as the
# desktop's other programs' does, and to /tmp/steam.log.
export PATH=/bin:/data/usr/bin HOME=/data/home USER=root LANG=C.UTF-8
waited=0
while [ ! -S /tmp/.X11-unix/X0 ] && [ $waited -lt 120 ]; do sleep 1; waited=$((waited + 1)); done
if [ ! -S /tmp/.X11-unix/X0 ]; then
    echo "steam-desktop: no X server on :0 after ${waited}s; Steam is not started" | tee /tmp/steam.log
    exit 1
fi
# A desktop whose session runs as ferrix (sessiond) starts this as ferrix,
# and init's data-home.service has already given the volume's home and
# Steam's tree to it: the client runs as it is.
if [ "$(id -u)" != 0 ]; then
    /bin/busybox sh /steam/client.sh 2>&1 | tee /tmp/steam.log
    exit
fi
# fuzzel's Steam entry runs this again after the client was closed; by then
# the client has written its tree as uid 1000, and a second walk of it is
# only slow.
if [ "$(stat -c %u /data/steam)" != 1000 ] || [ "$(stat -c %u /data/home)" != 1000 ]; then
    chown -R 1000:1000 /data/home /data/steam
fi
su -s /bin/sh ferrix -c '/bin/busybox sh /steam/client.sh' 2>&1 | tee /tmp/steam.log
