#!/usr/bin/env python3
"""Measure what docs/MICROKERNEL.md's lift has to cross.

Two figures, both read with the item-boundary gate's own classifier and
resolver, so they move with the manifest rather than with a regex:

  surface   what the load ring's product files name in the core and item
            rings: the shim `linuxd` needs (MICROKERNEL.md §4).
  orphans   requirement ids of the kernel whose only `Verifies:` tags are in
            load-ring files: the evidence that leaves ring 0 with the load
            unless it is re-homed first (MICROKERNEL.md §6.2, B5).

Usage: microkernel-surface.py [surface|orphans|all] [--list]
"""

import collections
import importlib.util
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
CHECK = ROOT / "tools/common/check"
ID = re.compile(r"\b([HL]\.[A-Za-z0-9_]+\.\d+)\b")


def gate():
    spec = importlib.util.spec_from_file_location("cib", CHECK / "check-item-boundary.py")
    module = importlib.util.module_from_spec(spec)
    sys.path.insert(0, str(CHECK))
    spec.loader.exec_module(module)
    return module


def surface(g, manifest, files, ring_of, listing):
    modules, refs, _ = g.build(files, lambda rel: (g.KERNEL_SRC / rel).read_text())
    resolver = g.Resolver(modules)
    targets, items, users = set(), set(), set()
    sites = 0
    by_top = collections.Counter()
    for ref in refs:
        if ring_of.get(ref.file) != "load" or g.is_test_file(ref.file, manifest):
            continue
        found = resolver.resolve(ref.module, ref.segments, ref.is_use)
        if found is None:
            continue
        if ring_of.get(resolver.file_of(found[0])) not in ("core", "item"):
            continue
        sites += 1
        users.add(ref.file)
        key = g.key_of(found[0])
        targets.add(key)
        items.add(key + ("" if found[1] else "::" + ref.segments[-1]))
        by_top[found[0][0] if found[0] else "(root)"] += 1
    print("surface: load product files reaching core/item:", len(users))
    print("surface: reference sites:", sites)
    print("surface: distinct target modules:", len(targets))
    print("surface: distinct target items:", len(items))
    print("surface: sites by top module:",
          ", ".join(f"{name} {count}" for name, count in by_top.most_common()))
    if listing:
        for item in sorted(items):
            print("  " + item)


def orphans(g, files, ring_of, listing):
    rings = collections.defaultdict(set)
    homes = collections.defaultdict(set)
    for rel in files:
        for line in (g.KERNEL_SRC / rel).read_text().splitlines():
            tag = line.split("Verifies:", 1)
            if len(tag) == 2:
                for ident in ID.findall(tag[1]):
                    rings[ident].add(ring_of.get(rel))
                    homes[ident].add(rel)
    only = sorted(ident for ident, seen in rings.items() if seen == {"load"})
    print("orphans: ids verified in kernel files:", len(rings))
    print("orphans: ids verified only from load-ring files:", len(only))
    by_file = collections.Counter(rel for ident in only for rel in homes[ident])
    print("orphans: by file:",
          ", ".join(f"{rel} {count}" for rel, count in by_file.most_common()))
    if listing:
        for ident in only:
            print(f"  {ident}: {', '.join(sorted(homes[ident]))}")


def main(argv):
    what = argv[1] if len(argv) > 1 and not argv[1].startswith("-") else "all"
    listing = "--list" in argv
    g = gate()
    manifest = g.load_manifest()
    files = g.kernel_files()
    ring_of, *_ = g.classify(manifest, files)
    if what in ("surface", "all"):
        surface(g, manifest, files, ring_of, listing)
    if what in ("orphans", "all"):
        orphans(g, files, ring_of, listing)


if __name__ == "__main__":
    main(sys.argv)
