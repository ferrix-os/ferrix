#!/usr/bin/env python3
"""What a `personality=user` kernel holds, from what rustc read to build it
(`docs/microkernel/M6.md` §3, ledger 5942 M.5).

The evidence is rustc's own dep-info for the ring-0 crates of a
`--personality user` build (no claim attaches: `fast` and `max` do not build
yet, M.6): every source file it lists must be in the `core` or `item` ring of
`certification-item.json`, or an unlisted stub beside them, and none may be on
LEFT, section 2 of the design. `nm` then looks for one symbol only each real
file has, because a stub keeps the real file's paths: a stub cannot be told
from the real file by a path, and rustc's list and the symbol together can.

Usage:  check-kernel-shape.py TARGET_DIR/ARCH-TRIPLE/PROFILE [--report]
Exit 1 names each breach.
"""
import argparse
import collections
import glob
import json
import os
import subprocess
import sys

ROOTS = {
    "ferrix_kernel_core": "kernel_core.rs",
    "ferrix_kernel_item": "kernel_item.rs",
    "ferrix_kernel": "main.rs",
}

# Section 2's list: files that must not be read for ring 0 under this shape.
LEFT = (
    "console/screen.rs",
    "panic/screen.rs",
    "discovery/powerctl.rs",
    "arch/x86_64/powerctl.rs",
    "discovery/firmware.rs",
    "platform/st/stm32mp1.rs",
    "platform/google/gs201/usb.rs",
)

# Crates no ring-0 crate of this shape may link.
LEFT_CRATES = ("ferrix_kernel_load", "ferrix_kernel_settings", "ferrix_fbtext", "ferrix_qr")

# (a symbol's text, the file it proves is not built).
REAL_SYMBOLS = (
    ("console::screen::Board::newline", "console/screen.rs"),
    ("panic::screen::draw_code", "panic/screen.rs"),
    ("powerctl::Restorer", "arch/x86_64/powerctl.rs"),
)


def dep_info(directory: str, crate: str, root: str) -> str | None:
    best = None
    for path in glob.glob(f"{directory}/deps/{crate}-*.d"):
        with open(path, encoding="utf-8", errors="replace") as handle:
            text = handle.read()
        if f"/{root}:" in text or f"src/{root}" in text:
            if best is None or os.path.getmtime(path) > os.path.getmtime(best):
                best = path
    return best


def read_files(directory: str, tree: str) -> set[str]:
    files: set[str] = set()
    for crate, root in ROOTS.items():
        path = dep_info(directory, crate, root)
        if path is None:
            raise SystemExit(f"check-kernel-shape: no dep-info for {crate} under {directory}")
        with open(path, encoding="utf-8", errors="replace") as handle:
            for line in handle:
                name = line.split(":")[0].strip()
                if name.endswith(".rs") and not line.startswith(" "):
                    files.add(os.path.normpath(os.path.join(tree, name)))
    return files


def ring_of(manifest: dict, relative: str) -> str | None:
    """The ring a file is in; a member that names a directory holds what is under it."""
    best = None
    best_length = -1
    for ring in ("core", "item", "load"):
        for entry in manifest["rings"][ring].get("members", []):
            name = entry if isinstance(entry, str) else entry.get("path", "")
            if not name:
                continue
            bare = name.removesuffix("/**").rstrip("/")
            if (relative == bare or relative.startswith(bare + "/")) and len(bare) > best_length:
                best = ring
                best_length = len(bare)
    return best


def code_lines(path: str) -> int:
    count = 0
    with open(path, encoding="utf-8", errors="replace") as handle:
        for line in handle:
            stripped = line.strip()
            if stripped and not stripped.startswith("//"):
                count += 1
    return count


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory")
    parser.add_argument("--report", action="store_true")
    parser.add_argument("--tree", default=".")
    args = parser.parse_args()
    tree = os.path.abspath(args.tree)
    with open(os.path.join(tree, "tools/common/data/certification-item.json"), encoding="utf-8") as handle:
        manifest = json.load(handle)
    files = read_files(args.directory, tree)
    breaches = []
    product = 0
    checks = 0
    by_top: collections.Counter[str] = collections.Counter()
    for path in sorted(files):
        marker = os.sep + os.path.join("src", "kernel", "src") + os.sep
        if marker not in path or not os.path.exists(path):
            continue
        relative = path.split(marker)[1]
        if relative in LEFT:
            breaches.append(f"{relative}: on M6's list, and rustc read it for ring 0")
        if ring_of(manifest, relative) == "load":
            breaches.append(f"{relative}: a load-ring file rustc read for ring 0")
        lines = code_lines(path)
        if relative.endswith("check.rs") or "_check" in relative:
            checks += lines
        else:
            product += lines
            by_top[relative.split("/")[0]] += lines
    kernel = os.path.join(args.directory, "ferrix-kernel")
    if os.path.exists(kernel):
        names = subprocess.run(
            ["nm", "--demangle=rust", kernel], capture_output=True, text=True, check=False
        ).stdout
        for crate in LEFT_CRATES:
            if f" {crate}::" in names or f"<{crate}::" in names:
                breaches.append(f"{crate}: a symbol of it is in the kernel")
        # A method demangles as `<path::Type>::method`, so the brackets go before the test.
        plain = names.replace("<", "").replace(">", "")
        for symbol, file in REAL_SYMBOLS:
            if symbol in plain:
                breaches.append(f"{symbol}: only {file} has it, and it is in the kernel")
    else:
        breaches.append(f"{kernel}: no kernel to read symbols from")
    if args.report:
        print(f"check-kernel-shape: {product} lines of product code, {checks} of checks, no comments")
        for top, lines in by_top.most_common():
            print(f"  {lines:7} {top}")
    for breach in breaches:
        print(f"check-kernel-shape: {breach}")
    if breaches:
        return 1
    print("check-kernel-shape: no load file, nothing on M6's list, in ring 0")
    return 0


if __name__ == "__main__":
    sys.exit(main())
