#!/bin/bash
# MACHINE-SPECIFIC: written for the N150 bring-up from the customer's Windows
# PC (2026-10-15, docs/N150.md §13.1). It hardcodes the card PC's address on the
# customer's LAN (192.168.178.39:8150, override with SERVER=). Adjust those
# before running it anywhere else.
#
# Replace Ferrix's kernel, initramfs and command line on the N150 box's ESP.
# Run from Ubuntu live on the box:
#   wget -qO- 192.168.178.39:8150/esp | sudo bash
# Finds the one partition labelled ferrix-esp (PARTLABEL or LABEL) on a
# SATA or NVMe disk, fetches esp-files/FERRIX/{KERNEL.ELF,INITRD.IMG,CMDLINE.TXT},
# checks each against esp-files/SHA256SUMS, replaces them, syncs, unmounts.
# Touches nothing but those three files.
set -euo pipefail

SERVER="${SERVER:-http://192.168.178.39:8150}"
BASE="$SERVER/esp-files"
FILES="KERNEL.ELF INITRD.IMG CMDLINE.TXT"

die() { echo "esp: FAILED: $*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run as root (| sudo bash)"

# The ESP: a partition of a sata/nvme disk with PARTLABEL or LABEL ferrix-esp.
candidates=""
while read -r name type pkname; do
    [ "$type" = "part" ] || continue
    ptran=$(lsblk -dno TRAN "/dev/$pkname" 2>/dev/null | tr -d ' ')
    case "$ptran" in sata|nvme|ata) ;; *) continue ;; esac
    plabel=$(lsblk -dno PARTLABEL "/dev/$name" 2>/dev/null | tr -d ' ')
    label=$(lsblk -dno LABEL "/dev/$name" 2>/dev/null | tr -d ' ')
    if [ "$plabel" = "ferrix-esp" ] || [ "$label" = "ferrix-esp" ]; then
        candidates="$candidates /dev/$name"
    fi
done < <(lsblk -rno NAME,TYPE,PKNAME)
set -- $candidates
[ "$#" -eq 1 ] || die "want exactly one ferrix-esp partition on a sata/nvme disk, found $#:$candidates"
part="$1"
fstype=$(lsblk -dno FSTYPE "$part" | tr -d ' ')
[ "$fstype" = "vfat" ] || die "$part is '$fstype', not vfat"
echo "esp: the ESP is $part ($(lsblk -dno SIZE "$part" | tr -d ' '), disk $(lsblk -dno PKNAME "$part"))"

# Fetch and check everything before touching the disk.
work=$(mktemp -d)
mnt=$(mktemp -d)
cleanup() {
    mountpoint -q "$mnt" && umount "$mnt" || true
    rm -rf "$work"
    rmdir "$mnt" 2>/dev/null || true
}
trap cleanup EXIT

wget -q -O "$work/SHA256SUMS" "$BASE/SHA256SUMS" || die "no $BASE/SHA256SUMS"
mkdir -p "$work/FERRIX"
for f in $FILES; do
    grep -q "  FERRIX/$f\$" "$work/SHA256SUMS" || die "SHA256SUMS lists no FERRIX/$f"
    echo "esp: fetching FERRIX/$f"
    wget -q -O "$work/FERRIX/$f" "$BASE/FERRIX/$f" || die "fetching FERRIX/$f"
done
(cd "$work" && sha256sum -c --strict --ignore-missing SHA256SUMS) || die "checksum mismatch"

mount -t vfat "$part" "$mnt" || die "mounting $part"
[ -d "$mnt/FERRIX" ] || die "$part has no FERRIX directory: not Ferrix's ESP"
need=0
for f in $FILES; do need=$((need + $(stat -c %s "$work/FERRIX/$f"))); done
have=0
for f in $FILES; do [ -f "$mnt/FERRIX/$f" ] && have=$((have + $(stat -c %s "$mnt/FERRIX/$f"))); done
free=$(( $(df -B1 --output=avail "$mnt" | tail -n 1) ))
[ $((free + have)) -gt "$need" ] || die "ESP too small: $need bytes needed, $((free + have)) available"

for f in $FILES; do
    rm -f "$mnt/FERRIX/$f"
    cp "$work/FERRIX/$f" "$mnt/FERRIX/$f"
done
sync
# Read back what was written, from the disk.
echo 3 > /proc/sys/vm/drop_caches
for f in $FILES; do
    want=$(grep "  FERRIX/$f\$" "$work/SHA256SUMS" | cut -d' ' -f1)
    got=$(sha256sum "$mnt/FERRIX/$f" | cut -d' ' -f1)
    [ "$want" = "$got" ] || die "FERRIX/$f reads back wrong from $part"
    echo "esp: FERRIX/$f $got"
done
umount "$mnt"
sync
echo "esp: CMDLINE.TXT: $(cat "$work/FERRIX/CMDLINE.TXT")"
echo DONE
