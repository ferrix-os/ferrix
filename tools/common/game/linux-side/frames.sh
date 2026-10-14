#!/bin/bash
# Linux guest: the frame recorder's test client under sway's Xwayland, with the recorder preloaded
# (docs/GAME-PATH.md §2.4, R1). What test-frames runs on Ferrix, run here.
#   frames.sh <dir holding libframerec.so and framegl> <label> <seconds> [stall ms]
# Writes ~/lb/<label>.rec (the record), .fps (swap-fps lines) and .out (framegl's lines), and prints
# the binaries' SHA-256 and framegl's lines. GL=nvidia draws on NVIDIA's GLX; otherwise Mesa's
# llvmpipe, as test-frames does on Ferrix. HEADLESS=1 runs it on the Xwayland of a second sway, on
# wlroots' headless backend at 60 Hz, instead of the session's: the image without the card shows on
# virtio-gpu, whose flips complete at once, so its session has no refresh to count.
set -u
d=$1 l=$2 secs=$3 stall=${4:-0}
export HOME=/home/ferrix DISPLAY=${DISPLAY:-:0} XDG_RUNTIME_DIR=/run/user/1000
if [ "${HEADLESS:-0}" = 1 ]; then
  hd=$HOME/lb/headless
  rm -rf "$hd"; mkdir -p -m 700 "$hd"
  printf 'xwayland enable\noutput HEADLESS-1 mode 1280x720@60Hz\nexec echo $DISPLAY > %s/display\n' \
    "$hd" > "$hd/config"
  XDG_RUNTIME_DIR=$hd WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
    sway -c "$hd/config" > "$hd/sway.log" 2>&1 &
  headless=$!
  for i in $(seq 1 50); do [ -s "$hd/display" ] && break; sleep 0.2; done
  DISPLAY=$(cat "$hd/display")
  echo "headless sway $headless, DISPLAY $DISPLAY"
fi
if [ "${GL:-soft}" = nvidia ]; then
  export __GLX_VENDOR_LIBRARY_NAME=nvidia
else
  export LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe __GLX_VENDOR_LIBRARY_NAME=mesa
fi
mkdir -p "$HOME/lb"
rm -f "$HOME/lb/$l.rec" "$HOME/lb/$l.fps"
FRAMEREC_OUT=$HOME/lb/$l.rec FRAMEREC_STALL_MS=$stall SWAPFPS_LOG=$HOME/lb/$l.fps \
  LD_PRELOAD=$d/libframerec.so "$d/framegl" "$secs" > "$HOME/lb/$l.out" 2>&1
echo "framegl exited $?"
[ -n "${headless:-}" ] && kill "$headless"
sha256sum "$d/libframerec.so" "$d/framegl"
cat "$HOME/lb/$l.out"
