#!/bin/bash
# MACHINE-SPECIFIC: written for the N150 bring-up from the customer's Windows
# PC (2026-10-15, docs/N150.md §13.1). It hardcodes the Windows PC's paths as
# WSL sees them (/mnt/g/GameLab/iso/ferrix/...). Adjust those before running
# it anywhere else.
#
# Build the Ferrix boot card image: MBR, one 4 GiB FAT32 partition (0x0C)
# holding the N150 ESP's tree with the newest FERRIX files. Run as root in WSL.
set -euo pipefail
D=/mnt/g/GameLab/iso/ferrix/card
NEW=/mnt/g/GameLab/iso/ferrix/n150/esp-files/FERRIX
OUT=$D/ferrix-card.img
SRC=/tmp/card-src
DST=/tmp/card-dst
cleanup() { umount "$SRC" 2>/dev/null || true; umount "$DST" 2>/dev/null || true; losetup -D 2>/dev/null || true; }
trap cleanup EXIT

PART_START=2048                      # sectors (1 MiB)
PART_SECTORS=$((4 * 1024 * 1024 * 2)) # 4 GiB
rm -f "$OUT" /tmp/card-fat.img
truncate -s $(( (PART_START + PART_SECTORS) * 512 )) "$OUT"
echo "label: dos
start=$PART_START, size=$PART_SECTORS, type=c, bootable" | sfdisk -q "$OUT"

truncate -s $((PART_SECTORS * 512)) /tmp/card-fat.img
mkfs.vfat -F 32 -n FERRIXBOOT /tmp/card-fat.img >/dev/null
mkdir -p "$SRC" "$DST"
mount -o loop,ro "$D/n150-esp.img" "$SRC"
mount -o loop /tmp/card-fat.img "$DST"
cp -r "$SRC"/. "$DST"/
cp "$NEW"/KERNEL.ELF "$NEW"/INITRD.IMG "$NEW"/CMDLINE.TXT "$DST"/FERRIX/
( cd "$NEW" && sha256sum KERNEL.ELF INITRD.IMG CMDLINE.TXT ) > /tmp/want
( cd "$DST/FERRIX" && sha256sum KERNEL.ELF INITRD.IMG CMDLINE.TXT ) > /tmp/got
cmp /tmp/want /tmp/got
echo "--- card tree"; (cd "$DST" && find . -type f -printf '%10s %p\n' | sort -k2)
df -h "$DST" | tail -1
umount "$DST"; umount "$SRC"
dd if=/tmp/card-fat.img of="$OUT" bs=1M seek=1 conv=notrunc status=none
rm -f /tmp/card-fat.img
sfdisk -l "$OUT" | tail -2
sha256sum "$OUT"
