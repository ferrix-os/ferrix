#!/usr/bin/env python3
"""Decode Ferrix's A/B control record and slot headers from read-only dumps.

    ferrix-slots-decode.py CONTROL.bin [SLOT_A.bin [SLOT_B.bin]]

CONTROL.bin holds at least the first 65536 + 512 bytes of the control
partition; each SLOT file the first 4096 bytes of a slot partition, A first
(the lower GPT entry). The formats are docs/AB-BOOT.md §2 and §3, and the
rules mirror `ferrix-slots` (src/lib/proto/slots): `test-postmortem` checks
this decoder against records and headers that crate wrote. Reads files only.
"""

import hashlib
import struct
import sys

RECORD_MAGIC = b"FXBOOTCR"
RECORD_LEN = 512
COPY_OFFSETS = (0, 65536)
SLOT_MAGIC = b"FXSLOTHD"
HEADER_LEN = 4096
SEAL_AT = 4064
SLOT_NAMES = {0: "A", 1: "B", 0xFF: "none"}


def crc32c(data):
    crc = 0xFFFFFFFF
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ (0x82F63B78 if crc & 1 else 0)
    return crc ^ 0xFFFFFFFF


def decode_record(block):
    """The record's fields, or the reason the copy is not valid."""
    if len(block) < RECORD_LEN:
        return None, "short read"
    block = block[:RECORD_LEN]
    if not any(block):
        return None, "zero (never written)"
    if block[:8] != RECORD_MAGIC:
        return None, "bad magic"
    version, length, reserved = struct.unpack_from("<HHI", block, 8)
    if version != 1 or length != RECORD_LEN:
        return None, f"version {version} length {length}"
    (crc,) = struct.unpack_from("<I", block, 508)
    if crc32c(block[:508]) != crc:
        return None, "CRC mismatch (torn or damaged)"
    if reserved != 0 or any(block[27:508]):
        return None, "reserved bytes not zero"
    (sequence,) = struct.unpack_from("<Q", block, 16)
    good, trial, tries = block[24], block[25], block[26]
    if good > 1 or trial not in (0, 1, 0xFF) or trial == good or tries > 7:
        return None, f"inconsistent good {good} trial {trial} tries {tries}"
    if trial == 0xFF and tries != 0:
        return None, "tries without a trial"
    return {"sequence": sequence, "good": good, "trial": trial, "tries": tries}, None


def describe_record(fields):
    good = SLOT_NAMES[fields["good"]]
    if fields["trial"] == 0xFF:
        state = "no trial armed"
    elif fields["tries"] > 0:
        state = f"trial {SLOT_NAMES[fields['trial']]} armed, {fields['tries']} tries left"
    else:
        state = f"trial {SLOT_NAMES[fields['trial']]} FAILED (tries spent; the loader boots {good})"
    return f"sequence {fields['sequence']}, good {good}, {state}"


def report_record(path):
    with open(path, "rb") as file:
        data = file.read()
    copies = []
    for index, offset in enumerate(COPY_OFFSETS):
        fields, why = decode_record(data[offset : offset + RECORD_LEN])
        copies.append(fields)
        print(f"  copy {index}: " + (describe_record(fields) if fields else f"not valid: {why}"))
    valid = [(c["sequence"], -i, c) for i, c in enumerate(copies) if c]
    if not valid:
        print("  current: uninitialised -> the loader tries A, then B, and writes nothing")
        return
    current = max(valid)[2]
    print(f"  current: {describe_record(current)}")


def report_slot(name, path):
    with open(path, "rb") as file:
        header = file.read(HEADER_LEN)
    if len(header) < HEADER_LEN or not any(header):
        print(f"  slot {name}: empty")
        return
    if header[:8] != SLOT_MAGIC:
        print(f"  slot {name}: no slot header (bad magic)")
        return
    version, header_len, blobs = struct.unpack_from("<HHI", header, 8)
    (generation,) = struct.unpack_from("<Q", header, 16)
    label = header[24:56].split(b"\0", 1)[0].decode("utf-8", "replace")
    sealed = hashlib.sha256(header[:SEAL_AT]).digest() == header[SEAL_AT:HEADER_LEN]
    sizes = []
    for index, what in enumerate(("kernel", "initrd", "cmdline")):
        offset, length = struct.unpack_from("<QQ", header, 56 + 48 * index)
        sizes.append(f"{what} {length} B @ {offset}")
    seal = "seal ok" if sealed else "SEAL MISMATCH (the loader will not boot it)"
    print(f"  slot {name}: '{label}', generation {generation}, v{version}, {seal}")
    print(f"          {', '.join(sizes)} (blob hashes are checked by the loader, not here)")


def main(argv):
    if len(argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    report_record(argv[1])
    for name, path in zip("AB", argv[2:]):
        report_slot(name, path)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
