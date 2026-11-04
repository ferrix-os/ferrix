#!/usr/bin/env python3
"""Every lock of a VMO's pages says what it is to a seal (docs/microkernel/K14.md,
K14 Q1, ledger 5745).

A sealed VMO stays immutable only if every path that changes a page tests the seal
under this lock, and K3's and K13's future paths must not miss it. So each
`pages.lock()` in vmo.rs carries, on the line before it, one of:

    // SEAL: refused        the site tests `sealed` and refuses
    // SEAL: read           the site changes no page and no page set
    // SEAL: sourced, why   only an object with a source reaches it; no seal takes one

and this fails on one with none. A `refused` site is also held to name `sealed`
within the twelve lines after it, so a tag cannot stand for a test that is not
there.
"""
import re
import sys
from pathlib import Path

FILE = "src/kernel/src/user/vmo.rs"
# rustfmt splits a chain over lines: `self\n.pages\n.lock()`.
LOCK = re.compile(r"\bpages\s*\.\s*lock\(\)")
TAG = re.compile(r"//\s*SEAL:\s*(refused|read|sourced)\b")


def statement_start(lines: list[str], index: int) -> int:
    """The first line of the statement a chain on line `index` belongs to."""
    while index > 0:
        here = lines[index].lstrip()
        above = lines[index - 1].rstrip()
        if here.startswith(".") or above.endswith("=") or above.endswith("self"):
            index -= 1
        else:
            break
    return index


def problems(text: str) -> list[str]:
    lines = text.split("\n")
    found = []
    for match in LOCK.finditer(text):
        index = text.count("\n", 0, match.start())
        line = lines[index]
        if line.lstrip().startswith("//"):
            continue
        first = statement_start(lines, index)
        before = lines[first - 1] if first else ""
        tag = TAG.search(before)
        end = text.count("\n", 0, match.end())
        if not tag:
            found.append(f"{FILE}:{end + 1}: a lock of a VMO's pages carries no SEAL tag")
            continue
        if tag.group(1) == "refused" and not any(
            "sealed" in later for later in lines[end + 1 : end + 13]
        ):
            found.append(f"{FILE}:{end + 1}: tagged refused, and no test of `sealed` follows")
    return found


def self_test() -> int:
    """Each shape rustfmt gives a lock, planted: tagged passes, untagged fires."""
    shapes = [
        "let a = self.pages.lock();",
        "let a = self\n    .pages\n    .lock()\n    .frames;",
        "let (a, b) =\n    self.pages\n        .lock()\n        .take(1);",
        "self.pages\n    .lock()\n    .frames\n    .len()",
    ]
    failed = 0
    for shape in shapes:
        if len(problems("fn f() {\n" + shape + "\n}\n")) != 1:
            print(f"check-vmo-seal-tags: self-test: untagged did not fire: {shape!r}")
            failed += 1
        if problems("fn f() {\n// SEAL: read\n" + shape + "\n}\n"):
            print(f"check-vmo-seal-tags: self-test: tagged fired: {shape!r}")
            failed += 1
    if failed:
        return 1
    print(f"check-vmo-seal-tags: self-test: {len(shapes)} shapes FIRED untagged and passed tagged")
    return 0


def main() -> int:
    if "--self-test" in sys.argv:
        return self_test()
    root = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(".")
    found = problems((root / FILE).read_text())
    for problem in found:
        print(problem)
    if found:
        print(f"check-vmo-seal-tags: {len(found)} problem(s)")
        return 1
    print("check-vmo-seal-tags: every lock of a VMO's pages is tagged")
    return 0


if __name__ == "__main__":
    sys.exit(main())
