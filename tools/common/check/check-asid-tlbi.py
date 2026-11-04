#!/usr/bin/env python3
"""No TLB invalidation on AArch64 narrows to an ASID but the slot's own.

A hit in `arch/aarch64/asid.rs` invalidates nothing, which is safe only because
every shootdown reaches every tag: `tlbi vaae1is` and `tlbi vmalle1is` name no ASID.
QEMU's TCG drops a page under every ASID whichever `tlbi` it is told, so a shootdown
narrowed to one ASID boots clean there and goes stale on a real core: this is the
check that sees it (`docs/microkernel/K1.md` §8.1, L.aarch64.80).

    python3 tools/common/check/check-asid-tlbi.py
    python3 tools/common/check/check-asid-tlbi.py --self-test
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
TREE = ROOT / "src/kernel/src/arch/aarch64"
# Every invalidation the kernel issues on AArch64.
ALLOWED = {
    "vmalle1is": "a shootdown of everything: every ASID",
    "vaae1is": "a shootdown of a page: every ASID",
    "vmalle1": "a starting core's own TLB, before it runs anything",
    "aside1": "a miss's slot, on this processor alone; cpu.rs's invalidating write",
}
TLBI = re.compile(r'^\s*"?\s*tlbi\s+(\w+)', re.M)


def problems_in(text: str, name: str) -> list[str]:
    """Each `tlbi` of `text` that is not allowed, or an `aside1` outside cpu.rs."""
    found = []
    for operation in TLBI.findall(text):
        if operation not in ALLOWED:
            found.append(f"{name}: tlbi {operation} is on no list")
        elif operation == "aside1" and name != "cpu.rs":
            found.append(f"{name}: tlbi aside1 belongs to cpu.rs's invalidating write")
    return found


def self_test() -> int:
    if not problems_in('"tlbi vae1is, {page}",', "cpu.rs"):
        print("asid-tlbi: self-test failed: a narrowed shootdown passed")
        return 1
    if problems_in('"tlbi vaae1is, {page}",', "cpu.rs"):
        print("asid-tlbi: self-test failed: a broad shootdown was refused")
        return 1
    print("asid-tlbi: self-test passed: a shootdown narrowed to an ASID fails")
    return 0


def main() -> int:
    if "--self-test" in sys.argv:
        return self_test()
    problems = []
    count = 0
    for path in sorted(TREE.rglob("*.rs")):
        text = path.read_text()
        count += len(TLBI.findall(text))
        problems += problems_in(text, path.name)
    for problem in problems:
        print(f"asid-tlbi: {problem} (docs/microkernel/K1.md §8.1)")
    print(f"asid-tlbi: {count} invalidations, {len(problems)} wrong")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
