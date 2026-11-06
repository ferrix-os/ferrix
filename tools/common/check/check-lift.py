#!/usr/bin/env python3
"""The lift's one source, built twice (docs/microkernel/M5-LIFT.md §1, §10).

Reads the tree, runs nothing. It holds what `personality=user` adds from reaching
what `present` and `desktop` build:

1. `lift_reuse` in `certification-item.json` lists by file every kernel source
   the shim compiles by position, each with a reason. The module tree of
   `lift_core.rs` and `lift_item.rs` is walked, and the set must equal the list:
   an unlisted file is refused, and so is a listed one nothing reaches.
2. No listed file holds `asm!`, `global_asm!` or `#[no_mangle]`, none is a
   `*_check.rs`, none a raised check's home.
3. `lift_cfg_sites` counts the `ferrix_load_built` cfgs in each load file other
   than the crate roots. A site more than the count fails: a load file learning
   about the lift is a decision, not a side effect.
4. `ferrix_load_built` is derived in `build.rs` alone: no flag table names it.
5. The lift's packages name no kernel crate as a dependency except by their own
   lib names, and no kernel crate names one of them.
6. `--plant KIND`, for the controls: pretend a violation of a given kind.
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import kernel_roots  # noqa: E402  (the optional parts' roots, docs/microkernel/CRATE-SPLIT.md S1)

ROOT = Path(__file__).resolve().parent.parent.parent.parent
SRC = ROOT / "src" / "kernel" / "src"
LIFT = ROOT / "src" / "kernel" / "lift"
MANIFEST = ROOT / "tools" / "common" / "data" / "certification-item.json"
FLAGS = ROOT / "tools" / "common" / "data" / "build-flags.toml"
ROOTS = {
    "main.rs",
    "kernel_core.rs",
    "kernel_item.rs",
    "kernel_load.rs",
    "kernel_raised.rs",
    "lift_core.rs",
    "lift_item.rs",
}
SHIM_DIR = "lift/"
MOD = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*(;|\{)")
# What the lift's build leaves out: the ring-0 load's own, and a checks build's.
CFG_OUT = re.compile(r'#\[cfg\((?:not\(ferrix_load_built\s*=\s*"lift"\)|ferrix_checks\s*=\s*"on")\)\]')
FORBIDDEN = re.compile(r"\b(?:global_)?asm!|#\[(?:unsafe\()?no_mangle")
SITE = re.compile(r"ferrix_load_built\s*=")


def strip_comment(line: str) -> str:
    at = line.find("//")
    return line if at < 0 else line[:at]


def file_of(prefix: list[str]) -> Path | None:
    """The file a module path names under `src/kernel/src`."""
    base = SRC.joinpath(*prefix)
    for candidate in (base.with_suffix(".rs"), base / "mod.rs"):
        if candidate.is_file():
            return candidate
    return None


def reached(path: Path, prefix: list[str], out: set[str]) -> None:
    """Every file under `path`'s module tree that `path` declares."""
    depth: list[tuple[str | None, int]] = []
    braces = 0
    excluded = False
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = strip_comment(raw)
        if CFG_OUT.search(line):
            excluded = True
            continue
        found = MOD.match(line)
        if found:
            name, how = found.groups()
            if how == ";":
                if not excluded:
                    here = [part for part, _ in depth if part]
                    child = file_of(prefix + here + [name])
                    if child is not None:
                        rel = child.relative_to(SRC).as_posix()
                        if rel not in out:
                            out.add(rel)
                            reached(child, prefix + here + [name], out)
                excluded = False
                continue
            depth.append((name, braces))
            braces += 1
            excluded = False
            braces += line.count("{") - 1 - line.count("}")
            continue
        if line.strip() and not line.lstrip().startswith("#"):
            excluded = False
        braces += line.count("{") - line.count("}")
        while depth and braces <= depth[-1][1]:
            depth.pop()


def computed_reuse() -> set[str]:
    out: set[str] = set()
    for root in ("lift_core.rs", "lift_item.rs"):
        reached(SRC / root, [], out)
    return {rel for rel in out if not rel.startswith(SHIM_DIR)}


def part_dirs() -> list[Path]:
    """Each optional part's package directory: its root's parent
    (`<parts_place>/<group>/<name>/src`, docs/microkernel/CRATE-SPLIT.md §2.1)."""
    return [(ROOT / root).parent for root in kernel_roots.others()]


def load_sources() -> list[tuple[str, Path]]:
    """(name, path) of every load-side source a cfg site may sit in: the files
    under src/kernel/src by their name there, a part's by its repo path, its
    `src/lib.rs` a crate root as the roots beside main.rs are."""
    out = [(path.relative_to(SRC).as_posix(), path) for path in sorted(SRC.rglob("*.rs"))]
    for part in part_dirs():
        out += [(path.relative_to(ROOT).as_posix(), path) for path in sorted((part / "src").rglob("*.rs"))
                if path != part / "src" / "lib.rs"]
    return out


def cfg_sites() -> dict[str, int]:
    counts: dict[str, int] = {}
    for rel, path in load_sources():
        if rel in ROOTS or rel.startswith(SHIM_DIR):
            continue
        count = sum(
            1 for line in path.read_text(encoding="utf-8").splitlines() if SITE.search(strip_comment(line))
        )
        if count:
            counts[rel] = count
    return counts


def package_name(path: Path) -> str:
    with path.open("rb") as handle:
        return tomllib.load(handle)["package"]["name"]


def dependencies(path: Path) -> set[str]:
    """The packages a manifest depends on: a renamed one under its real name."""
    with path.open("rb") as handle:
        cargo = tomllib.load(handle)
    names: set[str] = set()
    for table in ("dependencies", "build-dependencies"):
        for name, spec in cargo.get(table, {}).items():
            names.add(spec.get("package", name) if isinstance(spec, dict) else name)
    return names


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--plant", choices=["unlisted", "unreached", "asm", "site", "flag", "dep"])
    args = parser.parse_args()
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    listed: dict[str, str] = manifest.get("lift_reuse", {})
    allowed_sites: dict[str, int] = manifest.get("lift_cfg_sites", {})
    listed = {key: value for key, value in listed.items() if not key.startswith("//")}
    allowed_sites = {key: value for key, value in allowed_sites.items() if not key.startswith("//")}
    problems: list[str] = []

    reuse = computed_reuse()
    if args.plant == "unlisted":
        reuse.add("mm.rs")
    if args.plant == "unreached":
        listed = dict(listed, **{"no/such.rs": "planted"})
    for rel in sorted(reuse - set(listed)):
        problems.append(f"{rel}: compiled into linuxd by position and not in lift_reuse")
    for rel in sorted(set(listed) - reuse):
        problems.append(f"{rel}: in lift_reuse and nothing in the shim reaches it")
    for rel, reason in sorted(listed.items()):
        if not reason.strip():
            problems.append(f"{rel}: lift_reuse gives no reason")

    raised = manifest.get("raised_checks", {})
    raised_files = {entry["file"] for entry in raised.get("entries", [])} if isinstance(raised, dict) else set()
    for rel in sorted(reuse):
        source = (SRC / rel).read_text(encoding="utf-8")
        text = "\n".join(strip_comment(line) for line in source.splitlines())
        if args.plant == "asm" and rel == sorted(reuse)[0]:
            text += "\nasm!();"
        if FORBIDDEN.search(text) and rel not in manifest.get("lift_asm_exceptions", {}):
            problems.append(f"{rel}: reused by position and holds asm! or an entry")
        if rel.endswith("_check.rs") and rel not in manifest.get("lift_check_exceptions", {}):
            problems.append(f"{rel}: a check reused by position")
        if rel in raised_files:
            problems.append(f"{rel}: a raised check reused by position")

    sites = cfg_sites()
    if args.plant == "site":
        sites = dict(sites, **{"mm.rs": 1})
    for rel, count in sorted(sites.items()):
        most = allowed_sites.get(rel, 0)
        if count > most:
            problems.append(f"{rel}: {count} ferrix_load_built sites, lift_cfg_sites allows {most}")
    for rel, most in sorted(allowed_sites.items()):
        if sites.get(rel, 0) < most:
            problems.append(f"{rel}: lift_cfg_sites allows {most}, it has {sites.get(rel, 0)}: lower it")

    flags = FLAGS.read_text(encoding="utf-8")
    if args.plant == "flag":
        flags += "\nferrix_load_built = 1\n"
    if "ferrix_load_built" in flags:
        problems.append("build-flags.toml names ferrix_load_built: it is derived in build.rs, not a flag")

    # The shim's packages under src/kernel/lift/, and each part's twin in its
    # own lift/ directory (CRATE-SPLIT.md §2.1), by their package names.
    lift_packages = {
        package_name(path): path
        for path in sorted(LIFT.glob("*/Cargo.toml")) + [p / "lift" / "Cargo.toml" for p in part_dirs()]
        if path.is_file()
    }
    names = set(lift_packages)
    kernel_cargos = [ROOT / "src" / "kernel" / "Cargo.toml"] + [
        ROOT / "src" / "kernel" / ring / "Cargo.toml" for ring in ("core", "item", "load")
    ] + [part / "Cargo.toml" for part in part_dirs()]
    for cargo in kernel_cargos:
        if not cargo.is_file():
            # A kernel manifest this check would silently skip (S1: a moved
            # part's old place) is a problem, not an absence.
            problems.append(f"{cargo.relative_to(ROOT)}: no such kernel manifest")
        else:
            planted = {"ferrix-lift-core"} if args.plant == "dep" and cargo == kernel_cargos[0] else set()
            for name in sorted((dependencies(cargo) | planted) & names):
                problems.append(f"{cargo.relative_to(ROOT)}: a kernel crate names {name}")
    for name, cargo in lift_packages.items():
        for dep in sorted(dependencies(cargo)):
            # Any kernel package, a part's included: a twin names its
            # ferrix-kernel-* dependencies only as `package = "ferrix-lift-*"`
            # (CRATE-SPLIT.md §2.3, ledger 7608 S1.6).
            if dep == "ferrix-kernel" or dep.startswith("ferrix-kernel-"):
                problems.append(f"{cargo.relative_to(ROOT)}: the lift names the kernel crate {dep}")

    # The shim's map is true of the tree (5714 (1)): an item no rule places, or a stale map.
    mapped = subprocess.run(
        [sys.executable, str(ROOT / "tools/common/gen/m5-shim-map.py"), "--check"],
        capture_output=True,
        text=True,
    )
    if mapped.returncode != 0:
        problems.append(f"the shim's map: {mapped.stderr.strip()}")

    if problems:
        print(f"check-lift: {len(problems)} problem(s)", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1
    print(f"check-lift: {len(reuse)} files reused by position, each listed; {sum(sites.values())} cfg sites in {len(sites)} files, none over its count")
    return 0


if __name__ == "__main__":
    sys.exit(main())
