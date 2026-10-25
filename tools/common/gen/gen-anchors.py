#!/usr/bin/env python3
"""Write the UEFI loader's trust-policy blocks (docs/SIGNING.md §10.6).

    python3 tools/common/gen/gen-anchors.py              # write the blocks
    python3 tools/common/gen/gen-anchors.py --check      # the gate
    python3 tools/common/gen/gen-anchors.py --self-test

A block is 512 bytes, as `ferrix-boot-policy`'s `encode_block` lays it out:

     0  magic    "FXPOLICY"
     8  version  u16 = 1
    10  mode     u8: 0 off, 1 required
    11  count    u8, at most 8
    12  zero     [u8; 4]
    16  entries  8 x (kind u16 = 1, anchor id u16, HSS public key [u8; 52])
   464  zero     to 512

Four blocks, one per table and mode, in src/boot/common/uefi/anchors/:
`release-off.bin` and `release-required.bin` hold the `[release]` slot
anchors of tools/common/data/signing-anchors.toml, `development-off.bin`
and `development-required.bin` its `[development]` slot anchors. xtask
picks one for each loader it builds (FERRIX_ANCHORS): a development image
takes `development-off`, an image of a profile `release-<slot_signing>`,
and the signing gate's relinked loader `development-required`.

`--check` refuses a block that differs from what this would write, and
holds the profiles to the rules of §10.6 (ledger 4016):

* `slot_signing` is `off` or `required`, in each profile's `image` table;
* `slot_families` names families of hardware-families.toml only;
* a profile buildable today is not `required` while `[release]` holds no
  slot anchor, since an empty release table under `required` boots
  nothing.

A host test of `ferrix-boot` parses each block with `parse_block` and
checks its mode and anchors (the Rust and this script lay it out alike).

`--self-test` plants a stale block and a `required` profile with an empty
release table, and requires each refusal.
"""

from __future__ import annotations

import pathlib
import struct
import sys
import tempfile
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[3]
ANCHORS = ROOT / "tools/common/data/signing-anchors.toml"
FAMILIES = ROOT / "tools/common/data/hardware-families.toml"
PROFILES = ROOT / "tools/common/data/profiles"
OUT = ROOT / "src/boot/common/uefi/anchors"

BLOCK_LEN = 512
MAX_ANCHORS = 8
ENTRY_LEN = 4 + 52
MODES = {"off": 0, "required": 1}
SLOT_KIND = 1


def slot_anchors(data: dict, section: str) -> list[tuple[int, bytes]]:
    """The slot kind's anchors of one section, by anchor id."""
    out = []
    for entry in data.get(section, {}).values():
        if entry.get("kind") == "slot":
            out.append((int(entry["anchor_id"]), bytes.fromhex(entry["public_key"])))
    return sorted(out)


def block(mode: str, anchors: list[tuple[int, bytes]]) -> bytes:
    if len(anchors) > MAX_ANCHORS:
        raise ValueError(f"{len(anchors)} slot anchors; a block holds {MAX_ANCHORS}")
    out = bytearray(BLOCK_LEN)
    out[0:8] = b"FXPOLICY"
    out[8:10] = struct.pack("<H", 1)
    out[10] = MODES[mode]
    out[11] = len(anchors)
    for index, (anchor_id, key) in enumerate(anchors):
        if len(key) != 52:
            raise ValueError(f"anchor {anchor_id:#x}: the key is {len(key)} bytes, not 52")
        at = 16 + index * ENTRY_LEN
        out[at:at + 2] = struct.pack("<H", SLOT_KIND)
        out[at + 2:at + 4] = struct.pack("<H", anchor_id)
        out[at + 4:at + ENTRY_LEN] = key
    return bytes(out)


def blocks(data: dict) -> dict[str, bytes]:
    out = {}
    for table in ("release", "development"):
        anchors = slot_anchors(data, table)
        for mode in MODES:
            out[f"{table}-{mode}.bin"] = block(mode, anchors)
    return out


def profile_problems(profiles: dict[str, dict], data: dict, families: dict) -> list[str]:
    known = set(families.get("family", {}))
    release = slot_anchors(data, "release")
    problems = []
    for name, profile in sorted(profiles.items()):
        mode = profile.get("image", {}).get("slot_signing")
        if mode not in MODES:
            problems.append(f"profile {name}: slot_signing is {mode!r}, not off or required")
        listed = profile.get("slot_families")
        if not isinstance(listed, list):
            problems.append(f"profile {name}: slot_families is not a list")
            listed = []
        for family in listed:
            if family not in known:
                problems.append(f"profile {name}: slot_families names {family!r}, not in hardware-families.toml")
        if mode == "required" and profile.get("buildable_from") == "today" and not release:
            problems.append(
                f"profile {name}: slot_signing = \"required\" while [release] holds no slot "
                "anchor would boot nothing (docs/SIGNING.md §10.6)"
            )
    return problems


def load_profiles() -> dict[str, dict]:
    return {p.stem: tomllib.loads(p.read_text()) for p in sorted(PROFILES.glob("*.toml"))}


def stale(out: pathlib.Path, wanted: dict[str, bytes]) -> list[str]:
    problems = []
    for name, data in wanted.items():
        path = out / name
        if not path.exists() or path.read_bytes() != data:
            problems.append(f"{path.relative_to(ROOT) if path.is_relative_to(ROOT) else path} is stale; run python3 tools/common/gen/gen-anchors.py")
    extra = {p.name for p in out.glob("*.bin")} - set(wanted) if out.exists() else set()
    problems += [f"{out / name} is not a block this writes" for name in sorted(extra)]
    return problems


def self_test() -> int:
    data = tomllib.loads(ANCHORS.read_text())
    families = tomllib.loads(FAMILIES.read_text())
    wanted = blocks(data)
    with tempfile.TemporaryDirectory(prefix="gen-anchors-") as scratch:
        out = pathlib.Path(scratch)
        for name, bytes_ in wanted.items():
            (out / name).write_bytes(bytes_)
        if stale(out, wanted):
            print("gen-anchors: self-test: fresh blocks were refused", file=sys.stderr)
            return 1
        planted = bytearray(wanted["release-off.bin"])
        planted[11] = 1
        (out / "release-off.bin").write_bytes(bytes(planted))
        if not stale(out, wanted):
            print("gen-anchors: self-test: a stale block was NOT refused", file=sys.stderr)
            return 1
    empty = {"development": data.get("development", {})}
    required = {"p": {"image": {"slot_signing": "required"}, "slot_families": [], "buildable_from": "today"}}
    if not profile_problems(required, empty, families):
        print("gen-anchors: self-test: required with an empty release table was NOT refused", file=sys.stderr)
        return 1
    for why, profile in {
        "an unknown mode": {"image": {"slot_signing": "maybe"}, "slot_families": []},
        "an unknown family": {"image": {"slot_signing": "off"}, "slot_families": ["x86_64-mainframe"]},
        "no family list": {"image": {"slot_signing": "off"}},
    }.items():
        if not profile_problems({"p": profile}, data, families):
            print(f"gen-anchors: self-test: {why} was NOT refused", file=sys.stderr)
            return 1
    if profile_problems(load_profiles(), data, families):
        print("gen-anchors: self-test: the real profiles are refused", file=sys.stderr)
        return 1
    print("gen-anchors: self-tests pass")
    return 0


def main() -> int:
    args = sys.argv[1:]
    if args == ["--self-test"]:
        return self_test()
    data = tomllib.loads(ANCHORS.read_text())
    families = tomllib.loads(FAMILIES.read_text())
    wanted = blocks(data)
    problems = profile_problems(load_profiles(), data, families)
    if args == ["--check"]:
        problems += stale(OUT, wanted)
        for problem in problems:
            print(f"gen-anchors: {problem}", file=sys.stderr)
        if problems:
            return 1
        print(f"gen-anchors: {len(wanted)} blocks current; the profiles' slot keys hold")
        return 0
    if args:
        print(__doc__, file=sys.stderr)
        return 2
    if problems:
        for problem in problems:
            print(f"gen-anchors: {problem}", file=sys.stderr)
        return 1
    OUT.mkdir(parents=True, exist_ok=True)
    for name, bytes_ in wanted.items():
        (OUT / name).write_bytes(bytes_)
    print(f"gen-anchors: wrote {len(wanted)} blocks to {OUT.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
