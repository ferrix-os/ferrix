#!/usr/bin/env python3
"""Judge a partitions description by its bytes (docs/microkernel/BOOT-AUTHORITY.md §5).

T2 / TQL-5, and separate code from the generator (not F-27's independence): it
parses the file itself, reading the images from ROOT, and refuses a digest that is
not its image's, a device named twice, a reservation past the board's memory, and any
malformed field. Usage: check-partitions.py FILE ROOT [--memory-pages N].
"""
import argparse
import hashlib
import struct
import sys

HEADER, PARTITION, DEVICE, DEVICES = 16, 168 + 8 * 112, 112, 8


def text(field: bytes, what: str, problems: list[str]) -> str:
    end = field.find(b"\0")
    if end < 0 or field[end:].strip(b"\0"):
        problems.append(f"{what}: not NUL-padded")
        return ""
    return field[:end].decode(errors="replace")


def judge(data: bytes, root: str, memory_pages: int | None) -> list[str]:
    problems: list[str] = []
    if len(data) < HEADER or data[:4] != b"FXPD":
        return ["no FXPD header"]
    version, count, _processes, _ready_millis = struct.unpack_from("<HHII", data, 4)
    if version != 1:
        problems.append(f"version {version}")
    if count > 16:
        problems.append(f"{count} partitions")
    if len(data) != HEADER + count * PARTITION:
        return problems + [f"{len(data)} bytes for {count} partitions"]
    seen: set[tuple] = set()
    reserve = 0
    servers = 0
    held = 0
    for index in range(count):
        at = HEADER + index * PARTITION
        name = text(data[at : at + 32], f"partition {index}'s name", problems)
        digest = data[at + 32 : at + 64]
        image = text(data[at + 64 : at + 128], f"{name}'s image", problems)
        pages, _budget, _window, _restarts, used = struct.unpack_from("<QQQII", data, at + 128)
        reserve += pages
        try:
            with open(f"{root}/{image}", "rb") as handle:
                bytes_of = handle.read()
        except OSError as error:
            problems.append(f"{name}: {image}: {error}")
        else:
            padded = bytes_of + bytes(-len(bytes_of) % 4096)
            real = hashlib.sha256(struct.pack("<Q", len(padded)) + padded).digest()
            if real != digest:
                problems.append(f"{name}: its digest is not {image}'s")
        if used > DEVICES:
            problems.append(f"{name}: {used} devices")
            continue
        servers += struct.unpack_from("<I", data, at + 160)[0] == 1
        held += used
        for slot in range(used):
            place = at + 168 + slot * DEVICE
            kind, flags, bdf, ids = struct.unpack_from("<IIII", data, place)
            if flags & 1:
                problems.append(f"{name}: device {slot} needs a certified driver: this kernel has no grant")
            if kind not in (1, 2) or flags & ~1:
                problems.append(f"{name}: device {slot} of kind {kind}, flags {flags}")
            key = (kind, bdf, data[place + 16 : place + 80])
            if key in seen:
                problems.append(f"{name}: device {slot} is named twice")
            seen.add(key)
            _ = ids
    if servers and held:
        problems.append("a file server beside a safety device: no volume is bound yet")
    if 3 + 2 * servers + count + held > 64:
        problems.append(f"{count} images and {held} devices: more than the manager's first message holds")
    if memory_pages is not None and reserve > memory_pages:
        problems.append(f"{reserve} pages reserved of {memory_pages}")
    return problems


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("file")
    parser.add_argument("root")
    parser.add_argument("--memory-pages", type=int)
    args = parser.parse_args()
    with open(args.file, "rb") as handle:
        data = handle.read()
    problems = judge(data, args.root, args.memory_pages)
    for problem in problems:
        print(f"check-partitions: {problem}")
    if problems:
        return 1
    print("check-partitions: the description is what its images say")
    return 0


if __name__ == "__main__":
    sys.exit(main())
