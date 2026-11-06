#!/usr/bin/env python3
"""The orphan step (docs/MICROKERNEL.md section 6.2, B5): no requirement of the
kernel may be verified only from a check that leaves ring 0 with the load.

The load's in-kernel checks go with it at the lift. A requirement whose only
`Verifies:` tags are in load-ring files would lose its evidence then, without
anything failing. This step reads every `Verifies:` tag in src/kernel/src, with
the item-boundary gate's own classifier, and fails when an id is verified only
from load-ring files and is not on the shrinking list
tools/common/data/evidence-orphans.json. An id on the list that has a real home
now fails too, as stale, and one with no tag left fails as having lost its check,
so the list only ever shrinks, and only by a check that exists.

Scope, as section 6.2 states it: it counts `Verifies:` tags in kernel source
files only. An id with no such tag -- one whose only check is a host test or an
xtask row -- is outside this step and is argued apart; `--report` says how many.

A check file a crate above its ring builds (`raised_checks`: the load's crate, or
the composition root) does not link without the load, so it is no home: an id
resting on those and on load-ring files alone is on the file's second list,
`raised_only`, a ratchet held the same way (ledger 5782 (1), (3)). A new id there
fails until it is listed by a reviewed edit; one with a real home must leave. Those
files are M5's to bring down (docs/KERNEL-CRATES.md section 3 (3)), and the ratchet
becomes a gate at M5.

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


def homes(sources) -> dict[str, set[str]]:
    """Each id verified in `sources` (file to text), with the files it is in."""
    where = collections.defaultdict(set)
    for rel, text in sources.items():
        for ident in tags_of(text):
            where[ident].add(rel)
    return where


def orphans(where, ring_of) -> set[str]:
    """The ids verified only from load-ring files."""
    return {ident for ident, files in where.items() if {ring_of.get(rel) for rel in files} == {"load"}}


def raised_only(where, ring_of, raised) -> set[str]:
    """The ids resting on raised check files and load-ring files, at least one
    raised: no home in a kernel without the load, though the ring says so."""
    found = set()
    for ident, files in where.items():
        if all(ring_of.get(rel) == "load" or rel in raised for rel in files) and any(rel in raised for rel in files):
            found.add(ident)
    return found


def judge(found: set[str], allowed: set[str], where, what: str) -> list[str]:
    """What fails on one list: an id off it, one that lost its check, a stale one."""
    problems = [
        f"{ident}: verified only from {what} and not on the list: "
        "give it a check in a core or item file (docs/MICROKERNEL.md section 6.2)"
        for ident in sorted(found - allowed)
    ]
    for ident in sorted(allowed - found):
        if ident in where:
            problems.append(f"{ident}: on the {what} list and not resting on them now: take it off")
        else:
            problems.append(f"{ident}: on the {what} list with no Verifies tag left: it lost its check")
    return problems


def self_test() -> int:
    """The planted cases: `xtask check` runs these with the step."""
    rings = {"core.rs": "core", "item.rs": "item", "load_check.rs": "load", "raised_check.rs": "core"}
    raised = {"raised_check.rs"}
    home = {"core.rs": "/// Verifies: L.a.1\n", "load_check.rs": "/// Verifies: L.a.1, L.a.2\n"}
    assert orphans(homes(home), rings) == {"L.a.2"}, "one home in the core is a home"
    moved = {"core.rs": "", "load_check.rs": "/// Verifies: L.a.1\n"}
    where = homes(moved)
    found = orphans(where, rings)
    assert found == {"L.a.1"}, "a tag moved from a core check into a load-ring check is an orphan"
    assert len(judge(found, set(), where, "load-ring checks")) == 1, "an orphan off the list fails"
    assert judge(found, {"L.a.1"}, where, "load-ring checks") == [], "an orphan on the list passes"
    assert len(judge(set(), {"L.a.1"}, homes(home), "load-ring checks")) == 1, "a stale entry fails"
    gone = judge(set(), {"L.a.9"}, homes(home), "load-ring checks")
    assert len(gone) == 1 and "lost its check" in gone[0], "a deleted tag is no re-homing"
    assert orphans(homes({"item.rs": "// no tag\n"}), rings) == set(), "no tag is out of scope"
    lifted = homes({"raised_check.rs": "/// Verifies: L.a.3\n", "load_check.rs": "/// Verifies: L.a.3\n"})
    assert orphans(lifted, rings) == set(), "a raised tag hides it from the first list"
    assert raised_only(lifted, rings, raised) == {"L.a.3"}, "and the second list has it"
    assert len(judge({"L.a.3"}, set(), lifted, "raised check files")) == 1, "off the ratchet, it fails"
    print("evidence homes: self-test passed")
    return 0


def untagged_units(g, ring_of, where) -> int:
    """How many requirements whose units are in core or item files have no
    kernel tag at all: what "argued apart" has to answer for."""
    count = 0
    pattern = re.compile(r"requirement\s+<'([HL]\.[A-Za-z0-9_]+\.\d+)'>.*?(?=requirement\s+<'|\Z)", re.S)
    for model in sorted((ROOT / "docs/sysml").glob("*.sysml")):
        for match in pattern.finditer(model.read_text()):
            if match.group(1) in where:
                continue
            unit = re.search(r'unit\s*=\s*(\(.*?\)|"[^"]*")\s*;', match.group(0), re.S)
            for name in re.findall(r'"([^"]+)"', unit.group(1)) if unit else []:
                parts = name.split("::")
                files = ("/".join(parts[:k]) + suffix for k in range(len(parts), 0, -1) for suffix in (".rs", "/mod.rs"))
                if any(ring_of.get(rel) in ("core", "item") for rel in files):
                    count += 1
                    break
    return count


def main(argv: list[str]) -> int:
    if "--self-test" in argv:
        return self_test()
    g = gate()
    manifest = g.load_manifest()
    files = g.kernel_files()
    ring_of, *_ = g.classify(manifest, files)
    where = homes({rel: g.kernel_path(rel).read_text() for rel in files})
    raised = {
        entry["file"]
        for entry in manifest.get("raised_checks", {}).get("entries", [])
        if entry.get("crate") in ("ferrix-kernel-raised-checks", "ferrix-kernel")
    }
    found = orphans(where, ring_of)
    leaning = raised_only(where, ring_of, raised)
    recorded = json.loads(LIST.read_text())
    allowed, ratchet = set(recorded["ids"]), set(recorded["raised_only"])
    print(f"evidence homes: {len(where)} ids verified in kernel files, "
          f"{len(found)} only from load-ring checks ({len(allowed)} listed), "
          f"{len(leaning)} only from raised check files ({len(ratchet)} listed)")
    if "--report" in argv:
        print(f"evidence homes: {untagged_units(g, ring_of, where)} core and item requirements have no kernel tag: "
              "outside this step, argued apart")
        for ident in sorted(leaning):
            print(f"  {ident}: {', '.join(sorted(where[ident]))}")
    problems = judge(found, allowed, where, "load-ring checks")
    problems += judge(leaning, ratchet, where, "raised check files")
    for problem in problems:
        print(f"check-evidence-homes: {problem}")
    if problems:
        print(f"check-evidence-homes: FAILED, {len(problems)} problem(s)")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
