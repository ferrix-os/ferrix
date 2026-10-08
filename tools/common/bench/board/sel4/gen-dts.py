#!/usr/bin/env python3
"""Make seL4's tools/dts/stm32mp157a-dk1.dts from a Linux stable release.

    gen-dts.py <out-dir> [tag]        tag defaults to v7.2.9

Fetches arch/arm/boot/dts/st/stm32mp157a-dk1.dts and everything it includes
from git.kernel.org's stable tree at <tag> (following symlinks), preprocesses
it as Linux's dtbs target does, compiles it with dtc and decompiles it again,
the way seL4's tools/dts/update-dts.sh makes every tools/dts file. Writes
<out-dir>/stm32mp157a-dk1.dts with update-dts.sh's licence header, and keeps
the fetched sources under <out-dir>/linux-<tag>/.
"""
import os
import posixpath
import re
import subprocess
import sys
import urllib.request

BASE = "https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/plain/"
ROOT_DTS = "arch/arm/boot/dts/st/stm32mp157a-dk1.dts"
INCLUDE = re.compile(r'^\s*#\s*include\s*([<"])([^>"]+)[>"]', re.M)
LICENSE = """/*
 * Copyright Linux Kernel Team
 *
 * SPDX-License-Identifier: GPL-2.0-only
 *
 * This file is derived from an intermediate build stage of the
 * Linux kernel. The licenses of all input files to this process
 * are compatible with GPL-2.0-only.
 */
"""


def fetch(tree, tag, path, seen):
    """Fetch one file (following a symlink) and, recursively, its includes."""
    if path in seen:
        return
    seen.add(path)
    url = f"{BASE}{path}?h={tag}"
    # git.kernel.org refuses urllib's default User-Agent.
    req = urllib.request.Request(url, headers={"User-Agent": "curl/8.0"})
    with urllib.request.urlopen(req, timeout=60) as r:
        data = r.read()
    text = data.decode()
    # A symlink's blob is its target: one line, a relative path, no newline.
    if "\n" not in text and text and not text.startswith("/") and "." in text:
        target = posixpath.normpath(posixpath.join(posixpath.dirname(path), text))
        print(f"  {path} -> {target}")
        fetch(tree, tag, target, seen)
        dst = os.path.join(tree, path)
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        with open(os.path.join(tree, target), "rb") as f:
            data = f.read()
        with open(dst, "wb") as f:
            f.write(data)
        return
    dst = os.path.join(tree, path)
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    with open(dst, "wb") as f:
        f.write(data)
    print(f"  {path} ({len(data)} bytes)")
    for kind, inc in INCLUDE.findall(text):
        if kind == '"':
            fetch(tree, tag, posixpath.normpath(posixpath.join(posixpath.dirname(path), inc)), seen)
        elif inc.startswith("dt-bindings/"):
            fetch(tree, tag, "include/" + inc, seen)
        else:
            raise SystemExit(f"unexpected include <{inc}> in {path}")


def main():
    out = os.path.abspath(sys.argv[1])
    tag = sys.argv[2] if len(sys.argv) > 2 else "v7.2.9"
    tree = os.path.join(out, f"linux-{tag}")
    print(f"fetching {ROOT_DTS} at {tag}")
    fetch(tree, tag, ROOT_DTS, set())
    src = os.path.join(tree, ROOT_DTS)
    pre = os.path.join(out, "stm32mp157a-dk1.dts.pre")
    dtb = os.path.join(out, "stm32mp157a-dk1.dtb")
    # Linux's cmd_dtc: cpp with the dts include paths, then dtc.
    subprocess.run(["cpp", "-nostdinc", "-I", os.path.join(tree, "include"),
                    "-I", os.path.dirname(src), "-undef", "-D__DTS__",
                    "-x", "assembler-with-cpp", "-P", "-o", pre, src], check=True)
    subprocess.run(["dtc", "-q", "-I", "dts", "-O", "dtb", "-o", dtb, pre], check=True)
    dts = subprocess.run(["dtc", "-q", "-I", "dtb", "-O", "dts", dtb], check=True,
                         capture_output=True, text=True).stdout
    final = os.path.join(out, "stm32mp157a-dk1.dts")
    with open(final, "w") as f:
        f.write(LICENSE + "\n" + dts)
    print(f"wrote {final}")


if __name__ == "__main__":
    main()
