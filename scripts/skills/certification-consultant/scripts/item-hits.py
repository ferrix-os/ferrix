#!/usr/bin/env python3
"""List the commits in a range that the certification consultant must review.

    python3 scripts/skills/certification-consultant/scripts/item-hits.py <since>[..<until>]

For each commit, every changed file is classified the way
docs/CONVENTIONS.md, "Changes to the certified item go through review", does:

  core / item   a src/kernel/src file in that ring of certification-item.json
  crate:<ring>  a file of a src/lib crate the manifest's `crates` puts in core or item
  load+unsafe   a load-ring kernel file whose diff adds `unsafe`
  evidence      docs/certification/, docs/sysml/, tools/common/data/, or the manifest

Commits that hit none of these are counted, not listed. A commit that hits one
is marked `reviewed?` when its message names a review ("Reviewed", "consultant",
"cert OK"); that is a hint only, and the ledger decides.

Ring resolution is imported from tools/common/check/check-item-boundary.py, so
it cannot drift from the gate.
"""
from __future__ import annotations

import importlib.util
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
CHECKER = ROOT / "tools" / "common" / "check" / "check-item-boundary.py"
KERNEL = "src/kernel/src/"
EVIDENCE = ("docs/certification/", "docs/sysml/", "tools/common/data/")
REVIEWED = re.compile(r"review|consultant|cert(ification)? ok", re.IGNORECASE)


def load_checker():
    sys.path.insert(0, str(CHECKER.parent))
    spec = importlib.util.spec_from_file_location("check_item_boundary", CHECKER)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def git(*args: str) -> str:
    return subprocess.check_output(["git", "-C", str(ROOT), *args], text=True)


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__.strip().splitlines()[2], file=sys.stderr)
        return 2
    spec = sys.argv[1] if ".." in sys.argv[1] else f"{sys.argv[1]}..HEAD"

    checker = load_checker()
    manifest = checker.load_manifest()
    crates = {
        info["path"].rstrip("/") + "/": info["ring"]
        for name, info in manifest["crates"].items()
        if isinstance(info, dict) and info.get("ring") in ("core", "item")
    }
    rings = manifest["rings"]

    def ring_of(rel: str) -> str | None:
        hits = [n for n, r in rings.items() if any(checker.matches(p, rel) for p in r["members"])]
        return "/".join(hits) if hits else None

    commits = git("rev-list", "--reverse", "--no-merges", spec).split()
    quiet = 0
    for sha in commits:
        files = git("diff-tree", "--no-commit-id", "--name-only", "-r", sha).split()
        tags: dict[str, list[str]] = {}
        for path in files:
            tag = None
            if path == "tools/common/data/certification-item.json" or path.startswith(EVIDENCE):
                tag = "evidence"
            elif path.startswith(KERNEL) and path.endswith(".rs"):
                rel = path[len(KERNEL):]
                ring = ring_of(rel)
                if ring in ("core", "item") or (ring and "/" in ring):
                    tag = ring
                elif ring is None:
                    tag = "unclassified"
                elif ring == "load":
                    diff = git("show", "--format=", "-U0", sha, "--", path)
                    if re.search(r"^\+(?!\+).*\bunsafe\b", diff, re.MULTILINE):
                        tag = "load+unsafe"
            else:
                for prefix, ring in crates.items():
                    if path.startswith(prefix):
                        tag = f"crate:{ring}"
                        break
            if tag:
                tags.setdefault(tag, []).append(path)
        if not tags:
            quiet += 1
            continue
        message = git("log", "-1", "--format=%B", sha)
        subject = message.splitlines()[0] if message else ""
        mark = "reviewed?" if REVIEWED.search(message) else "NO REVIEW NAMED"
        print(f"{sha[:10]}  {mark:15}  {subject}")
        for tag, paths in sorted(tags.items()):
            shown = ", ".join(paths[:4]) + (f", +{len(paths) - 4}" if len(paths) > 4 else "")
            print(f"    {tag:12} {shown}")
    print(f"\n{len(commits)} commits in {spec}; {len(commits) - quiet} need a look, {quiet} outside the item.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
