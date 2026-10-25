#!/usr/bin/env python3
"""Refuse `cfg(target_...)` in the product files of the host-measured item
crates, `ferrix-sigverify` and `ferrix-sha256` (docs/SIGNING.md §8.5; the
consultant's S1-c, ledger lines 3738 and 3803 (4)).

Their tests and their coverage run on the host, and stand for the code the
UEFI loader, init and devmgr run on every target. That holds only while the
product code is the same on every target: one `#[cfg(target_arch = ...)]`,
`cfg!(target_os = ...)` or `cfg_attr(target_..., ...)` would compile code
the host never ran. So any `target_` inside a `cfg` in those files fails.

`--self-test` plants one and requires the refusal.

Usage:  python3 tools/common/check/check-no-target-cfg.py [--self-test]
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[3]
CRATES = [
    "src/lib/crypto/sigverify",
    "src/lib/crypto/sha256",
    "src/lib/proto/boot-policy",
    "src/lib/proto/slots",
]
TEST_FILES = {"tests.rs", "vectors.rs"}
PATTERN = re.compile(r"cfg(?:_attr)?!?\s*\([^)]*\btarget_")


def offenders(root: pathlib.Path) -> list[str]:
    found = []
    for crate in CRATES:
        for path in sorted((root / crate / "src").rglob("*.rs")):
            if path.name in TEST_FILES:
                continue
            for n, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
                if PATTERN.search(line):
                    found.append(f"{path.relative_to(root)}:{n}: {line.strip()}")
    return found


def self_test() -> int:
    with tempfile.TemporaryDirectory(prefix="check-no-target-cfg-") as scratch:
        root = pathlib.Path(scratch)
        for crate in CRATES:
            (root / crate / "src").mkdir(parents=True)
            (root / crate / "src" / "lib.rs").write_text("pub fn f() {}\n")
        (root / CRATES[0] / "src" / "tests.rs").write_text('#[cfg(target_os = "linux")]\nfn t() {}\n')
        if offenders(root):
            print("no-target-cfg: self-test: a test file was refused", file=sys.stderr)
            return 1
        (root / CRATES[0] / "src" / "lib.rs").write_text('#[cfg(target_arch = "arm")]\npub fn f() {}\n')
        if len(offenders(root)) != 1:
            print("no-target-cfg: self-test: a planted cfg(target_arch) was NOT refused", file=sys.stderr)
            return 1
    print("no-target-cfg: self-test passed: a planted cfg(target_arch) is refused")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    if parser.parse_args().self_test:
        return self_test()
    found = offenders(ROOT)
    for line in found:
        print(f"no-target-cfg: {line}: target-dependent code in a host-measured item crate", file=sys.stderr)
    if found:
        return 1
    print(f"no-target-cfg: no cfg(target_...) in {', '.join(CRATES)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
