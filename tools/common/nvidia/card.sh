#!/bin/bash
# The long-lived card VM: one Ferrix desktop on the RTX 3060 that agents
# deploy into over ssh instead of rebooting the domain for every test.
#
#   card.sh up [--everything] [xtask args]   boot it under card.lock (background)
#   card.sh status                            is it up, and where
#   card.sh ssh <command...>                  run a command in the guest as root
#   card.sh push <local> <guest-path>         copy a file in (atomic rename)
#   card.sh exec <command line>               start a program in the session (hyprctl dispatch exec)
#   card.sh swap yserver <binary>             /data/yserver/yserver, server restarted
#   card.sh swap hyprix <binary>              /bin/hyprix, hyprix.service restarted (the session restarts)
#   card.sh swap chrome-flags "<flags>"       Chrome restarted with its desktop line plus <flags>
#   card.sh swap <program> <binary>           /bin/<program>; <program>.service restarted if there is one
#   card.sh log                               follow the serial log
#   card.sh down                              end it; xtask destroys the domain
#   card.sh reboot [xtask args]               down, then up again (kernel or nvrm changes only)
#
# Every swap is announced in ~/.local/share/ferrix/steam-race/cardvm.md
# (CARD_AGENT=<your name> says who). The protocol is in that file.
#
# CARD_WT: the worktree `up` builds from (default: this script's checkout).
# CARD_PORT: the host port forwarded to the guest's sshd (default 2360).
set -u
D=${CARD_DIR:-$HOME/.local/share/ferrix/nvidia}
STATE=$D/cardvm.env
STOP=$D/cardvm-stop
ARGS=$D/cardvm.args
OUT=$D/cardvm-xtask.log
NOTES=$HOME/.local/share/ferrix/steam-race/cardvm.md
KEY=$HOME/.local/share/ferrix/ssh/id_ed25519
PORT=${CARD_PORT:-2360}
WT=${CARD_WT:-$(cd "$(dirname "$0")/../../.." && pwd)}
AGENT=${CARD_AGENT:-${USER}}
# The customer's G303 belongs to Ferrix; never the Keychron (host keyboard).
# n3c's private NVIDIA tree (virgl debs) when it is there: hyprix composites on the GPU.
[ -z "${FERRIX_NVIDIA:-}" ] && [ -d "$HOME/.local/share/ferrix/nvidia-n3c" ] && export FERRIX_NVIDIA=$HOME/.local/share/ferrix/nvidia-n3c
export FERRIX_NVIDIA_INPUT=${FERRIX_NVIDIA_INPUT:-/dev/input/by-id/usb-Logitech_Gaming_Mouse_G303_0F8934563031-event-mouse:/dev/input/by-id/usb-Logitech_Gaming_Mouse_G303_0F8934563031-if01-event-kbd}

keeper() { pgrep -f tv-keeper-cardvm.sh > /dev/null; }

die() { echo "card: $*" >&2; exit 1; }

load() {
  [ -f "$STATE" ] || die "no card VM is up ($STATE missing): card.sh up"
  # shellcheck disable=SC1090
  . "$STATE"
  kill -0 "$PID" 2>/dev/null || die "the card VM's xtask ($PID) is gone"
}

gssh() {
  ssh -o IdentitiesOnly=yes -o IdentityAgent=none -o StrictHostKeyChecking=no \
      -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=10 \
      -i "$KEY" -p "$PORT" root@127.0.0.1 "$@"
}

announce() {
  [ -f "$NOTES" ] && printf -- '- %s %s: %s\n' "$(date '+%F %T')" "$AGENT" "$*" >> "$NOTES"
}

# The session's runtime directory: the user's under --everything, /tmp else.
GUEST_ENV='if [ -d /run/user/1000/hypr ]; then export XDG_RUNTIME_DIR=/run/user/1000; else export XDG_RUNTIME_DIR=/tmp; fi'

push() { # local guest-path
  local src=$1 dst=$2 mode
  [ -f "$src" ] || die "$src is not a file"
  mode=$(stat -c %a "$src")
  gssh "cat > '$dst.card-new' && chmod $mode '$dst.card-new' && mv -f '$dst.card-new' '$dst'" < "$src" \
    || die "copying $src to $dst failed"
}

dispatch() { # command line: written to a script in the guest, which the
  # compositor runs as the session's user with the session's environment
  local script=/tmp/card-exec-$$.sh
  printf 'exec > %s 2>&1\n%s\n' "${script%.sh}.log" "$*" | gssh "cat > $script && chmod 644 $script && $GUEST_ENV && /bin/hyprctl dispatch exec '/bin/busybox sh $script'" && echo "card: output in ${script%.sh}.log in the guest"
}

up() {
  if [ -f "$STATE" ] && . "$STATE" && kill -0 "$PID" 2>/dev/null; then
    echo "card: already up: guest $ADDRESS, ssh port $PORT, log $LOG"; return 0
  fi
  if keeper; then
    echo "card: the TV keeper boots the card VM; waiting for it"
    local waited=0
    until [ -f "$STATE" ]; do sleep 2; waited=$((waited + 2)); done
    . "$STATE"
    until gssh true 2>/dev/null; do sleep 2; waited=$((waited + 2)); done
    echo "card: up after ${waited} s: guest $ADDRESS, ssh port $PORT, serial $LOG"
    return 0
  fi
  printf '%s\n' "$@" > "$ARGS"
  rm -f "$STOP" "$STATE"
  touch "$D/want-cardvm"
  echo "card: waiting for card.lock (want-cardvm is there); building in $WT"
  ( cd "$WT" && setsid flock "$D/card.lock" bash -c '
      rm -f "$1/want-cardvm"; shift
      exec cargo xtask run-compositor --nvidia --arch x86_64 --ssh "$@"' _ "$D" "$PORT" "$@" \
      > "$OUT" 2>&1 < /dev/null & )
  local waited=0
  until [ -f "$STATE" ]; do
    sleep 2; waited=$((waited + 2))
    if ! pgrep -f "run-compositor --nvidia --arch x86_64 --ssh $PORT" > /dev/null; then
      tail -20 "$OUT"; die "the xtask ended before the guest had an address; see $OUT"
    fi
  done
  . "$STATE"
  until gssh true 2>/dev/null; do sleep 2; waited=$((waited + 2)); done
  echo "card: up after ${waited} s: guest $ADDRESS, ssh port $PORT, serial $LOG"
  announce "card VM up (${*:-desktop}); guest $ADDRESS"
}

down() {
  [ -f "$STATE" ] || { echo "card: not up"; return 0; }
  . "$STATE"
  touch "$STOP"
  echo "card: asked xtask $PID to stop; it destroys ferrix-3060"
  while kill -0 "$PID" 2>/dev/null; do sleep 1; done
  rm -f "$STOP"
  keeper && echo "card: the TV keeper boots it again within seconds unless a want-* file is there"
  grep -E 'ferrix-3060: (destroyed|virsh)' "$OUT" | tail -1
  announce "card VM down"
}

swap() {
  local what=${1:-} t0 t1
  [ $# -ge 2 ] || die "swap yserver|hyprix|chrome-flags|<program> <binary|flags>"
  load
  t0=$(date +%s.%N)
  case $what in
    yserver)
      push "$2" /data/yserver/yserver
      gssh "pkill -f '[/]data/yserver/yserver'; i=0; while pgrep -f '[/]data/yserver/yserver' >/dev/null && [ \$i -lt 50 ]; do sleep 0.1; i=\$((i+1)); done; \
            rm -f /tmp/.X11-unix/X0 /tmp/.X0-lock"
      dispatch "/bin/busybox sh /etc/yserver.sh"
      gssh 'i=0; while [ ! -S /tmp/.X11-unix/X0 ] && [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done; [ -S /tmp/.X11-unix/X0 ] && echo "card: :0 is back" || { echo "card: no :0 after 30 s"; tail -5 /tmp/yserver.log; }'
      ;;
    hyprix)
      push "$2" /bin/hyprix
      # The session's processes live in user.slice, not in the unit's cgroup:
      # a plain restart leaves the old compositor holding the seat.
      gssh 'svc stop hyprix.service >/dev/null 2>&1
            for k in /sys/fs/cgroup/user.slice/user-*.slice/session-*.scope/cgroup.kill; do [ -e "$k" ] && echo 1 > "$k"; done
            pkill -x hyprix; i=0
            while pgrep -x hyprix >/dev/null && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done
            pkill -9 -x hyprix; sleep 0.2
            svc start hyprix.service; sleep 2; svc status hyprix.service | head -5
            i=0; while ! pgrep -f "^/bin/hyprix --config" >/dev/null && [ $i -lt 200 ]; do sleep 0.1; i=$((i+1)); done
            pgrep -f "^/bin/hyprix --config" >/dev/null && echo "card: hyprix is back" || echo "card: no hyprix"'

      ;;
    chrome-flags)
      local line
      line=$(gssh "grep -m1 '^exec-once = .*chrom' /etc/hyprland.conf | sed 's/^exec-once = //'")
      [ -n "$line" ] || die "no Chrome line in the guest's /etc/hyprland.conf"
      gssh 'pkill -x chrome; i=0
            while pgrep -x chrome >/dev/null && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done
            pkill -9 -x chrome; sleep 0.3
            rm -f /dev/shm/chrome/Singleton* 2>/dev/null; true'

      dispatch "$line $2"
      ;;
    *)
      push "$2" "/bin/$what"
      gssh "if [ -e /etc/ferrix/units/$what.service ]; then svc restart $what.service; else echo 'card: /bin/$what replaced; no $what.service to restart'; fi"
      ;;
  esac
  t1=$(date +%s.%N)
  local took
  took=$(printf '%.1f' "$(echo "$t1 - $t0" | bc)")
  local sum=""
  [ -f "$2" ] && sum=" sha256 $(sha256sum "$2" | cut -c1-12)"
  echo "card: swap $what took $took s"
  announce "swap $what ${2##*/}$sum (${took} s)"
}

cmd=${1:-status}; shift || true
case $cmd in
  up) up "$@" ;;
  down) down ;;
  reboot)
    if [ $# -eq 0 ] && [ -f "$ARGS" ]; then mapfile -t saved < "$ARGS"; set -- "${saved[@]}"; fi
    down; up "$@" ;;
  status)
    if [ -f "$STATE" ] && . "$STATE" && kill -0 "$PID" 2>/dev/null; then
      echo "card: up: guest $ADDRESS, ssh 127.0.0.1:$PORT, xtask $PID, serial $LOG"
    else echo "card: down"; fi ;;
  ssh) load; gssh "$@" ;;
  push) load; [ $# -eq 2 ] || die "push <local> <guest-path>"; push "$1" "$2"; announce "push ${1##*/} -> $2" ;;
  exec) load; dispatch "$*"; announce "exec $*" ;;
  swap) swap "$@" ;;
  log) load; exec tail -n 50 -f "$LOG" ;;
  *) sed -n '2,20p' "$0"; exit 2 ;;
esac
