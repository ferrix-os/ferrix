#!/usr/bin/env python3
"""Commit the certification consultant's ledger, refusing anything but an append.

    ledger-commit.py [--repo DIR] [--file NAME] [--message TEXT]
    ledger-commit.py --self-test

The ledger (`~/.local/share/ferrix/cert-consultant/reviews.md`) is cited by line
number from commit messages, this register and the manifest, so a line that
changes after it was cited names nothing (F-120). The directory is a local git
repository (F-120's first step). This tool commits what was added since the
last commit and refuses, with the first differing line, if anything already
committed is no longer there as it was: the committed bytes must be a prefix
of the file on disk. It also refuses a file whose last line is not finished,
which is an append in progress, and a repository with something else staged.

What is committed is the bytes this tool read and checked, not whatever the
file holds a moment later, so an append landing during the run is left for the
next run. No hook is installed anywhere, and nothing here pushes: where the
repository is published is the customer's decision (F-120).

Exit 0: committed, or nothing new. Exit 1: refused. Exit 2: not a repository.
"""
from __future__ import annotations

import argparse
import os
import subprocess
import sys
import tempfile
from pathlib import Path

DEFAULT_REPO = Path.home() / ".local/share/ferrix/cert-consultant"
DEFAULT_FILE = "reviews.md"


def git(repo: Path, *args: str, data: bytes | None = None) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(["git", "-C", str(repo), *args], input=data, capture_output=True)


def first_difference(old: bytes, new: bytes) -> str:
    old_lines = old.split(b"\n")
    new_lines = new.split(b"\n")
    for number, old_line in enumerate(old_lines, start=1):
        if number > len(new_lines):
            return f"line {number} and what follows it are gone (was {old_line[:70]!r})"
        if new_lines[number - 1] != old_line:
            return f"line {number} changed (was {old_line[:70]!r}, is {new_lines[number - 1][:70]!r})"
    return "no differing line found"


def commit(repo: Path, name: str, message: str) -> int:
    if git(repo, "rev-parse", "--git-dir").returncode != 0:
        print(f"ledger-commit: {repo} is not a git repository", file=sys.stderr)
        return 2
    path = repo / name
    new = path.read_bytes()
    shown = git(repo, "show", f"HEAD:{name}")
    old = shown.stdout if shown.returncode == 0 else b""
    if not new.startswith(old):
        print(f"ledger-commit: REFUSED, {name} is not an append: {first_difference(old, new)}", file=sys.stderr)
        return 1
    if new == old:
        print("ledger-commit: nothing new")
        return 0
    if not new.endswith(b"\n"):
        print(f"ledger-commit: REFUSED, {name} ends in an unfinished line (an append in progress); run again", file=sys.stderr)
        return 1
    if git(repo, "diff", "--cached", "--quiet").returncode != 0:
        print("ledger-commit: REFUSED, something else is staged in the repository", file=sys.stderr)
        return 1
    blob = git(repo, "hash-object", "-w", "--stdin", data=new).stdout.decode().strip()
    git(repo, "update-index", "--add", "--cacheinfo", f"100644,{blob},{name}")
    done = git(repo, "commit", "-q", "-m", message)
    if done.returncode != 0:
        print(done.stderr.decode(), file=sys.stderr)
        return 1
    added = new[len(old):].count(b"\n")
    print(f"ledger-commit: committed {added} new line(s) of {name}, now {new.count(b'\n')} lines")
    return 0


def self_test() -> int:
    failures = 0

    def expect(label: str, got: int, want: int) -> None:
        nonlocal failures
        ok = got == want
        failures += 0 if ok else 1
        print(f"  {'ok  ' if ok else 'FAIL'} {label}: rc {got}, wanted {want}")

    with tempfile.TemporaryDirectory() as tmp:
        repo = Path(tmp)
        env = {"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1", "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t"}
        os.environ.update(env)
        git(repo, "init", "-q", "-b", "main")
        ledger = repo / DEFAULT_FILE
        ledger.write_bytes(b"one\ntwo\nthree\n")
        git(repo, "add", DEFAULT_FILE)
        git(repo, "commit", "-q", "-m", "base")
        expect("nothing new", commit(repo, DEFAULT_FILE, "m"), 0)
        ledger.write_bytes(b"one\ntwo\nthree\nfour\n")
        expect("one line appended", commit(repo, DEFAULT_FILE, "m"), 0)
        committed = git(repo, "show", f"HEAD:{DEFAULT_FILE}").stdout
        expect("the append is what was committed", 0 if committed == b"one\ntwo\nthree\nfour\n" else 1, 0)
        # The planted-change controls: each must be refused and leave HEAD where it was.
        head = git(repo, "rev-parse", "HEAD").stdout
        for label, content in (
            ("old line edited, one appended", b"one\nTWO\nthree\nfour\nfive\n"),
            ("old line edited in place", b"one\ntwo\nthree\nFOUR\n"),
            ("last line cut", b"one\ntwo\nthree\n"),
            ("a line inserted above the end", b"one\nnew\ntwo\nthree\nfour\n"),
            ("appended with a changed line break", b"one\ntwo\nthree\nfour\r\nfive\n"),
            ("unfinished appended line", b"one\ntwo\nthree\nfour\nfi"),
        ):
            ledger.write_bytes(content)
            expect(f"refused: {label}", commit(repo, DEFAULT_FILE, "m"), 1)
        expect("HEAD did not move", 0 if git(repo, "rev-parse", "HEAD").stdout == head else 1, 0)
        ledger.write_bytes(b"one\ntwo\nthree\nfour\nfive\n")
        expect("append after the refusals", commit(repo, DEFAULT_FILE, "m"), 0)
    print("ledger-commit: self-test " + ("passed" if failures == 0 else f"FAILED ({failures})"))
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", type=Path, default=DEFAULT_REPO)
    parser.add_argument("--file", default=DEFAULT_FILE)
    parser.add_argument("--message", default="Ledger: new entries (append only, checked by ledger-commit.py)")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    return commit(args.repo, args.file, args.message)


if __name__ == "__main__":
    sys.exit(main())
