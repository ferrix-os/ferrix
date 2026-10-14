#!/bin/bash
# Stage 24's input-to-scan-out probe on Linux (docs/GAME-PATH.md §2.4, R2):
# a headless sway at 1920x1080 and 60 Hz with Xwayland, xlatency as its X
# client, vptr injecting relative motion through zwlr_virtual_pointer_v1 as
# the same programs do on Ferrix, then `xlatency summarize` on the two logs.
#
#   latency-sway.sh OUTDIR [DELAY_US] [SECONDS]
#
# XLATENCY and VPTR name the two programs (default: the static builds
# `cargo xtask test-xlatency` makes, under the target directory's
# compositor/hyprix/x86_64-unknown-linux-musl/release).
# DELAY_US is the probe's --delay-us, the negative control's 16700. OUTDIR
# gets sway.log, probe.txt, sends.txt and summary.txt; the summary is
# printed. On a desktop, the Linux guest of §2.1, run it with
# WLR_BACKENDS unset under the session's own sway instead (the inner script,
# OUTDIR/run.sh, is what to run there).
set -eu
out=${1:?usage: latency-sway.sh OUTDIR [DELAY_US] [SECONDS]}
delay=${2:-0}
seconds=${3:-12}
here=$(cd "$(dirname "$0")/../../.." && pwd)
built=${CARGO_TARGET_DIR:-$here/target}/compositor/hyprix/x86_64-unknown-linux-musl/release
xlatency=${XLATENCY:-$built/xlatency}
vptr=${VPTR:-$built/vptr}
for program in "$xlatency" "$vptr"; do
    [ -x "$program" ] || { echo "latency-sway: $program is not there" >&2; exit 2; }
done
mkdir -p "$out"
out=$(cd "$out" && pwd)
run=$(mktemp -d "${TMPDIR:-/tmp}/latency-sway.XXXXXX")
chmod 700 "$run"
cat > "$out/run.sh" <<EOF
#!/bin/bash
# Started by sway, which gives it DISPLAY and WAYLAND_DISPLAY.
"$xlatency" --seconds $((seconds + 6)) --delay-us $delay > "$out/probe.txt" 2>&1 &
probe=\$!
waited=0
until grep -q 'xlatency: ready' "$out/probe.txt" || [ \$waited -ge 100 ]; do
    sleep 0.1
    waited=\$((waited + 1))
done
"$vptr" 7 $seconds --log "$out/sends.txt" > "$out/vptr.txt" 2>&1
wait \$probe
"$xlatency" summarize "$out/sends.txt" "$out/probe.txt" > "$out/summary.txt" 2>&1
grep 'xlatency: summary' "$out/probe.txt" >> "$out/summary.txt"
swaymsg exit
EOF
chmod +x "$out/run.sh"
cat > "$out/sway.config" <<EOF
xwayland enable
output HEADLESS-1 mode 1920x1080@60Hz
default_border none
exec "$out/run.sh"
EOF
env -u DISPLAY -u WAYLAND_DISPLAY -u SWAYSOCK XDG_RUNTIME_DIR="$run" WLR_BACKENDS=headless \
    WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman \
    timeout $((seconds + 60)) sway -c "$out/sway.config" > "$out/sway.log" 2>&1 || true
rm -rf "$run"
cat "$out/summary.txt"
