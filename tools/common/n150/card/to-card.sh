#!/bin/bash
# MACHINE-SPECIFIC: written for the N150 bring-up from the customer's Windows
# PC (2026-10-15, docs/N150.md §13.1). It hardcodes the Windows PC's paths as
# Git Bash sees them (/f/Dokumente/projekte/os, the worktree
# .claude/worktrees/ahci, /g/GameLab/iso/ferrix/...) and the card as drive H:.
# Adjust those before running it anywhere else.
#
# Build the N150 ESP files from a commit of branch ahci and put them on the
# boot card (FAT32 FERRIXBOOT, mounted on Windows). Run from Git Bash:
#   bash /g/GameLab/iso/ferrix/card/to-card.sh [COMMIT] [DRIVE]
set -euo pipefail
WT=/f/Dokumente/projekte/os/.claude/worktrees/ahci
commit=$(git -C "$WT" rev-parse "${1:-HEAD}")
drive=${2:-H}
card="/${drive,,}/FERRIX"
[ -d "$card" ] || { echo "to-card: no $card: is the card in, as drive $drive:?"; exit 1; }
cd "$WT"
bash tools/common/n150/esp-files.sh "$commit"
NEW=/g/GameLab/iso/ferrix/n150/esp-files
cp "$NEW/FERRIX/KERNEL.ELF" "$NEW/FERRIX/INITRD.IMG" "$NEW/FERRIX/CMDLINE.TXT" "$card/"
mkdir -p "/${drive,,}/EFI/BOOT" && cp "$NEW/EFI/BOOT/BOOTX64.EFI" "/${drive,,}/EFI/BOOT/BOOTX64.EFI"
sync
( cd "/${drive,,}" && sha256sum -c "$NEW/SHA256SUMS" )
echo "to-card: $commit is on the card ($drive:). Eject it, then boot the box."
