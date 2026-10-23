#!/usr/bin/env python3
"""Linux-layer separation map: classify kernel files, count cross-class refs."""
import collections, importlib.util, sys, json, re
from pathlib import Path

ROOT = Path(sys.argv[1])
CHECK = ROOT / "tools/common/check"
spec = importlib.util.spec_from_file_location("cib", CHECK / "check-item-boundary.py")
g = importlib.util.module_from_spec(spec); sys.path.insert(0, str(CHECK)); spec.loader.exec_module(g)
manifest = g.load_manifest()
files = g.kernel_files()
ring_of, _, _ = g.classify(manifest, files)

NATIVE_SYSCALL = {"syscall/mod.rs", "syscall/native.rs", "syscall/program.rs"}
CORE_SYSCALL = {"syscall/uaccess.rs"}
LINUX_IN_CORE = {"signal_frame.rs"}

def cls(rel):
    if rel in NATIVE_SYSCALL: return "native-entry"
    if rel in CORE_SYSCALL: return "shared-core"
    if rel.startswith("syscall/"): return "linux-syscall"
    if rel.startswith("fs/"): return "linux-fs"
    if rel.startswith("net/"): return "linux-net"
    if rel.startswith("interfaces/"): return "device-cores"
    if rel.startswith("object/") or rel in ("claim.rs", "fastpath.rs", "discovery/devmgr.rs"): return "native-objects"
    if rel in LINUX_IN_CORE or re.match(r"arch/[^/]+/signal", rel): return "linux-in-core"
    if rel == "main.rs": return "composition-root"
    return "shared-core"

product = [f for f in files if not g.is_test_file(f, manifest)]
klass = {f: cls(f) for f in files}
lines = collections.Counter(); tlines = collections.Counter()
for f in files:
    n = sum(1 for _ in (g.KERNEL_SRC / f).open(encoding="utf-8", errors="replace"))
    (tlines if g.is_test_file(f, manifest) else lines)[klass[f]] += n
print("product lines by class:", dict(lines))
print("check lines by class:", dict(tlines))
ringlines = collections.Counter()
for f in product:
    n = sum(1 for _ in (g.KERNEL_SRC / f).open(encoding="utf-8", errors="replace"))
    ringlines[(ring_of.get(f), klass[f])] += n
print("ring x class:", {f"{a}/{b}": c for (a, b), c in sorted(ringlines.items(), key=str)})

modules, refs, _ = g.build(files, lambda rel: (g.KERNEL_SRC / rel).read_text())
res = g.Resolver(modules)
edges = collections.Counter()          # (from class, to class) -> sites
detail = collections.defaultdict(collections.Counter)  # (from,to) -> (file -> target) counts
first = {}
for r in refs:
    if g.is_test_file(r.file, manifest):
        continue
    found = res.resolve(r.module, r.segments, r.is_use)
    if found is None:
        continue
    tf = res.file_of(found[0])
    if not tf or tf == r.file:
        continue
    a, b = klass[r.file], klass.get(tf)
    if a == b or b is None:
        continue
    edges[(a, b)] += 1
    key = (r.file, g.key_of(found[0]))
    detail[(a, b)][key] += 1
    first.setdefault(key, r.line)
print("\nclass edges (sites):")
for (a, b), n in sorted(edges.items(), key=lambda x: -x[1]):
    print(f"  {a:16} -> {b:16} {n:5}  files={len({k[0] for k in detail[(a,b)]})} targets={len({k[1] for k in detail[(a,b)]})}")
want = sys.argv[2:] if len(sys.argv) > 2 else []
for w in want:
    a, b = w.split(">")
    print(f"\n== {a} -> {b}")
    for (f, t), n in sorted(detail[(a, b)].items()):
        print(f"  {f}:{first[(f,t)]} -> {t} ({n})")
