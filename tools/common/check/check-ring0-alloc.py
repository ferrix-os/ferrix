#!/usr/bin/env python3
"""No new infallible allocation in ring 0 (docs/ROBUSTNESS.md, class A).

`check-fallible-alloc.py` holds the certified item -- the `core` and `item`
rings and the six load files the item's own calls reach -- to allocation
that reports failure (finding F-23). Everything else the kernel runs was
outside it: the load ring's Linux layer, filesystems, sockets and
interfaces, and the libraries the kernel links that are not item crates
(`ferrix-vfs`, `ferrix-btrfs-vfs`, `ferrix-net`, `ferrix-procfs`, ...).
That is where the box's FX-0008 stops of 2026-11-08 came from: a `Box`,
`Vec` or `String` on a path a program drives, which the standard library
turns into a panic when the heap is empty. Every program can drive those
paths, so every one of those calls is a way for ring 3 to stop the machine.

This gate does not convert them. It stops the count growing, as the item's
gate did before the item reached zero: it scans the same token patterns,
with the same markers (`NOALLOC:`, `FALLIBLE:`, `FATAL-ALLOC:`, see that
script's docstring), over

  * every product file of the kernel's own source in the `load` ring that the
    item's gate does not scan (its `REACHED` files are held there), and
  * every product file of every workspace crate the kernel links -- the
    shipping dependencies of `ferrix-kernel`, `-core`, `-item` and `-load`,
    followed through the workspace -- other than the item's own crates
    (held there) and `ferrix-fallible` (the toolkit),

and holds a per-file count of unmarked calls in
`tools/common/data/ring0-alloc-baseline.json`.

What fails and what does not
----------------------------

  * A file whose unmarked count rises, or a new file with any, fails. Use
    `crate::fallible` in the kernel, `ferrix-fallible` or `try_reserve` in a
    crate, or argue the site with a marker.
  * A file whose count fell is reported and does not fail. This differs from
    the item's ratchet on purpose: the load changes in most landings, and a
    rule that every converted site re-records a shared file would put a
    conflict in that file in most rebases. The allowance a fall leaves is
    closed by the next `--record`, which any landing may run, and an
    R7 landing records its own files' falls in the same commit.
  * `--record` only lowers. It writes each file's smaller count, the
    baseline's or today's, and drops a file at 0. It never adds a file or
    raises a count: while any count is above the baseline it writes
    nothing and fails. A raise or a new file is a hand edit of the JSON,
    reviewed as such (the certification consultant, ledger 8202 R1.2). A
    commit that only lowers the baseline this way says "Certification:
    ring-0 baseline lowered only (ledger 8202)".

The residual, stated: between two records, a removal and an addition in
one file pass with nothing printed, and a fall is an allowance until the
next `--record`.

A change to `check-fallible-alloc.py`'s patterns or markers changes what
this gate counts: it re-records this baseline in the same commit, and
that commit is reviewed.
  * A crate the kernel starts to link appears with all its files, and fails
    if any has sites: linking a library into ring 0 is when its
    allocations become the kernel's.

Not to be confused with `check-kernel-alloc-audit.py`, K16's map of who
pays for each kernel file's allocations, marked or not: that one asks
whose memory, this one whether a failure can be reported.

Its blind spots are the item gate's (types, `.clone()`, conversions,
callees, `write!`): it is a ratchet on what can be seen by tokens, not a
proof. `.clone()` sites are counted for the report only (`--report`).

    python3 tools/common/check/check-ring0-alloc.py
    python3 tools/common/check/check-ring0-alloc.py --report   # by area, by file
    python3 tools/common/check/check-ring0-alloc.py --record   # lower the baseline to today's
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent.parent
BASELINE = ROOT / "tools" / "common" / "data" / "ring0-alloc-baseline.json"

# The kernel's packages: what they link is what ring 0 runs.
# The raised checks' crate (CRATE-SPLIT.md, S5a) is one of them: its files are
# the kernel's check files, read below by ring, and it has no `src/` of its own.
KERNEL_PACKAGES = (
    "ferrix-kernel",
    "ferrix-kernel-core",
    "ferrix-kernel-item",
    "ferrix-kernel-load",
    "ferrix-kernel-raised-checks",
)
# The toolkit the conversions are made with; its infallible calls are its job.
TOOLKIT_CRATES = {"ferrix-fallible"}
# Only what ships into the image: build scripts run on the host.
SHIPPING_TABLES = ("dependencies",)

CLONE = re.compile(r"\.\s*clone\s*\(\s*\)")


def _load(name: str, file: str):
    spec = importlib.util.spec_from_file_location(name, ROOT / "tools" / "common" / "check" / file)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


sys.path.insert(0, str(Path(__file__).resolve().parent))
FA = _load("fallible_alloc", "check-fallible-alloc.py")


def shipping_deps(cargo: dict, workspace_deps: dict) -> set[str]:
    """Package names in a manifest's shipping dependency tables, target ones
    included; a `package =` rename is followed here or in the workspace."""
    tables = [cargo.get(name, {}) for name in SHIPPING_TABLES]
    for target in cargo.get("target", {}).values():
        tables += [target.get(name, {}) for name in SHIPPING_TABLES]
    names = set()
    for table in tables:
        for key, spec in table.items():
            package = key
            if isinstance(spec, dict):
                if spec.get("workspace"):
                    root_spec = workspace_deps.get(key, {})
                    if isinstance(root_spec, dict):
                        package = root_spec.get("package", key)
                package = spec.get("package", package)
            names.add(package)
    return names


def kernel_crates(gate) -> dict[str, str]:
    """`{package: directory}` for every workspace crate the kernel links,
    transitively, the kernel's own packages left out."""
    packages = gate.workspace_packages()
    with (ROOT / "Cargo.toml").open("rb") as handle:
        workspace_deps = tomllib.load(handle).get("workspace", {}).get("dependencies", {})
    missing = [name for name in KERNEL_PACKAGES if name not in packages]
    if missing:
        raise SystemExit(f"ring0-alloc: not workspace members any more: {', '.join(missing)}")
    seen: set[str] = set()
    todo = list(KERNEL_PACKAGES)
    while todo:
        name = todo.pop()
        if name in seen or name not in packages:
            continue
        seen.add(name)
        todo += sorted(shipping_deps(packages[name][1], workspace_deps))
    return {name: packages[name][0] for name in sorted(seen) if name not in KERNEL_PACKAGES}


def masked(source: str) -> str:
    return FA.without_test_modules(FA.rustlex.mask(source))


def scope() -> dict[str, tuple[str, str]]:
    """`{path from the repository root: (area, source)}`: what this gate reads."""
    gate = FA.load_gate()
    manifest = gate.load_manifest()
    ring_of, _, _ = gate.classify(manifest, gate.kernel_files())
    out: dict[str, tuple[str, str]] = {}
    for rel, ring in ring_of.items():
        if ring != "load" or rel in FA.REACHED or rel in FA.TOOLKIT or gate.is_test_file(rel, manifest):
            continue
        # Named from the repository root: a kernel extension crate's file is
        # one already, the rest are under the kernel's source root.
        full = gate.kernel_path(rel)
        path = full.resolve().relative_to(ROOT).as_posix()
        area = "kernel " + ("extensions" if rel.startswith("src/") else (rel.split("/")[0] if "/" in rel else "(top)"))
        out[path] = (area, full.read_text(encoding="utf-8", errors="replace"))
    item_crates = {name for name, entry in gate.crate_entries(manifest).items() if entry["ring"] in ("core", "item")}
    problems = []
    for package, directory in kernel_crates(gate).items():
        if package in item_crates or package in TOOLKIT_CRATES:
            continue
        kinds, found = gate.crate_module_files(ROOT / directory / "src")
        problems += [f"{directory}/src/{p}" for p in found]
        for rel, kind in sorted(kinds.items()):
            if kind == "product":
                path = f"{directory}/src/{rel}"
                if path in out:
                    continue  # a kernel extension's file, read above as the kernel's
                out[path] = (package, (ROOT / path).read_text(encoding="utf-8", errors="replace"))
    if problems:
        raise SystemExit("ring0-alloc: cannot list a kernel crate's files:\n  " + "\n  ".join(problems))
    return out


def scan(files: dict[str, tuple[str, str]]) -> dict[str, list[dict]]:
    """Every flagged call per file. The kernel's own files share one set of
    allocating `Default`s, as in the item gate; each crate has its own."""
    kernel = [source for _, (area, source) in files.items() if area.startswith("kernel ")]
    kernel_defaults = frozenset(FA.allocating_defaults(kernel))
    by_crate: dict[str, list[str]] = {}
    for path, (area, source) in files.items():
        if not area.startswith("kernel "):
            by_crate.setdefault(area, []).append(source)
    crate_defaults = {area: frozenset(FA.allocating_defaults(sources)) for area, sources in by_crate.items()}
    out = {}
    for path, (area, source) in sorted(files.items()):
        defaults = kernel_defaults if area.startswith("kernel ") else crate_defaults[area]
        out[path] = FA.scan_source(source, defaults)
    return out


def unmarked(sites: dict[str, list[dict]]) -> dict[str, int]:
    counts = {}
    for path, found in sites.items():
        count = sum(1 for site in found if site["marker"] is None)
        if count:
            counts[path] = count
    return counts


def report(files: dict[str, tuple[str, str]], sites: dict[str, list[dict]]) -> None:
    counts = unmarked(sites)
    areas: dict[str, list[int]] = {}
    for path, (area, source) in files.items():
        row = areas.setdefault(area, [0, 0, 0])
        row[0] += counts.get(path, 0)
        row[1] += 1 if counts.get(path) else 0
        row[2] += len(CLONE.findall(masked(source)))
    print(f"ring0-alloc: {sum(counts.values())} unmarked allocating call(s) in {len(counts)} of "
          f"{len(files)} file(s); .clone() (not gated) {sum(r[2] for r in areas.values())}")
    print("  calls  files  clone  area")
    for area, (calls, nfiles, clones) in sorted(areas.items(), key=lambda item: -item[1][0]):
        print(f"  {calls:5}  {nfiles:5}  {clones:5}  {area}")
    print("  the files with most:")
    for path, count in sorted(counts.items(), key=lambda item: -item[1])[:30]:
        print(f"  {count:5}  {path}")


def lowered(baseline: dict[str, int], counts: dict[str, int]) -> tuple[dict[str, int], list]:
    """The baseline `--record` may write: each file's smaller count, files at
    0 dropped; and the rises, which forbid writing at all."""
    rises = sorted((path, baseline.get(path, 0), now) for path, now in counts.items()
                   if now > baseline.get(path, 0))
    out = {path: min(was, counts.get(path, 0)) for path, was in baseline.items()}
    return {path: n for path, n in sorted(out.items()) if n}, rises


def record(counts: dict[str, int]) -> None:
    BASELINE.write_text(
        json.dumps(
            {
                "//": [
                    "Unmarked calls to allocating standard-library APIs in ring 0 outside",
                    "the certified item -- the load ring's kernel files and the crates the",
                    "kernel links -- per file, as tools/common/check/check-ring0-alloc.py",
                    "counts them. Each is a way for a program to stop the machine when the",
                    "heap is empty (docs/ROBUSTNESS.md, class A). A debt register, not an",
                    "allowance: a count may not rise and a new file may not appear with",
                    "any. A fall passes, and the next --record closes the allowance it",
                    "left; --record only lowers, and a raise or a new file is a hand",
                    "edit reviewed by the certification consultant (ledger 8202 R1.2).",
                    "The target is an empty map.",
                ],
                "files": dict(sorted(counts.items())),
            },
            indent=2,
            ensure_ascii=False,
        )
        + "\n"
    )


_SELF_CARGO = {
    "dependencies": {"ferrix-a": {"workspace": True}, "renamed": {"workspace": True}, "plain": "1"},
    "build-dependencies": {"ferrix-host": {"workspace": True}},
    "target": {"cfg(x)": {"dependencies": {"ferrix-t": {"path": "x"}}}},
}
_SELF_WORKSPACE = {"ferrix-a": {"path": "a"}, "renamed": {"package": "ferrix-r", "path": "r"}}


def self_test() -> list[str]:
    failures = []
    got = shipping_deps(_SELF_CARGO, _SELF_WORKSPACE)
    want = {"ferrix-a", "ferrix-r", "plain", "ferrix-t"}
    if got != want:
        failures.append(f"shipping deps: got {sorted(got)}, expected {sorted(want)}")
    sites = {"a.rs": [{"marker": None}, {"marker": "noalloc"}, {"marker": None}], "b.rs": [{"marker": "fatal"}]}
    if unmarked(sites) != {"a.rs": 2}:
        failures.append(f"unmarked: got {unmarked(sites)}")
    # --record lowers, drops a file at 0, and refuses a rise or a new file.
    got = lowered({"a.rs": 5, "b.rs": 2, "c.rs": 1}, {"a.rs": 3, "b.rs": 2})
    if got != ({"a.rs": 3, "b.rs": 2}, []):
        failures.append(f"record lowers: got {got}")
    got = lowered({"a.rs": 5}, {"a.rs": 6, "new.rs": 1})
    if got[1] != [("a.rs", 5, 6), ("new.rs", 0, 1)]:
        failures.append(f"record refuses a rise and a new file: got {got[1]}")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--report", action="store_true")
    parser.add_argument("--record", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    failures = self_test()
    if failures:
        for failure in failures:
            print(f"ring0-alloc: self-test: {failure}", file=sys.stderr)
        return 1
    if args.self_test:
        print("ring0-alloc: self-test passes")
        return 0

    files = scope()
    sites = scan(files)
    counts = unmarked(sites)
    if args.report:
        report(files, sites)
        return 0
    if args.record:
        if not BASELINE.exists():
            print("ring0-alloc: no baseline to lower; a first one is a reviewed hand edit.", file=sys.stderr)
            return 1
        new, rises = lowered(json.loads(BASELINE.read_text())["files"], counts)
        if rises:
            print("ring0-alloc: not recorded: --record only lowers, and these rose or are new\n"
                  "  (a raise is a hand edit of the baseline, reviewed; ledger 8202 R1.2):", file=sys.stderr)
            for path, was, now in rises:
                print(f"    {path}: {was} -> {now}", file=sys.stderr)
            return 1
        record(new)
        print(f"ring0-alloc: recorded {sum(new.values())} call(s) in {len(new)} file(s)")
        return 0
    if not BASELINE.exists():
        print("ring0-alloc: no baseline; run with --record and commit it.", file=sys.stderr)
        return 1
    baseline = json.loads(BASELINE.read_text())["files"]

    grown = [(path, baseline.get(path, 0), now) for path, now in counts.items() if now > baseline.get(path, 0)]
    fell = [(path, was, counts.get(path, 0)) for path, was in baseline.items() if counts.get(path, 0) < was]
    if grown:
        print(
            "ring0-alloc: infallible allocation added in ring 0, where a program can\n"
            "  drive it and an empty heap then stops the machine (FX-0008). Use\n"
            "  crate::fallible in the kernel, ferrix-fallible or try_reserve in a crate,\n"
            "  or argue the site with NOALLOC:/FALLIBLE:/FATAL-ALLOC: (check-fallible-alloc.py's\n"
            "  docstring; docs/ROBUSTNESS.md, class A):",
            file=sys.stderr,
        )
        for path, was, now in grown:
            print(f"    {path}: {was} -> {now}", file=sys.stderr)
            for site in sites.get(path, []):
                if site["marker"] is None:
                    print(f"      line {site['line']}: {site['call']}", file=sys.stderr)
    if fell:
        print(f"ring0-alloc: {len(fell)} file(s) below the baseline (passes; --record closes the allowance):")
        for path, was, now in fell:
            print(f"    {path}: {was} -> {now}")
    print(f"ring0-alloc: {sum(counts.values())} unmarked allocating call(s) in ring 0 outside the item, "
          f"{len(counts)} of {len(files)} file(s); baseline {sum(baseline.values())}")
    return 1 if grown else 0


if __name__ == "__main__":
    sys.exit(main())
