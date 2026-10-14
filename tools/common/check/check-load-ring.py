#!/usr/bin/env python3
"""The load ratchet (docs/roadmap/microkernel-m0.md, D): how large the `load`
ring is, and how much this branch grew it.

The `load` ring of tools/common/data/certification-item.json is what the
microkernel switch lifts out of ring 0 (docs/MICROKERNEL.md): the Linux layer,
the filesystems, the interfaces. Feature work keeps landing in it, which the
customer allows as now; this report keeps the lift's size visible landing by
landing. It counts the lines of the ring's kernel files (product and test
apart, by the manifest's test patterns) and of its crates, here and at the
merge base with `main`, each side classified by its own manifest, and names
the files that grew most.

It reports growth and never fails on it. `--fail-on-growth` is for its
negative control alone (a planted file, through `gate.sh control` over
`cargo xtask test-flags --fail-on-growth`): it shows that growth is seen.

    python3 tools/common/check/check-load-ring.py [--base REF] [--fail-on-growth]
"""

from __future__ import annotations

import argparse
import fnmatch
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent.parent
MANIFEST = "tools/common/data/certification-item.json"
KERNEL_SRC = "src/kernel/src/"


def matches(pattern: str, rel: str) -> bool:
    """As check-item-boundary.py matches a ring member."""
    if pattern.endswith("/**"):
        return rel.startswith(pattern[:-2])
    return fnmatch.fnmatch(rel, pattern)


def is_test(rel: str, manifest: dict) -> bool:
    name = rel.rsplit("/", 1)[-1]
    for pattern in manifest.get("test_file_patterns", []):
        if ("/" in pattern and matches(pattern, rel)) or ("/" not in pattern and fnmatch.fnmatch(name, pattern)):
            return True
    return False


def git(*args: str) -> str | None:
    result = subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True)
    return result.stdout if result.returncode == 0 else None


def tree_files(ref: str | None) -> dict[str, int]:
    """Every .rs file under src/, with its line count, at `ref` or in the tree."""
    counts: dict[str, int] = {}
    if ref is None:
        for path in (ROOT / "src").rglob("*.rs"):
            rel = path.relative_to(ROOT).as_posix()
            try:
                counts[rel] = path.read_bytes().count(b"\n")
            except OSError:
                continue
        return counts
    listing = git("ls-tree", "-r", ref, "--", "src") or ""
    blobs = []
    for line in listing.splitlines():
        meta, _, rel = line.partition("\t")
        parts = meta.split()
        if len(parts) == 3 and parts[1] == "blob" and rel.endswith(".rs"):
            blobs.append((rel, parts[2]))
    if not blobs:
        return counts
    batch = subprocess.run(
        ["git", "cat-file", "--batch"],
        cwd=ROOT,
        input="".join(f"{sha}\n" for _, sha in blobs).encode(),
        capture_output=True,
    ).stdout
    at = 0
    for rel, _ in blobs:
        end = batch.index(b"\n", at)
        size = int(batch[at:end].split()[2])
        body = batch[end + 1 : end + 1 + size]
        counts[rel] = body.count(b"\n")
        at = end + 1 + size + 1
    return counts


def load_ring(manifest: dict, files: dict[str, int]) -> dict[str, tuple[int, str]]:
    """The load ring's files: path -> (lines, product|test|crate)."""
    members = manifest.get("rings", {}).get("load", {}).get("members", [])
    ring: dict[str, tuple[int, str]] = {}
    for rel, lines in files.items():
        if rel.startswith(KERNEL_SRC):
            inner = rel[len(KERNEL_SRC):]
            if any(matches(pattern, inner) for pattern in members):
                ring[rel] = (lines, "test" if is_test(inner, manifest) else "product")
    for crate in manifest.get("crates", {}).get("members", {}).values():
        if crate.get("ring") == "load" and crate.get("path"):
            prefix = crate["path"].rstrip("/") + "/"
            for rel, lines in files.items():
                if rel.startswith(prefix):
                    ring[rel] = (lines, "crate")
    return ring


def totals(ring: dict[str, tuple[int, str]]) -> dict[str, int]:
    out = {"product": 0, "test": 0, "crate": 0}
    for lines, kind in ring.values():
        out[kind] += lines
    return out


def base_ref(given: str | None) -> str | None:
    if given:
        return (git("rev-parse", "--verify", "-q", f"{given}^{{commit}}") or "").strip() or None
    for candidate in ("main", "origin/main"):
        base = git("merge-base", "HEAD", candidate)
        if base:
            return base.strip()
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--base", help="compare with this commit [default: the merge base with main]")
    parser.add_argument("--fail-on-growth", action="store_true", help="the negative control's mode: fail when the ring grew")
    args = parser.parse_args()

    manifest = json.loads((ROOT / MANIFEST).read_text(encoding="utf-8"))
    here = load_ring(manifest, tree_files(None))
    now = totals(here)
    print(
        f"load-ring: {now['product']} lines of product code and {now['test']} of test code in "
        f"{sum(1 for _, kind in here.values() if kind != 'crate')} kernel files, "
        f"{now['crate']} in the load ring's crates"
    )
    base = base_ref(args.base)
    shown = (git("show", f"{base}:{MANIFEST}") if base else None)
    if base is None or shown is None:
        print("load-ring: no merge base with main to compare with; size only")
        return 0
    then_ring = load_ring(json.loads(shown), tree_files(base))
    then = totals(then_ring)
    grew = {kind: now[kind] - then[kind] for kind in now}
    print(
        f"load-ring: since {base[:9]}: product {grew['product']:+d}, test {grew['test']:+d}, "
        f"crates {grew['crate']:+d} lines (reported, never failed: the customer allows load growth)"
    )
    changes = []
    for rel in set(here) | set(then_ring):
        delta = here.get(rel, (0, ""))[0] - then_ring.get(rel, (0, ""))[0]
        if delta:
            changes.append((delta, rel))
    for delta, rel in sorted(changes, reverse=True)[:8]:
        print(f"    {delta:+6d}  {rel}{'' if rel in then_ring else ' (new)'}{'' if rel in here else ' (gone)'}")
    total = sum(grew.values())
    if args.fail_on_growth and total > 0:
        print(f"load-ring: GREW by {total} lines since {base[:9]} (--fail-on-growth, the control's mode)", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
