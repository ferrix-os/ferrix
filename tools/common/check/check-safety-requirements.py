#!/usr/bin/env python3
"""Hold the safety manual to its evidence.

`docs/certification/SAFETY-MANUAL.md` is an out-of-context safety argument: an
integrator reads it, designs against its assumed safety requirements, and
discharges its assumptions of use. That makes a stale claim in it worse than no
manual at all, because a manual is believed and a missing one is not.

So every claim names its evidence in `tools/common/data/safety-requirements.json`, and
this asserts three things about the pair.

  1. **The manual and the register agree on which ids exist.** An `ASR-9` added
     to one and not the other fails, in either direction. A requirement that
     exists only in prose has no evidence; one that exists only in the register
     is claiming nothing.

  2. **Every named piece of evidence exists.** A file path must resolve, and a
     `path::symbol` reference must actually name something in that file. This
     is the check that earns its place: the manual cites `zero_frame`,
     `check_stacks`, `enable_user_access_protection` and a dozen others, and a
     rename that left the manual behind would leave an argument pointing at
     nothing.

  3. **Every requirement has evidence at all.** An ASR with an empty
     `demonstrated_by` is an assertion, and the manual's whole claim is that it
     does not make those.

The register also holds the unsafe obligations (finding F-26): the closed set
of reasons unsafe exists in the certified item, which every unsafe site there
names by id and `check-unsafe-audit.py` resolves. The same three checks hold
them: the manual's table of them (`| `(ID)` |` rows) and the register agree,
each one's `serves` names a requirement, failure mode or assumption of use the
register defines, and each one's `argued_by` resolves.

What it deliberately does not check: whether the evidence actually *supports*
the claim. No script can. It checks that the citation resolves, which is the
part that rots silently.

    python3 tools/common/check/check-safety-requirements.py
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent.parent
REGISTER = ROOT / "tools" / "common" / "data" / "safety-requirements.json"
MANUAL = ROOT / "docs" / "certification" / "SAFETY-MANUAL.md"

ID = re.compile(r"\b(ASR-\d+|FM-\d+|AoU-\d+)\b")
# A row of the manual's table of unsafe obligations: `| `(DEVICE)` | ...`.
OBLIGATION_ROW = re.compile(r"^\|\s*`\(([A-Z][A-Z0-9]*(?:-[A-Z0-9]+)*)\)`\s*\|", re.MULTILINE)
OBLIGATION_ID = re.compile(r"^[A-Z][A-Z0-9]*(?:-[A-Z0-9]+)*$")
# The heading that defines an id: `### AoU-28 — ...`.
DEFINITION = re.compile(r"^#+\s+(ASR-\d+|FM-\d+|AoU-\d+)\b", re.MULTILINE)


def ids_in_manual() -> set[str]:
    return set(ID.findall(MANUAL.read_text(encoding="utf-8")))


def ids_defined_twice() -> list[str]:
    """Ids the manual heads twice. Two branches that each took the next
    free number pass every other check here, since the id is in both the
    manual and the register (2026-10-11: AoU-28, the x86-64 clock's and the
    A/B slots')."""
    headed = DEFINITION.findall(MANUAL.read_text(encoding="utf-8"))
    return sorted({name for name in headed if headed.count(name) > 1})


def obligations_in_manual() -> set[str]:
    return set(OBLIGATION_ROW.findall(MANUAL.read_text(encoding="utf-8")))


def check_obligations(register: dict, registered: set[str]) -> tuple[int, int]:
    """Hold the unsafe obligations to the manual and their evidence.
    Returns (status, references checked)."""
    status = 0
    checked = 0
    obligations = register.get("unsafe_obligations", [])
    ids = [entry["id"] for entry in obligations]
    for name in sorted({name for name in ids if ids.count(name) > 1}):
        print(f"safety-requirements: unsafe obligation {name} is defined twice", file=sys.stderr)
        status = 1
    for name in ids:
        if not OBLIGATION_ID.match(name):
            print(
                f"safety-requirements: unsafe obligation `{name}` is not an upper-case id",
                file=sys.stderr,
            )
            status = 1
    tabled = obligations_in_manual()
    for name in sorted(set(ids) - tabled):
        print(
            f"safety-requirements: unsafe obligation ({name}) has no row in the manual's table",
            file=sys.stderr,
        )
        status = 1
    for name in sorted(tabled - set(ids)):
        print(
            f"safety-requirements: the manual tables unsafe obligation ({name}), "
            "which the register does not define",
            file=sys.stderr,
        )
        status = 1
    for entry in obligations:
        if not entry["serves"]:
            print(f"safety-requirements: ({entry['id']}) serves nothing", file=sys.stderr)
            status = 1
        for served in entry["serves"]:
            if served not in registered:
                print(
                    f"safety-requirements: ({entry['id']}) serves {served}, which is not registered",
                    file=sys.stderr,
                )
                status = 1
        if not entry["argued_by"]:
            print(f"safety-requirements: ({entry['id']}) has no evidence", file=sys.stderr)
            status = 1
        for reference in entry["argued_by"]:
            checked += 1
            if problem := evidence_exists(reference):
                print(f"safety-requirements: ({entry['id']}) cites {problem}", file=sys.stderr)
                status = 1
    return status, checked


def evidence_exists(reference: str) -> str | None:
    """`None` if the reference resolves, else why it does not."""
    if "::" in reference:
        path_part, symbol = reference.split("::", 1)
    else:
        path_part, symbol = reference, None

    path = ROOT / path_part
    if not path.is_file():
        return f"no such file: {path_part}"
    if symbol is None:
        return None

    text = path.read_text(encoding="utf-8", errors="replace")
    # A definition, not a mention: the symbol must appear after `fn`, `struct`,
    # `const`, `static` or `enum`, or as a Python `def`. A doc comment that
    # names it is not evidence that it exists.
    defined = re.search(
        rf"(?:fn|struct|const|static|enum|type|def)\s+{re.escape(symbol)}\b", text
    )
    return None if defined else f"{path_part} defines no `{symbol}`"


def main() -> int:
    register = json.loads(REGISTER.read_text(encoding="utf-8"))
    manual = ids_in_manual()
    status = 0

    registered: set[str] = set()
    registered |= {r["id"] for r in register["assumed_safety_requirements"]}
    registered |= {f["id"] for f in register["failure_modes"]}
    registered |= set(register["assumptions_of_use"])

    missing_from_manual = sorted(registered - manual)
    missing_from_register = sorted(manual - registered)

    if missing_from_manual:
        print(
            "safety-requirements: recorded but not stated in the manual. A claim\n"
            "  with evidence and no text says nothing to an integrator:",
            file=sys.stderr,
        )
        for name in missing_from_manual:
            print(f"    {name}", file=sys.stderr)
        status = 1

    if missing_from_register:
        print(
            "safety-requirements: stated in the manual with no entry here. The\n"
            "  manual's claim is that it cites evidence for everything; an id\n"
            "  with none is the claim it exists to avoid:",
            file=sys.stderr,
        )
        for name in missing_from_register:
            print(f"    {name}", file=sys.stderr)
        status = 1

    for name in ids_defined_twice():
        print(
            f"safety-requirements: the manual heads {name} twice; the one that "
            "landed second takes the next free number",
            file=sys.stderr,
        )
        status = 1

    checked = 0
    for requirement in register["assumed_safety_requirements"]:
        references = requirement["implemented_by"] + requirement["demonstrated_by"]
        if not requirement["demonstrated_by"]:
            print(
                f"safety-requirements: {requirement['id']} has no evidence",
                file=sys.stderr,
            )
            status = 1
        for reference in references:
            checked += 1
            if problem := evidence_exists(reference):
                print(
                    f"safety-requirements: {requirement['id']} cites {problem}",
                    file=sys.stderr,
                )
                status = 1

    for mode in register["failure_modes"]:
        for reference in mode["mitigated_by"]:
            checked += 1
            if problem := evidence_exists(reference):
                print(
                    f"safety-requirements: {mode['id']} cites {problem}",
                    file=sys.stderr,
                )
                status = 1

    obligation_status, obligation_checked = check_obligations(register, registered)
    status |= obligation_status
    checked += obligation_checked

    print(
        f"safety-requirements: {len(register['assumed_safety_requirements'])} "
        f"assumed requirements, {len(register['failure_modes'])} failure modes, "
        f"{len(register['assumptions_of_use'])} assumptions of use, "
        f"{len(register.get('unsafe_obligations', []))} unsafe obligations, "
        f"{checked} evidence references resolved"
    )
    return status


if __name__ == "__main__":
    sys.exit(main())
