# Ferrix guest: the Teeworlds client as /tmp/tw/tw.sh starts it, plus the frame recorder
# (libframerec.so, from `cargo xtask frames`, copied to /tmp/lb), which writes the swap-fps lines
# and the per-swap record <swap log>.rec.
#   busybox sh twrun.sh <gfx_vsync 0|1> <swap log>
export HOME=/home/ferrix DISPLAY=:0 PATH=/bin:/data/usr/bin
export LD_LIBRARY_PATH=/tmp/tw/lib:/usr/lib/x86_64-linux-gnu
export LIBGL_DRIVERS_PATH=/usr/lib/x86_64-linux-gnu/dri LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe
export __GLX_VENDOR_LIBRARY_NAME=mesa
export SDL_VIDEO_X11_DGAMOUSE=0 SDL_AUDIODRIVER=dummy
export LD_PRELOAD=/tmp/lb/libframerec.so SWAPFPS_LOG=$2 FRAMEREC_OUT=$2.rec
cd /tmp/tw/tw
exec ./teeworlds "inp_grab 1" "cl_showfps 1" "gfx_fullscreen 1" "gfx_vsync $1" "connect 127.0.0.1:8303" > $2.out 2>&1
