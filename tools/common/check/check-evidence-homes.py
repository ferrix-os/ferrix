#!/usr/bin/env python3
"""The orphan step (docs/MICROKERNEL.md section 6.2, B5): no requirement of the
kernel may be verified only from a check that leaves ring 0 with the load.

The load's in-kernel checks go with it at the lift. A requirement whose only
`Verifies:` tags are in load-ring files would lose its evidence then, without
anything failing. This step reads every `Verifies:` tag in src/kernel/src, with
the item-boundary gate's own classifier, and fails when an id is verified only
from load-ring files and is not on the shrinking list
tools/common/data/evidence-orphans.json. An id on the list that has a home now
fails too, as stale, so the list only ever shrinks by being edited.

Scope, as section 6.2 states it: it counts `Verifies:` tags in kernel source
files only. An id with no such tag -- one whose only check is a host test or an
xtask row -- is outside this step and is argued apart. A check file that a crate
above its ring builds (`raised_checks`) still counts as its own ring's: those
files are M5's to bring down (docs/KERNEL-CRATES.md section 3 (3)), and `--report`
lists the ids that rest on them alone.

    python3 tools/common/check/check-evidence-homes.py [--report]
    python3 tools/common/check/check-evidence-homes.py --self-test
"""

from __future__ import annotations

import collections
import importlib.util
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent.parent
LIST = ROOT / "tools/common/data/evidence-orphans.json"
ID = re.compile(r"\b([HL]\.[A-Za-z0-9_]+\.\d+)\b")


def gate():
    """check-item-boundary.py, whose classifier this step reads the rings with."""
    sys.path.insert(0, str(ROOT / "tools/common/check"))
    spec = importlib.util.spec_from_file_location("cib", ROOT / "tools/common/check/check-item-boundary.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def tags_of(text: str) -> list[str]:
    """The ids of every `Verifies:` tag in `text`, a tag being one line."""
    found = []
    for line in text.splitlines():
        tag = line.split("Verifies:", 1)
        if len(tag) == 2:
            found.extend(ID.findall(tag[1]))
    return found


def homes(sources, ring_of) -> dict[str, set[str]]:
    """Each id verified in `sources` (file to text), with the files it is in."""
    where = collections.defaultdict(set)
    for rel, text in sources.items():
        for ident in tags_of(text):
            where[ident].add(rel)
    return where


def orphans(where, ring_of) -> set[str]:
    """The ids verified only from load-ring files."""
    return {ident for ident, files in where.items() if {ring_of.get(rel) for rel in files} == {"load"}}


def judge(found: set[str], allowed: set[str]) -> list[str]:
    """What fails: an orphan not on the list, and a stale entry on it."""
    problems = [
        f"{ident}: verified only from load-ring checks and not on the list: "
        "give it a check in a core or item file (docs/MICROKERNEL.md section 6.2)"
        for ident in sorted(found - allowed)
    ]
    problems += [
        f"{ident}: on the list and no longer an orphan: take it off evidence-orphans.json"
        for ident in sorted(allowed - found)
    ]
    return problems


def self_test() -> int:
    """The planted cases, for a reader who wants them without a gate."""
    rings = {"core.rs": "core", "item.rs": "item", "load_check.rs": "load"}
    home = {"core.rs": "/// Verifies: L.a.1\n", "load_check.rs": "/// Verifies: L.a.1, L.a.2\n"}
    assert orphans(homes(home, rings), rings) == {"L.a.2"}, "one home in the core is a home"
    moved = {"core.rs": "", "load_check.rs": "/// Verifies: L.a.1\n"}
    found = orphans(homes(moved, rings), rings)
    assert found == {"L.a.1"}, "a tag moved into a load-ring check is an orphan"
    assert len(judge(found, set())) == 1, "an orphan off the list fails"
    assert judge(found, {"L.a.1"}) == [], "an orphan on the list passes"
    assert len(judge(set(), {"L.a.1"})) == 1, "a stale entry fails"
    assert orphans(homes({"item.rs": "// no tag\n"}, rings), rings) == set(), "no tag is out of scope"
    print("evidence homes: self-test passed")
    return 0


def main(argv: list[str]) -> int:
    if "--self-test" in argv:
        return self_test()
    g = gate()
    manifest = g.load_manifest()
    files = g.kernel_files()
    ring_of, *_ = g.classify(manifest, files)
    where = homes({rel: (g.KERNEL_SRC / rel).read_text() for rel in files}, ring_of)
    found = orphans(where, ring_of)
    allowed = set(json.loads(LIST.read_text())["ids"])
    print(f"evidence homes: {len(where)} ids verified in kernel files, "
          f"{len(found)} only from load-ring checks, {len(allowed)} on the list")
    if "--report" in argv:
        raised = {
            entry["file"]
            for entry in manifest.get("raised_checks", {}).get("entries", [])
            if entry.get("crate") in ("ferrix-kernel-load", "ferrix-kernel")
        }
        leaning = sorted(
            ident for ident, files_of in where.items()
            if ident not in found and all(ring_of.get(rel) == "load" or rel in raised for rel in files_of)
        )
        print(f"evidence homes: {len(leaning)} more rest only on checks raised above their ring (M5's):")
        for ident in leaning:
            print(f"  {ident}: {', '.join(sorted(where[ident]))}")
    problems = judge(found, allowed)
    for problem in problems:
        print(f"check-evidence-homes: {problem}")
    if problems:
        print(f"check-evidence-homes: FAILED, {len(problems)} problem(s)")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
