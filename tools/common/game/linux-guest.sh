#!/bin/bash
# R3 on the Linux guest without the card (docs/GAME-PATH.md §8.1): the
# baseline's Debian image under plain KVM with linux-3060.xml's processor,
# its pinning and its topology, the rustc volume attached read-only, and
# measure.sh run in it as the image's ordinary user. Nothing is written to
# the image: the disk is a snapshot.
#
#   linux-guest.sh [WARM-UP SECONDS]
#
# RIG_NO_INVTSC=1 leaves invtsc out, the clock's negative control: the
# guest must then not say tsc. Run from the host, never while linux-3060 or
# ferrix-3060 runs (the same host processors).
#
#   FERRIX_LINUX_BASELINE  the baseline's directory  ~/.local/share/ferrix/linux-baseline
#   FERRIX_RUSTC_SYSROOT   the rustc volume's        ~/.local/share/ferrix/rustc
#   QEMU                   the emulator              the patched one the card guests use
set -eu
here=$(cd "$(dirname "$0")" && pwd)
base=${FERRIX_LINUX_BASELINE:-$HOME/.local/share/ferrix/linux-baseline}
volume=${FERRIX_RUSTC_SYSROOT:-$HOME/.local/share/ferrix/rustc}/rustc.img
qemu=${QEMU:-/usr/local/lib/ferrix/qemu/bin/qemu-system-x86_64}
warm=${1:-30}
port=2369
# linux-3060.xml's <cputune>: vCPU n on host processor ${pins[n]}, QEMU's
# own threads on the rest of that CCD.
pins=(6 18 7 19 8 20 9 21)
emulator=10,22,11,23
cpu=host,migratable=off,host-phys-bits=on,+topoext,+invtsc
[ "${RIG_NO_INVTSC:-0}" = 1 ] && cpu=host,host-phys-bits=on,+topoext

run=$(mktemp -d "${TMPDIR:-/tmp}/rig-linux.XXXXXX")
trap 'kill "$(cat "$run/pid" 2>/dev/null)" 2>/dev/null || true; rm -rf "$run"' EXIT
cp "$base/img/smoke_VARS.fd" "$run/vars.fd"
cc -O2 -Wall -Wextra -static -o "$run/rig" "$here/rig.c"

"$qemu" -name rig-linux,debug-threads=on -machine q35,accel=kvm -cpu "$cpu" \
	-smp 8,sockets=1,cores=4,threads=2 -m 16384 \
	-drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
	-drive if=pflash,format=raw,file="$run/vars.fd" \
	-drive if=none,id=d0,format=qcow2,file="$base/img/linux-3060.qcow2",snapshot=on \
	-device virtio-blk-pci,drive=d0,bootindex=1 \
	-drive if=none,id=d1,format=raw,readonly=on,file="$base/img/seed.iso" -device virtio-blk-pci,drive=d1 \
	-drive if=none,id=d2,format=raw,readonly=on,file="$volume" -device virtio-blk-pci,drive=d2,serial=rustc \
	-netdev user,id=n0,hostfwd=tcp:127.0.0.1:$port-:22 -device virtio-net-pci,netdev=n0 \
	-display none -serial file:"$run/serial.log" -monitor none \
	-daemonize -pidfile "$run/pid"
pid=$(cat "$run/pid")
for task in /proc/"$pid"/task/*; do
	name=$(cat "$task/comm")
	case $name in
	"CPU "*/KVM) n=${name#CPU }; n=${n%/KVM}; taskset -pc "${pins[$n]}" "${task##*/}" >/dev/null ;;
	*) taskset -pc "$emulator" "${task##*/}" >/dev/null ;;
	esac
done
echo "linux-guest: pid $pid, vCPUs pinned to ${pins[*]}, QEMU on $emulator, -cpu $cpu"

ssho=(-o IdentitiesOnly=yes -o IdentityAgent=none -o StrictHostKeyChecking=no
	-o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=10 -i "$base/ssh/id_ed25519")
for _ in $(seq 1 90); do
	ssh "${ssho[@]}" -p $port ferrix@127.0.0.1 true 2>/dev/null </dev/null && break
	sleep 2
done
scp -q "${ssho[@]}" -P $port "$run/rig" "$here/load.sh" "$here/load-crate.rs" "$here/measure.sh" \
	ferrix@127.0.0.1:/tmp/
# The volume read-only at /mnt/rustc, by the root the image's user may
# reach with sudo; everything after that as that user.
ssh "${ssho[@]}" -p $port ferrix@127.0.0.1 \
	"sudo mkdir -p /mnt/rustc && sudo mount -o ro /dev/disk/by-id/virtio-rustc /mnt/rustc &&
	 echo \"linux-guest: \$(uname -sr), \$(nproc) processors, \$(id -un) (uid \$(id -u))\" &&
	 PATH=/tmp:/mnt/rustc/rust/bin:\$PATH RIG_DISK=\$HOME sh /tmp/measure.sh $warm" </dev/null
ssh "${ssho[@]}" -p $port ferrix@127.0.0.1 sudo poweroff </dev/null 2>/dev/null || true
for _ in $(seq 1 30); do kill -0 "$pid" 2>/dev/null || break; sleep 1; done
