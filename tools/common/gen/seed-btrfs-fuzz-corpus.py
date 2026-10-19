#!/usr/bin/env python3
"""Write the seed corpus for the `btrfs_read` fuzz target.

Each input is a selector byte and a list of edits to one of the real images in
`src/lib/fs/btrfs/format/testdata/` (see `src/tests/fuzz/fuzz_targets/btrfs_read.rs` for the layout).
The seeds are the untouched images, so the first run already mounts all four,
plus two resealed edits so a mutated-but-checksummed image is in the corpus
from the start rather than something the fuzzer has to stumble on.

Usage:
    python3 tools/common/gen/seed-btrfs-fuzz-corpus.py
"""

import pathlib
import struct

OUT = pathlib.Path(__file__).resolve().parent.parent.parent.parent / "src" / "tests" / "fuzz" / "corpus" / "btrfs_read"

RESEAL = 0x80
SET_U8, SET_U64 = 1, 3


def edit(kind: int, block: int, offset: int, value: bytes) -> bytes:
    return bytes([kind]) + struct.pack("<HH", block, offset) + value


SEEDS = {
    "none-untouched": bytes([0]),
    "zlib-untouched": bytes([1]),
    "lzo-untouched": bytes([2]),
    "zstd-untouched": bytes([3]),
    # Block ordinal 0 is the lowest non-zero block, the superblock at 64 KiB.
    # Offset 0x64 is `nritems` in a node header and inside `root` in a
    # superblock; either way a resealed image that says something different.
    "none-reseal-set-u8": bytes([RESEAL | 0]) + edit(SET_U8, 1, 0x64, b"\x07"),
    "zstd-reseal-set-u64": bytes([RESEAL | 3]) + edit(SET_U64, 5, 0x30, struct.pack("<Q", 1)),
}


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    for name, data in SEEDS.items():
        (OUT / name).write_bytes(data)
    print(f"wrote {len(SEEDS)} seeds to {OUT}")


if __name__ == "__main__":
    main()
