#!/usr/bin/env bash
# ferrix-postmortem.sh -- after a Ferrix boot that failed, run this on the host
# (Ubuntu) to see what the boot left behind. It never writes the disk.
#
#   tools/common/debug/ferrix-postmortem.sh                 the disk holding a
#                                                           partition named ferrix-esp
#   tools/common/debug/ferrix-postmortem.sh --disk /dev/X   that disk
#   tools/common/debug/ferrix-postmortem.sh --image FILE    a disk image (loop, read-only)
#   ... [--lines N]                                         lines of each log (default 200)
#
# What it prints (docs/POSTMORTEM.md):
#   * the loader's log of the last boot and the one before (FERRIX/BOOTLOG.TXT,
#     BOOTLOG.PRV on the ESP) and the kernel's log the next boot's loader
#     rescued from RAM (FERRIX/LASTKERNEL.TXT, LASTKERNEL.PRV), when present;
#   * the ESP's files and CMDLINE.TXT;
#   * the A/B control record (both copies) and both slot headers
#     (docs/AB-BOOT.md §2, §3), decoded from a read-only dump;
#   * the newest files under the root's /var/log and their tails.
#
# Read-only throughout: partitions are found by GPT name or type, never by
# device name; mounts are `ro` (btrfs also `norecovery`, so no log replay);
# an image is attached `losetup -r`; the record and slot headers are read
# with `dd` from the device into a private temporary directory. Root is asked
# for through sudo (or pkexec when sudo has no terminal).

set -euo pipefail

LINES=200
DISK=""
IMAGE=""
while [ $# -gt 0 ]; do
    case "$1" in
        --disk) DISK="$2"; shift 2 ;;
        --image) IMAGE="$2"; shift 2 ;;
        --lines) LINES="$2"; shift 2 ;;
        -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
        *) echo "ferrix-postmortem: unknown argument $1" >&2; exit 2 ;;
    esac
done

CONTROL_TYPE=c29aa8c5-c19c-4147-838f-01accca28be4
SLOT_TYPE=5475b681-82f5-47da-a5a1-55db0e7e2bd3

if [ "$(id -u)" -eq 0 ]; then
    AS_ROOT=()
elif command -v sudo >/dev/null && { [ -t 0 ] || sudo -n true 2>/dev/null; }; then
    AS_ROOT=(sudo)
else
    AS_ROOT=(pkexec)
fi
root() { "${AS_ROOT[@]}" "$@"; }

WORK=$(mktemp -d "${XDG_RUNTIME_DIR:-/tmp}/ferrix-postmortem.XXXXXX")
LOOP=""
MOUNTS=()
cleanup() {
    local m
    for m in "${MOUNTS[@]+"${MOUNTS[@]}"}"; do root umount "$m" 2>/dev/null || true; done
    [ -n "$LOOP" ] && root losetup -d "$LOOP" 2>/dev/null || true
    rm -rf "$WORK" 2>/dev/null || root rm -rf "$WORK" || true
}
trap cleanup EXIT INT TERM

section() { printf '\n==== %s ====\n' "$*"; }

if [ -n "$IMAGE" ]; then
    LOOP=$(root losetup -r -f -P --show "$IMAGE")
    DISK="$LOOP"
    root udevadm settle 2>/dev/null || true
fi

if [ -z "$DISK" ]; then
    # Every partition named ferrix-esp, by its parent disk.
    mapfile -t CANDIDATES < <(lsblk -lnpo PKNAME,PARTLABEL | awk '$2 == "ferrix-esp" {print $1}' | sort -u)
    case ${#CANDIDATES[@]} in
        0) echo "ferrix-postmortem: no partition named ferrix-esp; give --disk or --image" >&2; exit 1 ;;
        1) DISK="${CANDIDATES[0]}" ;;
        *) echo "ferrix-postmortem: more than one disk has a ferrix-esp: ${CANDIDATES[*]}; give --disk" >&2; exit 1 ;;
    esac
fi

# The disk's partitions: name, GPT name, GPT type. lsblk reads udev's cache;
# blkid is the fallback for a loop device udev has not seen yet.
PARTS="$WORK/parts"
lsblk -lnpo NAME,PARTLABEL,PARTTYPE,TYPE "$DISK" | awk '$NF == "part" {print $1, $2, $3}' > "$PARTS"
if ! grep -q ' ferrix-esp ' "$PARTS"; then
    : > "$PARTS"
    for p in "$DISK"p* "$DISK"[0-9]*; do
        [ -b "$p" ] || continue
        label=$(root blkid -p -s PART_ENTRY_NAME -o value "$p" 2>/dev/null || true)
        ptype=$(root blkid -p -s PART_ENTRY_TYPE -o value "$p" 2>/dev/null || true)
        echo "$p ${label:--} ${ptype:--}" >> "$PARTS"
    done
fi
by_label() { awk -v l="$1" '$2 == l {print $1; exit}' "$PARTS"; }
by_type() { awk -v t="$1" 'tolower($3) == t {print $1}' "$PARTS"; }

ESP=$(by_label ferrix-esp)
ROOTFS=$(by_label ferrix-root)
CONTROL=$(by_type "$CONTROL_TYPE" | head -n1)
mapfile -t SLOTS < <(by_type "$SLOT_TYPE")

section "disk $DISK"
lsblk -o NAME,SIZE,PARTLABEL,PARTTYPE,FSTYPE "$DISK" || true
echo "esp: ${ESP:-none}  root: ${ROOTFS:-none}  control: ${CONTROL:-none}  slots: ${SLOTS[*]:-none}"

show() { # file title
    if [ -f "$1" ]; then
        section "$2 ($(stat -c '%s bytes, %y' "$1"))"
        tail -n "$LINES" "$1" | tr -d '\r'
    fi
}

if [ -n "$ESP" ]; then
    mkdir -p "$WORK/esp"
    if root mount -t vfat -o ro,uid="$(id -u)",gid="$(id -g)",umask=077 "$ESP" "$WORK/esp"; then
        MOUNTS+=("$WORK/esp")
        E="$WORK/esp"
        F=$(find "$E" -maxdepth 1 -iname ferrix -type d | head -n1)
        section "ESP files"
        (cd "$E" && find . -type f -printf '%TY-%Tm-%Td %TH:%TM  %10s  %p\n' | sort)
        if [ -n "$F" ]; then
            ci() { find "$F" -maxdepth 1 -iname "$1" | head -n1; }
            show "$(ci BOOTLOG.TXT)" "loader log, last boot (FERRIX/BOOTLOG.TXT)"
            show "$(ci BOOTLOG.PRV)" "loader log, the boot before (FERRIX/BOOTLOG.PRV)"
            show "$(ci LASTKERNEL.TXT)" "kernel log rescued from RAM by the last boot (FERRIX/LASTKERNEL.TXT)"
            show "$(ci LASTKERNEL.PRV)" "kernel log rescued the boot before (FERRIX/LASTKERNEL.PRV)"
            show "$(ci CMDLINE.TXT)" "FERRIX/CMDLINE.TXT"
            [ -n "$(ci BOOTLOG.TXT)" ] || echo "(no FERRIX/BOOTLOG.TXT: the loader never ran, or it predates the boot log)"
        fi
    else
        echo "ferrix-postmortem: could not mount the ESP read-only" >&2
    fi
fi

# The A/B record and slot headers: dumped read-only, decoded here.
if [ -n "$CONTROL" ]; then
    root dd if="$CONTROL" of="$WORK/control.bin" bs=4096 count=32 status=none
    i=0
    for s in "${SLOTS[@]+"${SLOTS[@]}"}"; do
        root dd if="$s" of="$WORK/slot$i.bin" bs=4096 count=1 status=none
        i=$((i + 1))
    done
    root chown "$(id -u)" "$WORK"/*.bin 2>/dev/null || true
    section "A/B control record ($CONTROL) and slots (docs/AB-BOOT.md)"
    python3 -I "$(dirname "$0")/ferrix-slots-decode.py" "$WORK/control.bin" "$WORK"/slot*.bin
fi

if [ -n "$ROOTFS" ]; then
    mkdir -p "$WORK/root"
    if root mount -o ro,norecovery "$ROOTFS" "$WORK/root" 2>/dev/null \
        || root mount -o ro "$ROOTFS" "$WORK/root"; then
        MOUNTS=("$WORK/root" "${MOUNTS[@]}")
        if root test -d "$WORK/root/var/log"; then
            section "root /var/log, newest first"
            root find "$WORK/root/var/log" -type f -printf '%TY-%Tm-%Td %TH:%TM:%TS  %10s  %P\n' \
                | sort -r | head -n 20
            root find "$WORK/root/var/log" -type f -printf '%T@ %p\n' | sort -rn | head -n 5 \
                | while read -r _ f; do
                    section "tail of /var/log/${f#"$WORK/root/var/log/"}"
                    root tail -n 40 "$f" | tr -d '\r' | cat -v
                done
        else
            echo "(the root has no /var/log: Ferrix never got as far as its init)"
        fi
    else
        echo "ferrix-postmortem: could not mount the root read-only" >&2
    fi
fi

section "done (nothing was written)"
