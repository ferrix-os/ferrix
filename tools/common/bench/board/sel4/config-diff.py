#!/usr/bin/env python3
"""Print every configuration value of a seL4 build that differs from a baseline.

    config-diff.py <baseline-build-dir> <build-dir>
    config-diff.py --kernel <kernel-only-build-dir> <build-dir>

Reads every gen_config.json (the kernel's and each library's and app's
configuration, as their C headers see it) under both directories and prints
`<file> <KEY> <baseline> -> <value>` for each difference, then the cache
entries the build sets on the command line or by its settings that the
baseline lacks or holds differently (CMakeCache.txt, Kernel*, Lib*, App*,
Sel4*, Elfloader*).

With --kernel the baseline is the kernel configured alone for the same
platform, so the kernel's own defaults: every kernel value that the project's
settings or build.sh changed is printed as `<KEY> <default> -> <value>`.
"""
import json
import os
import re
import sys

CACHE = re.compile(r"^((?:Kernel|Lib|App|Sel4|Elfloader|CMAKE_BUILD_TYPE|RELEASE|FASTPATH|SMP|MCS)"
                   r"[A-Za-z0-9_-]*):([A-Z]+)=(.*)$")


def configs(root):
    out = {}
    for d, _, files in os.walk(root):
        if "gen_config.json" in files:
            rel = os.path.relpath(os.path.join(d, "gen_config.json"), root)
            with open(os.path.join(d, "gen_config.json")) as f:
                out[rel] = json.load(f)
    return out


def cache(root):
    out = {}
    with open(os.path.join(root, "CMakeCache.txt")) as f:
        for line in f:
            m = CACHE.match(line.rstrip("\n"))
            if m and m.group(2) != "INTERNAL":
                out[m.group(1)] = m.group(3)
    return out


def kernel_only(base, build):
    with open(os.path.join(base, "gen_config", "kernel", "gen_config.json")) as f:
        ka = json.load(f)
    with open(os.path.join(build, "kernel", "gen_config", "kernel", "gen_config.json")) as f:
        kb = json.load(f)
    print("# kernel configuration values that differ from the kernel's own defaults")
    for key in sorted(set(ka) | set(kb)):
        va, vb = ka.get(key, "(unset)"), kb.get(key, "(unset)")
        if va != vb:
            print(f"{key} {va} -> {vb}")


def main():
    if sys.argv[1] == "--kernel":
        kernel_only(sys.argv[2], sys.argv[3])
        return
    base, build = sys.argv[1], sys.argv[2]
    a, b = configs(base), configs(build)
    print(f"# configuration values that differ from {os.path.basename(base)}")
    for rel in sorted(set(a) | set(b)):
        ca, cb = a.get(rel, {}), b.get(rel, {})
        for key in sorted(set(ca) | set(cb)):
            va, vb = ca.get(key, "(unset)"), cb.get(key, "(unset)")
            if va != vb:
                print(f"{rel} {key} {va} -> {vb}")
    print("# cache entries that differ")
    ka, kb = cache(base), cache(build)
    for key in sorted(set(ka) | set(kb)):
        va, vb = ka.get(key, "(unset)"), kb.get(key, "(unset)")
        if va != vb:
            print(f"CMakeCache {key} {va} -> {vb}")


if __name__ == "__main__":
    main()
