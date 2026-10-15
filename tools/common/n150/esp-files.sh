#!/bin/bash
# Build the N150 box's ESP files from a commit and put them where the box's
# `esp` script fetches them (wget -qO- <host>:8150/esp | sudo bash).
#
#   tools/common/n150/esp-files.sh [COMMIT] [OUT]
#
# Run from Git Bash on the Windows PC. COMMIT (default HEAD) is pushed to
# nazuna as a new side ref, checked out in a detached worktree there, the
# box's desktop branch (n1-n150-image, e2c07dae0) merged in when it is not
# already an ancestor, and `cargo xtask n150-install` run on a small disk:
# the same ESP the box's disk gets (release kernel, the desktop initramfs,
# its CMDLINE.TXT). esp-files.py copies FERRIX/{KERNEL.ELF,INITRD.IMG,
# CMDLINE.TXT} out of that ESP with SHA256SUMS, and they are copied to
# OUT/esp-files (default G:/GameLab/iso/ferrix/n150).
set -euo pipefail

commit=$(git rev-parse "${1:-HEAD}")
out="${2:-/g/GameLab/iso/ferrix/n150}"
desktop=e2c07dae0
host=nazuna-wg
repo=Documents/projects/os/ferrix
stamp=$(date +%Y%m%d-%H%M%S)
ref="win/n150-esp-$stamp"
here=$(cd "$(dirname "$0")" && pwd)

echo "esp-files: $commit -> $host:$ref"
git push "$host:$repo" "$commit:refs/heads/$ref"
scp -q "$here/esp-files.py" "$host:esp-files-$stamp.py"

cat > "${TMPDIR:-/tmp}/n150-esp-$stamp.sh" <<EOF
set -euo pipefail
cd ~/$repo
# One worktree, reused: cargo's fingerprints name the source path, so a new
# path each run rebuilt every workspace crate.
wt=.claude/worktrees/win-n150-esp
if [ -d "\$wt" ]; then
    git -C "\$wt" reset -q --hard
    git -C "\$wt" checkout -q --detach "$ref"
else
    git worktree add --detach "\$wt" "$ref" >/dev/null
fi
cd "\$wt"
if ! git merge-base --is-ancestor $desktop HEAD; then
    # The desktop's xtask: its patch, but main.rs and n150.rs (which the
    # SATA harness also changed) taken whole from it; nothing outside xtask.
    git diff $desktop^ $desktop -- tools/common/xtask         ':!tools/common/xtask/src/main.rs' ':!tools/common/xtask/src/n150.rs'         ':!tools/common/xtask/src/n150/tests.rs' | git apply -3
    git checkout $desktop -- tools/common/xtask/src/main.rs tools/common/xtask/src/n150.rs         tools/common/xtask/src/n150/tests.rs
fi
git log --oneline -1; git status --short
mkdir -p /tmp/n150-esp-$stamp/tree
echo n150 > /tmp/n150-esp-$stamp/tree/README
export CARGO_TARGET_DIR=\$HOME/target-n150-esp
cargo xtask n150-install --tree /tmp/n150-esp-$stamp/tree --disk /tmp/n150-esp-$stamp/disk.img \
    --disk-bytes 4294967296 </dev/null | tee /tmp/n150-esp-$stamp/install.log
esp=\$(sed -n 's/.*the ESP from \(.*\), .ferrix-root.*/\1/p' /tmp/n150-esp-$stamp/install.log | tail -n 1)
[ -f "\$esp" ] || { echo "esp-files: no ESP path in the install log"; exit 1; }
python3 ~/esp-files-$stamp.py "\$esp" /tmp/n150-esp-$stamp/out
# Compressed for the trip back: the kernel is mostly debug information.
(cd /tmp/n150-esp-$stamp/out/FERRIX && zstd -q -T0 -f --rm KERNEL.ELF INITRD.IMG)
rm -f /tmp/n150-esp-$stamp/disk.img /tmp/n150-esp-$stamp/disk.img.*
EOF
sed -i 's/\r$//' "${TMPDIR:-/tmp}/n150-esp-$stamp.sh"
scp -q "${TMPDIR:-/tmp}/n150-esp-$stamp.sh" "$host:n150-esp-$stamp.sh"
ssh "$host" "bash n150-esp-$stamp.sh" </dev/null

rm -rf "$out/esp-files.new"
mkdir -p "$out/esp-files.new"
scp -q -r "$host:/tmp/n150-esp-$stamp/out/." "$out/esp-files.new/"
ssh "$host" "rm -rf /tmp/n150-esp-$stamp ~/n150-esp-$stamp.sh ~/esp-files-$stamp.py" </dev/null
(cd "$out/esp-files.new/FERRIX" && zstd -q -d -f --rm KERNEL.ELF.zst INITRD.IMG.zst)
(cd "$out/esp-files.new" && sha256sum -c --strict SHA256SUMS)
rm -rf "$out/esp-files"
mv "$out/esp-files.new" "$out/esp-files"
echo "esp-files: $out/esp-files ready from $commit; on the box: wget -qO- 192.168.178.39:8150/esp | sudo bash"
