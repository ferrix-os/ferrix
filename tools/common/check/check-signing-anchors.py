#!/usr/bin/env python3
"""Validate the signing anchors, and refuse a development anchor where it
must not be (docs/SIGNING.md §6, §6.4; the consultant's D1, ledger 3731).

    python3 tools/common/check/check-signing-anchors.py               # the file
    python3 tools/common/check/check-signing-anchors.py --scan PATH.. # files
    python3 tools/common/check/check-signing-anchors.py --self-test

The file, `tools/common/data/signing-anchors.toml`, must hold:

* only `[release.<name>]` and `[development.<name>]` tables, each with a
  kind (slot, config or driver), an anchor id and a public key;
* release ids 0 or 1 and development ids 32768 (0x8000) and up, every
  (kind, id) once;
* each public key 52 bytes of hex: `L` 1 or 2, an LMS type of the product
  set (M24_H10, H15, H20) and `LMOTS_SHA256_N24_W4` (§2.1);
* no key in both sections, and no key twice.

It also refuses a workspace package that depends on the soft signer as
more than a dev-dependency, and any path in a kernel package's normal and
build dependency closure (`cargo metadata`, every platform) to the
verifier or the signer (§5.1, §5.2); xtask refuses a kernel image that
holds either's name besides (`tools/common/xtask/src/signing.rs`).

It also validates the hardware-family registry,
`tools/common/data/hardware-families.toml` (§1.4): every id once, in its
arch's range, one generic family per arch, and a recognition rule each.

`--scan` refuses every file that holds a development key's tree root `T`
(its last 24 bytes, which any compiled anchor table carries whatever its
layout): what a claimed image's files must never hold. xtask runs the same refusal on the
files it puts into a claimed profile's image (`tools/common/xtask/src/
signing.rs`); this is the same rule for any file at hand, a release
artifact included.

`--self-test` plants a development key in a scratch file and requires the
refusal, and plants each malformed anchor and requires its refusal.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import subprocess
import sys
import tempfile
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[3]
ANCHORS = ROOT / "tools/common/data/signing-anchors.toml"
FAMILIES = ROOT / "tools/common/data/hardware-families.toml"
ARCHES = {"x86_64": 0x01, "aarch64": 0x02, "armv7a": 0x03}
KINDS = {"slot", "config", "driver"}
PRODUCT_LMS = {0x0B, 0x0C, 0x0D}
LMOTS_N24_W4 = 0x07
DEVELOPMENT_MIN = 0x8000


def problems_of(data: dict) -> list[str]:
    out = []
    seen_ids = set()
    keys: dict[bytes, str] = {}
    for section in data:
        if section not in ("release", "development"):
            out.append(f"[{section}] is neither release nor development")
    for section in ("release", "development"):
        for name, anchor in data.get(section, {}).items():
            where = f"[{section}.{name}]"
            kind = anchor.get("kind")
            ident = anchor.get("anchor_id")
            hexkey = anchor.get("public_key", "")
            if kind not in KINDS:
                out.append(f"{where}: kind {kind!r} is not slot, config or driver")
            if not isinstance(ident, int):
                out.append(f"{where}: anchor_id is not an integer")
                continue
            if section == "release" and ident not in (0, 1):
                out.append(f"{where}: a release anchor id is 0 or 1, not {ident}")
            if section == "development" and not DEVELOPMENT_MIN <= ident <= 0xFFFF:
                out.append(f"{where}: a development anchor id is 32768 to 65535, not {ident}")
            if (kind, ident) in seen_ids:
                out.append(f"{where}: kind {kind} id {ident} is given twice")
            seen_ids.add((kind, ident))
            if not isinstance(hexkey, str) or not re.fullmatch(r"(?:[0-9a-f]{2})*", hexkey):
                out.append(f"{where}: public_key is not lower-case hex without spaces")
                continue
            key = bytes.fromhex(hexkey)
            if len(key) != 52:
                out.append(f"{where}: public_key is {len(key)} bytes, not 52")
                continue
            levels = int.from_bytes(key[0:4], "big")
            lms = int.from_bytes(key[4:8], "big")
            ots = int.from_bytes(key[8:12], "big")
            if levels not in (1, 2) or lms not in PRODUCT_LMS or ots != LMOTS_N24_W4:
                out.append(f"{where}: L {levels}, LMS type {lms:#x}, LM-OTS type {ots:#x} is not the product set")
            if key in keys:
                out.append(f"{where}: the same key as {keys[key]}")
            keys[key] = where
    return out


def family_problems(data: dict) -> list[str]:
    """The hardware-family registry (SIGNING.md §1.4): every id once, its
    high byte its arch's, exactly one generic family per arch with low byte
    0x01, and a recognition rule for each."""
    out = []
    ids = {}
    generic = {}
    for section in data:
        if section != "family":
            out.append(f"[{section}] is not [family.*]")
    for name, fam in data.get("family", {}).items():
        where = f"[family.{name}]"
        ident, arch = fam.get("id"), fam.get("arch")
        if arch not in ARCHES:
            out.append(f"{where}: arch {arch!r} is not x86_64, aarch64 or armv7a")
            continue
        if not isinstance(ident, int) or not 0 < ident <= 0xFFFF:
            out.append(f"{where}: id is not an integer of 1 to 65535")
            continue
        if ident >> 8 != ARCHES[arch]:
            out.append(f"{where}: id {ident:#06x} is not in {arch}'s range {ARCHES[arch]:#04x}xx")
        if ident in ids:
            out.append(f"{where}: id {ident:#06x} is also {ids[ident]}'s")
        ids[ident] = name
        if not fam.get("recognised_by"):
            out.append(f"{where}: no recognised_by rule")
        if fam.get("generic") is True:
            if ident & 0xFF != 0x01:
                out.append(f"{where}: a generic family's low byte is 0x01")
            if arch in generic:
                out.append(f"{where}: {arch} has a second generic family besides {generic[arch]}")
            generic[arch] = name
    for arch in ARCHES:
        if arch not in generic:
            out.append(f"{arch} has no generic family")
    return out


SIGNER = "ferrix-softsign"
VERIFIER = {"ferrix-sigverify", "ferrix-softsign"}
DEP_TABLES = ("dependencies", "build-dependencies")


def normal_dependencies(manifest: dict) -> set[str]:
    """Every package a manifest names as a normal or build dependency,
    target-specific ones included (dev-dependencies are host tests)."""
    names = set()
    tables = [manifest] + list(manifest.get("target", {}).values())
    for table in tables:
        for kind in DEP_TABLES:
            for key, value in table.get(kind, {}).items():
                names.add(value.get("package", key) if isinstance(value, dict) else key)
    return names


def closure_problems(graph: dict[str, list[str]], kernel: list[str], names: dict[str, str]) -> list[str]:
    """Walk every kernel package's normal and build dependency closure
    (`graph`: package id to the ids it depends on, dev edges left out) and
    refuse any path to the verifier or the signer, naming it (ledger 3843
    (1))."""
    out = []
    for start in kernel:
        seen = {start: None}
        queue = [start]
        while queue:
            node = queue.pop(0)
            for dep in graph.get(node, []):
                if dep in seen:
                    continue
                seen[dep] = node
                if names.get(dep) in VERIFIER:
                    path, at = [], dep
                    while at is not None:
                        path.append(names.get(at, at))
                        at = seen[at]
                    out.append("a kernel package reaches " + names[dep] + ": " + " -> ".join(reversed(path)))
                    continue
                queue.append(dep)
    return out


def metadata_graph() -> tuple[dict[str, list[str]], list[str], dict[str, str]]:
    """The workspace's resolved graph from `cargo metadata` (no build):
    normal and build edges only, every platform's."""
    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--offline"],
            cwd=ROOT, check=True, capture_output=True, text=True,
        ).stdout
    )
    names = {p["id"]: p["name"] for p in meta["packages"]}
    kernel = [
        p["id"] for p in meta["packages"]
        if pathlib.Path(p["manifest_path"]).resolve().is_relative_to(ROOT / "src/kernel")
    ]
    graph = {}
    for node in meta["resolve"]["nodes"]:
        graph[node["id"]] = [
            dep["pkg"] for dep in node.get("deps", [])
            if any(kind.get("kind") in (None, "build") for kind in dep.get("dep_kinds", []))
        ]
    return graph, kernel, names


def dependency_problems(manifests: dict[str, dict]) -> list[str]:
    """No package depends on the soft signer but as a dev-dependency (no
    image can link it, docs/SIGNING.md §5.1). The kernel's rule, no path at
    all to the verifier or the signer, is [closure_problems]."""
    out = []
    for path, manifest in sorted(manifests.items()):
        deps = normal_dependencies(manifest)
        if SIGNER in deps:
            out.append(f"{path} depends on {SIGNER}, the soft signer, as more than a dev-dependency")
    return out


def workspace_manifests() -> dict[str, dict]:
    root = tomllib.loads((ROOT / "Cargo.toml").read_text())
    out = {}
    for member in root.get("workspace", {}).get("members", []):
        path = ROOT / member / "Cargo.toml"
        if path.exists():
            out[f"{member}/Cargo.toml"] = tomllib.loads(path.read_text())
    return out


def development_keys(data: dict) -> list[tuple[str, bytes]]:
    """Each development key's tree root `T`, its last 24 bytes: the part
    no anchor table can leave out, whatever layout it gives `L`, the type
    codes and `I` (ledger 3843 (3)). S4's tables keep `T` contiguous."""
    out = []
    for name, anchor in data.get("development", {}).items():
        try:
            key = bytes.fromhex(anchor.get("public_key", ""))
        except ValueError:
            continue
        if len(key) == 52:
            out.append((name, key[28:]))
    return out


def scan(paths: list[pathlib.Path], keys: list[tuple[str, bytes]]) -> list[str]:
    hits = []
    for path in paths:
        files = [p for p in path.rglob("*") if p.is_file()] if path.is_dir() else [path]
        for f in files:
            data = f.read_bytes()
            for name, key in keys:
                if key in data:
                    hits.append(f"{f} holds the development key {name}")
    return hits


def self_test() -> int:
    data = tomllib.loads(ANCHORS.read_text())
    keys = development_keys(data)
    with tempfile.TemporaryDirectory(prefix="check-signing-anchors-") as scratch:
        clean = pathlib.Path(scratch) / "clean.img"
        clean.write_bytes(b"\0" * 4096)
        planted = pathlib.Path(scratch) / "planted.img"
        planted.write_bytes(b"\0" * 100 + keys[0][1] + b"\0" * 100)
        if len(keys[0][1]) != 24:
            print("signing-anchors: self-test: the search is not for T alone", file=sys.stderr)
            return 1
        if scan([clean], keys) or len(scan([planted], keys)) != 1:
            print("signing-anchors: self-test: a planted development key was NOT refused", file=sys.stderr)
            return 1
    bad = {
        "a key with spaces": {"development": {"x": {"kind": "slot", "anchor_id": 32768, "public_key": "00 " * 52}}},
        "a non-product type code": {"development": {"x": {"kind": "slot", "anchor_id": 32768, "public_key": "00000001" + "0000000e" + "00000007" + "00" * 40}}},
        "a stray table": {"other": {}},
        "release id 2": {"release": {"x": {"kind": "slot", "anchor_id": 2, "public_key": "00" * 52}}},
        "development id 1": {"development": {"x": {"kind": "slot", "anchor_id": 1, "public_key": "00" * 52}}},
        "unknown kind": {"development": {"x": {"kind": "boot", "anchor_id": 32768, "public_key": "00" * 52}}},
        "short key": {"development": {"x": {"kind": "slot", "anchor_id": 32768, "public_key": "00" * 51}}},
    }
    for why, case in bad.items():
        if not problems_of(case):
            print(f"signing-anchors: self-test: {why} was NOT refused", file=sys.stderr)
            return 1
    families = tomllib.loads(FAMILIES.read_text())
    for why, tamper in {
        "a duplicated id": lambda f: f["family"].__setitem__("dup", dict(f["family"]["x86_64-n150"])),
        "an id outside its arch": lambda f: f["family"]["x86_64-n150"].__setitem__("id", 0x0202),
        "a second generic family": lambda f: f["family"]["x86_64-n150"].__setitem__("generic", True),
    }.items():
        copy = tomllib.loads(FAMILIES.read_text())
        tamper(copy)
        if not family_problems(copy):
            print(f"signing-anchors: self-test: {why} in the family registry was NOT refused", file=sys.stderr)
            return 1
    if family_problems(families):
        print("signing-anchors: self-test: the real family registry is refused", file=sys.stderr)
        return 1
    planted = {
        "src/user/system/native/devmgr/Cargo.toml": {"dependencies": {"ferrix-softsign": {"path": "x"}}},
        "src/lib/crypto/sigverify/Cargo.toml": {"dev-dependencies": {"ferrix-softsign": {"path": "x"}}},
    }
    if len(dependency_problems(planted)) != 1:
        print("signing-anchors: self-test: a planted signer dependency was NOT refused", file=sys.stderr)
        return 1
    names = {"k": "ferrix-kernel-load", "lib": "ferrix-something", "v": "ferrix-sigverify", "s": "ferrix-sha256"}
    indirect = {"k": ["lib"], "lib": ["s", "v"]}
    found = closure_problems(indirect, ["k"], names)
    if found != ["a kernel package reaches ferrix-sigverify: ferrix-kernel-load -> ferrix-something -> ferrix-sigverify"]:
        print(f"signing-anchors: self-test: an indirect kernel path to the verifier was NOT refused ({found})", file=sys.stderr)
        return 1
    if closure_problems({"k": ["lib"], "lib": ["s"]}, ["k"], names):
        print("signing-anchors: self-test: a clean kernel closure was refused", file=sys.stderr)
        return 1
    shared = {"release": {"a": dict(data["development"]["slot"], anchor_id=0)}, "development": data["development"]}
    if not any("the same key" in p for p in problems_of(shared)):
        print("signing-anchors: self-test: a development key in release was NOT refused", file=sys.stderr)
        return 1
    print("signing-anchors: self-test passed: a planted development key root, each malformed anchor and family, a planted signer dependency and an indirect kernel path to the verifier are refused")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--scan", nargs="+", type=pathlib.Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    data = tomllib.loads(ANCHORS.read_text())
    problems = (
        problems_of(data)
        + family_problems(tomllib.loads(FAMILIES.read_text()))
        + dependency_problems(workspace_manifests())
        + closure_problems(*metadata_graph())
    )
    if args.scan:
        problems += scan(args.scan, development_keys(data))
    for p in problems:
        print(f"signing-anchors: {p}", file=sys.stderr)
    if problems:
        return 1
    releases = len(data.get("release", {}))
    developments = len(data.get("development", {}))
    scanned = f"; {len(args.scan)} path(s) scanned, no development key" if args.scan else ""
    families = len(tomllib.loads(FAMILIES.read_text()).get("family", {}))
    print(f"signing-anchors: {releases} release and {developments} development anchor(s) and {families} hardware families, valid{scanned}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
