#!/usr/bin/env python3
"""Build the btrfs test images from real `mkfs.btrfs`, and pack them small.

`ferrix-btrfs` is tested against structures its own tests build, and that
catches a parser putting a field in the wrong place. It cannot catch a parser
that agrees with its own tests and disagrees with btrfs. Only an image btrfs
itself wrote can, so this makes some.

One source tree, four images: uncompressed, zlib, LZO and zstd. The tree is
generated from a fixed seed, so every image holds the same files:

* ~100 small files in nested directories, so the fs tree on 4 KiB nodes is
  more than one level deep and a lookup has to descend -- checked, not hoped;
* a highly compressible file over 128 KiB, which btrfs splits into several
  compressed extents;
* a file of incompressible bytes, which btrfs stores raw even when asked to
  compress;
* a sparse file with a hole in the middle;
* a symlink, an empty file, an empty directory, and a name that is not UTF-8.

`mkfs.btrfs` is not reproducible -- the chunk tree UUID and creation times
change on every run even with fixed UUIDs -- so there is no `--check` mode. The
images are committed as generated. What the tests compare against is
`manifest.txt`, which *is* a function of the seed: one line per path with its
type, size and CRC-32C.

A 128 MiB image is almost entirely zeros, so each is stored as its non-zero
4 KiB blocks: repeated records of a little-endian u64 offset followed by
4096 bytes. That brings each image to a few hundred KiB.

`default-subvol.img.packed` is a separate, small image whose default
subvolume is not the top-level tree (`mkfs.btrfs -u default:sub`): the top
level holds `top-level-only`, and the subvolume `sub` holds `marker` and
`nested/file`. Its contents are fixed text, listed in the tests themselves.

`blank.img.packed` is what the write path starts from: an empty volume made
with `mkfs.btrfs`'s *default* profiles and features -- DUP metadata and system
chunks, a single data chunk, the free-space tree, skinny metadata, no-holes --
so the writer is tested against the volume a user gets, not a simplified one.
Its nodes are 4 KiB so that a few hundred items already split leaves.

`dup.img.packed` keeps two copies of everything, data too (`-m dup -d dup`),
and holds one file, `data.bin`: 12 KiB of bytes from the seed. It is 256 MiB,
because `--rootdir` makes a 64 MiB DUP data chunk and grows a smaller image
to fit it. It is what
the reader's fallback to a second copy is tested on, for nodes and for data
sectors alike.

`root.img.packed` is the same empty volume at 1 GiB, labelled `ferrix-root`:
what `cargo xtask run` boots with as `/`, and the label is how the kernel
tells it from any other btrfs disk. The system alone is tens of megabytes,
which 128 MiB of DUP metadata and single data cannot hold with room to work.

`home.img.packed` is an empty 8 GiB volume labelled `ferrix-home`, with UUIDs
of its own: the disk `cargo xtask run` keeps the users' files on, at `/home`,
which `--reset-root` leaves alone. The kernel tells it apart by its label.

Usage:
    python3 tools/common/gen/gen-btrfs-fixtures.py [--only NAME ...]

`--only` rebuilds just the named images (`none`, `zlib`, `lzo`, `zstd`,
`default-subvol`, `blank`, `dup`, `root`, `home`), leaving the others' committed bytes
alone.
"""

from __future__ import annotations

import os
import pathlib
import shutil
import struct
import subprocess
import sys
import tempfile

OUTPUT = pathlib.Path("src/lib/fs/btrfs/format/testdata")
VARIANTS = ("none", "zlib", "lzo", "zstd")
DEFAULT_SUBVOL = "default-subvol"
BLANK = "blank"
DUP = "dup"
DUP_FILE_SIZE = 12 * 1024
DUP_SIZE = 256 * 1024 * 1024
ROOT = "root"
IMAGE_SIZE = 128 * 1024 * 1024
ROOT_SIZE = 1024 * 1024 * 1024
ROOT_LABEL = "ferrix-root"
HOME = "home"
HOME_SIZE = 8 * 1024 * 1024 * 1024
HOME_LABEL = "ferrix-home"
HOME_FS_UUID = "0f3c3a5e-2222-4222-8333-444455556666"
HOME_DEVICE_UUID = "0f3c3a5e-bbbb-4bbb-8ccc-ddddeeeeffff"
BLOCK = 4096
FS_UUID = "0f3c3a5e-1111-4222-8333-444455556666"
DEVICE_UUID = "0f3c3a5e-aaaa-4bbb-8ccc-ddddeeeeffff"
TIMESTAMP = "2026-01-01 00:00:00 UTC"
SEED = 0x5EED_B7F5
# A packed image larger than this means the tree grew by accident.
BUDGET = 512 * 1024


def repository_root() -> pathlib.Path:
    return pathlib.Path(__file__).resolve().parent.parent.parent.parent


class Rng:
    """xorshift64*, so the tree does not depend on Python's `random` version."""

    def __init__(self, seed: int) -> None:
        self.state = seed or 1

    def next(self) -> int:
        x = self.state
        x ^= (x >> 12) & 0xFFFFFFFFFFFFFFFF
        x ^= (x << 25) & 0xFFFFFFFFFFFFFFFF
        x ^= x >> 27
        self.state = x & 0xFFFFFFFFFFFFFFFF
        return (self.state * 0x2545F4914F6CDD1D) & 0xFFFFFFFFFFFFFFFF

    def bytes(self, count: int) -> bytes:
        out = bytearray()
        while len(out) < count:
            out += struct.pack("<Q", self.next())
        return bytes(out[:count])


def crc32c(data: bytes) -> int:
    crc = 0xFFFFFFFF
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ (0x82F63B78 if crc & 1 else 0)
    return crc ^ 0xFFFFFFFF


def build_tree(root: pathlib.Path) -> None:
    rng = Rng(SEED)
    words = [b"alpha", b"beta", b"gamma", b"delta", b"epsilon", b"\n", b" "]
    for d in range(8):
        directory = root / f"dir{d:02}" / f"sub{d % 3}"
        directory.mkdir(parents=True)
        for f in range(12):
            size = rng.next() % 1200
            body = b"".join(words[rng.next() % len(words)] for _ in range(size // 4))
            (directory / f"file{f:02}.txt").write_bytes(body[:size])
    # Just over 128 KiB: two compressed extents, and one uncompressed one.
    line = b"compressible line of text\n"
    (root / "big.txt").write_bytes((line * (140_000 // len(line) + 1))[:140_000])
    (root / "random.bin").write_bytes(rng.bytes(24_000))
    with open(root / "sparse.bin", "wb") as sparse:
        sparse.write(rng.bytes(8192))
        sparse.seek(1024 * 1024)
        sparse.write(rng.bytes(4096))
    (root / "empty").write_bytes(b"")
    (root / "empty-dir").mkdir()
    os.symlink("dir00/sub0/file00.txt", root / "link")
    (root / os.fsdecode(b"not-utf8-\xff")).write_bytes(b"latin-1 name\n")


def manifest(root: pathlib.Path) -> bytes:
    lines = []
    for path in sorted(root.rglob("*"), key=lambda p: os.fsencode(p.relative_to(root))):
        name = os.fsencode(path.relative_to(root)).hex()
        if path.is_symlink():
            target = os.fsencode(os.readlink(path))
            lines.append(f"link {name} {len(target)} {crc32c(target):08x}")
        elif path.is_dir():
            lines.append(f"dir {name} 0 00000000")
        else:
            data = path.read_bytes()
            lines.append(f"file {name} {len(data)} {crc32c(data):08x}")
    header = "# type hex(path) size crc32c -- generated by tools/common/gen/gen-btrfs-fixtures.py\n"
    return (header + "\n".join(lines) + "\n").encode()


def make_image(source: pathlib.Path, image: pathlib.Path, variant: str) -> None:
    with open(image, "wb") as f:
        f.truncate(IMAGE_SIZE)
    command = [
        "mkfs.btrfs", "-q", "-m", "single", "-d", "single", "--nodesize", str(BLOCK),
        "-U", FS_UUID, "--device-uuid", DEVICE_UUID, "--rootdir", str(source),
    ]
    if variant != "none":
        command += ["--compress", variant]
    subprocess.run(command + [str(image)], check=True)


def build_default_subvol_tree(root: pathlib.Path) -> None:
    (root / "top-level-only").write_bytes(b"only in the top-level tree\n")
    (root / "sub" / "nested").mkdir(parents=True)
    (root / "sub" / "marker").write_bytes(b"in the default subvolume\n")
    (root / "sub" / "nested" / "file").write_bytes(b"nested in the default subvolume\n")


def make_default_subvol_image(source: pathlib.Path, image: pathlib.Path) -> None:
    with open(image, "wb") as f:
        f.truncate(IMAGE_SIZE)
    subprocess.run([
        "mkfs.btrfs", "-q", "-m", "single", "-d", "single", "--nodesize", str(BLOCK),
        "-U", FS_UUID, "--device-uuid", DEVICE_UUID, "--rootdir", str(source),
        "-u", "default:sub", str(image),
    ], check=True)


def make_blank_image(image: pathlib.Path, size: int = IMAGE_SIZE, label: str = "",
                     uuids: tuple[str, str] = (FS_UUID, DEVICE_UUID)) -> None:
    with open(image, "wb") as f:
        f.truncate(size)
    # No profile or feature flags: the defaults are the point.
    subprocess.run([
        "mkfs.btrfs", "-q", "--nodesize", str(BLOCK),
        "-U", uuids[0], "--device-uuid", uuids[1],
        *(["-L", label] if label else []), str(image),
    ], check=True)


def make_dup_image(source: pathlib.Path, image: pathlib.Path) -> None:
    with open(image, "wb") as f:
        f.truncate(DUP_SIZE)
    subprocess.run([
        "mkfs.btrfs", "-q", "-m", "dup", "-d", "dup", "--nodesize", str(BLOCK),
        "-U", FS_UUID, "--device-uuid", DEVICE_UUID, "--rootdir", str(source),
        str(image),
    ], check=True)


def fs_tree_level(image: pathlib.Path) -> int:
    dump = subprocess.run(
        ["btrfs", "inspect-internal", "dump-tree", "-t", "fs", str(image)],
        capture_output=True, text=True, check=True,
    ).stdout
    for line in dump.splitlines():
        parts = line.split()
        if len(parts) > 3 and parts[0] in ("node", "leaf") and parts[2] == "level":
            return int(parts[3])
    raise RuntimeError(f"no fs tree root in {image}")


def pack(image: pathlib.Path) -> bytes:
    out = bytearray()
    zero = bytes(BLOCK)
    with open(image, "rb") as f:
        offset = 0
        while block := f.read(BLOCK):
            if block != zero[: len(block)]:
                out += struct.pack("<Q", offset) + block.ljust(BLOCK, b"\0")
            offset += BLOCK
    return bytes(out)


def main() -> int:
    if shutil.which("mkfs.btrfs") is None:
        print("mkfs.btrfs is not installed (btrfs-progs)", file=sys.stderr)
        return 1
    names = VARIANTS + (DEFAULT_SUBVOL, BLANK, DUP, ROOT, HOME)
    only = sys.argv[2:] if sys.argv[1:2] == ["--only"] else list(names)
    if len(sys.argv) > 1 and sys.argv[1] != "--only" or not only or set(only) - set(names):
        print(f"usage: {sys.argv[0]} [--only NAME ...], NAME in {', '.join(names)}",
              file=sys.stderr)
        return 2
    os.chdir(repository_root())
    OUTPUT.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as scratch:
        source = pathlib.Path(scratch) / "tree"
        source.mkdir()
        build_tree(source)
        subprocess.run(["find", str(source), "-exec", "touch", "-h", "-d", TIMESTAMP, "{}", "+"],
                       check=True)
        (OUTPUT / "manifest.txt").write_bytes(manifest(source))
        for variant in (v for v in VARIANTS if v in only):
            image = pathlib.Path(scratch) / f"{variant}.img"
            make_image(source, image, variant)
            level = fs_tree_level(image)
            if level < 1:
                print(f"{variant}: fs tree is a single leaf; add files so lookups descend",
                      file=sys.stderr)
                return 1
            packed = pack(image)
            if len(packed) > BUDGET:
                print(f"{variant}: packed image is {len(packed)} bytes, over {BUDGET}",
                      file=sys.stderr)
                return 1
            (OUTPUT / f"{variant}.img.packed").write_bytes(packed)
            print(f"{variant}: fs tree level {level}, {len(packed) // (BLOCK + 8)} blocks, "
                  f"{len(packed)} bytes")
        if DEFAULT_SUBVOL in only:
            source = pathlib.Path(scratch) / "subvol-tree"
            source.mkdir()
            build_default_subvol_tree(source)
            subprocess.run(["find", str(source), "-exec", "touch", "-h", "-d", TIMESTAMP, "{}", "+"],
                           check=True)
            image = pathlib.Path(scratch) / f"{DEFAULT_SUBVOL}.img"
            make_default_subvol_image(source, image)
            packed = pack(image)
            if len(packed) > BUDGET:
                print(f"{DEFAULT_SUBVOL}: packed image is {len(packed)} bytes, over {BUDGET}",
                      file=sys.stderr)
                return 1
            (OUTPUT / f"{DEFAULT_SUBVOL}.img.packed").write_bytes(packed)
            print(f"{DEFAULT_SUBVOL}: {len(packed) // (BLOCK + 8)} blocks, {len(packed)} bytes")
        if BLANK in only:
            image = pathlib.Path(scratch) / f"{BLANK}.img"
            make_blank_image(image)
            packed = pack(image)
            (OUTPUT / f"{BLANK}.img.packed").write_bytes(packed)
            print(f"{BLANK}: {len(packed) // (BLOCK + 8)} blocks, {len(packed)} bytes")
        if DUP in only:
            source = pathlib.Path(scratch) / "dup-tree"
            source.mkdir()
            (source / "data.bin").write_bytes(Rng(SEED).bytes(DUP_FILE_SIZE))
            subprocess.run(["find", str(source), "-exec", "touch", "-h", "-d", TIMESTAMP, "{}", "+"],
                           check=True)
            image = pathlib.Path(scratch) / f"{DUP}.img"
            make_dup_image(source, image)
            packed = pack(image)
            if len(packed) > BUDGET:
                print(f"{DUP}: packed image is {len(packed)} bytes, over {BUDGET}",
                      file=sys.stderr)
                return 1
            (OUTPUT / f"{DUP}.img.packed").write_bytes(packed)
            print(f"{DUP}: {len(packed) // (BLOCK + 8)} blocks, {len(packed)} bytes")
        if ROOT in only:
            image = pathlib.Path(scratch) / f"{ROOT}.img"
            make_blank_image(image, ROOT_SIZE, ROOT_LABEL)
            packed = pack(image)
            if len(packed) > BUDGET:
                print(f"{ROOT}: packed image is {len(packed)} bytes, over {BUDGET}",
                      file=sys.stderr)
                return 1
            (OUTPUT / f"{ROOT}.img.packed").write_bytes(packed)
            print(f"{ROOT}: {len(packed) // (BLOCK + 8)} blocks, {len(packed)} bytes")
        if HOME in only:
            image = pathlib.Path(scratch) / f"{HOME}.img"
            make_blank_image(image, HOME_SIZE, HOME_LABEL, (HOME_FS_UUID, HOME_DEVICE_UUID))
            packed = pack(image)
            if len(packed) > BUDGET:
                print(f"{HOME}: packed image is {len(packed)} bytes, over {BUDGET}",
                      file=sys.stderr)
                return 1
            (OUTPUT / f"{HOME}.img.packed").write_bytes(packed)
            print(f"{HOME}: {len(packed) // (BLOCK + 8)} blocks, {len(packed)} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
