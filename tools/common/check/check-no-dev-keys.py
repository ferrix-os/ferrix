#!/usr/bin/env python3
"""Assert no development signing key is in the tree (docs/SIGNING.md §6.4, D5).

The development soft signer, `ferrix-softsign`, keeps each key's private
state in a file whose first line is `FXSOFTSIGN-STATE-1`, under
`~/.local/share/ferrix/signing-dev/`, outside every repository and image.
It refuses to make a key inside a git work tree. This check holds the other
direction: a state file copied into the tree by hand, or committed by
mistake, fails the gate, and the development anchor it belongs to is then
replaced (the consultant's D5, ledger line 3731).

It reads every file git tracks or would add (tracked and untracked, not
ignored), and refuses any whose first line is the magic. The soft signer's
own source names the magic inside a string, never as a first line, and this
script likewise, so neither matches.

`--self-test` plants a state file in a scratch git tree and requires the
refusal: the negative control.

Usage:  python3 tools/common/check/check-no-dev-keys.py [--self-test]
"""

from __future__ import annotations

import argparse
import pathlib
import subprocess
import sys
import tempfile

MAGIC = b"FXSOFTSIGN-STATE-1"
ROOT = pathlib.Path(__file__).resolve().parents[3]


def candidates(root: pathlib.Path) -> list[str]:
    out = subprocess.run(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cwd=root,
        check=True,
        capture_output=True,
    ).stdout
    return [p for p in out.decode("utf-8", "replace").split("\0") if p]


def keys_in(root: pathlib.Path) -> list[str]:
    found = []
    for rel in candidates(root):
        path = root / rel
        try:
            with path.open("rb") as f:
                first = f.readline(64).rstrip(b"\r\n")
        except (OSError, IsADirectoryError):
            continue
        if first == MAGIC:
            found.append(rel)
    return found


def self_test() -> int:
    with tempfile.TemporaryDirectory(prefix="check-no-dev-keys-") as scratch:
        root = pathlib.Path(scratch)
        subprocess.run(["git", "init", "-q"], cwd=root, check=True)
        (root / "clean.txt").write_text("FXSOFTSIGN-STATE-1 is only named here\n")
        if keys_in(root):
            print("no-dev-keys: self-test: a file that only names the magic was refused", file=sys.stderr)
            return 1
        (root / "keys").mkdir()
        (root / "keys" / "state").write_bytes(MAGIC + b"\nlms_type 0x0000000b\n")
        if keys_in(root) != ["keys/state"]:
            print("no-dev-keys: self-test: a planted state file was NOT refused", file=sys.stderr)
            return 1
    print("no-dev-keys: self-test passed: a planted state file is refused")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    found = keys_in(ROOT)
    for rel in found:
        print(
            f"no-dev-keys: {rel} is a development signing key's private state: remove it, "
            "and replace the development anchor it belongs to (docs/SIGNING.md §6.4, D5)",
            file=sys.stderr,
        )
    if found:
        return 1
    print("no-dev-keys: no development signing key in the tree")
    return 0


if __name__ == "__main__":
    sys.exit(main())
