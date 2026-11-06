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

Files are classed by every kernel source root (kernel_source_roots,
docs/microkernel/CRATE-SPLIT.md S0; ledger 7429 S.2): a file of an optional
part is load by the record's rule (ledger 7346 (10)), so rustc reading one for
ring 0 is a breach wherever the parts live.

Usage:  check-kernel-shape.py TARGET_DIR/ARCH-TRIPLE/PROFILE [--report]
        check-kernel-shape.py TARGET_DIR/ARCH-TRIPLE/release --budget SET --arch ARCH
        check-kernel-shape.py --sets
        check-kernel-shape.py --self-test
Exit 1 names each breach. `--budget` is the size ratchet of docs/microkernel/TINY-KERNEL.md §7
(Cut 0): ring 0's count against tools/common/data/ring0-budget.json, `cargo xtask test-ring0-budget`.
"""
import argparse
import bisect
import collections
import glob
import json
import os
import re
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import kernel_roots  # noqa: E402  (every kernel source root, CRATE-SPLIT.md S0)

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
    # The board glue, parts since S2 (docs/microkernel/CRATE-SPLIT.md): their
    # files by their repo paths, and their crates in LEFT_CRATES below.
    f"{kernel_roots.parts_place()}/board/stm32mp1/src/platform/st/stm32mp1.rs",
    f"{kernel_roots.parts_place()}/board/gs201-usb/src/platform/google/gs201/usb.rs",
    # The log's control channel, a part since S3, built only where
    # ferrix_load_built is ring0 or lift (ledger 7630 S3.1).
    f"{kernel_roots.parts_place()}/device/logctl/src/interfaces/logctl/mod.rs",
    f"{kernel_roots.parts_place()}/device/logctl/src/interfaces/logctl/check.rs",
    # A network driver's ring, a part since S4, built where logctl is.
    f"{kernel_roots.parts_place()}/device/net-ring/src/interfaces/net_ring/mod.rs",
    f"{kernel_roots.parts_place()}/device/net-ring/src/interfaces/net_ring/check.rs",
)

# Crates no ring-0 crate of this shape may link.
LEFT_CRATES = ("ferrix_kernel_load", "ferrix_kernel_settings", "ferrix_kernel_board_stm32mp1",
               "ferrix_kernel_board_gs201_usb", "ferrix_kernel_logctl",
               "ferrix_kernel_net_ring", "ferrix_fbtext", "ferrix_qr")

# Symbols that must not be in this shape's kernel: normal mode's FS record does not move while
# K1's fast path has the mark up (ledger 6464 Z2, 6981).
ABSENT_SYMBOLS = ("x86_64::syscall::arch_prctl",)

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


# What this shape rules out, by the attribute's text: a report's figure, by a brace walk, and no
# evidence; the shape is judged above, by rustc's list and the symbols.
OUT_OF_THIS_SHAPE = (
    '#[cfg(not(ferrix_personality = "user"))]',
    '#[cfg(not(ferrix_shape = "micro"))]',
    '#[cfg(ferrix_load_built = "ring0")]',
    '#[cfg(ferrix_personality = "kernel")]',
)


def lines_cfgd_out(path: str) -> int:
    """How many code lines of `path` an attribute of OUT_OF_THIS_SHAPE rules out."""
    with open(path, encoding="utf-8", errors="replace") as handle:
        source = handle.read().split("\n")
    out = 0
    at = 0
    while at < len(source):
        if source[at].strip() not in OUT_OF_THIS_SHAPE:
            at += 1
            continue
        depth = 0
        opened = False
        at += 1
        while at < len(source):
            text = source[at].split("//")[0]
            if source[at].strip() and not source[at].lstrip().startswith("//"):
                out += 1
            depth += text.count("{") - text.count("}")
            opened = opened or "{" in text
            at += 1
            if (opened and depth <= 0) or (not opened and text.rstrip().endswith((";", ","))):
                break
    return out


def code_lines(path: str) -> int:
    count = 0
    with open(path, encoding="utf-8", errors="replace") as handle:
        for line in handle:
            stripped = line.strip()
            if stripped and not stripped.startswith("//"):
                count += 1
    return count


# ---------------------------------------------------------------------------
# The size ratchet (docs/microkernel/TINY-KERNEL.md §1 and §7, Cut 0; ledger 7256
# TK.3, TK.4). Count A, ring 0's product lines: every file rustc's dep-info lists for
# the three ring-0 crates, less the check files, plus every library file the image's
# own .debug_line names, whole files, no comments. Count B: the crates' files alone.
# Checks are printed beside, never capped. A rise fails, a fall past 1% with no edit of
# the baseline fails, and the baseline itself may rise only with a new ledger line.

BUDGET = "tools/common/data/ring0-budget.json"
FALL_SLACK = 0.01
LINE_ENTRY = re.compile(r"^(\S+)\s+(\d+)\s+(0x[0-9a-f]+)")


def budget_class(relative: str, roots: list[str]) -> tuple[str, str]:
    """A tree-relative file's class for the budget, by every kernel source root
    (kernel_source_roots; CRATE-SPLIT.md S0): ("kernel", its kernel file name, as
    ring0-budget.json's check_files and the rings name it), ("library", path),
    ("foreign", path) for code from elsewhere in src/, or ("other", path). A file
    of an optional part, under the parts' place, is kernel code, not foreign."""
    for root in roots:
        if relative.startswith(root + "/"):
            return "kernel", relative.removeprefix(root + "/") if root == kernel_roots.PRIMARY else relative
    if relative.startswith("src/lib/"):
        return "library", relative
    if relative.startswith("src/"):
        return "foreign", relative
    return "other", relative


def is_check_file(relative: str) -> bool:
    """The tool's one rule for a check, which is why the list of check files is reviewed."""
    return relative.endswith("check.rs") or "_check" in relative


def tree_relative(path: str, tree: str) -> str:
    """`path` relative to the tree when it is inside it, else as it is, normalised."""
    full = os.path.normpath(path if os.path.isabs(path) else os.path.join(tree, path))
    return os.path.relpath(full, tree) if full.startswith(tree + os.sep) else full


def line_table(kernel: str, tree: str) -> dict[str, set[int]] | None:
    """Each source file the image's .debug_line names, with its lines that carry code.

    `readelf -W` gives every block's file with its directory: without it a unit's own
    file is printed by its base name, and is lost."""
    result = subprocess.run(
        ["readelf", "-W", "--debug-dump=decodedline", kernel],
        capture_output=True, text=True, errors="replace", check=False,
    )
    if result.returncode != 0 or "Contents of the .debug_line section" not in result.stdout:
        return None
    table: dict[str, set[int]] = collections.defaultdict(set)
    current = None
    for line in result.stdout.split("\n"):
        if line.endswith(":") and not line.startswith("Contents of"):
            name = line[:-1].removeprefix("CU: ")
            current = tree_relative(name, tree) if name.endswith(".rs") else None
            continue
        match = LINE_ENTRY.match(line)
        if match and current and int(match.group(2)):
            table[current].add(int(match.group(2)))
    return dict(table) if any(name.startswith("src/") for name in table) else None


def text_bytes(kernel: str) -> int:
    sections = subprocess.run(["readelf", "-SW", kernel], capture_output=True, text=True, check=False).stdout
    for line in sections.split("\n"):
        fields = line.replace("[", " ").replace("]", " ").split()
        if len(fields) > 5 and fields[1] == ".text":
            return int(fields[5], 16)
    return 0


CHECK_SYMBOL = re.compile(r"_check(::|$)|::check::|::checks::|[a-z_]*_check\b|check_[a-z_]+\b")


def code_lines_reached(kernel: str, tree: str, table_lines: dict[str, set[int]]) -> int | None:
    """Count C on x86-64: lines with code in the image's product and library files, over the
    functions reachable from the image's roots with every check function taken out of the call
    graph. Roots: symbols with no Rust mangling, and every function an address in data names.
    AArch64's `adrp` pairs are not seen by this method, so there it answers None."""
    names = subprocess.run(["nm", "-n", "--defined-only", kernel], capture_output=True, text=True, check=False).stdout
    demangled = subprocess.run(
        ["nm", "-n", "--defined-only", "--demangle=rust", kernel], capture_output=True, text=True, check=False
    ).stdout
    symbols, readable = [], {}
    for raw, plain in zip(names.splitlines(), demangled.splitlines()):
        fields, plain_fields = raw.split(), plain.split(None, 2)
        if len(fields) == 3 and fields[1] in "tTwW" and len(plain_fields) == 3:
            symbols.append((int(fields[0], 16), fields[2]))
            readable[fields[2]] = plain_fields[2]
    if not symbols:
        return None
    starts = [address for address, _ in symbols]
    by_address = dict(symbols)
    dump = subprocess.run(
        ["objdump", "-d", "--no-show-raw-insn", "-M", "intel", kernel],
        capture_output=True, text=True, errors="replace", check=False,
    ).stdout
    edges: dict[str, set[str]] = collections.defaultdict(set)
    current = None
    target = re.compile(r"<([^>+]+)(\+0x[0-9a-f]+)?>")
    for line in dump.split("\n"):
        if line.endswith(">:"):
            head = re.match(r"[0-9a-f]+ <(.+)>:", line)
            current = head.group(1) if head else None
        elif current and "\t" in line:
            for found in target.finditer(line.split("\t", 1)[1]):
                if found.group(1) != current:
                    edges[current].add(found.group(1))
    roots = {name for _, name in symbols if not name.startswith(("_ZN", "_R"))}
    with open(kernel, "rb") as handle:
        image = handle.read()
    sections = subprocess.run(["readelf", "-SW", kernel], capture_output=True, text=True, check=False).stdout
    for line in sections.split("\n"):
        fields = line.replace("[", " ").replace("]", " ").split()
        if len(fields) < 8 or fields[2] in ("NOBITS",) or not fields[0].isdigit():
            continue
        flags = fields[7] if not fields[7].isdigit() else ""
        if "A" not in flags or "X" in flags:
            continue
        offset, size = int(fields[4], 16), int(fields[5], 16)
        for at in range(offset, offset + size - 7, 8):
            value = int.from_bytes(image[at:at + 8], "little")
            if value in by_address:
                roots.add(by_address[value])
    dropped = lambda name: bool(CHECK_SYMBOL.search(readable.get(name, name)))
    seen: set[str] = set()
    stack = [name for name in roots if not dropped(name)]
    while stack:
        name = stack.pop()
        if name in seen:
            continue
        seen.add(name)
        stack.extend(edge for edge in edges.get(name, ()) if edge in readable and edge not in seen and not dropped(edge))
    result = subprocess.run(
        ["readelf", "-W", "--debug-dump=decodedline", kernel],
        capture_output=True, text=True, errors="replace", check=False,
    ).stdout
    entries = []
    current_file = None
    for line in result.split("\n"):
        if line.endswith(":") and not line.startswith("Contents of"):
            name = line[:-1].removeprefix("CU: ")
            current_file = tree_relative(name, tree) if name.endswith(".rs") else None
            continue
        match = LINE_ENTRY.match(line)
        if match and current_file and int(match.group(2)):
            entries.append((int(match.group(3), 16), current_file, int(match.group(2))))
    reached = {address for address, name in symbols if name in seen}
    ends = {address: (starts[index + 1] if index + 1 < len(starts) else address) for index, address in enumerate(starts)}
    spans = sorted((address, ends[address]) for address in reached)
    spans_start = [start for start, _ in spans]
    counted: set[tuple[str, int]] = set()
    for address, name, number in entries:
        at = bisect.bisect_right(spans_start, address) - 1
        if at >= 0 and spans[at][0] <= address < spans[at][1]:
            kind, inner = budget_class(name, kernel_roots.roots())
            if kind == "library" or (kind == "kernel" and not is_check_file(inner)):
                counted.add((name, number))
    return len(counted)


def previous_budget(tree: str) -> tuple[dict | None, bool]:
    """The baseline file as it last differed in history, and whether history could be read."""
    path = os.path.join(tree, BUDGET)
    with open(path, encoding="utf-8") as handle:
        now = handle.read()
    log = subprocess.run(
        ["git", "-C", tree, "log", "--format=%H", "-n", "50", "--", BUDGET],
        capture_output=True, text=True, check=False,
    )
    if log.returncode != 0:
        return None, False
    for commit in log.stdout.split():
        shown = subprocess.run(
            ["git", "-C", tree, "show", f"{commit}:{BUDGET}"], capture_output=True, text=True, check=False
        )
        if shown.returncode == 0 and shown.stdout != now:
            return json.loads(shown.stdout), True
    return None, True


def budget_problems(budget: dict, tree: str) -> list[str]:
    """The baseline's own rules: a rise names a new ledger line; every check file listed exists."""
    out = []
    rise = budget.get("rise") or {}
    if not isinstance(rise.get("ledger"), int) or not rise.get("why"):
        out.append(f"{BUDGET}: `rise` names no ledger line and why (M.7)")
    for listed in budget.get("check_files", []):
        if not os.path.exists(os.path.join(tree, "src/kernel/src", listed)):
            out.append(f"{BUDGET}: lists {listed} as a check file, and it is gone: drop it")
    earlier, readable = previous_budget(tree)
    if not readable:
        out.append(f"{BUDGET}: its history could not be read, so a rise of the baseline cannot be judged")
    elif earlier is not None:
        # A rise, or a measure that shrank (ledger 7448 CZ.3): each needs a new ledger line.
        rose = []
        for name, by_arch in budget["baseline"].items():
            for arch, counts in by_arch.items():
                before = earlier.get("baseline", {}).get(name, {}).get(arch, {})
                for count, value in counts.items():
                    if value > before.get(count, -1):
                        rose.append(f"{name} {arch} {count} {before.get(count, 'none')} to {value}")
        for name, by_arch in earlier.get("baseline", {}).items():
            for arch in by_arch:
                if arch not in budget["baseline"].get(name, {}):
                    rose.append(f"the baseline's {name} {arch} removed")
        for added in sorted(set(budget.get("check_files", [])) - set(earlier.get("check_files", []))):
            rose.append(f"check_files gained {added}")
        for key in ("sets", "architectures", "profile"):
            if budget.get(key) != earlier.get(key):
                rose.append(f"`{key}` changed")
        if rose and rise.get("ledger") == (earlier.get("rise") or {}).get("ledger"):
            out.append(
                f"{BUDGET}: the baseline rose ({'; '.join(rose)}) and names no new ledger line: "
                "a rise is an explicit edit naming one (M.7)"
            )
    return out


def budget(directory: str, name: str, arch: str, tree: str) -> int:
    with open(os.path.join(tree, BUDGET), encoding="utf-8") as handle:
        plan = json.load(handle)
    say = f"ring0-budget: {name} {arch} {plan['profile']}"
    problems = [f"{say}: {problem}" for problem in budget_problems(plan, tree)]
    kernel = os.path.join(directory, "ferrix-kernel")
    if not os.path.exists(kernel):
        print(f"{say}: no kernel at {kernel}: the budget fails closed")
        return 1
    read = {tree_relative(path, tree) for path in read_files(directory, tree)}
    reviewed = set(plan.get("check_files", []))
    roots = kernel_roots.roots()
    product, checks, outside = 0, 0, []
    for relative in sorted(read):
        if not relative.endswith(".rs"):
            continue
        path = os.path.join(tree, relative) if not os.path.isabs(relative) else relative
        if not os.path.exists(path):
            problems.append(f"{say}: {relative}: rustc read it and it is not there: the dep-info is stale")
            continue
        kind, inner = budget_class(relative, roots)
        if kind == "kernel":
            if is_check_file(inner):
                checks += code_lines(path)
                if inner not in reviewed:
                    problems.append(
                        f"{say}: {inner}: read as a check, and {BUDGET}'s list of check files does not name it"
                    )
                continue
        else:
            outside.append(relative)
        product += code_lines(path)
    table = line_table(kernel, tree)
    if table is None:
        print(f"{say}: no .debug_line in {kernel}: the budget fails closed")
        return 1
    libraries, library_files = 0, 0
    for relative in sorted(table):
        kind, _ = budget_class(relative, roots)
        if kind == "kernel" and relative not in read:
            problems.append(f"{say}: the dep-info is short: the image's line table names {relative}, rustc's list does not")
        elif kind == "library" and os.path.exists(os.path.join(tree, relative)):
            libraries += code_lines(os.path.join(tree, relative))
            library_files += 1
        elif kind == "foreign":
            problems.append(f"{say}: {relative}: code in the image from outside the kernel's and the libraries' trees")
    count_a = product + libraries
    reach = code_lines_reached(kernel, tree, table) if arch == "x86_64" else None
    count_c = f"{reach:,}" if reach is not None else "not computed (this method does not see adrp)"
    print(
        f"{say}: count A {count_a:,} = kernel crates {product:,} + {library_files} library files {libraries:,}; "
        f"checks {checks:,} beside, in the image; count C {count_c}; .text {text_bytes(kernel):,} bytes"
        + (f"; read outside the kernel's source roots, counted in B: {', '.join(outside)}" if outside else "")
    )
    base = plan["baseline"].get(name, {}).get(arch)
    if base is None:
        problems.append(f"{say}: {BUDGET} has no baseline for this set")
    else:
        for label, value, held in (
            ("count A", count_a, base["a"]),
            ("count B", product, base["b"]),
            ("library files", libraries, base["a"] - base["b"]),
        ):
            if value > held:
                problems.append(f"{say}: {label} rose to {value:,} against {held:,} (+{value - held:,})")
            elif value < held * (1 - FALL_SLACK) and label != "library files":
                problems.append(
                    f"{say}: {label} fell to {value:,} against {held:,} "
                    f"(-{100 * (held - value) / held:.1f}%), past 1% with no edit of {BUDGET}: lower the baseline"
                )
    for problem in problems:
        print(problem)
    if problems:
        return 1
    print(f"{say}: within {BUDGET}")
    return 0


def budget_sets(tree: str) -> int:
    """One line a build: set, architecture, then its `--set` values, for xtask. Every baseline
    entry must have its build, and every build its baseline, or no build is listed at all."""
    with open(os.path.join(tree, BUDGET), encoding="utf-8") as handle:
        plan = json.load(handle)
    builds = {(name, arch) for name in plan["sets"] for arch in plan["architectures"]}
    held = {(name, arch) for name, by_arch in plan["baseline"].items() for arch in by_arch}
    if not builds or builds != held:
        print(f"ring0-budget: {BUDGET}: builds {sorted(builds)} and baseline {sorted(held)} differ", file=sys.stderr)
        return 1
    for name, sets in plan["sets"].items():
        for arch in plan["architectures"]:
            print(" ".join([name, arch, *sets.get("all", []), *sets.get(arch, [])]))
    return 0


def kernel_file(path: str, manifest: dict) -> tuple[str, bool] | None:
    """A path rustc read, as its kernel file name and whether it is a part's,
    or None if it is under no kernel source root. Matched by the root's
    components anywhere in the path, so the tree it was built in does not matter."""
    for root in kernel_roots.roots(manifest):
        marker = os.sep + root.replace("/", os.sep) + os.sep
        if marker in path:
            rest = path.split(marker, 1)[1].replace(os.sep, "/")
            primary = root == kernel_roots.PRIMARY
            return (rest if primary else f"{root}/{rest}"), not primary
    return None


def file_breaches(name: str, part: bool, manifest: dict) -> list[str]:
    breaches = []
    if name in LEFT:
        breaches.append(f"{name}: on M6's list, and rustc read it for ring 0")
    if part:
        breaches.append(f"{name}: an optional part's file (load, ledger 7346 (10)) rustc read for ring 0")
    elif ring_of(manifest, name) == "load":
        breaches.append(f"{name}: a load-ring file rustc read for ring 0")
    return breaches


def self_test() -> list[str]:
    place = kernel_roots.parts_place()
    # A part that exists, since kernel_roots refuses a root with no package
    # above it (ledger 7608 S1.5).
    part_root = f"{place}/settings/src"
    manifest = {
        "kernel_source_roots": {"parts_place": place, "roots": ["src/kernel/src", part_root]},
        "rings": {"core": {"members": ["mm/**"]}, "item": {"members": []},
                  "load": {"members": ["fs/**", f"{place}/settings/**"]}},
    }
    failures = []
    tree = os.sep + "t"
    cases = (
        (f"{tree}/src/kernel/src/mm/frame.rs", ("mm/frame.rs", False), False),
        (f"{tree}/src/kernel/src/fs/pipe.rs", ("fs/pipe.rs", False), True),
        (f"{tree}/{part_root}/lib.rs", (f"{part_root}/lib.rs", True), True),
        (f"{tree}/src/lib/kernel/sync/src/lib.rs", None, False),
    )
    # The budget classes a part's file as kernel code, by its repo path (the coordinator's
    # relay of 19d996767: it failed closed on a part's file).
    roots = ["src/kernel/src", part_root]
    for relative, want in ((f"{part_root}/lib.rs", ("kernel", f"{part_root}/lib.rs")),
                           ("src/kernel/src/mm/frame.rs", ("kernel", "mm/frame.rs")),
                           ("src/lib/kernel/sync/src/lib.rs", ("library", "src/lib/kernel/sync/src/lib.rs")),
                           ("src/user/system/native/rt/src/lib.rs", ("foreign", "src/user/system/native/rt/src/lib.rs"))):
        if budget_class(relative, roots) != want:
            failures.append(f"budget: {relative} classed as {budget_class(relative, roots)}, not {want}")
    for path, want, breach in cases:
        path = path.replace("/", os.sep)
        got = kernel_file(path, manifest)
        if got != want:
            failures.append(f"{path} classed as {got}, not {want}")
        elif got is not None and bool(file_breaches(*got, manifest)) != breach:
            failures.append(f"{path} taken as {'no ' if breach else ''}breach")
    return failures


def main() -> int:
    if "--self-test" in sys.argv[1:]:
        failures = self_test()
        for failure in failures:
            print(f"check-kernel-shape: self-test: {failure}")
        if failures:
            return 1
        print("check-kernel-shape: self-test passed: a load file and a part's file read for ring 0 are breaches; "
              "the budget counts a part's file as kernel code")
        return 0
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", nargs="?")
    parser.add_argument("--report", action="store_true")
    parser.add_argument("--tree", default=".")
    parser.add_argument("--budget", metavar="SET", help="hold this build to ring0-budget.json's baseline for SET")
    parser.add_argument("--arch", help="with --budget: the build's architecture")
    parser.add_argument("--sets", action="store_true", help="print ring0-budget.json's builds, one a line")
    args = parser.parse_args()
    tree = os.path.abspath(args.tree)
    if args.sets:
        return budget_sets(tree)
    if args.budget:
        return budget(args.directory, args.budget, args.arch, tree)
    with open(os.path.join(tree, "tools/common/data/certification-item.json"), encoding="utf-8") as handle:
        manifest = json.load(handle)
    files = read_files(args.directory, tree)
    breaches = []
    product = 0
    built = 0
    checks = 0
    by_top: collections.Counter[str] = collections.Counter()
    for path in sorted(files):
        found = kernel_file(path, manifest)
        if found is None or not os.path.exists(path):
            continue
        relative, part = found
        breaches += file_breaches(relative, part, manifest)
        lines = code_lines(path)
        if relative.endswith("check.rs") or "_check" in relative:
            checks += lines
        else:
            product += lines
            built += lines - lines_cfgd_out(path)
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
        for symbol in ABSENT_SYMBOLS:
            if symbol in plain:
                breaches.append(f"{symbol}: a user kernel holds no such writer, and it is in the kernel")
    else:
        breaches.append(f"{kernel}: no kernel to read symbols from")
    if args.report:
        print(
            f"check-kernel-shape: {product} lines of product code in the files rustc read, "
            f"{built} of them not under a cfg this shape rules out, {checks} of checks, no comments"
        )
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
