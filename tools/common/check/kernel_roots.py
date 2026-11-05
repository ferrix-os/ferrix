"""The kernel's source roots, read from one record (docs/microkernel/CRATE-SPLIT.md S0).

`certification-item.json`'s `kernel_source_roots.roots` lists the directories whose
Rust files are kernel source. Today that is `src/kernel/src` alone; each optional
part moved out of `src/kernel/src` adds its crate's `src/` there, under
`kernel_source_roots.parts_place` (the customer's name, 2026-11-05, DECISIONS.md):
that record is the one place the directory is named. Every tool that
walks kernel files takes them from here, never from a literal of its own, so a
part added to the record is walked by every one of them (ledger 7346 (4), (5)).

A walker calls `note_walked(tool, paths)` with the files it read. When
`FERRIX_WALK_LOG` names a file, the paths are appended to it, one
`<tool>\\t<repo-relative path>` line each; `check-kernel-roots.py` runs every
registered walker so, and compares what it walked with its own count of the
files under the roots (`git ls-files`), never through the walker's own walk.
"""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
MANIFEST = ROOT / "tools" / "common" / "data" / "certification-item.json"
EXTRA_ENV = "FERRIX_KERNEL_ROOTS_EXTRA"


PRIMARY = "src/kernel/src"


def _record(manifest: dict | None) -> dict:
    if manifest is None:
        manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    return manifest["kernel_source_roots"]


def place_problems(place, why) -> list[str]:
    """What is wrong with `parts_place` (ledger 7445 B.1): a directory, no trailing
    /, under src/kernel/ unless the record says why it is not."""
    if not isinstance(place, str) or not place or place.endswith("/"):
        return ["kernel_source_roots.parts_place: a directory without a trailing /"]
    if not place.startswith("src/"):
        # The whole-tree walkers (check-kernel-roots.py's register) hold every
        # root because every root is under src/.
        return [f"kernel_source_roots.parts_place: {place} is not under src/"]
    if place == PRIMARY or place.startswith(PRIMARY + "/"):
        return [f"kernel_source_roots.parts_place: {place} is under {PRIMARY}"]
    if not place.startswith("src/kernel/") and not why:
        return [f"kernel_source_roots.parts_place: {place} is not under src/kernel/ and "
                "parts_place_why does not say why"]
    return []


def root_problems(listed, place: str) -> list[str]:
    """What is wrong with a roots list (ledger 7429 Q.2, 7445 B.1): src/kernel/src
    first, every other root under `place`, none nested in another or under
    src/kernel/src, none ending in /."""
    if not listed or any(not isinstance(r, str) or not r or r.endswith("/") for r in listed):
        return ["kernel_source_roots.roots: a non-empty list of directories without a trailing /"]
    problems = []
    if listed[0] != PRIMARY:
        problems.append(f"kernel_source_roots.roots: the first root is {PRIMARY}, not {listed[0]}")
    for r in listed:
        if r != PRIMARY and r.startswith(PRIMARY + "/"):
            problems.append(f"kernel_source_roots.roots: {r} is under {PRIMARY}")
        elif r != PRIMARY and not r.startswith(place + "/"):
            problems.append(f"kernel_source_roots.roots: {r} is not under parts_place {place}")
        for other in listed:
            if other != r and r.startswith(other + "/"):
                problems.append(f"kernel_source_roots.roots: {r} is nested in {other}")
    if len(set(listed)) != len(listed):
        problems.append("kernel_source_roots.roots: a root listed twice")
    return problems


def parts_place(manifest: dict | None = None) -> str:
    """Where the kernel's optional parts live: the one place the directory's name
    is written (`kernel_source_roots.parts_place`); every Python tool takes it
    from here, so a rename is the record's value alone."""
    record = _record(manifest)
    place = record.get("parts_place")
    problems = place_problems(place, record.get("parts_place_why"))
    if problems:
        raise ValueError("; ".join(problems))
    return place


def fixture_root(manifest: dict | None = None) -> str:
    """check-kernel-roots.py's fixture part's root: placed as a part is, under
    parts_place, so rule 4 and every walker are shown a root under src/kernel/
    (ledger 7429 Q.1). It is a root only in the guard's fixture run."""
    return f"{parts_place(manifest)}/fixture/kernel-roots/src"


def roots(manifest: dict | None = None) -> list[str]:
    """The kernel's source roots, repo-relative, `/`-separated."""
    record = _record(manifest)
    listed = record["roots"]
    # check-kernel-roots.py's fixture run adds its fixture part's root here, so that
    # every walker is shown to walk a root beyond src/kernel/src before any part
    # exists. It only adds: a check never walks less for it.
    extra = [r for r in os.environ.get(EXTRA_ENV, "").split(":") if r]
    every = list(listed) + [r for r in extra if r not in listed]
    problems = root_problems(every, parts_place(manifest))
    if problems:
        raise ValueError("; ".join(problems))
    return every


def tracked_files(suffix: str = ".rs", manifest: dict | None = None, root: Path = ROOT) -> list[str]:
    """Every tracked file under the roots ending in `suffix`, repo-relative.

    This is the parity guard's own count: `git ls-files`, not a walk."""
    out = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-z", "--cached", "--others", "--exclude-standard",
         "--", *roots(manifest)],
        check=True, capture_output=True, text=True,
    ).stdout
    return sorted(p for p in out.split("\0") if p.endswith(suffix))


def root_dirs(manifest: dict | None = None, root: Path = ROOT) -> list[Path]:
    """The roots as directories, for a walker's `rglob`."""
    return [root / r for r in roots(manifest)]


def note_walked(tool: str, paths) -> None:
    """Record the files a walker read, when the parity guard asks for it."""
    log = os.environ.get("FERRIX_WALK_LOG")
    if not log:
        return
    lines = []
    for path in paths:
        p = Path(path)
        if p.is_absolute():
            try:
                p = p.resolve().relative_to(ROOT)
            except ValueError:
                continue
        lines.append(f"{tool}\t{p.as_posix()}\n")
    with open(log, "a", encoding="utf-8") as handle:
        handle.writelines(lines)


def outside(*covered: str, manifest: dict | None = None) -> tuple[str, ...]:
    """The roots no directory in `covered` already holds, for a walker that
    surveys broader trees (`src/kernel`, `src/lib`, ...) and must add the rest.
    A directory holds a root only by a whole path component: `src/kernel` does
    not hold `src/kernelx` (ledger 7346 (4)). Under src/kernel/ itself a part's
    root is held by `src/kernel`: a tool that must see every root outside
    src/kernel/src takes `others()` instead (ledger 7429 Q.1)."""
    return tuple(r for r in roots(manifest)
                 if not any(r == c or r.startswith(c.rstrip("/") + "/") for c in covered))


def others(manifest: dict | None = None) -> list[str]:
    """Every root but src/kernel/src: the optional parts' (and, in the guard's
    fixture run, the fixture's)."""
    return [r for r in roots(manifest) if r != PRIMARY]


def kernel_name(repo_path: str, manifest: dict | None = None) -> str | None:
    """A repo-relative path's kernel file name, as check-item-boundary's
    `kernel_files` gives it, or None if it is no kernel file."""
    for r in roots(manifest):
        if repo_path.startswith(r + "/"):
            return repo_path[len(r) + 1:] if r == PRIMARY else repo_path
    return None


if __name__ == "__main__":
    import sys

    # `kernel_roots.py roots` prints every root; `kernel_roots.py outside DIR...`
    # those no DIR holds, one a line, for the shell checks.
    if len(sys.argv) >= 2 and sys.argv[1] == "roots":
        print("\n".join(roots()))
    elif len(sys.argv) >= 2 and sys.argv[1] == "others":
        print("\n".join(others()))
    elif len(sys.argv) >= 2 and sys.argv[1] == "place":
        print(parts_place())
    elif len(sys.argv) >= 2 and sys.argv[1] == "outside":
        print("\n".join(outside(*sys.argv[2:])))
    else:
        print("usage: kernel_roots.py roots | others | place | outside DIR...", file=sys.stderr)
        sys.exit(2)
