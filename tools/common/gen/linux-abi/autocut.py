#!/usr/bin/env python3
"""Cut (un-declare) every check module that fails to compile, cascading; stop on product errors.

Usage: autocut.py <worktree> <log-dir>
"""
import json, os, re, subprocess, sys
from pathlib import Path

wt = Path(sys.argv[1]); logd = Path(sys.argv[2]); logd.mkdir(exist_ok=True)
src = wt / "src/kernel/src"
cut = []
state = logd / "cut.json"
if state.exists():
    cut = json.loads(state.read_text())

def is_check(rel):
    b = rel.rsplit("/", 1)[-1]
    return b in ("check.rs", "checks.rs") or b.endswith("_check.rs") or "/check/" in rel or "_check/" in rel

def module_root(rel):
    """The file that is the module: for a/b/check/x.rs the check module a/b/check(.rs|/mod.rs)."""
    parts = rel[:-3].split("/")
    # climb to the outermost check-named component
    for k, p in enumerate(parts):
        if p in ("check",) or p.endswith("_check"):
            return "/".join(parts[: k + 1])
    return "/".join(parts)

def parent_decl(modpath):
    parts = modpath.split("/")
    name = parts[-1]
    if parts[-1] == "mod":
        parts = parts[:-1]; name = parts[-1]
    parent = parts[:-1]
    cands = [ "/".join(parent) + ".rs", "/".join(parent) + "/mod.rs"] if parent else ["main.rs"]
    for c in cands:
        p = src / c
        if p.exists():
            t = p.read_text()
            if re.search(r"(?m)^\s*(pub(\([^)]*\))?\s+)?mod\s+" + name + r"\s*;", t):
                return c, name
    return None, name

for rnd in range(60):
    out = subprocess.run(["cargo", "check", "-p", "ferrix-kernel", "--target", (sys.argv[3] if len(sys.argv) > 3 else "x86_64-unknown-none"),
                          "--message-format", "short"], cwd=wt, capture_output=True, text=True).stderr
    (logd / f"round{rnd}.txt").write_text(out)
    errs = re.findall(r"^src/kernel/src/([^:]+):(\d+):(\d+): error", out, re.M)
    print(f"round {rnd}: {len(errs)} errors", flush=True)
    if not errs:
        break
    prod = sorted({f"{e[0]}:{e[1]}" for e in errs if not is_check(e[0])})
    if prod:
        print("  product errors:", prod); break
    mods = sorted({module_root(e[0]) for e in errs})
    progressed = False
    for m in mods:
        decl, name = parent_decl(m)
        if not decl:
            print("  no decl for", m); continue
        p = src / decl; t = p.read_text()
        t2 = re.sub(r"(?m)^(\s*)((pub(\([^)]*\))?\s+)?mod\s+" + name + r"\s*;)", r"\1// cut: \2", t, count=1)
        if t2 != t:
            p.write_text(t2); cut.append(m); progressed = True
            print("  cut", m, "in", decl)
    state.write_text(json.dumps(cut, indent=1))
    if not progressed:
        print("  no progress"); break
