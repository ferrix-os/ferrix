#!/usr/bin/env python3
"""Hold the line on this project's founding constraint: it is written in Rust.

The goal is not "no assembly" -- an exception vector table, a syscall entry
trampoline and a context switch cannot be expressed in Rust, because each is
defined by what the *machine* does to registers before and after the first
instruction runs. The goal is that assembly appears only there, and that adding
a new assembly site is a deliberate act somebody had to argue for in a diff.

So this is an allow-list, not a threshold. Four rules:

  1. Assembly may only appear in a file listed in `tools/common/data/asm-allowlist.json`,
     each entry carrying a `reason` for why Rust cannot express it.
  2. Each entry declares a `max_lines` budget the file must stay under, so a
     trampoline cannot quietly grow into a runtime.
  3. A stale entry fails, the way `#[expect]` does: a file that no longer
     contains assembly must lose its exemption rather than keep it warm for
     someone.
  4. The tree's total assembly, across every file, stays under
     `max_total_lines`.

Rule 4 is the one that carries the weight, and it is an *absolute* cap rather
than a percentage on purpose. A kernel's assembly is a fixed cost -- vectors,
syscall entry, context switch, the CPU primitives with no Rust spelling -- that
does not grow as the OS is built out. Capping it absolutely says exactly that:
this list is finished, and a scheduler or a filesystem must add none of it.

The Rust share is reported at every run alongside it, because that number is
the project's founding claim. Read it as a trend, not a gate: with assembly
held flat by rule 4, the percentage rises on its own as the system grows, and a
ratio ceiling that binds today would only be measuring how young the tree is.
`max_ratio` is a backstop set above where the tree sits now, ratcheted down as
it grows; `target_ratio` records where it is going.

Usage:  python3 tools/common/check/check-asm-budget.py [--json]
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import kernel_roots  # noqa: E402

# The kernel's optional parts outside src/kernel come from kernel_source_roots
# (docs/microkernel/CRATE-SPLIT.md S0), never from a literal here.
ROOTS = ("src/kernel", *kernel_roots.outside("src/kernel"), "src/boot", "src/lib", "src/user/system/native", "tools/common/xtask")

# The macros that introduce assembly. `asm!` and `naked_asm!` may be reached
# through a `core::arch::` path, so allow a qualified prefix.
ASM_MACRO = re.compile(r"(?<![\w:])(?:core::arch::)?(global_asm|naked_asm|asm)\s*!\s*[({\[]")
# Rust comment and attribute lines, excluded from the Rust source count so the
# ratio is not flattered by this tree's comment density.
RUST_SKIP = re.compile(r"^\s*(//|/\*|\*|#\[|#!\[)?\s*$|^\s*(//|///|//!)")

STANDALONE_ASM_SUFFIXES = (".s", ".S", ".asm")


RAW_STRING = re.compile(r'(?<![\w])(?:br|r)(#*)"')

# A line of a raw string that holds no instruction: blank, or an assembler
# comment only.
ASM_COMMENT = re.compile(r"^\s*(//|/\*|\*|#|;|$)")


def string_end(source: str, index: int) -> tuple[int, str] | None:
    """If a string literal starts at `index`, the index just past it and its
    kind, "raw" or "plain"; otherwise None.

    A raw string, `r"..."` or `r#"..."#`, ends at the first quote followed by
    as many hashes as it opened with, and a backslash in it escapes nothing.
    """
    raw = RAW_STRING.match(source, index)
    if raw and (index == 0 or not (source[index - 1].isalnum() or source[index - 1] == "_")):
        closing = '"' + raw.group(1)
        found = source.find(closing, raw.end())
        return (len(source) if found < 0 else found + len(closing)), "raw"
    if source[index] == '"':
        at = index + 1
        while at < len(source):
            if source[at] == "\\":
                at += 2
                continue
            if source[at] == '"':
                return at + 1, "plain"
            at += 1
        return len(source), "plain"
    return None


def balanced(source: str, opening: int) -> int:
    """Index just past the delimiter closing the one at `opening`."""
    pairs = {"(": ")", "{": "}", "[": "]"}
    open_ch = source[opening]
    close_ch = pairs[open_ch]
    depth = 0
    index = opening
    while index < len(source):
        literal = string_end(source, index)
        if literal is not None:
            index = literal[0]
            continue
        char = source[index]
        if char == open_ch:
            depth += 1
        elif char == close_ch:
            depth -= 1
            if depth == 0:
                return index + 1
        index += 1
    return len(source)


def asm_lines(source: str) -> int:
    """Lines of assembly text inside the assembly macros of one Rust file.

    A line holding an ordinary string literal counts once, however many
    literals are on it: operand lines (`in(reg) x,`) and the closing paren do
    not count. A raw string counts every line of its text that holds an
    instruction or a directive. It once counted as the one or two lines its
    quotes were on, which let a `global_asm!(r#"...")` of any length pass for
    two lines.
    """
    total = 0
    for match in ASM_MACRO.finditer(source):
        opening = match.end() - 1
        closing = balanced(source, opening)
        quoted_lines: set[int] = set()
        index = opening
        while index < closing:
            literal = string_end(source, index)
            if literal is None:
                index += 1
                continue
            end, kind = literal
            first = source.count("\n", 0, index)
            if kind == "raw":
                text = source[index:end]
                text = text[text.index('"') + 1 : text.rindex('"')]
                total += sum(1 for line in text.splitlines() if not ASM_COMMENT.match(line))
            else:
                last = source.count("\n", 0, end)
                quoted_lines.update(range(first, last + 1))
            index = end
        total += len(quoted_lines)
    return total


def rust_lines(source: str) -> int:
    """Non-blank, non-comment lines of Rust."""
    return sum(1 for line in source.splitlines() if not RUST_SKIP.match(line))


def standalone_lines(source: str) -> int:
    """Instruction lines of a hand-written `.s` file."""
    return sum(
        1
        for line in source.splitlines()
        if line.strip() and not line.strip().startswith(("//", "#", ";"))
    )


def sources(root: pathlib.Path):
    """Every source file under the surveyed roots, as (relative path, path)."""
    for name in ROOTS:
        directory = root / name
        if not directory.is_dir():
            continue
        for path in sorted(directory.rglob("*")):
            if "target" in path.parts or not path.is_file():
                continue
            kernel_roots.note_walked("check-asm-budget", [path])
            yield path.relative_to(root).as_posix(), path


def survey(root: pathlib.Path) -> tuple[dict[str, int], int]:
    """Assembly lines per file, and the tree's total Rust line count."""
    measured: dict[str, int] = {}
    total_rust = 0

    for relative, path in sources(root):
        if path.suffix in STANDALONE_ASM_SUFFIXES:
            measured[relative] = standalone_lines(path.read_text(encoding="utf-8"))
        elif path.suffix == ".rs":
            source = path.read_text(encoding="utf-8")
            total_rust += rust_lines(source)
            count = asm_lines(source)
            if count:
                measured[relative] = count

    return measured, total_rust


def check_allowlist(measured: dict[str, int], allowed: dict[str, dict]) -> list[str]:
    """Rules 1-3: only allow-listed files, inside budget, with no stale entries."""
    problems: list[str] = []

    for relative, count in sorted(measured.items()):
        entry = allowed.get(relative)
        if entry is None:
            problems.append(
                f"{relative}: contains {count} line(s) of assembly but is not in "
                "tools/common/data/asm-allowlist.json"
            )
        elif count > entry["max_lines"]:
            problems.append(
                f"{relative}: {count} lines of assembly exceeds its budget of "
                f"{entry['max_lines']}"
            )

    for relative in sorted(allowed):
        if relative not in measured:
            problems.append(
                f"{relative}: allow-listed for assembly but contains none; "
                "remove the entry rather than leaving it warm"
            )

    return problems


def main() -> int:
    parser = argparse.ArgumentParser()
    _ = parser.add_argument("--json", action="store_true", help="machine-readable report")
    args = parser.parse_args()

    root = pathlib.Path(__file__).resolve().parent.parent.parent.parent
    policy = json.loads((root / "tools" / "common" / "data" / "asm-allowlist.json").read_text(encoding="utf-8"))
    allowed = {entry["file"]: entry for entry in policy["files"]}

    measured, total_rust = survey(root)
    problems = check_allowlist(measured, allowed)

    total_asm = sum(measured.values())
    denominator = total_asm + total_rust
    ratio = (total_asm / denominator) if denominator else 0.0
    max_ratio = policy["max_ratio"]
    max_total = policy["max_total_lines"]

    # Rule 4: the absolute cap, which is the one that binds.
    if total_asm > max_total:
        problems.append(
            f"the tree contains {total_asm} lines of assembly, over the cap of "
            f"{max_total} in tools/common/data/asm-allowlist.json.\n"
            "  That list is meant to be finished. If a new construct genuinely "
            "has no Rust spelling,\n  raise the cap in the same commit that "
            "explains why."
        )
    # The backstop. Expected to pass with room; it exists so that a sudden
    # collapse in Rust line count cannot go unremarked either.
    if ratio > max_ratio:
        problems.append(
            f"assembly is {ratio * 100:.3f}% of the tree, over the "
            f"{max_ratio * 100:.3f}% backstop in tools/common/data/asm-allowlist.json"
        )

    if args.json:
        print(
            json.dumps(
                {
                    "asm_lines": total_asm,
                    "rust_lines": total_rust,
                    "ratio": ratio,
                    "max_ratio": max_ratio,
                    "max_total_lines": max_total,
                    "target_ratio": policy["target_ratio"],
                    "files": measured,
                    "problems": problems,
                },
                indent=2,
            )
        )
    else:
        print(f"asm-budget: {total_asm} lines of assembly ({max_total} allowed), "
              f"{total_rust} lines of Rust")
        target = policy["target_ratio"]
        print(
            f"            {(1 - ratio) * 100:.4f}% Rust  "
            f"(backstop {(1 - max_ratio) * 100:.4f}%, goal {(1 - target) * 100:.4f}%)"
        )
        if ratio > target:
            need = int(total_asm / target) - denominator
            print(f"            {need} more lines of Rust reach the goal at today's assembly")
        for relative, count in sorted(measured.items(), key=lambda item: -item[1]):
            entry = allowed.get(relative)
            budget = f"/{entry['max_lines']}" if entry else "  (NOT ALLOW-LISTED)"
            print(f"    {count:5}{budget:>8}  {relative}")

    if problems:
        print(file=sys.stderr)
        for problem in problems:
            print(problem, file=sys.stderr)
        print(file=sys.stderr)
        print(
            "Assembly is confined to the constructs the machine defines before "
            "the first instruction runs.\nSee docs/ASSEMBLY.md.",
            file=sys.stderr,
        )
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
