#!/usr/bin/env python3
"""Write `.ferrix/partitions` from a TOML (docs/microkernel/BOOT-AUTHORITY.md §5).

Uncertified: `tools/common/check/check-partitions.py`, separate code, judges the
bytes this writes. Usage: gen-partitions.py TOML ROOT OUT, ROOT being where the
images' paths are relative to.
"""
import hashlib
import struct
import sys
import tomllib

HEADER = 16
MAX_DEVICES = 8
DEVICE = 112
KINDS = {"pci": 1, "tree": 2}
ROLES = {"plain": 0, "files": 1, "linux": 2}


def pad(text: str, size: int) -> bytes:
    raw = text.encode()
    if len(raw) >= size:
        raise SystemExit(f"gen-partitions: {text!r} does not fit {size} bytes")
    return raw.ljust(size, b"\0")


def device(entry: dict) -> bytes:
    return (
        struct.pack(
            "<IIII",
            KINDS[entry["kind"]],
            1 if entry.get("needs_certified") else 0,
            entry.get("bdf", 0),
            entry.get("vendor", 0) | entry.get("device", 0) << 16,
        )
        + pad(entry.get("path", ""), 64)
        + pad(entry.get("compatible", ""), 32)
    )


def partition(entry: dict, root: str) -> bytes:
    with open(f"{root}/{entry['image']}", "rb") as handle:
        image = handle.read()
    # K14's digest: the length, then the pages zero-padded.
    padded = image + bytes(-len(image) % 4096)
    if not padded:
        padded = bytes(4096)
    digest = hashlib.sha256(struct.pack("<Q", len(padded)) + padded).digest()
    devices = entry.get("devices", [])
    if len(devices) > MAX_DEVICES:
        raise SystemExit("gen-partitions: too many devices")
    body = (
        pad(entry["name"], 32)
        + digest
        + pad(entry["image"], 64)
        + struct.pack(
            "<QQQII",
            entry.get("reserve_pages", 0),
            entry.get("budget_ns", 0),
            entry.get("window_ns", 0),
            entry.get("restarts", 0),
            len(devices),
        )
        + struct.pack("<II", ROLES[entry.get("role", "plain")], 0)
    )
    for item in devices:
        body += device(item)
    return body + bytes(DEVICE * (MAX_DEVICES - len(devices)))


def main() -> int:
    toml_path, root, out = sys.argv[1:4]
    with open(toml_path, "rb") as handle:
        config = tomllib.load(handle)
    partitions = config.get("partition", [])
    data = b"FXPD" + struct.pack("<HHII", 1, len(partitions), config.get("max_processes", 0), config.get("ready_ms", 0))
    for entry in partitions:
        data += partition(entry, root)
    with open(out, "wb") as handle:
        handle.write(data)
    return 0


if __name__ == "__main__":
    sys.exit(main())
