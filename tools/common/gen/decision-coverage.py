#!/usr/bin/env python3
"""Decision (branch) coverage of the kernel's object code, from the drcov
traces `coverage-report.py` already reads.

DO-178C table A-7 objective 6 (DAL B and above) asks that every decision in
the software has taken every outcome. `coverage-report.py` answers objective 5,
statement coverage, from QEMU's `drcov` plugin: the basic blocks a boot
executed, read against the DWARF line table. The same traces answer a
narrower question about decisions, without instrumenting anything:

    For each conditional branch instruction in the kernel image, did control
    leave it both ways?

QEMU ends a translation block at every conditional branch, and control that
leaves one enters the next block *at its first instruction*: the branch's
target if it was taken, the instruction after it if it was not. The plugin
records the address every executed block starts at. So a branch took its
"taken" outcome if some block starts at its target, and its "not taken"
outcome if some block starts at its fall-through.

    python3 tools/common/gen/decision-coverage.py --arch x86_64 \\
        --elf build/coverage/x86_64/ferrix-kernel-<hash>.elf \\
        --drcov build/coverage/x86_64/*.drcov

`cargo xtask coverage` runs it after the statement report, over the same
traces and the same reference ELF.

What is counted
---------------

A *branch* is a direct conditional branch in the image's code:

  x86-64    every `jcc`, and `jrcxz`/`loop` should the compiler emit them
  AArch64   `b.<cond>`, `cbz`/`cbnz`, `tbz`/`tbnz`
  ARMv7-A   `b<cond>`, and `cbz`/`cbnz` should Thumb code appear

A conditional *call* (`bl<cond>`) or a conditional *indirect* branch
(`bx<cond> lr`, `pop<cond> {..., pc}`) has an outcome the block starts cannot
tell apart -- a call returns to its fall-through either way, and an indirect
target is not in the instruction -- so each is counted and listed apart as
not measured, never as covered.

Each branch is attributed to the file and line the line table gives its
address -- the innermost source, so a branch of an inlined kernel function is
that function's, not its caller's -- and to a ring by the boundary gate's
classifier, as a statement is. A branch in a file outside `src/kernel/src`
(`core`, `alloc`, the `src/lib/` crates) is outside the item, as its statements
are; the report says how many there were.

The unit is the *object-code* branch. A source decision the compiler copied --
inlined into six callers, or monomorphised twice -- is six or two branches,
and each has to take both ways. A source decision the compiler turned into a
`cmov`, `csel` or a predicated ARM instruction is no branch at all, and is not
counted. This is branch coverage of object code, not source decision coverage
and not MC/DC; VERIFICATION.md section 3.6 argues what that is worth.

Each branch is sorted into one of five:

  both          both successors began a block
  taken         only the target did
  not-taken     only the fall-through did
  no-outcome    a block containing the branch ran, and neither successor began
                one: the boot ended there, a fault left the block before the
                branch, or the method has missed something. Expected to be
                close to zero, and printed, because it is the method's own
                check
  unreached     no executed block contains the branch

Guards
------

A branch one of whose successors runs, with no other decision or call first,
into a function that panics -- `core::panicking`, `unwrap_failed`, a slice
index failure, `handle_alloc_error`, the kernel's `fatal!` -- is a *guard*:
an overflow or bounds check, an `unwrap`, a `debug_assert!`, a precondition
`core` checks in a debug build, a `fatal!`. A passing run never takes its
panicking way, so no passing suite can take it both ways, and the report
gives the item's figure with guards and without them. The guards are the
object-code counterpart of the statement residual's "reached only when
something has already failed".

Two figures and a view
----------------------

  both ways   a block began at each successor. An upper bound: a block can
              begin at a successor for a reason other than this branch --
              another conditional branch's fall-through, a jump, a call's
              return or a jump table can enter the same address.
  sure        each of those blocks is this branch's doing: every *other*
              instruction with a known edge into that successor -- a branch
              or jump to it, a conditional branch it follows, a call it
              returns from -- lies in no executed block. A lower bound, short
              only of an indirect jump, or an interrupt or exception return,
              landing there.

  The truth lies between the two. drcov records blocks, not the edges between
  them, so nothing in its traces can narrow it further; QEMU's `cflow` plugin
  records edges and would.

  by source   a decision is (file, line, its order among the branches at that
              line in one compiled copy), guards left out, and it is covered
              when any copy of it took both ways -- nearer the source-level
              decision DO-178C means, and optimistic where two copies order
              one line's branches differently.

Several builds
--------------

Gates build different kernels (`coverage-report.py` says why). A branch is
named across builds by the function it is in, the file and line it is
attributed to, and its order among that function's branches at that line;
each trace is read against its own ELF and slide, and the outcomes are unioned
by name onto the branches of `--elf`, which is the denominator. A branch of
another build with no namesake in `--elf` is counted and reported, not added.
"""

from __future__ import annotations

import argparse
import bisect
import importlib.util
import json
import re
import subprocess
import sys
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent.parent


def load_statement_tool():
    """`coverage-report.py`, whose trace, ELF and line-table readers this
    shares, so that the two figures read the traces the same way."""
    spec = importlib.util.spec_from_file_location(
        "coverage_report", ROOT / "tools" / "common" / "gen" / "coverage-report.py"
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


cov = load_statement_tool()

CONDITIONS = "eq|ne|cs|hs|cc|lo|mi|pl|vs|vc|hi|ls|ge|lt|gt|le"

# Direct conditional branches, per architecture: the mnemonic as the
# toolchain's llvm-objdump prints it.
CONDITIONAL = {
    "x86_64": re.compile(r"^(j(?!mp)[a-z]+|jrcxz|jecxz|loop|loope|loopne)$"),
    "aarch64": re.compile(rf"^(b\.({CONDITIONS})|cbz|cbnz|tbz|tbnz)$"),
    "armv7a": re.compile(rf"^(b({CONDITIONS})(\.w|\.n)?|cbz|cbnz)$"),
}

# Conditional instructions whose outcome block starts cannot tell: a
# conditional call returns to its fall-through either way, and a conditional
# indirect branch has no target in the instruction. ARM only.
UNMEASURED = {
    "armv7a": re.compile(
        rf"^(bl({CONDITIONS})|blx({CONDITIONS})|bx({CONDITIONS})"
        rf"|pop({CONDITIONS})|ldm({CONDITIONS})|ldr({CONDITIONS}))(\.w|\.n)?$"
    ),
}

# Unconditional direct transfers, whose targets the sure outcomes must know
# are also entered from elsewhere.
DIRECT = {
    "x86_64": re.compile(r"^(jmp|jmpq|call|callq)$"),
    "aarch64": re.compile(r"^(b|bl)$"),
    "armv7a": re.compile(r"^(b|bl|blx)(\.w|\.n)?$"),
}

# `ffffffff80000120 <name>:` -- a symbol, which begins a function.
SYMBOL = re.compile(r"^([0-9a-f]+) <(.*)>:$")
# `ffffffff80000010:      	jne	0xffffffff80000033 <_start+0x33>`
INSTRUCTION = re.compile(r"^\s*([0-9a-f]+):\s+(\S+)\s*(.*)$")
# The branch target: the address objdump annotates with a symbol, which is
# the last operand -- not the first hex number, which in `tbz w8, #0x3, ...`
# is the bit tested.
TARGET = re.compile(r"(?:^|[\s,])(?:0x)?([0-9a-f]+) <[^\n]*>\s*$")
ARM_COMMENT = re.compile(r"\s+@ .*$")
# A register list that includes the pc: `{r4, r5, r11, pc}`.
LOADS_PC = re.compile(r"\{[^}]*\bpc\b[^}]*\}")
# LLVM's local-symbol suffix, which differs between builds of the same code.
LLVM_SUFFIX = re.compile(r"\.llvm\.\d+$")


@dataclass
class Branch:
    """One conditional branch instruction and what the traces saw of it."""

    address: int
    function: str
    instance: int
    mnemonic: str
    target: int | None
    fallthrough: int
    file: str | None = None
    line: int = 0
    key: tuple = ()
    reached: bool = False
    taken: bool = False
    not_taken: bool = False
    # Each outcome again, counted only when no other instruction with an
    # edge into that successor ran: the lower bound.
    taken_sure: bool = False
    not_taken_sure: bool = False
    measured: bool = True
    # Which outcome runs straight into a panic, if one does.
    guard: str | None = None

    @property
    def outcome(self) -> str:
        if self.taken and self.not_taken:
            return "both"
        if self.taken:
            return "taken"
        if self.not_taken:
            return "not-taken"
        return "no-outcome" if self.reached else "unreached"


# Where a straight run of instructions ends, per architecture: an
# unconditional jump (followed once, when direct), a return, a trap, or a
# conditional branch.
JUMP = {
    "x86_64": re.compile(r"^jmpq?$"),
    "aarch64": re.compile(r"^b$"),
    "armv7a": re.compile(r"^b(\.w|\.n)?$"),
}
STOP = {
    "x86_64": re.compile(r"^(retq?|ud2|int3|hlt|jmpq?)$"),
    "aarch64": re.compile(r"^(ret|br|brk|udf|hlt|b)$"),
    "armv7a": re.compile(r"^(bx|udf|bkpt|b(\.w|\.n)?)$"),
}
CALL = {
    "x86_64": re.compile(r"^callq?$"),
    "aarch64": re.compile(r"^(bl|blr)$"),
    "armv7a": re.compile(r"^(bl|blx)$"),
}

# Functions that do not return: a panic, and what the kernel calls before one.
# A branch one of whose successors runs straight into a call to one of these is
# a *guard*: an overflow, bounds, `unwrap`, `debug_assert!` or core
# precondition check, or a `fatal!`, whose panicking outcome a passing run
# never takes.
PANICKING = re.compile(
    r"^(core::panicking::"
    r"|core::option::(unwrap|expect)_failed"
    r"|core::result::unwrap_failed"
    r"|core::slice::index::slice_\w*fail"
    r"|core::str::slice_error_fail"
    r"|core::cell::panic_"
    r"|alloc::alloc::handle_alloc_error"
    r"|alloc::raw_vec::(handle_error|capacity_overflow)"
    r"|rust_begin_unwind"
    r"|ferrix_kernel::panic::explain"
    r"|ferrix_kernel::trap::fatal)"
)
# `callq *0x54dd86(%rip) # 0xffffffff8054ddd8 <...>`: x86-64's kernel code
# calls most functions through a slot the loader relocates.
SLOT = re.compile(r"^\*0x[0-9a-f]+\(%rip\)\s+# 0x([0-9a-f]+)")
# `<name>` after a direct target, without an offset: the callee's symbol.
CALLEE = re.compile(r"<([^<>]*(?:<[^<>]*(?:<[^<>]*>[^<>]*)*>[^<>]*)*)>\s*$")


def writes_pc(mnemonic: str, operands: str) -> bool:
    """Whether an ARM instruction other than a branch writes the pc."""
    if mnemonic.startswith(("pop", "ldm")):
        return LOADS_PC.search(operands) is not None
    return operands.startswith("pc,")


def llvm_tool(name: str) -> str:
    """A tool of the pinned toolchain's `llvm-tools` component, which
    rust-toolchain.toml installs and which reads all three architectures."""
    sysroot = subprocess.run(
        ["rustc", "--print", "sysroot"],
        capture_output=True,
        text=True,
        check=True,
        cwd=ROOT,
    ).stdout.strip()
    found = sorted(Path(sysroot).glob(f"lib/rustlib/*/bin/{name}"))
    if not found:
        raise SystemExit(f"no {name} under {sysroot}: `rustup component add llvm-tools`")
    return str(found[0])


def slot_targets(elf: Path) -> dict[int, int]:
    """{slot address: function address} from the image's RELATIVE
    relocations, which is what an x86-64 `call *slot(%rip)` calls."""
    output = subprocess.run(
        ["readelf", "-rW", str(elf)], capture_output=True, text=True, check=True
    ).stdout
    slots: dict[int, int] = {}
    for line in output.splitlines():
        parts = line.split()
        if len(parts) >= 4 and parts[2].endswith("_RELATIVE"):
            addend = parts[3]
            value = int(addend.lstrip("+-"), 16)
            if addend.startswith("-"):
                value = -value
            slots[int(parts[0], 16)] = value & 0xFFFFFFFFFFFFFFFF
    return slots


def function_names(elf: Path) -> dict[int, str]:
    """{address: demangled name} of every function symbol."""
    output = subprocess.run(
        [llvm_tool("llvm-nm"), "-C", "--defined-only", str(elf)],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    names: dict[int, str] = {}
    for line in output.splitlines():
        parts = line.split(" ", 2)
        if len(parts) == 3 and parts[1] in "tTwW":
            try:
                names.setdefault(int(parts[0], 16), parts[2])
            except ValueError:
                continue
    return names


def disassemble(elf: Path, arch: str) -> tuple[list[Branch], dict[int, list[int]]]:
    """Every conditional branch in `elf`, and for each address the
    instructions whose known edges enter it: direct branches and jumps to it,
    a conditional branch it follows, a call it returns from. Each branch knows whether a
    successor is a panic."""
    conditional = CONDITIONAL[arch]
    unmeasured = UNMEASURED.get(arch)
    direct = DIRECT[arch]
    output = subprocess.run(
        [llvm_tool("llvm-objdump"), "-d", "--no-show-raw-insn", "-C", str(elf)],
        capture_output=True,
        text=True,
        check=True,
    ).stdout

    branches: list[Branch] = []
    entries: dict[int, list[int]] = defaultdict(list)
    pending: list[Branch] = []  # waiting for the next instruction's address
    # The whole stream, for following a successor to a panic.
    stream: list[tuple[int, str, str, int | None]] = []
    functions: set[int] = set()
    function = "?"
    # Two symbols of one name (LLVM's suffix stripped) are told apart by
    # their order in the image.
    seen: dict[str, int] = defaultdict(int)
    instance = 0
    call = CALL[arch]
    returns_here: int | None = None  # the call before this instruction
    for raw in output.splitlines():
        symbol = SYMBOL.match(raw)
        if symbol:
            functions.add(int(symbol.group(1), 16))
            function = LLVM_SUFFIX.sub("", symbol.group(2))
            instance = seen[function]
            seen[function] += 1
            continue
        insn = INSTRUCTION.match(raw)
        if not insn:
            continue
        address = int(insn.group(1), 16)
        mnemonic = insn.group(2)
        # ARM's disassembly ends a branch with `@ imm = #0x18c`.
        operands = ARM_COMMENT.sub("", insn.group(3))
        for branch in pending:
            branch.fallthrough = address
        # A block begins here if a conditional branch before it was not
        # taken or a call before it returned.
        for branch in pending:
            entries[address].append(branch.address)
        if returns_here is not None:
            entries[address].append(returns_here)
        pending = []
        returns_here = address if call.match(mnemonic) else None

        target_match = TARGET.search(operands)
        target = int(target_match.group(1), 16) if target_match else None
        stream.append((address, mnemonic, operands, target))
        if conditional.match(mnemonic) and target is not None:
            entries[target].append(address)
            branch = Branch(address, function, instance, mnemonic, target, 0)
            branches.append(branch)
            pending.append(branch)
        elif unmeasured is not None and unmeasured.match(mnemonic):
            # `ldr<c>` and `ldm<c>`/`pop<c>` only when they load the pc --
            # not a `ldreq r0, [pc, #8]`, which reads a literal.
            if mnemonic.startswith("ldr") and not operands.startswith("pc,"):
                continue
            if mnemonic.startswith(("ldm", "pop")) and not LOADS_PC.search(operands):
                continue
            is_call = mnemonic.startswith("bl") and not mnemonic.startswith("blx")
            branch = Branch(
                address,
                function,
                instance,
                mnemonic,
                target if is_call else None,
                0,
                measured=False,
            )
            branches.append(branch)
            pending.append(branch)
        elif direct.match(mnemonic) and target is not None:
            entries[target].append(address)
    # A branch that is the image's last instruction falls through to nothing.
    for branch in pending:
        branch.fallthrough = branch.address

    mark_guards(branches, stream, functions, elf, arch)
    return branches, entries


def mark_guards(
    branches: list[Branch],
    stream: list[tuple[int, str, str, int | None]],
    functions: set[int],
    elf: Path,
    arch: str,
) -> None:
    """Set `guard` on each branch one of whose successors runs, without
    another decision or call, into a call that panics; and which of them it
    is. Stopping at the first call errs toward counting a guard as an
    ordinary decision, which lowers the figure for the rest."""
    names = function_names(elf)
    slots = slot_targets(elf) if arch == "x86_64" else {}
    index = {address: i for i, (address, _, _, _) in enumerate(stream)}
    call, stop, jump = CALL[arch], STOP[arch], JUMP[arch]
    conditional = CONDITIONAL[arch]
    memo: dict[int, bool] = {}

    def callee(mnemonic: str, operands: str, target: int | None) -> str | None:
        slot = SLOT.match(operands)
        if slot:
            destination = slots.get(int(slot.group(1), 16))
            return names.get(destination) if destination is not None else None
        if target is not None:
            return names.get(target)
        return None

    def panics(address: int) -> bool:
        if address in memo:
            return memo[address]
        found = False
        i = index.get(address)
        jumps = 0
        steps = 0
        while i is not None and i < len(stream) and steps < 24:
            _, mnemonic, operands, target = stream[i]
            steps += 1
            if call.match(mnemonic):
                # The first call decides. Anything else called first may not
                # return -- `idle_loop`, `leave_current` -- and the panic after
                # it is another block's, laid out behind it.
                name = callee(mnemonic, operands, target)
                found = name is not None and PANICKING.match(name) is not None
                break
            elif jump.match(mnemonic) and target is not None and jumps == 0:
                jumps += 1
                i = index.get(target)
                continue
            elif stop.match(mnemonic) or conditional.match(mnemonic):
                break
            elif arch == "armv7a" and writes_pc(mnemonic, operands):
                break  # a return: `pop {..., pc}`, `ldr pc, ...`
            i += 1
            if i < len(stream) and stream[i][0] in functions:
                break  # the next function
        memo[address] = found
        return found

    for branch in branches:
        if not branch.measured:
            continue
        if branch.target is not None and panics(branch.target):
            branch.guard = "taken"
        elif panics(branch.fallthrough):
            branch.guard = "not-taken"


def read_all_rows(elf: Path) -> tuple[list[int], list[tuple[str | None, int]]]:
    """The whole line table as a lookup: sorted addresses, and for each the
    (file, line) in force from it, `(None, 0)` past a sequence's end.

    `coverage-report.py`'s reader keeps only `is_stmt` rows, the statements.
    A branch needs the row in force at its address, statement or not, so this
    keeps every row and walks the objdump output with the same header rules.
    """
    low, high = cov.elf_text_range(elf)
    output = subprocess.run(
        ["objdump", "--dwarf=decodedline", "-w", str(elf)],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    rows: list[tuple[int, int, int, str | None, int]] = []
    current: str | None = None
    unit: str | None = None
    order = 0
    for raw in output.splitlines():
        text = raw.strip()
        header = cov.FILE_HEADER.match(text)
        if header:
            path = header.group(2)
            current = path if "/" in path else None
            if header.group(1):
                unit = current
            continue
        row = cov.LINE_ROW.match(text)
        if not row:
            continue
        address = int(row.group(2), 16)
        order += 1
        if row.group(1) == "-":
            # An end of sequence: nothing is in force from here until the
            # next row. Sorted before a row at the same address.
            if low <= address < high:
                rows.append((address, 0, order, None, 0))
            current = unit
            continue
        if not low <= address < high:
            continue
        rows.append((address, 1, order, current, int(row.group(1))))
    rows.sort()
    return [r[0] for r in rows], [(r[3], r[4]) for r in rows]


def attribute(branches: list[Branch], elf: Path) -> None:
    """Give each branch its file, line and cross-build name."""
    addresses, places = read_all_rows(elf)
    ordinal: dict[tuple, int] = defaultdict(int)
    for branch in branches:
        index = bisect.bisect_right(addresses, branch.address) - 1
        if index >= 0:
            branch.file, branch.line = places[index]
        base = (branch.function, branch.instance, branch.file, branch.line)
        branch.key = base + (ordinal[base],)
        ordinal[base] += 1


def observe(
    branches: list[Branch],
    ranges: list[tuple[int, int]],
    starts: set[int],
    entries: dict[int, list[int]],
) -> None:
    """Mark each branch reached, taken and not taken from one build's blocks.

    A block starting at a successor shows that *some* edge entered it. The
    outcome is sure when every other instruction with a known edge into that
    successor -- a branch or jump to it, a branch it follows, a call it returns
    from -- lies in no executed block, so that this branch's edge is the only
    one that can have been taken.
    """

    def ran(address: int) -> bool:
        index = bisect.bisect_right(ranges, (address, 1 << 65))
        for begin, end in reversed(ranges[max(0, index - 64) : index]):
            if begin <= address < end:
                return True
        return False

    def only_this(branch: Branch, successor: int) -> bool:
        return not any(
            source != branch.address and ran(source)
            for source in entries.get(successor, ())
        )

    for branch in branches:
        branch.reached = branch.reached or ran(branch.address)
        if not branch.measured:
            continue
        if branch.target is not None and branch.target in starts:
            branch.taken = True
            branch.taken_sure |= only_this(branch, branch.target)
        if branch.fallthrough != branch.address and branch.fallthrough in starts:
            branch.not_taken = True
            branch.not_taken_sure |= only_this(branch, branch.fallthrough)


def blocks_of(traces: list[Path], elf: Path, slide_given: int | None):
    """The executed blocks of `traces`, at `elf`'s link addresses."""
    low, high = cov.elf_text_range(elf)
    ranges: set[tuple[int, int]] = set()
    count = 0
    for trace in traces:
        found = cov.read_drcov(trace)
        count += len(found)
        slide = cov.trace_slide(trace, slide_given)
        for low32, size in found:
            start = cov.reconstruct(low32, low + slide, high + slide)
            if start is not None:
                start -= slide
                ranges.add((start, start + max(size, 1)))
    return sorted(ranges), {start for start, _ in ranges}, count


def empty_counts() -> dict[str, int]:
    return {
        "branches": 0,
        "both": 0,
        "both_sure": 0,
        "taken": 0,
        "not_taken": 0,
        "no_outcome": 0,
        "unreached": 0,
        # Of the branches, those guarding a panic; how many of those took
        # both ways, and how many took the panicking way at all.
        "guards": 0,
        "guards_both": 0,
        "guards_panicked": 0,
    }


def tally(counts: dict[str, int], branch: Branch) -> None:
    counts["branches"] += 1
    outcome = branch.outcome.replace("-", "_")
    counts[outcome] += 1
    if branch.taken_sure and branch.not_taken_sure:
        counts["both_sure"] += 1
    if branch.guard is not None:
        counts["guards"] += 1
        if outcome == "both":
            counts["guards_both"] += 1
        if (branch.guard == "taken" and branch.taken) or (
            branch.guard == "not-taken" and branch.not_taken
        ):
            counts["guards_panicked"] += 1


def percent(part: int, whole: int) -> str:
    return f"{100.0 * part / whole:5.1f}%" if whole else "   --"


def evidence(summary: dict, files: dict[str, dict]) -> str:
    """The committed JSON: the summary indented, then one line per file, so
    that a re-measurement's diff is one line for each file that moved."""
    keep = ("ring", "branches", "both", "taken", "not_taken", "unreached", "guards")
    # A non-empty dict indented ends in "\n}"; the files go before it.
    head = json.dumps(summary, indent=2)
    rows = ",\n".join(
        f"    {json.dumps(rel)}: {json.dumps({k: files[rel][k] for k in keep})}"
        for rel in sorted(files)
    )
    return head[:-2] + ',\n  "files": {\n' + rows + "\n  }\n}\n"


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--drcov", type=Path, required=True, nargs="+")
    parser.add_argument(
        "--elf",
        type=Path,
        required=True,
        help="the reference build, whose branches are the denominator",
    )
    parser.add_argument(
        "--arch", required=True, choices=sorted(CONDITIONAL), help="the architecture"
    )
    parser.add_argument(
        "--slide",
        type=lambda text: int(text, 16),
        default=None,
        help="KASLR slide in hex for every trace; by default `<trace>.slide`",
    )
    parser.add_argument("--json", type=Path, help="per-ring and per-file totals")
    parser.add_argument(
        "--branches",
        type=Path,
        help="every item branch not taken both ways, with its outcome: the "
        "worklist, too long to commit",
    )
    parser.add_argument(
        "--top", type=int, default=15, help="files to list by branches short of both"
    )
    args = parser.parse_args()

    gate = cov.load_gate()
    manifest = gate.load_manifest()
    ring_of, _, _ = gate.classify(manifest, gate.kernel_files())

    by_elf: dict[Path, list[Path]] = defaultdict(list)
    for trace in args.drcov:
        by_elf[cov.kernel_of(trace, args.elf).resolve()].append(trace)
    profile = cov.one_profile([args.elf, *by_elf])
    reference_path = args.elf.resolve()

    reference, entries = disassemble(reference_path, args.arch)
    attribute(reference, reference_path)
    by_key = {branch.key: branch for branch in reference}

    blocks = 0
    unmatched = 0
    for elf, traces in by_elf.items():
        ranges, starts, count = blocks_of(traces, elf, args.slide)
        blocks += count
        if elf == reference_path:
            observe(reference, ranges, starts, entries)
            print(f"decisions: {len(traces)} traces of {elf.name} (reference)")
            continue
        others, other_entries = disassemble(elf, args.arch)
        attribute(others, elf)
        observe(others, ranges, starts, other_entries)
        missing = 0
        for other in others:
            mine = by_key.get(other.key)
            if mine is None:
                missing += 1
                continue
            mine.reached |= other.reached
            mine.taken |= other.taken
            mine.not_taken |= other.not_taken
            mine.taken_sure |= other.taken_sure
            mine.not_taken_sure |= other.not_taken_sure
        unmatched += missing
        print(
            f"decisions: {len(traces)} traces of {elf.name}: "
            f"{len(others) - missing} of {len(others)} branches named in the reference"
        )

    rings: dict[str, dict[str, int]] = defaultdict(empty_counts)
    files: dict[str, dict] = {}
    outside = empty_counts()
    unmeasured: dict[str, int] = defaultdict(int)
    worklist: dict[str, list] = defaultdict(list)
    # The source view: a decision is (file, line, its order at that line in
    # one copy), and it is covered when any compiled copy of it took both
    # ways. Guards are left out of it.
    by_source: dict[str, dict[tuple, bool]] = defaultdict(dict)
    for branch in reference:
        rel = cov.relative(branch.file) if branch.file else None
        ring = ring_of.get(rel) if rel else None
        if ring is None or gate.is_test_file(rel, manifest):
            if branch.measured and ring is None:
                tally(outside, branch)
            continue
        if not branch.measured:
            unmeasured[ring] += 1
            continue
        tally(rings[ring], branch)
        entry = files.setdefault(rel, {"ring": ring, **empty_counts()})
        tally(entry, branch)
        if branch.guard is None:
            place = (rel, branch.line, branch.key[-1])
            seen = by_source[ring].get(place, False)
            by_source[ring][place] = seen or branch.outcome == "both"
        if branch.outcome != "both" and ring in ("core", "item"):
            worklist[rel].append(
                {
                    "line": branch.line,
                    "address": f"{branch.address:#x}",
                    "function": branch.function,
                    "insn": branch.mnemonic,
                    "outcome": branch.outcome,
                    "guard": branch.guard,
                }
            )

    item = empty_counts()
    for ring in ("core", "item"):
        for name, value in rings[ring].items():
            item[name] += value
    source = {
        ring: {"decisions": len(places), "both": sum(places.values())}
        for ring, places in sorted(by_source.items())
    }
    source["certified_item"] = {
        name: sum(source.get(ring, {}).get(name, 0) for ring in ("core", "item"))
        for name in ("decisions", "both")
    }

    print(
        f"decisions: {args.arch}, {profile} profile, {len(args.drcov)} traces, "
        f"{blocks} blocks"
    )
    print("decisions: object-code conditional branches, by certification ring")
    print(
        f"  {'':<15} {'both ways':>20}  {'sure':>9}  {'taken':>5}"
        f"  not-taken  no-outcome  unreached"
    )

    def row(name: str, counts: dict[str, int]) -> None:
        total = counts["branches"]
        print(
            f"  {name:<15} {counts['both']:>6} / {total:<6} {percent(counts['both'], total)}"
            f"  {percent(counts['both_sure'], total):>9}"
            f"  {counts['taken']:>5}  {counts['not_taken']:>9}"
            f"  {counts['no_outcome']:>10}  {counts['unreached']:>9}"
        )

    for ring in ("core", "item", "load"):
        if rings[ring]["branches"]:
            row(ring, rings[ring])
    print(f"  {'-' * 20}")
    row("certified item", item)
    outcomes = 2 * item["both"] + item["taken"] + item["not_taken"]
    print(
        f"  {'outcomes seen':<15} {outcomes:>6} / {2 * item['branches']:<6}"
        f" {percent(outcomes, 2 * item['branches'])}"
    )
    row("outside kernel", outside)

    decisions = item["branches"] - item["guards"]
    decided = item["both"] - item["guards_both"]
    print(
        f"decisions: {item['guards']} of the item's branches guard a panic "
        f"(overflow, bounds, unwrap, assertion, precondition, fatal!); "
        f"{item['guards_panicked']} took the panicking way"
    )
    print(
        f"decisions: the item's other branches, both ways: "
        f"{decided} / {decisions} {percent(decided, decisions).strip()}"
    )
    whole = source["certified_item"]
    print(
        f"decisions: by source line (any copy of a decision, guards left out): "
        f"{whole['both']} / {whole['decisions']} "
        f"{percent(whole['both'], whole['decisions']).strip()}"
    )
    if unmeasured:
        print(
            "decisions: conditional calls and indirect branches, not measured: "
            + ", ".join(f"{ring} {n}" for ring, n in sorted(unmeasured.items()))
        )
    if unmatched:
        print(
            f"decisions: {unmatched} branches of the other builds have no "
            f"namesake in the reference, and are not counted"
        )

    short = sorted(
        (
            (v["branches"] - v["guards"] - (v["both"] - v["guards_both"]), rel, v)
            for rel, v in files.items()
            if v["ring"] in ("core", "item")
        ),
        key=lambda t: (-t[0], t[1]),
    )
    if args.top and short:
        print("decisions: item files by non-guard branches short of both ways")
        for missing, rel, v in short[: args.top]:
            own = v["branches"] - v["guards"]
            print(
                f"  {missing:>5}  {rel:<36} {v['both'] - v['guards_both']:>5} / {own:<5}"
                f" {percent(v['both'] - v['guards_both'], own)}"
                f"   all {v['both']:>5} / {v['branches']:<5}"
                f" {percent(v['both'], v['branches'])}"
            )

    if args.json:
        args.json.write_text(
            evidence(
                {
                    "//": [
                        "Decision (branch) coverage of the kernel's object code:",
                        "a direct conditional branch counts as taken both ways",
                        "when executed blocks began at its target and at its",
                        "fall-through. `guards` are branches one of whose",
                        "successors runs straight into a panic. Written by",
                        "tools/common/gen/decision-coverage.py; VERIFICATION.md 3.6.",
                    ],
                    "arch": args.arch,
                    "profile": profile,
                    "traces": len(args.drcov),
                    "builds": len(by_elf),
                    "blocks_executed": blocks,
                    "unmatched_other_builds": unmatched,
                    "item": item,
                    "rings": {k: dict(v) for k, v in sorted(rings.items())},
                    "by_source_line": source,
                    "outside_kernel": outside,
                    "not_measured": dict(sorted(unmeasured.items())),
                },
                files,
            )
        )
        print(f"decisions: per-file totals in {args.json}")

    if args.branches:
        args.branches.write_text(
            json.dumps(
                {
                    "arch": args.arch,
                    "files": dict(
                        sorted(worklist.items(), key=lambda kv: -len(kv[1]))
                    ),
                },
                indent=1,
            )
            + "\n"
        )
        listed = sum(map(len, worklist.values()))
        print(f"decisions: {listed} item branches listed in {args.branches}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
