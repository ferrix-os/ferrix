#!/usr/bin/env python3
"""Every place a handle lands in another job's table goes by K17's door.

`docs/microkernel/K17.md` §3 lists them. A function of the native ABI that puts
an object into a process's table other than its caller's, or into a region's,
is a door: it asks `object::level::may_hold` first, or it is on the list below
with the reason it need not. A new one fails this until it says which.

It scans the two native-call files in FILES alone: a door written in another file
goes unseen, so one added there is a review's to catch.

    python3 tools/common/check/check-level-doors.py
    python3 tools/common/check/check-level-doors.py --self-test
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
FILES = (
    "src/kernel/src/syscall/native.rs",
    "src/kernel/src/syscall/native/region.rs",
)
# Function name, and why it needs no door of its own.
NOT_DOORS = {
    "vmo_executable": "the caller's own table: the same object, one right more",
    # pager_keep and pager_take_up go through this too: the caller's own table only because
    # the door has a row for a keeper, whose level is its pager's (ledger 6575 D1).
    "insert_new": "the caller's own table, of an object just made for it",
    "insert_all": "the caller's own table: channel_create's ends",
    "deliver": "asks transfers_refused first",
    "deliver_some": "asks may_hold for each",
    "process_bootstrap": "the slot: give_bootstrap asked as it filled it",
    "handle_to_shared": "one process, whose region is one level (K17 §2)",
    "handle_to_private": "one process, whose region is one level (K17 §2)",
    "channel_write": "takes handles out of the caller's table, and puts them back on failure",
    "handle_duplicate": "the caller's own table",
    "handle_replace": "the caller's own table",
}
INSERT = re.compile(r"\.insert(_many)?\(")
FN = re.compile(r"^\s*(?:pub(?:\([a-z]+\))? )?fn (\w+)")


def doors_in(text: str) -> list[str]:
    """The functions of `text` that insert into a table and ask no door."""
    problems = []
    name = None
    body: list[str] = []
    for line in text.splitlines() + ["fn end() {"]:
        found = FN.match(line)
        if found:
            joined = "\n".join(body)
            if (
                name is not None
                and INSERT.search(joined)
                and "table" in joined
                and "may_hold" not in joined
                and name not in NOT_DOORS
            ):
                problems.append(name)
            name = found.group(1)
            body = []
        body.append(line)
    return problems


def self_test() -> int:
    bad = "fn sneak() {\n    to.with_handles(|table| table.insert(o, r));\n}\n"
    good = "fn door() {\n    may_hold(l, o, r)?;\n    to.with_handles(|table| table.insert(o, r));\n}\n"
    if doors_in(bad) != ["sneak"] or doors_in(good):
        print("level-doors: self-test failed")
        return 1
    print("level-doors: self-test passed: a planted insert with no door fails")
    return 0


def main() -> int:
    if "--self-test" in sys.argv:
        return self_test()
    problems = []
    for file in FILES:
        problems += [
            f"{file}: {name}" for name in doors_in((ROOT / file).read_text())
        ]
    for problem in problems:
        print(f"level-doors: {problem} puts a handle in a table and asks no door "
              "(docs/microkernel/K17.md §3): ask object::level::may_hold, or list it with why not")
    print(f"level-doors: {len(FILES)} files, {len(problems)} unasked")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
