#!/bin/busybox sh
# Start GRYPHLINK rootless with Endfield on the iGPU: ef-start.sh with
# /data/ef-player-anv.sh in place of /data/ef-player.sh. As root (the
# desktop's /bin/gryphlink, or ef-gpu-run.sh) it drops to ferrix; as ferrix
# it runs the player directly. EF_* settings pass through to the player.
keep=
for v in EF_VK EF_GAME_DXVK EF_HUD EF_PROTON_LOG; do
    eval "val=\${$v-}"
    [ -n "$val" ] && keep="$keep $v='$val'"
done
if [ "$(id -u)" = 0 ]; then
    for d in /tmp/ferrix-run /tmp/ferrix-plog; do
        mkdir -p $d && chown 1000:1000 $d && chmod 700 $d
    done
    exec su ferrix -c "/bin/busybox env$keep /data/ef-player-anv.sh"
fi
mkdir -p /tmp/ferrix-run /tmp/ferrix-plog && chmod 700 /tmp/ferrix-run /tmp/ferrix-plog
exec /data/ef-player-anv.sh
