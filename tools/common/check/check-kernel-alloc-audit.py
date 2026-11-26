#!/usr/bin/env python3
"""Where a kernel of `personality=user` allocates, against K16's audit
(`docs/microkernel/K16.md` §3, ledger 5942 K16.3, 6226 A.1-A.7).

K16 means no shared kernel heap and no shared frame pool after the seal, so every
place that draws on either has to say who pays. The audit is by file, in
`tools/common/data/kernel-alloc-audit.json`, each with a bucket, its worst site's, and
the count of its sites. This fails on a file of the kernel's source that allocates and
is not listed, on a listed one that no longer does, on a count that moved (a new site
in a listed file is unread until someone reads it), and on an unknown bucket.

A site is what `check-fallible-alloc.py` flags, marked or not, and what it does not:
the frame allocator's calls, `vmap`, and the wrappers of `fallible`.

It reads source, so it is a map and proves nothing: the count check K16.3 asks for
needs K12b. The architectures not yet read are named in UNREAD and reported on every
run, so they cannot pass as audited.

With `--built TARGET/ARCH/PROFILE` it also holds NOT_BUILT against what rustc read for
ring 0 (M.5's dep-info): a file read that this skips fails. `test-kernel-shape` runs that.

Usage: check-kernel-alloc-audit.py [ROOT] [--built DIR] | --self-test
"""
import glob
import importlib.util
import json
import os
import re
import sys
sys.path.insert(0, str(__import__("pathlib").Path(__file__).resolve().parent))
import kernel_roots  # noqa: E402  (docs/microkernel/CRATE-SPLIT.md S0)

SOURCE = "src/kernel/src"
AUDIT = "tools/common/data/kernel-alloc-audit.json"
BUCKETS = {"boot", "job", "pool", "frames", "none", "nojob"}
FRAMES = re.compile(
    r"allocate_frames(?:_below)?\(|allocate_frame_run\(|allocate_user_(?:frame|run)\("
    r"|allocate_program_frames\(|fallible::\w+|Buffer::zeroed|vmap::\w+\(|\.try_reserve\w*\("
)
# Not ring 0 under this shape: the load ring's, the lift's. `--built` checks this.
NOT_BUILT = re.compile(
    r"^(fs/|net/|interfaces/|lift/|lift_|kernel_load|tty|cgroup|init"
    r"|syscall/(?!native|program\.rs|uaccess\.rs|mod\.rs)"
    r"|console/screen\.rs|panic/screen\.rs|discovery/powerctl|discovery/firmware\.rs"
    r"|arch/x86_64/powerctl)"
)
# The board glue's parts: compiled into every kernel with board_glue=on, so rustc
# reads them and `--built` cannot hold them out, but named by no code of this shape,
# so no symbol of theirs is in the image: check-kernel-shape.py's LEFT_CRATES, by nm,
# in the same row (R.4, ledger 7017, 10421 I2). Not read here, as not linked.
NOT_LINKED = re.compile(r"^(platform/st/|platform/google/gs201/usb\.rs)")
# Each NOT_LINKED part, a file of it, and its crate in check-kernel-shape.py's LEFT_CRATES.
NOT_LINKED_CRATES = {
    "platform/st/stm32mp1.rs": "ferrix_kernel_board_stm32mp1",
    "platform/google/gs201/usb.rs": "ferrix_kernel_board_gs201_usb",
}
# Read for no claim yet (A.1): armv7a with a row. platform/ was read for R.4
# (ledger 10421): what it builds is in the audit, the board glue in NOT_LINKED,
# and a new file there is unread until the audit names it.
UNREAD = ("arch/armv7a/",)



def fallible_gate():
    here = os.path.dirname(os.path.abspath(__file__))
    spec = importlib.util.spec_from_file_location(
        "check_fallible_alloc", os.path.join(here, "check-fallible-alloc.py")
    )
    module = importlib.util.module_from_spec(spec)
    sys.modules["check_fallible_alloc"] = module
    spec.loader.exec_module(module)
    return module


def sites_in(text: str, gate) -> int:
    count = len(gate.scan_source(text))
    for line in text.split("\n"):
        if line.lstrip().startswith("//"):
            continue
        count += len(FRAMES.findall(line))
    return count


def is_check(path: str) -> bool:
    name = os.path.basename(path)
    return "check" in name or "/check/" in f"/{path}" or name == "tests.rs"


def problems(found: dict, listed: dict) -> list[str]:
    out = []
    for path in sorted(set(found) - set(listed)):
        out.append(f"{path}: {found[path]} sites, and K16's audit does not say who pays")
    for path in sorted(set(listed) - set(found)):
        out.append(f"{path}: in K16's audit, and allocates nothing or is gone")
    for path, entry in sorted(listed.items()):
        if entry.get("bucket") not in BUCKETS:
            out.append(f"{path}: no bucket K16 §3 has")
        if path in found and entry.get("sites") != found[path]:
            out.append(f"{path}: {found[path]} sites, the audit read {entry.get('sites')}: read it again")
    return out


def self_test(gate) -> int:
    failed = []
    if sites_in("let a = fallible::try_box(1);", gate) != 1 or sites_in("// fallible::try_box(1)", gate):
        failed.append("the frame pattern")
    if not sites_in("fn f() { let a = Box::new(1); }", gate):
        failed.append("Box::new")
    if not problems({"a.rs": 1}, {}):
        failed.append("an unlisted file")
    if not problems({}, {"a.rs": {"bucket": "job", "sites": 1}}):
        failed.append("a listed file with none")
    if not problems({"a.rs": 2}, {"a.rs": {"bucket": "job", "sites": 1}}):
        failed.append("a moved count")
    if not problems({"a.rs": 1}, {"a.rs": {"bucket": "made up", "sites": 1}}):
        failed.append("a bad bucket")
    # R.4 (ledger 10421 I3): a new file under platform/ is unread until the audit
    # names it, so it fails rather than passing as unread.
    if "platform/google/new.rs".startswith(UNREAD) or not problems({"platform/google/new.rs": 1}, {}):
        failed.append("a new platform/ file")
    # V2 (ledger 10522): every NOT_LINKED path is a part whose crate check-kernel-shape.py
    # holds out of the image by nm (LEFT_CRATES), so the two lists cannot part.
    shape = importlib.util.spec_from_file_location(
        "shape", os.path.join(os.path.dirname(os.path.abspath(__file__)), "check-kernel-shape.py")
    )
    held = importlib.util.module_from_spec(shape)
    shape.loader.exec_module(held)
    for path, crate in NOT_LINKED_CRATES.items():
        if not NOT_LINKED.match(path) or crate not in held.LEFT_CRATES:
            failed.append(f"{path}: NOT_LINKED with no crate in LEFT_CRATES")
    if not NOT_LINKED_CRATES:
        failed.append("NOT_LINKED names no part")
    # I2: the board glue is not linked, so not read; nothing else under platform/ is.
    if not NOT_LINKED.match("platform/st/stm32mp1.rs") or NOT_LINKED.match("platform/google/gs201/watchdog.rs"):
        failed.append("the board glue's class")
    for what in failed:
        print(f"check-kernel-alloc-audit: self-test: {what} did not fire")
    if failed:
        return 1
    print("check-kernel-alloc-audit: self-test: 8 planted differences FIRED")
    return 0


def scan(root: str, gate) -> tuple[dict, list[str]]:
    found = {}
    unread = []
    # Every kernel source root (kernel_source_roots), SOURCE's files named
    # relative to it and a part's by its repo path, as check-item-boundary does.
    for source in kernel_roots.roots():
        base = os.path.join(root, source)
        for directory, _, names in os.walk(base):
            for name in names:
                if not name.endswith(".rs"):
                    continue
                full = os.path.join(directory, name)
                kernel_roots.note_walked("check-kernel-alloc-audit", [os.path.abspath(full)])
                # A part's file keeps its module path within its root, and the
                # classes below (NOT_BUILT, UNREAD) are module paths, so they are
                # matched on that; it is reported by its repo path (S2).
                module = os.path.relpath(full, base).replace(os.sep, "/")
                rel = module if source == SOURCE else f"{source}/{module}"
                if is_check(rel) or NOT_BUILT.match(module) or NOT_LINKED.match(module):
                    continue
                with open(full, encoding="utf-8", errors="replace") as handle:
                    count = sites_in(handle.read(), gate)
                if not count:
                    continue
                if module.startswith(UNREAD):
                    unread.append(rel)
                else:
                    found[rel] = count
    return found, sorted(unread)


def built_files(directory: str) -> set[str]:
    read = set()
    for crate, root in (
        ("ferrix_kernel_core", "kernel_core.rs"),
        ("ferrix_kernel_item", "kernel_item.rs"),
        ("ferrix_kernel", "main.rs"),
    ):
        best = None
        for path in glob.glob(f"{directory}/deps/{crate}-*.d"):
            with open(path, encoding="utf-8", errors="replace") as handle:
                first = handle.readline()
            if f"src/{root}" in first and "lift" not in first.split(":")[1][:80]:
                if best is None or os.path.getmtime(path) > os.path.getmtime(best):
                    best = path
        if best is None:
            continue
        with open(best, encoding="utf-8", errors="replace") as handle:
            first = handle.readline()
        for token in first.split(":", 1)[1].split():
            normal = os.path.normpath(token)
            if normal.endswith(".rs") and "src/kernel/src/" in normal:
                read.add(normal.split("src/kernel/src/", 1)[1])
    return read


def main() -> int:
    gate = fallible_gate()
    arguments = sys.argv[1:]
    if "--self-test" in arguments:
        return self_test(gate)
    built = None
    if "--built" in arguments:
        at = arguments.index("--built")
        built = arguments[at + 1]
        del arguments[at : at + 2]
    root = arguments[0] if arguments else "."
    with open(os.path.join(root, AUDIT), encoding="utf-8") as handle:
        listed = json.load(handle)["files"]
    found, unread = scan(root, gate)
    out = problems(found, listed)
    if built is not None:
        for path in sorted(built_files(built)):
            if NOT_BUILT.match(path) and not is_check(path):
                out.append(f"{path}: rustc read it for ring 0, and this calls it not built")
    for line in out:
        print(line)
    if out:
        print(f"check-kernel-alloc-audit: {len(out)} problem(s)")
        print(
            "check-kernel-alloc-audit: read the new sites, edit the row by hand in this commit and say so in "
            'its message ("audit: <file> n to m, <bucket>"): docs/BACKLOG.md, What a landing runs; ledger 6803 BL.10'
        )
        return 1
    print(
        f"check-kernel-alloc-audit: {len(listed)} files with who pays; "
        f"NOT read, no claim for them: {len(unread)} files of {', '.join(UNREAD)}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
