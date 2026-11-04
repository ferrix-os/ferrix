#!/usr/bin/env python3
"""Each first process's row, read from the real source (docs/microkernel/BOOT-AUTHORITY.md §3, BA.5).

Three things. `first_rows.rs`'s const tables are what the design's table says, row by
row. No handle given at the kernel's three start sites takes its rights from anywhere
but those tables. No site gives the root job. L.object.321; a tool of the same class as
check-partitions, and separate code from what it reads (not F-27's independence).
Usage: check-first-rows.py [--self-test]
"""
import re
import sys
from pathlib import Path

ROWS = "src/kernel/src/first_rows.rs"
DESIGN = "docs/microkernel/BOOT-AUTHORITY.md"
SITES = (
    "src/kernel/src/boot_authority.rs",
    "src/kernel/src/linuxd_start.rs",
    "src/kernel/src/discovery/devmgr.rs",
)
GIVING = re.compile(r"\b(give\(\s*&region|push_within\(\s*&mut\s+(?:transfers|more))")


def tables_in_source(text: str) -> dict[str, list[tuple[str, str]]]:
    tables = {}
    for name, body in re.findall(r"pub(?:\(crate\))? const (\w+): Rows = Rows\(&\[(.*?)\]\);", text, re.S):
        rows = re.findall(r"Row \{\s*held: Held::(\w+),\s*rights: (.*?),\s*most: \w+,?\s*\},", body, re.S)
        tables[name] = [(held, " ".join(rights.split())) for held, rights in rows]
    return tables


def tables_in_design(text: str) -> dict[str, list[tuple[str, str]]]:
    tables: dict[str, list[tuple[str, str]]] = {}
    for name, held, rights in re.findall(r"^\| `(\w+)` \| `(\w+)` \| `([^`]+)` \|$", text, re.M):
        tables.setdefault(name, []).append((held, rights))
    return tables


def call_at(text: str, start: int) -> str:
    depth = 0
    for at in range(text.index("(", start), len(text)):
        depth += {"(": 1, ")": -1}.get(text[at], 0)
        if depth == 0:
            return text[start : at + 1]
    return text[start:]


def judge_sites(sites: dict[str, str]) -> list[str]:
    problems = []
    for path, text in sites.items():
        for match in GIVING.finditer(text):
            call = call_at(text, match.start())
            line = text.count("\n", 0, match.start()) + 1
            if ".of(Held::" not in call:
                problems.append(f"{path}:{line}: a handle's rights do not come from a row")
        for match in re.finditer(r"Object::Job\(\s*(?:Arc::clone\(\s*&?)?job::root", text):
            line = text.count("\n", 0, match.start()) + 1
            problems.append(f"{path}:{line}: the root job is given to a first process")
    return problems


def judge(source: str, design: str, sites: dict[str, str]) -> list[str]:
    problems = []
    written, designed = tables_in_source(source), tables_in_design(design)
    if not written:
        problems.append(f"{ROWS}: no tables found")
    for name in sorted(set(written) | set(designed)):
        ours, theirs = written.get(name), designed.get(name)
        if ours is None or theirs is None:
            problems.append(f"{name}: in only one of {ROWS} and {DESIGN}")
        elif ours != theirs:
            for row in ours:
                if row not in theirs:
                    problems.append(f"{name}: {row[0]} with {row[1]} is not in the design")
            for row in theirs:
                if row not in ours:
                    problems.append(f"{name}: the design's {row[0]} with {row[1]} is not in the source")
    return problems + judge_sites(sites)


def self_test() -> list[str]:
    source = "pub const A: Rows = Rows(&[\n    Row { held: Held::Job, rights: Rights::JOB, most: 1 },\n]);"
    design = "| `A` | `Job` | `Rights::JOB` |\n"
    good: dict[str, str] = {"s": "give(&region, Object::DebugLog, const { A.of(Held::Job) })"}
    cases = (
        ("the good one", source, design, good, None),
        ("a row more", source.replace("]);", "    Row { held: Held::Image, rights: X, most: 1 },\n]);"), design, good, "not in the design"),
        ("a right wider", source, design.replace("JOB", "ALL"), good, "not in the design"),
        ("a role less", source, design + "| `B` | `Job` | `Rights::JOB` |\n", good, "in only one"),
        ("rights by hand", source, design, {"s": "give(&region, Object::DebugLog, Rights::ALL)"}, "do not come from a row"),
        ("root job", source, design, {"s": "x(Object::Job(job::root()), y)"}, "root job"),
    )
    failed = []
    for what, text, table, sites, expected in cases:
        found = judge(text, table, sites)
        if (expected is None) != (not found) or (expected and not any(expected in line for line in found)):
            failed.append(f"self-test: {what}: {found}")
    return failed


def main() -> int:
    if "--self-test" in sys.argv:
        failed = self_test()
        print("\n".join(failed) or "check-first-rows: self-test passed")
        return 1 if failed else 0
    problems = judge(
        Path(ROWS).read_text(),
        Path(DESIGN).read_text(),
        {path: Path(path).read_text() for path in SITES},
    )
    for problem in problems:
        print(f"check-first-rows: {problem}")
    if problems:
        return 1
    print("check-first-rows: each first process's row is the design's, and every given handle is from it")
    return 0


if __name__ == "__main__":
    sys.exit(main())
