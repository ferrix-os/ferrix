#!/bin/bash
# Ferrix onto the N150's NVMe, from an Ubuntu live session:
#   wget -qO- 192.168.178.39:8150/go | sudo bash
# Finds the one internal NVMe drive, checks its size, asks for ERASE,
# streams the image onto it and checks the result. The only typing is ERASE.
set -u
SERVER=http://192.168.178.39:8150
IMAGE=n150-disk.img.zst
IMAGE_SHA=464dd412c84a7ccdf53d797bbc29b8f42256998bda6ca3927f0ea58356a5e42d
DISK_BYTES=512110190592
INFO=/tmp/n150-info.txt

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
die() { printf '\n\033[1;31mSTOP: %s\033[0m\n' "$*"; exit 1; }

[ "$(id -u)" = 0 ] || die "run it with sudo: wget -qO- 192.168.178.39:8150/go | sudo bash"

say "The box (also saved in $INFO, photograph it)"
{
    echo "--- ip"; ip -br addr
    echo "--- lspci"; lspci -nn
    echo "--- dmesg (network)"; dmesg | grep -iE 'yt6801|motorcomm|stmmac|1f0a|rtw89|8852' | tail -20
    echo "--- disks"; lsblk -d -o NAME,MODEL,SERIAL,SIZE,TRAN
} 2>&1 | tee "$INFO"

say "Network"
wget -q --spider "$SERVER/$IMAGE" || die "cannot reach $SERVER/$IMAGE (is the server running on the PC, is the box on the LAN or Wi-Fi?)"
echo "the server answers"

if ! command -v zstd >/dev/null; then
    say "Installing zstd"
    apt-get install -y zstd >/dev/null 2>&1 || { apt-get update >/dev/null 2>&1 && apt-get install -y zstd >/dev/null 2>&1; } || die "could not install zstd"
fi

say "The drive"
mapfile -t NVME < <(lsblk -dn -b -o NAME,TRAN,SIZE | awk '$2 == "nvme" || $2 == "sata" { print $1 " " $3 }')
[ "${#NVME[@]}" = 1 ] || die "expected exactly one internal (NVMe or SATA) drive, found ${#NVME[@]}: ${NVME[*]:-none}"
NAME=${NVME[0]% *}
SIZE=${NVME[0]#* }
[ "$SIZE" = "$DISK_BYTES" ] || die "/dev/$NAME is $SIZE bytes, the image is built for $DISK_BYTES; rebuild it: cargo xtask n150-install --disk-bytes $SIZE"
D=""
for link in /dev/disk/by-id/nvme-* /dev/disk/by-id/ata-*; do
    case "$link" in *-part*|*nvme-eui.*|*_1|*-part*) continue ;; esac
    [ "$(readlink -f "$link")" = "/dev/$NAME" ] && { D=$link; break; }
done
[ -n "$D" ] || D=/dev/$NAME
lsblk -o NAME,MODEL,SERIAL,SIZE,FSTYPE,LABEL "/dev/$NAME"
echo
echo "Target: $D"
echo "Everything on it is erased (whatever is installed there now)."

printf '\nType ERASE to overwrite it: '
read -r answer </dev/tty
[ "$answer" = ERASE ] || die "not confirmed, nothing written"

for part in $(lsblk -ln -o NAME "/dev/$NAME" | tail -n +2); do
    umount "/dev/$part" 2>/dev/null
    wipefs -a "/dev/$part" >/dev/null 2>&1
done
swapoff -a 2>/dev/null

say "Discarding the whole drive (seconds on an SSD)"
wipefs -a "/dev/$NAME" >/dev/null 2>&1
blkdiscard -f "/dev/$NAME" || echo "no discard (the old signatures are wiped; the rest is free space)"

# The image's data is its first 6.31 GB plus three small pieces (a btrfs
# superblock copy, a 2 MiB block near the end, the backup GPT); everything
# else is zeros. Write exactly those, then read them back.
HEAD_BLOCKS=95                     # x 64 MiB = 6375342080 bytes
HEAD_SHA=cb0e2cd2c5b8e8da2f7557b305a72bc1010a2af1f0b871c3d82c707e745fbce1
say "Writing the first 6.4 GB"
wget -qO- "$SERVER/$IMAGE" | zstd -dc 2>/dev/null \
    | dd of="/dev/$NAME" bs=64M count=$HEAD_BLOCKS iflag=fullblock conv=fsync status=progress
st=("${PIPESTATUS[@]}")
[ "${st[2]}" = 0 ] || die "writing the drive failed (dd exit ${st[2]})"
say "Writing the three pieces at the end"
wget -qO /tmp/n150-pieces.sums "$SERVER/pieces/SHA256SUMS" || die "no pieces/SHA256SUMS"
for p in 67240192 125026304 125026897; do
    wget -qO "/tmp/$p.blk" "$SERVER/pieces/$p.blk" || die "fetching piece $p"
    want=$(awk -v f="$p.blk" '$2 == f { print $1 }' /tmp/n150-pieces.sums)
    [ "$(sha256sum < "/tmp/$p.blk" | cut -d' ' -f1)" = "$want" ] || die "piece $p downloaded wrong; run the line again"
    dd if="/tmp/$p.blk" of="/dev/$NAME" bs=4096 seek=$p conv=notrunc,fsync status=none || die "writing piece $p"
done
sync
say "Reading back"
echo 3 > /proc/sys/vm/drop_caches
got=$(dd if="/dev/$NAME" bs=64M count=$HEAD_BLOCKS iflag=fullblock status=none | sha256sum | cut -d' ' -f1)
[ "$got" = "$HEAD_SHA" ] || die "the first 6.4 GB read back wrong ($got)"
echo "first 6.4 GB OK"
for p in 67240192 125026304 125026897; do
    n=$(( $(stat -c %s "/tmp/$p.blk") / 4096 ))
    want=$(awk -v f="$p.blk" '$2 == f { print $1 }' /tmp/n150-pieces.sums)
    got=$(dd if="/dev/$NAME" bs=4096 skip=$p count=$n status=none | sha256sum | cut -d' ' -f1)
    [ "$got" = "$want" ] || die "piece $p read back wrong"
done
echo "pieces OK"

say "Check"
partprobe "/dev/$NAME" 2>/dev/null; udevadm settle 2>/dev/null; sleep 1
lsblk -o NAME,SIZE,FSTYPE,LABEL,PARTLABEL "/dev/$NAME" | tee -a "$INFO"
labels=$(lsblk -ln -o LABEL,PARTLABEL "/dev/$NAME" | tr '\n' ' ')
case "$labels" in *ferrix-root*) ;; *) die "no ferrix-root on the drive after writing" ;; esac
command -v sgdisk >/dev/null && sgdisk -v "/dev/$NAME"

printf '\n\033[1;32mDONE. Shut down, take the card out, and power on: the box boots Ferrix from the NVMe.\033[0m\n'
printf 'First boot installs and is slower. Photograph the screens, and any panic whole.\n'
