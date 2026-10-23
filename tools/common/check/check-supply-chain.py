#!/usr/bin/env python3
"""Flag what a reviewer of an outside pull request must read twice.

    check-supply-chain.py <base>..<head> [--markdown] [--strict]

It reads the range's diff as data and never builds or runs it, so a workflow
may run it on an untrusted pull request (docs/REPO-FLOW.md, "The outside
lane"). It does not decide anything: it lists, for the person who approves and
for the AI security review that reads the same diff, the places where
smuggled code usually hides:

  build      files that run at build or CI time: .github/, .githooks/, build.rs,
             xtask, scripts, Cargo manifests and .cargo/config, toolchain
             files, components.toml pins, fetch scripts and their checksums
  deps       crates added to a Cargo.lock, with any name one or two edits
             from a crate already locked (typosquatting)
  binary     binary files added or changed
  unicode    bidirectional controls and invisible characters in added lines
             (CVE-2021-42574, "Trojan Source")
  blob       added lines over 400 characters, or long base64/hex runs:
             minified or encoded payloads
  reach      added lines that open the network, start processes, or embed
             files at compile time: URLs, curl/wget, TcpStream, Command::new,
             include_bytes!, /dev/tcp, base64 -d, eval

Exit 0 unless --strict and something was found.
"""
from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())

BUILD_PATHS = (
    re.compile(r"^\.github/"), re.compile(r"^\.githooks/"), re.compile(r"(^|/)build\.rs$"),
    re.compile(r"^tools/common/xtask/"), re.compile(r"^tools/common/fetch/"),
    re.compile(r"(^|/)Cargo\.(toml|lock)$"), re.compile(r"(^|/)\.cargo/"),
    re.compile(r"(^|/)rust-toolchain(\.toml)?$"), re.compile(r"^components\.toml$"),
    re.compile(r"\.(sh|bash|py|ps1|mk)$"), re.compile(r"(^|/)Makefile$"),
    re.compile(r"(^|/)deny\.toml$"), re.compile(r"(sha256|checksum|pinned)", re.I),
    re.compile(r"^\.claude/"), re.compile(r"^vendor/"),
)
INVISIBLE = re.compile("[‪-‮⁦-⁩​-‏  ﻿­]")
BLOB = re.compile(r"[A-Za-z0-9+/]{120,}={0,2}|[0-9a-fA-F]{160,}")
REACH = re.compile(
    r"https?://|\bcurl\b|\bwget\b|TcpStream|UdpSocket|reqwest|ureq|hyper::|"
    r"Command::new|std::process|include_bytes!|include_str!|/dev/tcp|base64 -d|"
    r"\beval\b|\bexec\b|LD_PRELOAD|chmod \+x|\bsudo\b"
)


def git(*args: str) -> str:
    return subprocess.check_output(["git", "-C", str(ROOT), *args], text=True, errors="replace")


def lev(a: str, b: str, cap: int = 2) -> int:
    if abs(len(a) - len(b)) > cap:
        return cap + 1
    prev = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        cur = [i]
        for j, cb in enumerate(b, 1):
            cur.append(min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (ca != cb)))
        prev = cur
    return prev[-1]


def locked_names(rev: str) -> set[str]:
    names: set[str] = set()
    for path in git("ls-tree", "-r", "--name-only", rev).splitlines():
        if path.endswith("Cargo.lock"):
            names |= set(re.findall(r'^name = "([^"]+)"', git("show", f"{rev}:{path}"), re.M))
    return names


def main() -> int:
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if len(args) != 1 or ".." not in args[0]:
        print(__doc__.strip().splitlines()[2], file=sys.stderr)
        return 2
    base, head = args[0].split("..", 1)
    base = git("merge-base", base, head).strip()
    found: dict[str, list[str]] = {}

    def flag(kind: str, text: str) -> None:
        found.setdefault(kind, []).append(text)

    for line in git("diff", "--numstat", base, head).splitlines():
        added, _deleted, path = line.split("\t", 2)
        if added == "-":
            flag("binary", path)
        if any(p.search(path) for p in BUILD_PATHS):
            flag("build", path)

    path = "?"
    for line in git("diff", "-U0", "--no-color", base, head).splitlines():
        if line.startswith("+++ "):
            path = line[6:] if line.startswith("+++ b/") else line[4:]
            continue
        if not line.startswith("+") or line.startswith("+++"):
            continue
        text = line[1:]
        if INVISIBLE.search(text):
            cps = sorted({f"U+{ord(c):04X}" for c in INVISIBLE.findall(text)})
            flag("unicode", f"{path}: {', '.join(cps)}: {text[:80]!r}")
        if len(text) > 400 or BLOB.search(text):
            flag("blob", f"{path}: {len(text)} chars: {text[:80]!r}")
        if m := REACH.search(text):
            flag("reach", f"{path}: `{m.group(0)}`: {text.strip()[:120]}")

    before, after = locked_names(base), locked_names(head)
    for name in sorted(after - before):
        near = [o for o in before if o != name and lev(name, o) <= 2]
        flag("deps", f"{name}" + (f" (one or two edits from {', '.join(sorted(near)[:3])})" if near else ""))

    md = "--markdown" in sys.argv
    if not found:
        print("No supply-chain flags." if not md else "**Supply-chain flags:** none.")
        return 0
    print("## Supply-chain flags\n" if md else "Supply-chain flags:")
    print("Read each before approving. These are places to look, not verdicts.\n" if md else "")
    for kind in ("unicode", "binary", "deps", "build", "blob", "reach"):
        items = found.get(kind, [])
        if not items:
            continue
        print(f"### {kind} ({len(items)})" if md else f"{kind} ({len(items)}):")
        for it in items[:60]:
            print(f"- {it}" if md else f"  {it}")
        if len(items) > 60:
            print(f"- ... {len(items) - 60} more" if md else f"  ... {len(items) - 60} more")
    return 1 if "--strict" in sys.argv else 0


if __name__ == "__main__":
    sys.exit(main())
