#!/usr/bin/env python3
"""Statement coverage for the kernel, measured from a QEMU boot.

`docs/sysml/11-assurance.sysml` records the reachability problem plainly: a
fuzzer cannot drive a page-fault handler and Miri cannot interpret a privileged
instruction, so `src/kernel/` is reachable only by booting it. That left the kernel
with no structural coverage measurement at all, which is the single largest
open objective for DO-178C DAL C (table A-7) and a recommended technique for
EN 50716 SIL 2.

This closes it without touching the toolchain. QEMU's `drcov` TCG plugin
records every basic block the guest translates and executes; the kernel's own
DWARF line table says which source statement each address belongs to. Intersect
the two and you have statement coverage of ring 0, per certification ring.

    cargo xtask test-boot --arch x86_64 --accel tcg   # with FERRIX_QEMU_PLUGIN
    python3 tools/common/gen/coverage-report.py \
        --drcov /tmp/ferrix-cov.drcov \
        --elf target/x86_64-unknown-none/debug/ferrix-kernel

What is being counted
---------------------

The denominator is every source line the compiler marked `is_stmt` in the DWARF
line table -- the line table's own notion of "a statement begins here", which is
what gcov-shaped tools count and the closest available analogue to the
statement coverage DAL C asks for. The numerator is those whose address fell
inside a basic block the boot executed.

Line 0 rows are compiler-generated and are not statements; they are skipped.

Two limitations, stated here rather than in a footnote, because a coverage
number people trust is worse than none if its caveats travel separately:

  * Coverage is of the profile actually booted. On an optimised build the line
    table is approximate -- inlining maps one address to several source lines,
    and a line with no instructions of its own cannot be reached. A DAL C
    submission has to either measure the configuration it ships or argue the
    mapping; this tool reports which profile it measured so the question
    cannot be quietly skipped.
  * A basic block that was translated and executed counts every statement
    inside it. That over-reports where a block was entered and left early by a
    trap. The effect is small and always in the optimistic direction, which is
    the direction worth declaring.

A suite, not one boot
---------------------

`--drcov` takes every trace of a suite and reports the union. Gates build
different kernels -- `test-shell` builds its program in, `test-vfs` its command
list -- so an address means a statement only in the build that ran it. A trace
`cargo xtask` wrote has a `<trace>.kernel` file beside it naming the ELF that
boot ran, and each trace is read against its own ELF, less its own KASLR
slide (`<trace>.slide`); a trace without a kernel file is read against
`--elf`. What is unioned is the *statement*, a source file and
line, and `--elf` alone says which statements there are to reach: the
denominator is the build that ships, and the other builds only say which of its
statements a test executed.

`cargo xtask coverage` runs the suite and calls this with `--floor`, the
ratchet: the certified item's share may not fall below the figure recorded for
the architecture in `docs/certification/coverage-floor.json`.
"""

from __future__ import annotations

import argparse
import bisect
import importlib.util
import json
import re
import struct
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent.parent

# `src/kernel/src/syscall/image.rs:` -- objdump groups the line table by source
# file and prints the path, relative to the compilation directory, as a header.
#
# Some headers carry no directory at all (`mod.rs:`), which names a file this
# script cannot place. Those must still be recognised as headers: treating one
# as an ordinary row leaves the parser attributing another file's statements to
# whichever path came before it, which silently inflates that file's coverage.
# An unplaceable header clears the current file instead, so its rows are
# skipped rather than misfiled.
#
# A header is printed only when a row's file differs from the last one
# printed, and the end of a sequence (a `-` in the line column) resets the
# line program's file to the unit's first, which objdump does not print
# again. Run wide (`-w`), it names that file once per unit, as `CU: path:`,
# and a sequence end returns to it. Before 2026-09-26 the rows after one were
# filed under whichever header came last -- `core`'s `map.rs` counted as
# lines of `arch/arm_common/gicv2.rs`, `check.rs` and `drm.rs` as `syscall/uaccess.rs`
# -- about 700 statement rows on AArch64.
FILE_HEADER = re.compile(r"^(CU: )?(\S+\.rs):$")
# `image.rs   86   0xffffffff80000110   x` -- name, line (`-` ends a
# sequence), address, then an optional view column and an `x` when the row
# begins a statement.
LINE_ROW = re.compile(r"^\S+\s+(\d+|-)\s+(0x[0-9a-fA-F]+)\s*(\d*)\s*(x?)\s*$")


def load_gate():
    """Reuse the boundary gate's classifier so both agree on the rings."""
    spec = importlib.util.spec_from_file_location(
        "boundary", ROOT / "tools" / "common" / "check" / "check-item-boundary.py"
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def read_drcov(path: Path) -> list[tuple[int, int]]:
    """Every (start, size) basic block the run executed.

    The format is a short text header, a `BB Table: N bbs` line, then N packed
    records of `uint32 start, uint16 size, uint16 mod_id`. The start is only 32
    bits wide, which for a higher-half kernel means the top half is missing;
    `reconstruct` puts it back.
    """
    blob = path.read_bytes()
    if not blob:
        raise SystemExit(
            f"{path}: empty. The plugin writes its table when QEMU exits, and a "
            f"QEMU killed rather than stopped never does."
        )
    marker = b"BB Table: "
    index = blob.find(marker)
    if index < 0:
        raise SystemExit(f"{path}: no BB table -- did the plugin run under tcg?")
    newline = blob.index(b"\n", index)
    count = int(blob[index + len(marker) : newline].split()[0])

    body = blob[newline + 1 :]
    if len(body) < count * 8:
        raise SystemExit(
            f"{path}: truncated, {len(body)} bytes for {count} blocks. A boot "
            f"killed before QEMU exited does not flush the table."
        )

    return [
        (start, size)
        for start, size, _mod in struct.iter_unpack("<IHH", body[: count * 8])
    ]


def elf_text_range(elf: Path) -> tuple[int, int]:
    """The virtual address span of the image's loadable segments."""
    output = subprocess.run(
        ["readelf", "-lW", str(elf)], capture_output=True, text=True, check=True
    ).stdout
    lows, highs = [], []
    for line in output.splitlines():
        parts = line.split()
        if len(parts) > 5 and parts[0] == "LOAD":
            vaddr = int(parts[2], 16)
            memsz = int(parts[5], 16)
            lows.append(vaddr)
            highs.append(vaddr + memsz)
    if not lows:
        raise SystemExit(f"{elf}: no LOAD segments")
    return min(lows), max(highs)


def reconstruct(low32: int, low: int, high: int) -> int | None:
    """Put back the top 32 bits drcov's record dropped.

    An image whose loadable span sits inside one 4 GiB window -- which a kernel
    linked at a fixed base does -- is uniquely identified by the bottom 32 bits
    of its addresses. Anything that does not land in the span is the loader, a
    user program or firmware, and is not this image's coverage.
    """
    candidate = (low & ~0xFFFFFFFF) | low32
    return candidate if low <= candidate < high else None


def trace_slide(trace: Path, given: int | None) -> int:
    """How far KASLR moved the kernel in the boot `trace` recorded.

    The trace holds the addresses the kernel ran at; the line table holds the
    ones it was linked at. The loader moves the image each boot and prints the
    difference (`src/boot/common/uefi/src/kaslr.rs`), and xtask writes it beside the trace as
    `<trace>.slide` (`tools/common/xtask/src/kaslr.rs`). `--slide` overrides it for every
    trace; a trace with neither is taken to be of a kernel that did not move,
    as one built `--mitigations off` or booted with `nokaslr` does not.
    """
    if given is not None:
        return given
    sidecar = trace.with_name(trace.name + ".slide")
    if sidecar.exists():
        return int(sidecar.read_text().strip(), 16)
    return 0


def read_line_table(elf: Path) -> dict[str, dict[int, list[int]]]:
    """{source path: {line: [addresses]}} for every statement row the image
    holds.

    A row outside the image's loadable span is not a statement of this image.
    The linker drops every function nothing calls out of line -- at
    `opt-level = 1` that is most small wrappers, whose every caller inlined
    them -- but leaves the function's line-table rows behind with the
    address relocated against a discarded section, which reads as a small
    offset from zero. Those rows name a line that has no instruction in the
    kernel, so no run can reach it; until 2026-09-26 they were counted as
    statements nobody reached, about a quarter of every architecture's
    residual. A line whose inlined copies are in the image keeps those rows
    and is still counted, once.
    """
    low, high = elf_text_range(elf)
    output = subprocess.run(
        ["objdump", "--dwarf=decodedline", "-w", str(elf)],
        capture_output=True,
        text=True,
        check=True,
    ).stdout

    table: dict[str, dict[int, list[int]]] = defaultdict(lambda: defaultdict(list))
    current: str | None = None
    unit: str | None = None

    for raw in output.splitlines():
        text = raw.strip()
        header = FILE_HEADER.match(text)
        if header:
            path = header.group(2)
            current = path if "/" in path else None
            if header.group(1):
                unit = current
            continue
        row = LINE_ROW.match(text)
        if not row:
            continue
        if row.group(1) == "-":
            current = unit
            continue
        if current is None:
            continue
        line = int(row.group(1))
        # Line 0 is compiler-generated; the `x` column is DWARF's is_stmt, and
        # a row without it is a continuation of a statement already counted.
        if line == 0 or row.group(4) != "x":
            continue
        address = int(row.group(2), 16)
        if not low <= address < high:
            continue
        table[current][line].append(address)

    return table


def kernel_of(trace: Path, default: Path) -> Path:
    """The ELF a trace's boot ran: its sidecar's, or `default`."""
    sidecar = trace.with_name(trace.name + ".kernel")
    if sidecar.is_file():
        return trace.parent / sidecar.read_text(encoding="utf-8").strip()
    return default


def relative(path: str) -> str | None:
    """A line-table path as the boundary gate names it; None outside the kernel."""
    if "src/kernel/src/" not in path:
        return None
    return path.split("src/kernel/src/", 1)[1]


def statements_reached(
    table: dict[str, dict[int, list[int]]], executed: list[tuple[int, int]]
) -> set[tuple[str, int]]:
    """Every (file, line) with an address inside an executed block."""

    def was_executed(address: int) -> bool:
        # The sentinel has to sort after every block that starts *at* this
        # address, so it must exceed any end. It was `1 << 62` until
        # 2026-09-26, which is below every higher-half address: on x86-64 and
        # AArch64 a block starting exactly on a statement was skipped, and
        # both read about a third lower than they were. ARMv7-A's 32-bit
        # addresses never met it, which is why it alone looked right.
        index = bisect.bisect_right(executed, (address, 1 << 65))
        # A block starting at or before this address may contain it. Blocks are
        # short, so walking back a few is cheaper than an interval tree.
        for start, end in reversed(executed[max(0, index - 64) : index]):
            if start <= address < end:
                return True
        return False

    reached: set[tuple[str, int]] = set()
    for path, lines in table.items():
        rel = relative(path)
        if rel is None:
            continue
        for line, addresses in lines.items():
            if any(was_executed(address) for address in addresses):
                reached.add((rel, line))
    return reached


def read_floor(path: Path, arch: str | None) -> float:
    """The floor `path` records for `arch`."""
    floors = json.loads(path.read_text(encoding="utf-8"))
    if arch is None or arch not in floors.get("item", {}):
        raise SystemExit(f"{path}: no floor recorded for --arch {arch}")
    return float(floors["item"][arch])



# The cargo profiles a measured kernel can come from, by the directory cargo
# builds it into (target/<triple>/<profile>/ferrix-kernel). `cargo xtask
# coverage` keeps each boot's kernel as ferrix-kernel-<profile>-<digest>.elf.
PROFILES = ("debug", "coverage", "release", "iterate")
KEPT_KERNEL = re.compile(r"ferrix-kernel-(" + "|".join(PROFILES) + r")-[0-9a-f]{16}\.elf")


def profile_of(elf: Path) -> str:
    """The cargo profile that built `elf`, from its path, or exit naming it.

    Until 2026-10-15 any path without `/release/` was reported as the debug
    profile, so a kept copy of a release kernel, whose name says nothing,
    was published as debug. A path that names no profile is refused now, not
    guessed (the certification consultant's ruling, ledger line 1472, B2).
    """
    if elf.parent.name in PROFILES:
        return elf.parent.name
    kept = KEPT_KERNEL.fullmatch(elf.name)
    if kept:
        return kept.group(1)
    raise SystemExit(
        f"{elf}: cannot tell which cargo profile built it: give the ELF where "
        f"cargo wrote it (target/<triple>/<profile>/ferrix-kernel, profile one "
        f"of {', '.join(PROFILES)}) or as `cargo xtask coverage` keeps it "
        f"(ferrix-kernel-<profile>-<16 hex>.elf)"
    )


def one_profile(elves) -> str:
    """The one profile every ELF in `elves` was built in, or exit: a union of
    traces of different profiles' kernels is no one build's coverage."""
    profiles = {profile_of(elf) for elf in elves}
    if len(profiles) != 1:
        raise SystemExit(
            "the traces ran kernels of different profiles: " + ", ".join(sorted(profiles))
        )
    return profiles.pop()


def format_lines(numbers) -> str:
    """`1-3, 7`: the form carry-coverage.py reads and writes."""
    numbers = sorted(set(numbers))
    parts, i = [], 0
    while i < len(numbers):
        j = i
        while j + 1 < len(numbers) and numbers[j + 1] == numbers[j] + 1:
            j += 1
        parts.append(str(numbers[i]) if i == j else f"{numbers[i]}-{numbers[j]}")
        i = j + 1
    return ", ".join(parts)

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--drcov",
        type=Path,
        required=True,
        nargs="+",
        help="one or more traces; coverage is the union, as a test suite's is",
    )
    parser.add_argument("--elf", type=Path, required=True)
    parser.add_argument(
        "--slide",
        type=lambda text: int(text, 16),
        default=None,
        help="how far KASLR moved the kernel, in hex, for every trace; by "
        "default each trace's own `<trace>.slide`, or 0 without one",
    )
    parser.add_argument("--json", type=Path, help="write the per-file detail here")
    parser.add_argument(
        "--residual",
        type=Path,
        help="write every unreached statement in the item here, by file and "
        "line. DO-178C wants each one either driven by a test or justified "
        "as unreachable, and neither can start from a percentage.",
    )
    parser.add_argument(
        "--min-item",
        type=float,
        default=None,
        help="fail below this percent covered in the certified item",
    )
    parser.add_argument(
        "--floor",
        type=Path,
        help="as --min-item, with the figure this file records for --arch",
    )
    parser.add_argument(
        "--arch",
        help="the architecture measured, recorded in --json and --residual",
    )
    args = parser.parse_args()
    if args.floor is not None:
        args.min_item = read_floor(args.floor, args.arch)

    gate = load_gate()
    manifest = gate.load_manifest()
    ring_of, _, _ = gate.classify(manifest, gate.kernel_files())

    # Each trace with the ELF its boot ran: the one its sidecar names, or
    # `--elf`. Grouped, so that each build's line table is read once.
    by_elf: dict[Path, list[Path]] = defaultdict(list)
    for trace in args.drcov:
        by_elf[kernel_of(trace, args.elf)].append(trace)
    profile = one_profile([args.elf, *by_elf])

    # The union across traces: a statement reached by any test in the suite is
    # a covered statement, which is how every coverage tool aggregates a run.
    # Keyed by (file, line) rather than by address, because the builds differ.
    reached: set[tuple[str, int]] = set()
    blocks_executed = 0
    blocks_in_image = 0
    table = None
    for elf, traces in by_elf.items():
        low, high = elf_text_range(elf)
        ranges: set[tuple[int, int]] = set()
        for trace in traces:
            found = read_drcov(trace)
            blocks_executed += len(found)
            # Reconstructed against where the image ran, then moved back to
            # where it was linked, which is what the line table is in.
            slide = trace_slide(trace, args.slide)
            for low32, size in found:
                start = reconstruct(low32, low + slide, high + slide)
                if start is not None:
                    start -= slide
                    ranges.add((start, start + max(size, 1)))
            print(
                f"coverage: {trace.name}: {len(found)} blocks, slide {slide:#x}, "
                f"kernel {elf.name}"
            )
        blocks_in_image += len(ranges)
        elf_table = read_line_table(elf)
        reached |= statements_reached(elf_table, sorted(ranges))
        if elf.resolve() == args.elf.resolve():
            table = elf_table

    if table is None:
        table = read_line_table(args.elf)

    totals: dict[str, dict[str, int]] = defaultdict(lambda: {"total": 0, "hit": 0})
    per_file: dict[str, dict[str, int]] = {}
    residual: dict[str, dict] = {}
    verification: dict[str, dict] = {}

    for path, lines in table.items():
        rel = relative(path)
        if rel is None:
            continue
        ring = ring_of.get(rel)
        if ring is None:
            continue
        kind = "test" if gate.is_test_file(rel, manifest) else "product"
        if kind == "test":
            # A check's own statements are not the item's coverage, and are
            # counted apart: they say which checks this run executed, the
            # run-time half of docs/certification/TRACEABILITY.md.
            verification[rel] = {
                "ring": ring,
                "reached": format_lines(line for line in lines if (rel, line) in reached),
            }
            continue

        total = hit = 0
        missed: list[int] = []
        for line in lines:
            total += 1
            if (rel, line) in reached:
                hit += 1
            else:
                missed.append(line)

        totals[ring]["total"] += total
        totals[ring]["hit"] += hit
        per_file[rel] = {"ring": ring, "total": total, "hit": hit}
        if missed:
            residual[rel] = {"ring": ring, "lines": sorted(missed)}

    print(f"coverage: {blocks_executed} basic blocks executed, "
          f"{blocks_in_image} inside the kernel images ({profile} profile)")
    print("coverage: statements reached, by certification ring")

    item_total = item_hit = 0
    for ring in ("core", "item", "load"):
        total = totals[ring]["total"]
        hit = totals[ring]["hit"]
        if not total:
            continue
        if ring in ("core", "item"):
            item_total += total
            item_hit += hit
        print(f"  {ring:<6} {hit:>6} / {total:<6} {100.0 * hit / total:5.1f}%")

    if item_total:
        share = 100.0 * item_hit / item_total
        print(f"  {'':<6} {'-' * 20}")
        print(f"  certified item  {item_hit} / {item_total}  {share:.1f}%")

    if args.json:
        args.json.write_text(
            json.dumps(
                {
                    "arch": args.arch,
                    "profile": profile,
                    "traces": len(args.drcov),
                    "blocks_executed": blocks_executed,
                    "blocks_in_image": blocks_in_image,
                    "rings": {k: dict(v) for k, v in totals.items()},
                    "files": per_file,
                    "verification": dict(sorted(verification.items())),
                },
                indent=2,
            )
            + "\n"
        )
        print(f"coverage: per-file detail in {args.json}")

    if args.residual:
        inside = {k: v for k, v in residual.items() if v["ring"] in ("core", "item")}
        count = sum(len(v["lines"]) for v in inside.values())
        args.residual.write_text(
            json.dumps(
                {
                    "//": [
                        "Every statement in the certified item that the measured",
                        "runs did not reach. DO-178C table A-7 wants each either",
                        "covered by a requirements-based test or justified as",
                        "unreachable defensive code; this is the list that work",
                        "starts from. A percentage cannot be argued with.",
                    ],
                    "arch": args.arch,
                    "profile": profile,
                    "unreached": count,
                    "files": dict(sorted(inside.items(), key=lambda kv: -len(kv[1]["lines"]))),
                },
                indent=2,
            )
            + "\n"
        )
        print(f"coverage: {count} unreached statements listed in {args.residual}")

    if args.min_item is not None and item_total:
        if share < args.min_item:
            print(
                f"coverage: {share:.1f}% of the certified item, below the "
                f"{args.min_item:.1f}% this gate requires",
                file=sys.stderr,
            )
            return 1
        print(f"coverage: at or above the {args.min_item:.1f}% floor")

    return 0


if __name__ == "__main__":
    sys.exit(main())
