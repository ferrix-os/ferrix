#!/usr/bin/env bash
# Run host `btrfs check` over every volume the btrfs write path's tests write,
# and over the volumes `ferrix-btrfs-mkfs` makes.
#
# `src/lib/fs/btrfs-write` checks its own output with a consistency checker of its
# own and with the stage 11 reader. Neither is btrfs. This script is the
# third oracle: the ignored test `btrfs_check_images` writes each image the
# write path produces into a scratch directory, and `btrfs check` from
# btrfs-progs reads every one, data checksums included. Any error it reports,
# or a non-zero exit, fails the script.
#
# Needs btrfs-progs on PATH; refuses to run, rather than passing, without it.
#
# Also runs the power-fail test's host half at full size, which is too slow
# for every `cargo test`.
#
# Usage: tools/common/test/btrfs-check-writer.sh

set -euo pipefail

if ! command -v btrfs > /dev/null; then
    echo "btrfs-check-writer: btrfs-progs is not installed" >&2
    exit 1
fi

out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT

FERRIX_BTRFS_OUT="$out" cargo test -q -p ferrix-btrfs-write -- \
    --ignored --exact tests::btrfs_check_images > "$out/test.log" 2>&1 \
    || { cat "$out/test.log" >&2; exit 1; }
# And what `ferrix-btrfs-mkfs` makes, fresh and filled by the write path.
FERRIX_BTRFS_OUT="$out" cargo test -q --release -p ferrix-btrfs-mkfs -- \
    --ignored --exact tests::btrfs_check_images > "$out/mkfs.log" 2>&1 \
    || { cat "$out/mkfs.log" >&2; exit 1; }

shopt -s nullglob
images=("$out"/*.img)
if [[ ${#images[@]} -eq 0 ]]; then
    echo "btrfs-check-writer: the test wrote no images" >&2
    exit 1
fi

failed=0
for image in "${images[@]}"; do
    name=$(basename "$image" .img)
    if btrfs check --readonly --check-data-csum "$image" > "$out/$name.log" 2>&1; then
        echo "btrfs check: $name clean"
    else
        echo "btrfs check: $name FAILED" >&2
        cat "$out/$name.log" >&2
        failed=1
    fi
done

# The power-fail test's host half at full size: two hundred seeds, each cut
# at twenty-five points, every write after the last flush kept or dropped at
# random (`src/lib/fs/btrfs-write/src/tests/powerfail.rs`). Release, because it
# is five thousand volumes opened, replayed and checked.
if cargo test -q --release -p ferrix-btrfs-write -- \
    --ignored --exact tests::powerfail::hundreds_of_cuts > "$out/cuts.log" 2>&1; then
    echo "power-fail: 5000 cuts replayed and checked clean"
else
    echo "power-fail: FAILED" >&2
    cat "$out/cuts.log" >&2
    failed=1
fi
exit "$failed"
