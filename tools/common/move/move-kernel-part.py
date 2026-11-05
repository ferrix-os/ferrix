#!/usr/bin/env python3
"""Move kernel files out of src/kernel/src into an optional part, line for line
(docs/microkernel/CRATE-SPLIT.md §4.1, S0; ledger 7346).

A part keeps the module path its files had: `src/kernel/src/<rel>` becomes
`<parts_place>/<group>/<name>/src/<rel>` (parts_place from kernel_source_roots). This script does the part of a move
that must be mechanical, and is rerun on a fresh base at landing time:

1. `git mv` each file (or every file under a directory given with a trailing /);
2. writes `rewrite-paths.sed`, one `s|old|new|g` per moved file, longest first,
   which an open branch runs on its tip before it rebases (§4.1);
3. with `--rewrite`, runs that sed over every tracked text file that names a
   moved path (records, anchors, the panic catalog's `see:` lines, goldens),
   and lists them: the landing message names each (ledger 7346 (8));
4. checks that every moved file is staged as a 100% rename (`R100`): a move
   that changed a byte fails, before anything is committed (§3.1).

It does not write the part's manifest, its `lib.rs` or the module declarations
cut from the old root: those are the landing's reviewed edits (§2.1's lib.rs
shape). Nothing is committed.

Usage:
  move-kernel-part.py --part device/logctl FILE_OR_DIR... [--rewrite] [--dry-run]
    FILE_OR_DIR: relative to src/kernel/src (`interfaces/logctl/`)
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "check"))
import kernel_roots  # noqa: E402  (the parts' place, docs/microkernel/CRATE-SPLIT.md S0)

ROOT = Path(__file__).resolve().parents[3]
SRC = "src/kernel/src/"
PARTS = kernel_roots.parts_place() + "/"


def git(*args: str, check: bool = True) -> str:
    done = subprocess.run(["git", "-C", str(ROOT), *args], capture_output=True, text=True)
    if check and done.returncode:
        sys.exit(f"move-kernel-part: git {' '.join(args)}: {done.stderr.strip()}")
    return done.stdout


def plan(part: str, items: list[str]) -> list[tuple[str, str]]:
    """(old, new) repo paths for every tracked file the items name."""
    if part.startswith("/") or ".." in part.split("/") or part.count("/") != 1:
        sys.exit(f"move-kernel-part: --part {part}: <group>/<name>")
    tracked = set(git("ls-files", "-z", "--", SRC).split("\0")) - {""}
    out = []
    for item in items:
        old = SRC + item
        found = sorted(p for p in tracked if p.startswith(old)) if item.endswith("/") else \
            ([old] if old in tracked else [])
        if not found:
            sys.exit(f"move-kernel-part: {old}: no tracked file")
        out += [(p, f"{PARTS}{part}/src/{p[len(SRC):]}") for p in found]
    if len({o for o, _ in out}) != len(out):
        sys.exit("move-kernel-part: a file is named twice")
    return out


def sed_script(moves: list[tuple[str, str]]) -> str:
    # Longest first, so `a/b.rs` is rewritten before a shorter path it starts with.
    rows = sorted(moves, key=lambda m: -len(m[0]))
    return "".join(f"s|{old}|{new}|g\n" for old, new in rows)


def naming_files(moves: list[tuple[str, str]]) -> list[str]:
    """Tracked files that name a moved path, the moved files themselves excluded."""
    moved = {new for _, new in moves} | {old for old, _ in moves}
    hits: set[str] = set()
    for old, _ in moves:
        listed = git("grep", "-lzF", "-e", old, "--", ".", check=False).split("\0")
        hits |= {p for p in listed if p and p not in moved}
    return sorted(hits)


def relative_records(moves: list[tuple[str, str]]) -> list[str]:
    """The data files that name a moved file relative to src/kernel/src (rings,
    `lift_cfg_sites`, ...): the sed does not reach those names, so the landing
    edits each by hand and its message lists them."""
    found = set()
    for old, _ in moves:
        listed = git("grep", "-lzF", "-e", f'"{old[len(SRC):]}"', "--", "tools/common/data", check=False)
        found |= {p for p in listed.split("\0") if p}
    return sorted(found)


def not_r100(moves: list[tuple[str, str]]) -> list[str]:
    status = git("diff", "--cached", "-M100%", "--name-status", "--", SRC, PARTS)
    renamed = {}
    for row in status.splitlines():
        fields = row.split("\t")
        if len(fields) == 3 and fields[0].startswith("R"):
            renamed[fields[1]] = (fields[0], fields[2])
    bad = []
    for old, new in moves:
        got = renamed.get(old)
        if got != ("R100", new):
            bad.append(f"{old} -> {new}: staged as {got[0] + ' ' + got[1] if got else 'no rename'}, not R100")
    return bad


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--part", required=True, help="<group>/<name> under the parts' place (kernel_source_roots.parts_place)")
    parser.add_argument("items", nargs="+", help="files or directories (with /) relative to src/kernel/src")
    parser.add_argument("--rewrite", action="store_true", help="run the sed over the files naming a moved path")
    parser.add_argument("--dry-run", action="store_true", help="print the plan, move nothing")
    options = parser.parse_args()
    moves = plan(options.part, options.items)
    script = sed_script(moves)
    hits = naming_files(moves)
    for old, new in moves:
        print(f"move: {old} -> {new}")
    for path in hits:
        print(f"names a moved path: {path}")
    for path in relative_records(moves):
        print(f"names a moved file relative to src/kernel/src, edit by hand: {path}")
    if options.dry_run:
        print(f"move-kernel-part: dry run, {len(moves)} file(s), {len(hits)} file(s) naming them")
        return 0
    for old, new in moves:
        (ROOT / new).parent.mkdir(parents=True, exist_ok=True)
        git("mv", old, new)
    (ROOT / "rewrite-paths.sed").write_text(script, encoding="utf-8")
    if options.rewrite:
        for path in hits:
            subprocess.run(["sed", "-i", "-f", str(ROOT / "rewrite-paths.sed"), str(ROOT / path)], check=True)
    bad = not_r100(moves)
    for line in bad:
        print(f"move-kernel-part: {line}")
    if bad:
        print("move-kernel-part: a move must leave every file's bytes as they were (CRATE-SPLIT.md §3.1)")
        return 1
    print(f"move-kernel-part: {len(moves)} file(s) moved as R100; rewrite-paths.sed written"
          + (f"; {len(hits)} file(s) rewritten" if options.rewrite else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
