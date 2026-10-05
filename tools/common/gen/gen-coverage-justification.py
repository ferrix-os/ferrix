#!/usr/bin/env python3
"""Sort the uncovered statements into the categories DO-178C asks about.

Table A-7 wants every statement either exercised by a requirements-based test
or justified as unreachable. `tools/common/gen/coverage-report.py --residual` produces
the list, one per architecture; this sorts it, because a list of line numbers
is a number and not an argument.

The categories, in the order the analysis applies them:

  other-architecture   Code for an architecture or a board the measured run
                       was not. `iommu/smmuv3.rs` is AArch64's IOMMU and no
                       x86-64 run can reach it. **Legitimately unreachable,
                       per configuration** -- and the same statements are
                       ordinary covered code on the architecture that owns
                       them, which is why the justification has to be made per
                       configuration rather than once.

  failure-path         Reached only when the kernel is stopping: the panic
                       report, the catalogue, the backtrace walker.
                       Legitimately unreachable in a passing run, and testing
                       it means deliberately crashing.

  absent-hardware      Enumeration and setup for devices the measured machine
                       does not have. Unreachable *on this machine*, which is
                       weaker than the first category: a different QEMU
                       invocation would reach some of it.

  needs-a-test         Everything else. This is the real work, and it is the
                       number that has to reach zero.

The first two are arguments. The third is a configuration statement. Only the
fourth is a gap, and separating them is the whole point: a single percentage
cannot be argued with and these four numbers can.

The file lists sort whole files. A statement in a needs-a-test file is moved
out of the gap by an argument of its own, in `coverage-argued-<arch>.json`:

  {"file": "arch/x86_64/mod.rs", "lines": "1232-1234",
   "category": "defensive", "match": "for _ in 0..10 {",
   "why": "what would have to go wrong for this to run, and why it cannot
           be made to on the measured machine"}

`category` is one of the four above, `defensive` -- reached only when
hardware or an invariant has already failed, which a passing run cannot
show -- or `credited-elsewhere`: a statement a test runs whose only row in
the line table is in an inlined copy that cannot run it. `match` is text the first line must contain, so that an argument
cannot drift onto another statement when the file changes. Each argued
line must be in the residual: an argument for a line a test now reaches, or
one that no longer exists, is stale and `--check` fails, as the boundary
gate does for its debt register. An architecture without the file has no
line arguments.

Two documents come out:

  COVERAGE-RESIDUAL.md  the four categories on each architecture.
  COVERAGE-WORKLIST.md  the fourth category alone, grouped by module, with
                        each file's count on every architecture: the list
                        test-writing starts from, one module at a time,
                        and then what the next run owes: the rows of
                        coverage-owed.json, code a landing changed after
                        the last measurement.

    python3 tools/common/gen/gen-coverage-justification.py
    python3 tools/common/gen/gen-coverage-justification.py --check
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent.parent
CERT = ROOT / "docs" / "certification"
KERNEL = ROOT / "src" / "kernel" / "src"
OUTPUT = CERT / "COVERAGE-RESIDUAL.md"
WORKLIST = CERT / "COVERAGE-WORKLIST.md"
OWED = CERT / "coverage-owed.json"

ARCHES = ("x86_64", "aarch64", "armv7a")


def residual_path(arch: str) -> Path:
    return CERT / f"coverage-residual-{arch}.json"


def coverage_path(arch: str) -> Path:
    return CERT / f"coverage-{arch}.json"


def argued_path(arch: str) -> Path:
    return CERT / f"coverage-argued-{arch}.json"


def parse_lines(text: str) -> list[int]:
    """`"1232-1234, 1240"` as `[1232, 1233, 1234, 1240]`."""
    out: list[int] = []
    for part in text.split(","):
        part = part.strip()
        if "-" in part:
            low, high = part.split("-", 1)
            out.extend(range(int(low), int(high) + 1))
        elif part:
            out.append(int(part))
    return out


def load_arguments(arch: str, residual: dict) -> tuple[dict, list[dict], list[str]]:
    """The architecture's line arguments: `{(file, line): category}`, the
    arguments themselves, and every reason one of them is stale or malformed.
    """
    path = argued_path(arch)
    if not path.is_file():
        return {}, [], []
    arguments = json.loads(path.read_text(encoding="utf-8"))["arguments"]
    argued: dict[tuple[str, int], str] = {}
    problems: list[str] = []
    for index, argument in enumerate(arguments):
        where = f"{path.relative_to(ROOT)} #{index} ({argument.get('file')} {argument.get('lines')})"
        file = argument.get("file", "")
        category = argument.get("category")
        if category not in LINE_CATEGORIES:
            problems.append(f"{where}: category {category!r} is not one of {LINE_CATEGORIES}")
            continue
        if not argument.get("why", "").strip():
            problems.append(f"{where}: no argument given")
        lines = parse_lines(argument.get("lines", ""))
        unreached = set(residual["files"].get(file, {}).get("lines", []))
        stale = [line for line in lines if line not in unreached]
        if not lines or stale:
            problems.append(
                f"{where}: {ranges(stale) or 'no lines'} not in the residual -- "
                f"reached now, or moved; delete or re-aim the argument"
            )
        source = KERNEL / file
        text = source.read_text(encoding="utf-8").splitlines() if source.is_file() else []
        first = text[lines[0] - 1] if lines and 0 < lines[0] <= len(text) else ""
        if argument.get("match", "") not in first or not argument.get("match"):
            problems.append(f"{where}: line {lines[0] if lines else '?'} does not contain {argument.get('match')!r}")
        for line in lines:
            argued[(file, line)] = category
    return argued, arguments, problems


# Files belonging to an architecture other than the measured one, or to a
# board the measured machine is not. Only what the build compiles for the
# measured architecture can appear in its residual, so each list names the
# generic-path code that belongs elsewhere: another architecture's IOMMU, a
# board's UART, the Pixel 7's SoC.
OTHER_ARCH_MARKERS = {
    "x86_64": (
        "arch/aarch64/",
        "arch/armv7a/",
        "arch/arm_common/",
        "platform/google/gs201/",
        "iommu/smmuv3.rs",
        "stm32mp1",
    ),
    "aarch64": (
        "arch/x86_64/",
        "arch/armv7a/",
        "arch/arm_common/stm32_usart.rs",
        "platform/google/gs201/watchdog.rs",
        "iommu/vtd.rs",
        "stm32mp1",
    ),
    "armv7a": (
        "arch/x86_64/",
        "arch/aarch64/",
        "arch/arm_common/stm32_usart.rs",
        "platform/google/gs201/",
        "iommu/vtd.rs",
        "stm32mp1",
    ),
}
FAILURE_PATH = ("panic.rs", "panic/", "backtrace.rs")
ABSENT_HARDWARE_COMMON = ("device.rs", "pci.rs", "pci/", "acpi.rs", "fdt.rs")
# Per architecture, the unit QEMU's machine could present and this one does
# not: x86-64's VT-d is attached but its fault paths are not provoked, and
# ARMv7-A's `virt` is given no SMMU at all. AArch64 has none: the suite boots
# its `virt` with the default GICv2 and once more with a GICv3 and its ITS.
ABSENT_HARDWARE_ARCH = {
    "x86_64": ("iommu/vtd.rs",),
    "aarch64": (),
    "armv7a": ("iommu/smmuv3.rs",),
}


def categorise(path: str, arch: str) -> str:
    if any(marker in path for marker in OTHER_ARCH_MARKERS[arch]):
        return "other-architecture"
    if any(path.endswith(m) or m in path for m in FAILURE_PATH):
        return "failure-path"
    absent = ABSENT_HARDWARE_COMMON + ABSENT_HARDWARE_ARCH[arch]
    if any(path.endswith(m) or path.startswith(m) for m in absent):
        return "absent-hardware"
    return "needs-a-test"


HEADINGS = {
    "other-architecture": (
        "Unreachable on the measured architecture",
        "Justified. These statements belong to another architecture or another "
        "board, and no run on {arch} can reach them. The same code is ordinary "
        "covered code where it belongs, so this justification is per "
        "configuration and the other architectures owe their own.",
    ),
    "failure-path": (
        "Reached only when the kernel is stopping",
        "Justified. The panic report, its catalogue and the backtrace walker "
        "run when the kernel has already decided to stop. Exercising them means "
        "crashing deliberately, which only `test-shell`'s `ferrix.onexit=panic` "
        "boot does -- and a passing run that reached the rest would be a "
        "failing run.",
    ),
    "defensive": (
        "Reached only when something has already failed",
        "Justified, line by line. Each of these runs only when hardware "
        "misbehaves or an invariant the rest of the kernel keeps has broken: "
        "a counter that never advances, a reset that did not reset, a table "
        "that lost an entry it was just given. A passing run cannot show one "
        "without first breaking what it defends against, and removing it "
        "would leave the failure unhandled. Each argument below says what "
        "would have to go wrong.",
    ),
    "credited-elsewhere": (
        "Run, and credited to another line",
        "Justified, line by line. The statement runs, and a test shows what it "
        "does, but the line table gives it a statement row only in an inlined "
        "copy that cannot execute it; the copy that does run carries its "
        "instructions under another line's row. No run can credit the line, "
        "and no test could make one. Each argument names the test and the row "
        "that carries it.",
    ),
    "absent-hardware": (
        "Hardware the measured machine does not have",
        "**Not a justification, a configuration statement.** Enumeration and "
        "setup for devices this QEMU invocation does not present. A different "
        "machine would reach some of it, so the honest closure is either to "
        "measure on a machine that has the hardware or to state which devices "
        "the claim excludes.",
    ),
    "needs-a-test": (
        "Needs a test",
        "**The real gap.** No argument covers these; they are reachable on the "
        "measured configuration and nothing exercised them. This is the number "
        "that has to reach zero for DO-178C table A-7 objective 5. "
        "[COVERAGE-WORKLIST.md](COVERAGE-WORKLIST.md) groups them by module.",
    ),
}

ARGUED = ("other-architecture", "failure-path", "defensive", "credited-elsewhere")
LINE_CATEGORIES = tuple(HEADINGS)

# {arch: {(file, line): category}}, from `coverage-argued-<arch>.json`;
# `main` fills it before anything is rendered.
LINE_ARGUMENTS: dict[str, dict[tuple[str, int], str]] = {}
# {arch: the arguments as written}, for the line-by-line tables.
ARGUMENTS: dict[str, list[dict]] = {}


def line_category(path: str, line: int, arch: str) -> str:
    """A line's own argument if it has one, else its file's category."""
    return LINE_ARGUMENTS.get(arch, {}).get((path, line)) or categorise(path, arch)


def buckets_of(residual: dict, arch: str) -> dict[str, dict[str, int]]:
    buckets: dict[str, dict[str, int]] = {name: {} for name in HEADINGS}
    for path, entry in residual["files"].items():
        for line in entry["lines"]:
            bucket = buckets[line_category(path, line, arch)]
            bucket[path] = bucket.get(path, 0) + 1
    return buckets


def render(residuals: dict[str, dict]) -> str:
    lines = [
        "# Coverage residual",
        "",
        "*Generated by `tools/common/gen/gen-coverage-justification.py`. Do not edit.*",
        "",
        "The statements in the certified item that the measured suite did not "
        "reach, on each architecture, sorted into the categories DO-178C table "
        "A-7 asks about. The suite is every boot gate `cargo xtask coverage` "
        "runs on that architecture; see VERIFICATION.md §3.",
        "",
        "| Architecture | Profile | Unreached | Argued | Hardware absent | Needs a test |",
        "|---|---|---:|---:|---:|---:|",
    ]
    for arch, residual in residuals.items():
        buckets = buckets_of(residual, arch)
        argued = sum(sum(buckets[n].values()) for n in ARGUED)
        absent = sum(buckets["absent-hardware"].values())
        gap = sum(buckets["needs-a-test"].values())
        lines.append(
            f"| {arch} | {residual['profile']} | {residual['unreached']} | "
            f"{argued} | {absent} | **{gap}** |"
        )
    lines += [
        "",
        "*Argued* is the first four categories below; *hardware absent* is a "
        "statement about which machine was measured rather than an argument; "
        "*needs a test* is the gap.",
        "",
    ]

    for arch, residual in residuals.items():
        buckets = buckets_of(residual, arch)
        total = residual["unreached"]
        lines += [
            "---",
            "",
            f"## {arch}",
            "",
            f"**{total}** unreached statements, {residual['profile']} profile.",
            "",
            "| Category | Statements | Share |",
            "|---|---:|---:|",
        ]
        for name in HEADINGS:
            count = sum(buckets[name].values())
            lines.append(
                f"| {HEADINGS[name][0]} | {count} | "
                f"{100.0 * count / (total or 1):.0f}% |"
            )
        lines.append("")

        for name, (heading, rationale) in HEADINGS.items():
            entries = sorted(buckets[name].items(), key=lambda kv: (-kv[1], kv[0]))
            count = sum(buckets[name].values())
            lines += [
                f"### {arch}: {heading} — {count} statements",
                "",
                rationale.format(arch=arch),
                "",
            ]
            if not entries:
                lines += ["None.", ""]
                continue
            lines += ["| Statements | Ring | File |", "|---:|---|---|"]
            for path, n in entries[:25]:
                ring = residual["files"][path]["ring"]
                lines.append(f"| {n} | `{ring}` | `{path}` |")
            if len(entries) > 25:
                rest = sum(n for _, n in entries[25:])
                lines.append(f"| {rest} | | *and {len(entries) - 25} more files* |")
            lines.append("")

        lines += render_arguments(arch)

    lines += [
        "---",
        "",
        "## What this does not do",
        "",
        "It sorts by file, and by line only where an argument has been "
        "written. A file in *needs-a-test* may hold individual lines that are "
        "genuinely unreachable -- a defensive `else` on an invariant the type "
        "system already forces -- until someone argues them in "
        "`coverage-argued-<arch>.json`, and a file in a justified category may "
        "hold a line that is not. Closing F-10 means walking the gap line by "
        "line; this says which lines to walk and which not to bother with, "
        "which is the part a percentage could not.",
        "",
    ]
    return "\n".join(lines) + "\n"


def render_arguments(arch: str) -> list[str]:
    """The architecture's line arguments, one row each."""
    arguments = ARGUMENTS.get(arch, [])
    if not arguments:
        return []
    count = sum(len(parse_lines(a["lines"])) for a in arguments)
    out = [
        f"### {arch}: argued line by line — {count} statements",
        "",
        f"From `coverage-argued-{arch}.json`. Each row is one argument, for "
        "the lines it names and no others; the category is the one it is "
        "counted in above.",
        "",
        "| File | Lines | Category | Why it is not reached |",
        "|---|---|---|---|",
    ]
    for argument in sorted(arguments, key=lambda a: (a["file"], parse_lines(a["lines"])[0])):
        why = " ".join(argument["why"].split()).replace("|", "\\|")
        out.append(
            f"| `{argument['file']}` | {argument['lines']} | "
            f"{HEADINGS[argument['category']][0]} | {why} |"
        )
    out.append("")
    return out


def module_of(path: str) -> str:
    """The module a file belongs to: `iommu.rs` and `iommu/vtd.rs` are both
    `iommu`, `arch/x86_64/cpu.rs` is `arch/x86_64`, and a file of its own is
    its own module."""
    parts = path.split("/")
    if parts[0] == "arch":
        return "/".join(parts[:2]) if len(parts) > 2 else "arch"
    return parts[0].removesuffix(".rs")


def ranges(numbers: list[int]) -> str:
    """`[1, 2, 3, 7]` as `1-3, 7`."""
    out: list[str] = []
    numbers = sorted(numbers)
    start = previous = None
    for n in numbers:
        if start is None:
            start = previous = n
        elif n == previous + 1:
            previous = n
        else:
            out.append(f"{start}" if start == previous else f"{start}-{previous}")
            start = previous = n
    if start is not None:
        out.append(f"{start}" if start == previous else f"{start}-{previous}")
    return ", ".join(out)


def load_owed() -> tuple[list[dict], list[str]]:
    """What the next coverage run owes (coverage-owed.json), and every reason
    an entry is malformed."""
    if not OWED.is_file():
        return [], []
    owed = json.loads(OWED.read_text(encoding="utf-8")).get("owed", [])
    problems: list[str] = []
    for index, entry in enumerate(owed):
        where = f"{OWED.relative_to(ROOT)} #{index}"
        for field in ("landing", "dropped", "what"):
            if not str(entry.get(field, "")).strip():
                problems.append(f"{where}: no {field!r} given")
        arches = entry.get("architectures", [])
        if not arches or any(arch not in ARCHES for arch in arches):
            problems.append(f"{where}: 'architectures' must name some of {ARCHES}")
    return owed, problems


def render_owed(owed: list[dict]) -> list[str]:
    """The worklist's first section: what the next run owes, one row per
    landing, or nothing when nothing is owed."""
    if not owed:
        return []
    lines = [
        "## Owed at the next coverage run",
        "",
        "Code changed after the last measurement, listed in "
        "`coverage-owed.json`. Its lines are unmeasured, so F-10's percentages "
        "are not re-claimed for it until the run that deletes its row.",
        "",
        "| Landing | Architectures | Dropped by the carry | Owed |",
        "|---|---|---|---|",
    ]
    for entry in owed:
        cells = [
            entry["landing"],
            ", ".join(entry["architectures"]),
            entry["dropped"],
            entry["what"],
        ]
        lines.append("| " + " | ".join(cell.replace("|", "\\|") for cell in cells) + " |")
    return lines + [""]


def render_worklist(
    residuals: dict[str, dict], coverage: dict[str, dict], owed: list[dict]
) -> str:
    # {file: {arch: [lines]}} for the needs-a-test category only, with an
    # empty list where the architecture compiles the file, counts it as a gap
    # and reached all of it: that architecture has nothing left to close, and
    # a statement it reached is not unreached "everywhere".
    gap: dict[str, dict[str, list[int]]] = {}
    rings: dict[str, str] = {}
    for arch, measured in coverage.items():
        unreached = residuals[arch]["files"]
        for path, entry in measured["files"].items():
            if entry["ring"] not in ("core", "item"):
                continue
            if categorise(path, arch) != "needs-a-test":
                continue
            gap.setdefault(path, {})[arch] = [
                line
                for line in unreached.get(path, {}).get("lines", [])
                if line_category(path, line, arch) == "needs-a-test"
            ]
            rings[path] = entry["ring"]
    gap = {path: arches for path, arches in gap.items() if any(arches.values())}

    modules: dict[str, list[str]] = {}
    for path in gap:
        modules.setdefault(module_of(path), []).append(path)

    def everywhere(path: str) -> set[int]:
        """Lines unreached on every architecture that counts the file a gap."""
        sets = [set(v) for v in gap[path].values()]
        return set.intersection(*sets) if sets else set()

    def module_total(module: str, arch: str) -> int:
        return sum(len(gap[p].get(arch, [])) for p in modules[module])

    totals = {arch: sum(module_total(m, arch) for m in modules) for arch in residuals}
    order = sorted(
        modules,
        key=lambda m: (-max(module_total(m, a) for a in residuals), m),
    )

    head = " | ".join(residuals)
    rule = "|".join("---:" for _ in residuals)
    lines = [
        "# Coverage worklist",
        "",
        "*Generated by `tools/common/gen/gen-coverage-justification.py`. Do not edit.*",
        "",
        "The *needs a test* category of [COVERAGE-RESIDUAL.md](COVERAGE-RESIDUAL.md), "
        "grouped by module so that each module can be taken as one piece of "
        "work. The argued categories -- another architecture's code, the "
        "failure path, hardware the machine does not present -- are not here; "
        "their argument is in COVERAGE-RESIDUAL.md.",
        "",
        "Counts are statements unreached by the whole suite on that "
        "architecture. A dash is a file the architecture does not compile, or "
        "one whose residual there is argued rather than a gap; a zero is one "
        "it reached in full. *Everywhere* is the statements unreached on every "
        "architecture that counts the file a gap -- the ones a single generic "
        "test would close on all of them at once -- and its lines are listed, "
        "so a test can be aimed without re-running anything. The "
        "per-architecture lines are in `coverage-residual-<arch>.json`.",
        "",
        "**Taking a module.** Write the test on the architecture with the "
        "largest count first, re-run `cargo xtask coverage --arch <arch>`, "
        "regenerate the evidence (VERIFICATION.md §3.3) and raise the floor in "
        "`coverage-floor.json`. A line that turns out to be unreachable "
        "defensive code needs its argument written, not a test.",
        "",
        f"| Module | {head} | Files |",
        f"|---|{rule}|---:|",
    ]
    for module in order:
        counts = " | ".join(str(module_total(module, a) or "-") for a in residuals)
        lines.append(f"| [`{module}`](#{anchor(module)}) | {counts} | {len(modules[module])} |")
    total_row = " | ".join(f"**{totals[a]}**" for a in residuals)
    lines += [f"| **Total** | {total_row} | {len(gap)} |", ""]
    if owed:
        lines += ["---", "", *render_owed(owed)]

    for module in order:
        lines += [
            "---",
            "",
            f"## `{module}`",
            "",
            f"| File | Ring | {head} | Everywhere | Lines unreached everywhere |",
            f"|---|---|{rule}|---:|---|",
        ]
        files = sorted(
            modules[module],
            key=lambda p: (-max(len(gap[p].get(a, [])) for a in residuals), p),
        )
        for path in files:
            counts = " | ".join(
                str(len(gap[path][a])) if a in gap[path] else "-" for a in residuals
            )
            common = everywhere(path)
            lines.append(
                f"| `{path}` | `{rings[path]}` | {counts} | {len(common)} | "
                f"{ranges(list(common)) or '-'} |"
            )
        lines.append("")
    return "\n".join(lines) + "\n"


def anchor(module: str) -> str:
    """The anchor GitHub gives a `## \\`module\\`` heading."""
    return "".join(c for c in module.lower() if c.isalnum() or c in "-_")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()

    residuals: dict[str, dict] = {}
    coverage: dict[str, dict] = {}
    for arch in ARCHES:
        for path, into in ((residual_path(arch), residuals), (coverage_path(arch), coverage)):
            if not path.is_file():
                print(
                    f"gen-coverage-justification: {path.relative_to(ROOT)} is missing.\n"
                    f"  Produce it with `coverage-report.py --json --residual`.",
                    file=sys.stderr,
                )
                return 1
            into[arch] = json.loads(path.read_text(encoding="utf-8"))

    # Malformed or stale line arguments fail the generator as well as the
    # check: a document rendered from them would argue lines that are not
    # the ones it names.
    problems: list[str] = []
    for arch in ARCHES:
        argued, arguments, found = load_arguments(arch, residuals[arch])
        LINE_ARGUMENTS[arch] = argued
        ARGUMENTS[arch] = arguments
        problems += found
    owed, found = load_owed()
    problems += found
    for problem in problems:
        print(f"gen-coverage-justification: {problem}", file=sys.stderr)
    if problems:
        print(
            "gen-coverage-justification: if a change to the kernel moved these lines, carry the\n"
            "  evidence's anchors with `python3 tools/common/gen/carry-coverage.py` and run the generator.",
            file=sys.stderr,
        )
        return 1

    outputs = {
        OUTPUT: render(residuals),
        WORKLIST: render_worklist(residuals, coverage, owed),
    }

    if args.check:
        # The anchors themselves: a kernel change since the evidence was
        # last written that nobody carried (finding F-62). Every page above
        # can be current and still name the wrong lines.
        carried = subprocess.run(
            [sys.executable, str(ROOT / "tools" / "common" / "gen" / "carry-coverage.py"), "--check"],
            cwd=ROOT,
        )
        if carried.returncode:
            return 1
        stale = [
            path
            for path, rendered in outputs.items()
            if not path.exists() or path.read_text(encoding="utf-8") != rendered
        ]
        for path in stale:
            print(
                "gen-coverage-justification: "
                f"{path.relative_to(ROOT)} is stale. Run the generator.",
                file=sys.stderr,
            )
        if stale:
            return 1
        print("gen-coverage-justification: current")
        return 0

    for path, rendered in outputs.items():
        path.write_text(rendered, encoding="utf-8")
        print(f"gen-coverage-justification: wrote {path.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
