#!/usr/bin/env python3
"""Every tool that walks kernel files walks every kernel source root
(docs/microkernel/CRATE-SPLIT.md S0; the consultant's ledger line 7346 (4), (5)).

Moving an optional part out of src/kernel/ must not let a check stop seeing its
files and still pass. This holds that in two halves.

1. The census. Every file under tools/, scripts/ and .github/ that names a kernel
   source root bare (`src/kernel`, `src/kernel/`, `src/kernel/src`, or in segments,
   `"src" / "kernel"`, `os.path.join("src", "kernel", ...)`) is found by grep and
   must be classed in tools/common/data/kernel-path-literals.json; an entry whose
   file names none any more is stale. A `walker` or `packages` file must read the
   roots (`kernel_roots`, or xtask's KERNEL_PARTS_PLACE). A `walker` this guard
   does not run must say in the register's `parity_by` how parity holds for it
   (ledger 7429 S.4). No file but xtask's workspace.rs names the parts' place
   (kernel_source_roots.parts_place) literally: the record is the one place it is
   written (ledger 7445 B.2).

2. The parity guard. Each walker below runs twice with FERRIX_WALK_LOG set: as the
   tree is, and with the fixture part tools/common/data/kernel-roots-fixture/src
   added as a root (FERRIX_KERNEL_ROOTS_EXTRA). Each run, every file the guard
   counts itself (`git ls-files` under the roots, never the walker's own walk)
   must be in what the walker says it walked. In the fixture run,
   check-kernel-alloc-audit.py must report the fixture's planted allocation and
   check-crate-layering.sh's rule 4 its planted architecture conditional.

Usage: check-kernel-roots.py [--self-test]
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import kernel_roots  # noqa: E402  (after the path insert)

ROOT = kernel_roots.ROOT
REGISTER = ROOT / "tools" / "common" / "data" / "kernel-path-literals.json"
# The fixture part sits where a part does, under the parts' place, so the run
# shows every walker and rule 4 a root beneath src/kernel/ (ledger 7429 Q.1).
FIXTURE_ROOT = kernel_roots.fixture_root()
FIXTURE_FILE = f"{FIXTURE_ROOT}/planted.rs"
SURVEYED = ("tools", "scripts", ".github")
CLASSES = {"walker", "boundary", "whole-tree", "records", "packages", "prose", "test"}
READS_ROOTS = re.compile(r"kernel_roots|KERNEL_PARTS_PLACE|KERNEL_PLACES")
# A kernel source root named bare: not followed by more of a path (a file beneath
# it, `src/kernel/load`) nor by a name that merely starts the same (`src/kernelx`);
# or written in segments, `"src" / "kernel"` or `join("src", "kernel"` (ledger 7429 S.1).
BARE = re.compile(r"src/kernel(?:/src)?/?(?![\w./-])"
                  r"""|["']src["']\s*(?:/|,|\)\s*\.join\()\s*["']kernel["']""")
# The one file beside the record allowed to write the parts' place: xtask's copy,
# held to the record by its unit test (ledger 7445 B.2, B.3).
PLACE_COPY = "tools/common/xtask/src/workspace.rs"

# The walkers: (name as it logs, command). Each walks every .rs file under the roots.
WALKERS = (
    ("check-item-boundary", ["python3", "-I", "tools/common/check/check-item-boundary.py"]),
    ("check-unsafe-audit", ["python3", "-I", "tools/common/check/check-unsafe-audit.py"]),
    ("check-panic-audit", ["python3", "-I", "tools/common/check/check-panic-audit.py"]),
    ("check-asm-budget", ["python3", "-I", "tools/common/check/check-asm-budget.py"]),
    ("check-kernel-alloc-audit", ["python3", "-I", "tools/common/check/check-kernel-alloc-audit.py"]),
    ("check-boot-workers", ["python3", "-I", "tools/common/check/check-boot-workers.py"]),
    ("check-device-access", ["python3", "-I", "tools/common/check/check-device-access.py"]),
    ("check-xsave-sites", ["python3", "-I", "tools/common/check/check-xsave-sites.py"]),
    ("check-load-ring", ["python3", "-I", "tools/common/check/check-load-ring.py"]),
)
WALKER_FILES = {command[-1] for _, command in WALKERS}
# The shim and the lift's roots are walked by check-lift.py, not as kernel files.
NOT_KERNEL = re.compile(r"^src/kernel/src/(lift/|lift_core\.rs$|lift_item\.rs$)")
def census_problems(hits: dict[str, str], register: dict[str, str],
                    parity_by: dict[str, str] | None = None) -> list[str]:
    """`hits`: file -> its text, for every file the grep found a bare root in."""
    parity_by = parity_by or {}
    problems = []
    for path, text in sorted(hits.items()):
        cls = register.get(path)
        if cls is None:
            problems.append(f"{path}: names a kernel source root bare and is not classed in "
                            f"{REGISTER.relative_to(ROOT)} (walker, boundary, whole-tree, records, packages, prose, test)")
        elif cls not in CLASSES:
            problems.append(f"{path}: class {cls!r} is none of {sorted(CLASSES)}")
        elif cls in ("walker", "packages") and not READS_ROOTS.search(text):
            problems.append(f"{path}: a {cls} that reads no kernel_source_roots (kernel_roots or KERNEL_PARTS_PLACE)")
        if cls == "walker" and path not in WALKER_FILES and not parity_by.get(path, "").strip():
            problems.append(f"{path}: a walker this guard does not run, and the register's parity_by "
                            "does not say how parity holds for it (ledger 7429 S.4)")
    for path in sorted(set(register) - set(hits)):
        problems.append(f"{REGISTER.relative_to(ROOT)}: {path} names no kernel source root bare any more; drop it")
    for path in sorted(set(parity_by) - {p for p, c in register.items() if c == "walker"}):
        problems.append(f"{REGISTER.relative_to(ROOT)}: parity_by names {path}, which is no walker")
    return problems


def place_problems(texts: dict[str, str], place: str) -> list[str]:
    """`texts`: file -> its text, for every surveyed file. A file naming the
    parts' place literally, other than xtask's one copy, fails (ledger 7445 B.2):
    the place is read from the record, so a rename edits no tool."""
    named = re.compile(re.escape(place) + r"(?![\w-])")
    return [f"{path}: names the parts' place {place} literally; read it from kernel_source_roots "
            "(kernel_roots.parts_place(), or xtask's KERNEL_PARTS_PLACE)"
            for path, text in sorted(texts.items()) if path != PLACE_COPY and named.search(text)]


def surveyed_texts() -> dict[str, str]:
    """Every surveyed file's text but the records (.json), which hold paths as data."""
    listed = subprocess.run(["git", "-C", str(ROOT), "ls-files", "-z", "--cached", "--others",
                             "--exclude-standard", "--", *SURVEYED],
                            check=True, capture_output=True, text=True).stdout.split("\0")
    texts = {}
    for path in listed:
        if not path or path.endswith(".json"):
            continue
        try:
            texts[path] = (ROOT / path).read_text(encoding="utf-8")
        except (UnicodeDecodeError, FileNotFoundError, IsADirectoryError):
            continue
    return texts


def bare_hits(texts: dict[str, str]) -> dict[str, str]:
    return {path: text for path, text in texts.items() if BARE.search(text)}


def parity_problems(walked: dict[str, set[str]], expected: list[str], label: str) -> list[str]:
    problems = []
    for name, _ in WALKERS:
        want = {p for p in expected if not NOT_KERNEL.match(p)}
        missed = sorted(want - walked.get(name, set()))
        if missed:
            problems.append(f"{name} ({label}): did not walk {len(missed)} kernel file(s) under "
                            f"kernel_source_roots, first {missed[0]}")
    return problems


def run_walkers(extra: str | None) -> tuple[dict[str, set[str]], dict[str, str]]:
    walked: dict[str, set[str]] = {}
    outputs: dict[str, str] = {}
    with tempfile.TemporaryDirectory() as scratch:
        log = Path(scratch) / "walk.log"
        env = dict(os.environ, FERRIX_WALK_LOG=str(log))
        env.pop(kernel_roots.EXTRA_ENV, None)
        if extra:
            env[kernel_roots.EXTRA_ENV] = extra
        commands = list(WALKERS)
        if extra:
            commands.append(("check-crate-layering", ["bash", "tools/common/check/check-crate-layering.sh"]))
        for name, command in commands:
            ran = subprocess.run(command, cwd=ROOT, env=env, capture_output=True, text=True,
                                 stdin=subprocess.DEVNULL)
            outputs[name] = ran.stdout + ran.stderr
        if log.exists():
            for line in log.read_text(encoding="utf-8").splitlines():
                tool, _, path = line.partition("\t")
                walked.setdefault(tool, set()).add(path)
    return walked, outputs


def fixture_problems(outputs: dict[str, str]) -> list[str]:
    problems = []
    if FIXTURE_FILE not in outputs.get("check-kernel-alloc-audit", ""):
        problems.append("check-kernel-alloc-audit: did not report the fixture part's planted allocation "
                        f"({FIXTURE_FILE}): a part's allocations would go unaudited")
    layering = outputs.get("check-crate-layering", "")
    if not any(FIXTURE_FILE in line and "target_arch" in line for line in layering.splitlines()):
        problems.append("check-crate-layering: rule 4 did not refuse the fixture part's planted "
                        f"target_arch conditional ({FIXTURE_FILE})")
    return problems


def self_test() -> list[str]:
    failures = []
    # The census: an unclassed file, a stale entry, a walker reading no roots.
    got = census_problems({"tools/a.py": 'R = "src/kernel/src"\n'}, {})
    if len(got) != 1 or "not classed" not in got[0]:
        failures.append(f"census: an unclassed file passed: {got}")
    if not census_problems({}, {"tools/b.py": "prose"}):
        failures.append("census: a stale entry passed")
    if not census_problems({"tools/c.py": 'R = "src/kernel/src"\n'}, {"tools/c.py": "walker"}):
        failures.append("census: a walker that reads no roots passed")
    if census_problems({"tools/d.py": 'import kernel_roots\nR = "src/kernel/src"\n'}, {"tools/d.py": "walker"},
                       {"tools/d.py": "a reason"}):
        failures.append("census: a walker that reads the roots failed")
    for text, want in (('"src/kernel/src"', True), ('"src/kernel/"', True), ('"src/kernel"', True),
                       ('"src/kernel/src/mm.rs"', False), ('"src/kernelx"', False),
                       ('"src/kernel/load"', False), ('ROOT / "src" / "kernel" / "src"', True),
                       ('os.path.join("src", "kernel", "src")', True), ("root.join('src').join('kernel')", True),
                       ('"src" / "lib"', False)):
        if bool(BARE.search(text)) != want:
            failures.append(f"census: {text} taken as {'not ' if want else ''}a bare root")
    # A walker the guard does not run passes only with a parity_by reason.
    other = {"tools/e.py": 'import kernel_roots\nR = "src/kernel/src"\n'}
    if not census_problems(other, {"tools/e.py": "walker"}):
        failures.append("census: an unrun walker without a parity_by reason passed")
    if census_problems(other, {"tools/e.py": "walker"}, {"tools/e.py": "walks all of src/"}):
        failures.append("census: an unrun walker with a parity_by reason failed")
    # The parts' place written literally: any tool fails, xtask's copy passes.
    place = "src/kernel/someplace"
    got = place_problems({"tools/f.py": f'P = "{place}/"\n', PLACE_COPY: f'const P: &str = "{place}";\n',
                          "tools/g.py": 'P = "src/kernel/someplacex"\n'}, place)
    if len(got) != 1 or "tools/f.py" not in got[0]:
        failures.append(f"place: a literal place outside {PLACE_COPY} passed, or the copy failed: {got}")
    # kernel_roots' own refusals (ledger 7429 Q.2, 7445 B.1).
    for listed, want in ((["src/kernel/src"], True), (["src/kernel/src", f"{place}/net/ring/src"], True),
                         (["src/kernel/src", "src/kernel/src/net"], False),
                         (["src/kernel/src", f"{place}/a/src", f"{place}/a/src/b"], False),
                         (["src/kernel/src", "src/lib/ring/src"], False),
                         (["src/kernel/src", "src/kernel/load/src"], False),
                         ([f"{place}/a/src"], False), (["src/kernel/src", "src/kernel/src"], False)):
        if (not kernel_roots.root_problems(listed, place)) != want:
            failures.append(f"roots: {listed} taken as {'refused' if want else 'accepted'}")
    for at, why, want in ((place, None, True), ("src/kernel/src/someplace", None, False),
                          ("src/kernel-parts", None, False), ("src/kernel-parts", "a reason", True),
                          ("parts", "a reason", False), (place + "/", None, False)):
        if (not kernel_roots.place_problems(at, why)) != want:
            failures.append(f"place: {at} (why {why!r}) taken as {'refused' if want else 'accepted'}")
    # Parity: a walker that skipped a file under a root.
    files = ["src/kernel/src/mm.rs", FIXTURE_FILE, "src/kernel/src/lift/core/sys.rs"]
    full = {name: set(files) for name, _ in WALKERS}
    if parity_problems(full, files, "test"):
        failures.append("parity: a full walk failed")
    short = dict(full, **{"check-panic-audit": {"src/kernel/src/mm.rs"}})
    got = parity_problems(short, files, "test")
    if len(got) != 1 or "check-panic-audit" not in got[0]:
        failures.append(f"parity: a walk that missed the fixture passed: {got}")
    # The fixture's planted sites, reported or not.
    if not fixture_problems({}):
        failures.append("fixture: silence from both checks passed")
    said = {"check-kernel-alloc-audit": f"{FIXTURE_FILE}: allocates\n",
            "check-crate-layering": f"{FIXTURE_FILE}:16:#[cfg(target_arch = \"x86_64\")]\n"}
    if fixture_problems(said):
        failures.append("fixture: both planted sites reported, and it still failed")
    return failures


def main() -> int:
    if "--self-test" in sys.argv[1:]:
        failures = self_test()
        for failure in failures:
            print(f"kernel-roots: self-test: {failure}")
        if failures:
            return 1
        print("kernel-roots: self-test passed: an unclassed file, a stale entry, a walker reading no roots, "
              "an unrun walker without a reason, a literal place, a nested or misplaced root, "
              "a missed file and an unreported fixture each fail")
        return 0
    record = json.loads(REGISTER.read_text(encoding="utf-8"))
    register = record["files"]
    texts = surveyed_texts()
    problems = census_problems(bare_hits(texts), register, record.get("parity_by", {}))
    problems += place_problems(texts, kernel_roots.parts_place())
    walked, _ = run_walkers(None)
    problems += parity_problems(walked, kernel_roots.tracked_files(), "as the tree is")
    walked, outputs = run_walkers(FIXTURE_ROOT)
    os.environ[kernel_roots.EXTRA_ENV] = FIXTURE_ROOT
    try:
        expected = kernel_roots.tracked_files()
    finally:
        os.environ.pop(kernel_roots.EXTRA_ENV, None)
    problems += parity_problems(walked, expected, "with the fixture part")
    problems += fixture_problems(outputs)
    for problem in problems:
        print(f"kernel-roots: {problem}")
    if problems:
        print(f"kernel-roots: {len(problems)} problem(s); docs/microkernel/CRATE-SPLIT.md S0")
        return 1
    roots = kernel_roots.roots()
    print(f"kernel-roots: {len(register)} files name a kernel root and are classed; {len(WALKERS)} walkers "
          f"walked every file under {len(roots)} root(s) and the fixture part")
    return 0


if __name__ == "__main__":
    sys.exit(main())
