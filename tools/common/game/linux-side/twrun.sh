#!/bin/bash
# Linux guest: the Teeworlds client as Ferrix starts it, plus the frame recorder (libframerec.so,
# which measure-frames.sh puts in ~/rig), writing the swap-fps lines and the record <swap log>.rec.
#   twrun.sh <nvidia|soft> <gfx_vsync 0|1> <swap log>
# nvidia = NVIDIA's GLX on the 3060; soft = Mesa llvmpipe, as Ferrix draws the game today.
export HOME=/home/ferrix DISPLAY=:0 XDG_RUNTIME_DIR=/run/user/1000
export LD_LIBRARY_PATH=/home/ferrix/lib
if [ "$1" = soft ]; then
  export LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe __GLX_VENDOR_LIBRARY_NAME=mesa
else
  export __GLX_VENDOR_LIBRARY_NAME=nvidia __GL_SYNC_TO_VBLANK=$2
fi
export SDL_VIDEO_X11_DGAMOUSE=0 SDL_AUDIODRIVER=dummy
export LD_PRELOAD=/home/ferrix/rig/libframerec.so SWAPFPS_LOG=$3 FRAMEREC_OUT=$3.rec
cd /home/ferrix/tw
pgrep -x teeworlds_srv >/dev/null || { setsid ./teeworlds_srv "sv_map dm1" > /home/ferrix/lb/srv.out 2>&1 < /dev/null & sleep 2; }
rm -f "$3"
LD_PRELOAD= glxinfo -B 2>/dev/null | grep -E 'OpenGL renderer|OpenGL version' > "$3.gl"
setsid ./teeworlds "cl_showfps 1" "gfx_fullscreen 1" "gfx_vsync $2" "connect 127.0.0.1:8303" > "$3.out" 2>&1 < /dev/null &
