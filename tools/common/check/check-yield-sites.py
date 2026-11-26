#!/usr/bin/env python3
"""Every `yield_now()` the kernel calls is one K9 §4.1 answers (ledger 7562 WZ.5, WZ.6, 10087 A2).

Under `schedule=windows` a yield passes only to equals, so a spin-wait whose waiter is above
what it waits for, on one processor in one partition, waits for good. A wait for another task
calls `sched::give_way` (a sleep under windows, a yield elsewhere); a `yield_now` that stays is
one of the sites below, each with why it is no such wait, and K9 §4.1's tables say the same. A
file with more calls than its line, or one not listed, fails by name; so does a listed file with
fewer, so the list is never longer than the tree. A `yield_now` passed as a value counts as a call,
and one brought in under another name (`use ... as`) fails (ledger 10222 W1).

    python3 tools/common/check/check-yield-sites.py
    python3 tools/common/check/check-yield-sites.py --self-test
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(Path(__file__).resolve().parent))
import kernel_roots  # noqa: E402  (every kernel source root, CRATE-SPLIT.md S0)
import rustlex  # noqa: E402  (comments and literals blanked)

LINUX = f"{kernel_roots.parts_place()}/personality/linux/src/"
NOT_UNDER_WINDOWS = (
    "the load's checks, in ferrix-kernel-load: schedule=windows requires personality=user, whose kernel does not "
    "build that crate (ferrix_load_built = none), which check-kernel-shape.py refuses by any symbol of it in the "
    "kernel; syscall/check.rs and seccomp_filters_check.rs are also gated ring0, block_ring/check.rs not(lift), and "
    "linuxd is built with checks off"
)

# (path from the repository's root, calls, why each stays a yield).
SITES = (
    # Product sites (WZ.5), K9 §4.1's first table.
    ("src/kernel/src/sched/mod.rs", 1, "give_way's own yield, under every schedule but windows"),
    ("src/kernel/src/iommu/gate.rs", 1, "a unit's register: hardware, no task awaited, its caller's deadline"),
    ("src/kernel/src/sched/affinity.rs", 1, "the awaited runs on another processor; SETTER_WAIT_NANOS"),
    ("src/kernel/src/lift/item/cores.rs", 1, "linuxd's two cores threads, one level and partition"),
    ("src/kernel/src/lift/core/sched/wait.rs", 1, "before any task of linuxd: the awaited is at its level or in its own window"),
    ("src/kernel/src/syscall/native/lift.rs", 1, "ThreadYield: the program's own yield, not a kernel wait"),
    (LINUX + "syscall/linux.rs", 1, "sched_yield: the program's own yield, not a kernel wait"),
    ("src/kernel/src/sched/reserve.rs", 2, "one yield after a change of class, schedule=edf alone"),
    # Check-only sites (WZ.6), K9 §4.1's second table: yields that are what the check tests, or a
    # hand-off that waits for nothing.
    ("src/kernel/src/sched/borrow_check.rs", 1, "the check counts the switches a yield makes: the yield is its subject"),
    ("src/kernel/src/sched/rehome_check.rs", 2, "two yielders of one level on one processor, the yield its subject"),
    ("src/kernel/src/sched/check.rs", 4, "three single hand-offs that wait for nothing, and the idle-switch case's yield, its subject"),
    ("src/kernel/src/syscall/region_check.rs", 1, "one yield to move off a processor after set_affinity, under partition alone"),
    ("src/kernel/src/user/check.rs", 1, "the root-swap case's turn between its two tasks of one level: a switch, not a wait"),
    (LINUX + "syscall/check.rs", 3, NOT_UNDER_WINDOWS),
    (LINUX + "syscall/seccomp_filters_check.rs", 9, NOT_UNDER_WINDOWS),
    (LINUX + "interfaces/block_ring/check.rs", 1, NOT_UNDER_WINDOWS),
)

TOKEN = re.compile(r"\byield_now\b")
DEFINITION = re.compile(r"\bfn\s+yield_now\b")
USE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?use\b")
RENAMED = re.compile(r"\byield_now\s+as\b")


def calls_in(text: str) -> tuple[int, int]:
    """`text`'s `yield_now` tokens outside comments, its definition and a plain `use`, each
    counted as a call whether called or passed as a value (ledger 10222 W1); and its renaming
    `use ... yield_now as`, which would hide calls under another name."""
    count, renamed = 0, 0
    for code in rustlex.mask(text).splitlines():
        if DEFINITION.search(code):
            continue
        if USE.match(code):
            renamed += len(RENAMED.findall(code))
            continue
        count += len(TOKEN.findall(code))
    return count, renamed


def judge(found: dict[str, int], renamed: dict[str, int] | None = None) -> list[str]:
    listed = {path: calls for path, calls, _ in SITES}
    problems = [f"{path}: yield_now brought in under another name: call it by its own"
                for path, count in sorted((renamed or {}).items()) if count]
    for path, calls in sorted(found.items()):
        if calls and path not in listed:
            problems.append(f"{path}: {calls} yield_now call(s) K9 §4.1 does not answer: give_way, or list and say why")
        elif path in listed and calls > listed[path]:
            problems.append(f"{path}: {calls} yield_now calls, {listed[path]} answered: give_way, or list and say why")
    for path, calls in listed.items():
        if found.get(path, 0) < calls:
            problems.append(f"{path}: {found.get(path, 0)} yield_now calls, {calls} listed: shorten the list")
    return problems


def self_test() -> int:
    assert calls_in("        crate::sched::yield_now();\n") == (1, 0)
    assert calls_in("    // sched::yield_now() was here\n") == (0, 0)
    assert calls_in("pub fn yield_now() {\n") == (0, 0)
    assert calls_in("        super::give_way();\n") == (0, 0)
    assert calls_in('    say("a wait: `sleep_for`, `yield_now`, a pipe");\n') == (0, 0)
    # W1: a yield passed as a value counts; a plain use does not; a rename fails.
    assert calls_in("    let pass = sched::yield_now;\n    spin_with(crate::sched::yield_now);\n") == (2, 0)
    assert calls_in("use crate::sched::{self, yield_now};\n") == (0, 0)
    assert calls_in("use crate::sched::yield_now as pass;\n") == (0, 1)
    assert "another name" in judge({path: calls for path, calls, _ in SITES}, {"src/kernel/src/x.rs": 1})[0]
    clean = {path: calls for path, calls, _ in SITES}
    assert judge(clean) == []
    planted = dict(clean, **{"src/kernel/src/object/timer_check.rs": 1})
    assert "does not answer" in judge(planted)[0]
    more = dict(clean, **{"src/kernel/src/sched/check.rs": 5})
    assert "4 answered" in judge(more)[0]
    fewer = dict(clean, **{"src/kernel/src/sched/check.rs": 3})
    assert "shorten the list" in judge(fewer)[0]
    print("yield-sites: self-test passed: an unlisted call, one too many, one too few and a rename each fail")
    return 0


def main() -> int:
    if "--self-test" in sys.argv:
        return self_test()
    paths = sorted(p for d in kernel_roots.root_dirs() for p in d.rglob("*.rs"))
    kernel_roots.note_walked("check-yield-sites", paths)
    read = {p.relative_to(ROOT).as_posix(): calls_in(p.read_text()) for p in paths}
    found = {path: calls for path, (calls, _) in read.items() if calls}
    renamed = {path: count for path, (_, count) in read.items() if count}
    problems = judge(found, renamed)
    for problem in problems:
        print(f"yield-sites: {problem}")
    print(f"yield-sites: {sum(found.values())} yield_now calls in {len(found)} files, each answered by K9 §4.1"
          if not problems else f"yield-sites: FAILED, {len(problems)} files")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
