#!/bin/sh
# Boot Ferrix's x86-64 desktop in QEMU, from the release archive this script
# came in. Nothing but QEMU is needed: no Rust, no build.
#
#     sh run.sh                  a window, with the hypervisor if there is one
#     sh run.sh --vnc 5          no window: the screen on VNC, 127.0.0.1:5905
#     sh run.sh --tcg            emulate even where a hypervisor is available
#     sh run.sh --memory 4096 --smp 2 --size 1920x1080
#     sh run.sh -- <qemu args>   anything after -- goes to QEMU as it is
#
# The serial console is this terminal: the kernel's and the desktop's log,
# and a monitor behind Ctrl-A c. Ctrl-A x quits QEMU.
#
# QEMU_SYSTEM names another QEMU; OVMF_CODE and OVMF_VARS name the UEFI
# firmware, when the search below does not find what your QEMU or your
# distribution installed.
#
# These are the devices `cargo xtask run-compositor --tmpfs-root` gives the
# same desktop, less the test devices only its self-checks read
# (tools/common/xtask/src/qemu.rs). Kept in step with
# tools/common/release/desktop/run.ps1.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
image=$here/ferrix-x86_64.img
qemu=${QEMU_SYSTEM:-qemu-system-x86_64}
memory=2048
smp=4
size=1280x800
vnc=
accel=

die() {
    echo "run.sh: $*" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case $1 in
        --vnc) vnc=${2:?--vnc needs a display number, e.g. 5}; shift 2 ;;
        --tcg) accel=tcg; shift ;;
        --memory) memory=${2:?--memory needs MiB}; shift 2 ;;
        --smp) smp=${2:?--smp needs a count}; shift 2 ;;
        --size) size=${2:?--size needs WIDTHxHEIGHT}; shift 2 ;;
        --) shift; break ;;
        -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
        *) die "unknown option $1 (--help lists them)" ;;
    esac
done

[ -f "$image" ] || die "no $image: run this script from the unpacked archive"
command -v "$qemu" >/dev/null 2>&1 || die "$qemu is not on PATH. Install QEMU:
  Debian/Ubuntu: sudo apt install qemu-system-x86 ovmf
  Fedora:        sudo dnf install qemu-system-x86 edk2-ovmf
  Arch:          sudo pacman -S qemu-full edk2-ovmf
  macOS:         brew install qemu"
width=${size%x*}
height=${size#*x}

# QEMU's version, for the one feature older ones emulate wrongly (below).
version=$("$qemu" --version | sed -n 's/.*version \([0-9]*\)\.\([0-9]*\).*/\1 \2/p' | head -n 1)
major=${version% *}
minor=${version#* }

# The accelerator: KVM on Linux, HVF on an Intel Mac, and QEMU's own
# emulator, TCG, everywhere else. TCG works, slowly: expect a minute or two
# to the desktop and a sluggish one after that.
accels=$("$qemu" -accel help 2>/dev/null || true)
has_accel() { printf '%s\n' "$accels" | grep -qx "$1"; }
if [ -z "$accel" ]; then
    accel=tcg
    case $(uname -s) in
        Linux)
            if [ -r /dev/kvm ] && [ -w /dev/kvm ] && has_accel kvm; then
                accel=kvm
            elif [ -e /dev/kvm ]; then
                echo "run.sh: /dev/kvm is there but not yours to open: emulating." >&2
                echo "        (sudo usermod -aG kvm \$USER, then log in again, for KVM)" >&2
            fi
            ;;
        Darwin)
            if [ "$(uname -m)" = x86_64 ] && has_accel hvf &&
                [ "$(sysctl -n kern.hv_support 2>/dev/null)" = 1 ]; then
                accel=hvf
            fi
            ;;
    esac
fi

# The processor: x86-64-v3 with SMEP, SMAP and UMIP, as xtask's boots have
# it (`x86_cpu` in tools/common/xtask/src/qemu.rs), and under a hypervisor
# the host's speculation controls and an invariant TSC.
cpu=qemu64,+pdpe1gb,+smep,+smap,+umip,+rdrand,+rdseed,+ssse3,+sse4.1,+sse4.2,+popcnt
cpu=$cpu,+cx16,+movbe,+xsave,+xsaveopt,+avx,+avx2,+f16c,+fma,+bmi1,+bmi2,+abm
cpu=$cpu,+pclmulqdq,+aes,+x2apic
machine=q35
case $accel in
    kvm)
        cpu=$cpu,+spec-ctrl,+stibp,+ssbd,+arch-capabilities,+auto-ibrs,+invtsc
        # QEMU's I/O APIC, which interrupt remapping needs beside KVM.
        machine=q35,kernel-irqchip=split
        ;;
    hvf)
        cpu=$cpu,+spec-ctrl,+stibp,+ssbd,+arch-capabilities
        ;;
    tcg)
        # Before 9.1, TCG reads a user-mode far return's stack as ring 0
        # under SMAP; the kernel runs without SMAP there.
        if [ -n "$major" ] && { [ "$major" -lt 9 ] || { [ "$major" -eq 9 ] && [ "$minor" -lt 1 ]; }; }; then
            cpu=$(printf '%s' "$cpu" | sed 's/,+smap//')
        fi
        ;;
esac

# UEFI firmware: OVMF, which QEMU or the distribution installs. Each code
# file with the variable store that goes with it; never a Secure Boot
# build, which would refuse Ferrix's unsigned loader.
pairs="edk2-x86_64-code.fd:edk2-i386-vars.fd
OVMF_CODE_4M.fd:OVMF_VARS_4M.fd
OVMF_CODE.4m.fd:OVMF_VARS.4m.fd
OVMF_CODE.fd:OVMF_VARS.fd
OVMF.fd:"
dirs=$("$qemu" -L help 2>/dev/null || true)
dirs="$dirs
$(dirname "$(command -v "$qemu")")/../share/qemu
/usr/share/OVMF
/usr/share/ovmf
/usr/share/ovmf/x64
/usr/share/edk2/ovmf
/usr/share/edk2/x64
/usr/share/edk2-ovmf/x64
/usr/share/qemu
/usr/local/share/qemu
/opt/homebrew/share/qemu
/opt/local/share/qemu"
code=${OVMF_CODE:-}
vars_template=${OVMF_VARS:-}
if [ -z "$code" ]; then
    # One directory a line, since a directory's name may have a space in it.
    saved_ifs=$IFS
    IFS='
'
    for pair in $pairs; do
        for dir in $dirs; do
            if [ -f "$dir/${pair%%:*}" ]; then
                code=$dir/${pair%%:*}
                if [ -n "${pair#*:}" ] && [ -f "$dir/${pair#*:}" ]; then
                    vars_template=$dir/${pair#*:}
                fi
                break 2
            fi
        done
    done
    IFS=$saved_ifs
fi
[ -n "$code" ] || die "no UEFI firmware (OVMF) found. Install your distribution's
  ovmf or edk2-ovmf package, or set OVMF_CODE (and OVMF_VARS) to its files."
echo "run.sh: $("$qemu" --version | head -n 1), under $accel, firmware $code"

# A fresh variable store for every boot, as xtask makes one: a store an
# earlier boot left can hold boot entries that send firmware elsewhere.
vars=$(mktemp "${TMPDIR:-/tmp}/ferrix-vars.XXXXXX")
trap 'rm -f "$vars"' EXIT INT TERM
if [ -n "$vars_template" ]; then
    cat "$vars_template" > "$vars"
fi
# The firmware and the screen go in front of whatever came after `--`, so
# that each path stays one argument however it is spelt.
case ${code##*/} in
    # The code and the store in one file, which QEMU loads as a ROM.
    OVMF.fd) set -- -bios "$code" "$@" ;;
    *) set -- -drive "if=pflash,format=raw,readonly=on,file=$code" \
        -drive "if=pflash,format=raw,file=$vars" "$@" ;;
esac

# Where the screen goes: a window of QEMU's own, or VNC on this machine
# only, which is also what a Linux session with no display gets.
if [ -z "$vnc" ] && [ "$(uname -s)" = Linux ] &&
    [ -z "${DISPLAY:-}" ] && [ -z "${WAYLAND_DISPLAY:-}" ]; then
    vnc=5
    echo "run.sh: no display here to open a window on" >&2
fi
if [ -n "$vnc" ]; then
    set -- -display "vnc=127.0.0.1:$vnc,display=gpu0" "$@"
    echo "run.sh: the screen is on VNC at 127.0.0.1:$((5900 + vnc)); from another machine," \
        "ssh -L $((5900 + vnc)):127.0.0.1:$((5900 + vnc)) <this host> first"
fi

flags=disable-legacy=on,iommu_platform=on
"$qemu" \
    -accel "$accel" -machine "$machine" -cpu "$cpu" -m "$memory" -smp "$smp" \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    -device intel-iommu,intremap=on,eim=off \
    -vga none \
    -drive "format=raw,file=$image,if=none,id=disk" \
    -device ide-hd,drive=disk,bus=ide.0,bootindex=0 \
    -device pcie-root-port,id=ferrix.port0,chassis=1,slot=1 \
    -device "virtio-rng-pci,$flags,bus=ferrix.port0" \
    -device "virtio-gpu-pci,id=gpu0,addr=0x10,$flags,ioeventfd=on,edid=on,xres=$width,yres=$height" \
    -device "virtio-keyboard-pci,$flags" \
    -device "virtio-tablet-pci,$flags" \
    -device VGA,addr=0xa \
    -netdev user,id=net0 \
    -device "virtio-net-pci,netdev=net0,mac=52:54:00:12:34:56,$flags" \
    -serial mon:stdio \
    -no-reboot \
    "$@"
