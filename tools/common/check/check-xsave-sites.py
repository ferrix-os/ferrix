#!/usr/bin/env python3
"""Vector state is read in `arch/x86_64/switch.rs` alone.

With K1's lazy vector state (`docs/microkernel/K1.md` §7.1 (8)) `CR0.TS` may be set while
normal mode runs, so an `XSAVE`-class instruction in ring 0 anywhere but the paths that
take the mark down first faults in the kernel. This fails on a new site elsewhere.

    python3 tools/common/check/check-xsave-sites.py
    python3 tools/common/check/check-xsave-sites.py --self-test
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
TREE = ROOT / "src/kernel/src"
HOME = "arch/x86_64/switch.rs"
# Its own check, which runs with `CR0.TS` clear and is in no path of a program's.
CHECK = "arch/x86_64/switch/check.rs"
SITE = re.compile(r"\b_(xsave|xrstor|fxsave|fxrstor)\w*\(|\"\s*(xsave|xrstor|fxsave|fxrstor)\w*\b")


def sites_in(text: str) -> list[str]:
    """The XSAVE-class instructions `text` issues, comments left out."""
    found = []
    for line in text.splitlines():
        code = line.split("//", 1)[0]
        if "fn " not in code:
            found += [m.group(0) for m in SITE.finditer(code)]
    return found


def self_test() -> int:
    if not sites_in('    unsafe { core::arch::x86_64::_xsave64(at, 7) };'):
        print("xsave-sites: self-test failed: a planted site passed")
        return 1
    if sites_in("// XSAVE is the switch's"):
        print("xsave-sites: self-test failed: a comment counted")
        return 1
    print("xsave-sites: self-test passed: a planted site fails")
    return 0


def main() -> int:
    if "--self-test" in sys.argv:
        return self_test()
    bad = []
    for path in sorted(TREE.rglob("*.rs")):
        name = path.relative_to(TREE).as_posix()
        if name not in (HOME, CHECK) and sites_in(path.read_text()):
            bad.append(name)
    for name in bad:
        print(f"xsave-sites: {name}: an XSAVE-class instruction outside {HOME} (K1 §7.1 (8))")
    print(f"xsave-sites: {len(bad)} files outside {HOME}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
