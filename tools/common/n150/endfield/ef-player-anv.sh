#!/bin/busybox sh
# GRYPHLINK and Endfield as user ferrix, Endfield on the N150's iGPU:
# ef-player.sh (ef-cache's version) with Mesa's anv beside lavapipe.
# Started by /data/ef-start-anv.sh, which drops root to ferrix first.
#
# EF_VK picks the Vulkan devices (default split):
#   split  the launcher and every other program on lavapipe, only
#          Endfield.exe on anv. Both ICDs are listed, so every process
#          sees both devices; DXVK's own device filter, set per program in
#          the config file below, picks one per process.
#   anv    everything on anv (the 2026-10-20 run, without its d3d9=b).
#   lvp    everything on lavapipe (the A/B against ef-player.sh).
#
# Why the split goes through DXVK's config file (DXVK 3.1.1 sources,
# src/util/config/config.cpp and src/dxvk/dxvk_device_filter.cpp):
# * DXVK reads $DXVK_CONFIG_FILE, else dxvk.conf in the current directory
#   (not the exe's), then $DXVK_CONFIG. In the file, a line "[Name.exe]"
#   makes the lines after it apply only to the program of that file name
#   (env::getExeName(), exact, case-sensitive); lines before any section
#   apply to every program. $DXVK_CONFIG is parsed afterwards with the
#   section reset, so Proton's appends to it stay global.
# * dxvk.deviceFilter is the config form of DXVK_FILTER_DEVICE_NAME: a
#   substring of the Vulkan device name. The environment variable wins
#   over the file, so it is unset here.
# * Environment cannot split the processes: Proton's WINEDLLOVERRIDES
#   beat the registry's AppDefaults, every Wine child inherits the
#   launcher's environment, and the Vulkan loader's per-application
#   settings key on the Unix executable, which is Wine's for all of them.
#
# Other differences from ef-player.sh, as before:
# * MESA_VK_WSI_DEBUG=sw: yserver has no DRI3 (docs/STEAM.md); anv
#   presents through the WSI's CPU copy and PutImage. Lavapipe ignores it.
# * dxgi.syncInterval = 0, for the game only: on that copy path the present
#   id never advances, and DXVK 3's FIFO present waits on it forever (QEMU,
#   fake back end). EF_GAME_DXVK replaces the game's extra lines, ";"
#   between them, e.g. "dxgi.syncInterval = 0; dxgi.maxFrameRate = 30";
#   EF_GAME_DXVK=none adds nothing (once the driver agent's fix is in).
# * Mesa's shader cache has its own directory for this script.
# * DXVK logs at info level to /home/ferrix/plog/dxvk/<exe>_<dll>.log, and
#   Wine's fps channel is on for Endfield.exe alone ("Endfield.exe:+fps",
#   a per-process WINEDEBUG channel), so proton.out carries one
#   "trace:fps:... @ approx N fps" line about every 1.5 s of the game.
[ "$(id -u)" = 1000 ] || { echo "ef-player-anv.sh: run as ferrix (/data/ef-start-anv.sh)" >&2; exit 1; }
export HOME=/home/ferrix USER=ferrix LOGNAME=ferrix DISPLAY=:0 XDG_RUNTIME_DIR=/tmp/ferrix-run
export STEAM_COMPAT_DATA_PATH=/home/ferrix/cd-dwp STEAM_COMPAT_CLIENT_INSTALL_PATH=/home/ferrix/steam
export SteamGameId=0 UMU_ID=umu-endfield
export PATH=/data/dwproton/files/bin:/usr/bin:/bin
plog=/home/ferrix/plog
mkdir -p $plog $plog/dxvk
mode=${EF_VK:-split}
fps='Endfield.exe:+fps'
if [ "${EF_PROTON_LOG:-0}" = 1 ]; then
    export PROTON_LOG=1 PROTON_LOG_DIR=/tmp/ferrix-plog
    # Proton's own list (proton, init_session), plus the game's fps.
    export WINEDEBUG="+timestamp,+pid,+tid,+seh,+unwind,+threadname,+debugstr,+loaddll,+mscoree,$fps"
else
    unset PROTON_LOG PROTON_LOG_DIR
    export WINEDEBUG="-all,$fps"
fi
icd=/data/usr/share/vulkan/icd.d
lvp=$icd/lvp_icd.json anv=$icd/intel_icd.json
if [ "$mode" != lvp ]; then
    node=
    for candidate in /dev/dri/renderD*; do
        [ -e "$candidate" ] && node=$candidate
    done
    if [ -z "$node" ]; then
        echo "no render node: is the kernel started with ferrix.intelgpu? $(date +%T)" > $plog/status
        exit 2
    fi
    if [ ! -r "$node" ] || [ ! -w "$node" ]; then
        echo "ferrix cannot open $node ($(ls -l $node)) $(date +%T)" > $plog/status
        exit 2
    fi
    [ -e $anv ] || { echo "no $anv: run anv-box-install.sh" > $plog/status; exit 2; }
fi
[ -e $lvp ] || { echo "no $lvp" > $plog/status; exit 2; }

# The per-program DXVK configuration, written fresh each run.
conf=$XDG_RUNTIME_DIR/ef-dxvk.conf
game_extra=${EF_GAME_DXVK:-dxgi.syncInterval = 0}
[ "$game_extra" = none ] && game_extra=
{
    echo "# ef-player-anv.sh $mode $(date +%F' '%T): every program first, then Endfield.exe"
    case $mode in
    split)
        echo "dxvk.deviceFilter = llvmpipe"
        echo "[Endfield.exe]"
        echo "dxvk.deviceFilter = Intel"
        ;;
    anv)
        # The launcher's D3D9 froze at its first FIFO present on anv.
        echo "dxgi.syncInterval = 0"
        echo "d3d9.presentInterval = 0"
        echo "[Endfield.exe]"
        ;;
    lvp)
        echo "[Endfield.exe]"
        ;;
    *) echo "ef-player-anv.sh: EF_VK=$mode is not split, anv or lvp" > $plog/status; exit 2 ;;
    esac
    [ "${EF_HUD:-1}" = 1 ] && echo "dxvk.hud = devinfo,fps"
    echo "$game_extra" | tr ';' '\n' | sed 's/^ *//; s/ *$//' | grep .
} > $conf
unset DXVK_FILTER_DEVICE_NAME DXVK_FILTER_DEVICE_UUID DXVK_CONFIG DXVK_HUD
# Windows paths: DXVK opens both from inside Wine (Z: is the Unix root).
export DXVK_CONFIG_FILE="Z:$conf" DXVK_LOG_PATH="Z:$plog/dxvk" DXVK_LOG_LEVEL=info
rm -f $plog/dxvk/*.log

case $mode in
split) export VK_ICD_FILENAMES=$lvp:$anv MESA_VK_WSI_DEBUG=sw ;;
anv)   export VK_ICD_FILENAMES=$anv MESA_VK_WSI_DEBUG=sw ;;
lvp)   export VK_ICD_FILENAMES=$lvp; unset MESA_VK_WSI_DEBUG ;;
esac
if [ "$mode" = lvp ]; then
    export MESA_SHADER_CACHE_DIR=/home/ferrix/.cache MESA_SHADER_CACHE_MAX_SIZE=8G
else
    # One directory for both drivers in a split run: Mesa keys entries by
    # driver, and the launcher's lavapipe entries are small.
    export MESA_SHADER_CACHE_DIR=/home/ferrix/.cache/anv MESA_SHADER_CACHE_MAX_SIZE=8G
    if [ -x /data/usr/bin/vulkaninfo ]; then
        /data/usr/bin/vulkaninfo --summary > $plog/vulkaninfo.txt 2>&1
    fi
fi
# From 2026-10-20's experiment: WINEDLLOVERRIDES d3d9=b is gone (it sent
# the launcher to wined3d; the split keeps the launcher on DXVK and
# lavapipe, the path that works). LIBGL_ALWAYS_SOFTWARE=1 stays as a
# guard: the game draws no GL, and any GL a launcher process might use
# through Wine's opengl32 stays on llvmpipe instead of reaching iris.
export LIBGL_ALWAYS_SOFTWARE=1
mkdir -p "$MESA_SHADER_CACHE_DIR"
cd /data/GRYPHLINK || exit 1
echo "start $mode $(date +%T)" > $plog/status
# Proton's and Wine's output into the log under `proton` (Wine's err:
# lines say what an error dialog does not), and to proton.out as before.
{ /data/dwproton/proton waitforexitandrun /data/GRYPHLINK/Launcher.exe 2>&1; echo $? > $plog/.proton-exit; } \
    | /bin/lode cat -t proton --tee $plog/proton.out
echo "exit $(cat $plog/.proton-exit 2>/dev/null) $(date +%T)" >> $plog/status
