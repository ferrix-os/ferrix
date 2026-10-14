# Ferrix guest: the Teeworlds client on NVIDIA's GLX (through yserver's DRI3/Present), plus the frame
# recorder (as twrun.sh).
#   busybox sh twrun-nv.sh <gfx_vsync 0|1> <swap log>
export HOME=/home/ferrix DISPLAY=:0 PATH=/bin:/data/usr/bin
export LD_LIBRARY_PATH=/tmp/tw/lib:/usr/lib/x86_64-linux-gnu
export __GLX_VENDOR_LIBRARY_NAME=nvidia __GL_SYNC_TO_VBLANK=$1
export SDL_VIDEO_X11_DGAMOUSE=0 SDL_AUDIODRIVER=dummy
export LD_PRELOAD=/tmp/lb/libframerec.so SWAPFPS_LOG=$2 FRAMEREC_OUT=$2.rec
cd /tmp/tw/tw
exec ./teeworlds "inp_grab 1" "cl_showfps 1" "gfx_fullscreen 1" "gfx_vsync $1" "connect 127.0.0.1:8303" > $2.out 2>&1
