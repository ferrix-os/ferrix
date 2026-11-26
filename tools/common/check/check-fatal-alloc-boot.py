#!/usr/bin/env python3
"""Every FATAL-ALLOC site shown boot-only, from the image (ledger 5942 K16.5, 10421 H1-H8).

    check-fatal-alloc-boot.py <kernel ELF> <x86_64|aarch64> [--paths] | --self-test

A `FATAL-ALLOC:` marker says an allocation's failure stops the machine, and that it runs
only at boot. Under `kernel_memory=retyped` the shared heap refuses after the seal, so a
marked site reached after it stops the product. This reads one kernel image and fails on
a function that holds a marked site and that something able to run after the seal reaches.

SITES (H1): `check-fallible-alloc.py`'s own scan of the item's files, its sites whose
marker is FATAL-ALLOC. A site is mapped to the functions of the image any of whose
instructions has its line in its inline chain (`addr2line -i`), so a site inlined into a
caller counts as the caller's. A site no instruction comes from is listed as not in this
image, by file and line.

ROOTS (H2): what can run after the seal, read from the image: the trap entry
(`trap::dispatch` on AArch64; the system call entry and every interrupt stub on x86-64,
which the IDT takes by address), and every function whose address the image takes,
which covers task entries, timer and deferred-work callbacks, `dyn` methods and function
tables: a function pointer is a root unless EXEMPT names it with a reason. EXEMPT starts
empty, and its length is printed (H3).

CALLS: `check-stack-depth.py`'s reader of direct calls and tail calls (x86-64's through
the GOT followed); calls through a register are covered by taking every address-taken
function as a root.

LIMITS (H7): one image's call graph; indirect calls over-approximated by address-taken
roots; the debug line table trusted to attribute an instruction to its source line; a
root that runs only before the seal (a boot task spawned and finished early) reads as
post-seal until it is exempted by name. Markers are counted beside the sites: a marker
covering no flagged call, or one call flagged twice, is named in the count line.
"""
import importlib.util
import os
import re
import struct
import subprocess
import sys
from collections import deque
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import kernel_roots  # noqa: E402  (docs/microkernel/CRATE-SPLIT.md S0)

FINDER = "ferrix_kernel_core::discovery::finder::Finder>::find"
# Roots the walk takes as not running after the seal, each with its reason and the guard
# it rests on (ledger 10563 R4, 10588 G1-G4; K16.md §3). A guard is a key of GUARDS.
EXEMPT: dict[str, tuple[str, str]] = {
    f"<ferrix_kernel_core::discovery::tree::VirtioMmio as {FINDER}": (
        "a finder: its only caller is device::publish's walk of the finder list", "publish"),
    f"<ferrix_kernel_item::discovery::pci::Enumeration as {FINDER}": (
        "a finder: its only caller is device::publish's walk of the finder list", "publish"),
    f"<ferrix_kernel_core::discovery::board::Boards as {FINDER}": (
        "a finder: its only caller is device::publish's walk of the finder list", "publish"),
    "ferrix_kernel_core::arch::aarch64::smp::secondary_start": (
        "a secondary processor's entry: only a processor start enters it", "start_secondaries"),
    "ferrix_kernel_core::arch::x86_64::smp::secondary_start": (
        "a secondary processor's entry: only a processor start enters it", "start_secondaries"),
    "_start": ("the image's entry: its address is taken only where PINNED_START lists", "pinned"),
}

# `_start`'s exemption pinned (10588 G3 (ii)): every take of its address, and every reader of
# a data word holding it, by function; one beyond these fails. The readers use the value in
# arithmetic, the image's base (read by sebastian-27e, 2026-11-26); BACKLOG owes (i), a
# linker symbol for the base so that `_start`'s address is taken nowhere.
PINNED_START = {
    "aarch64": {"ferrix_kernel_core::panic::conclude", "ferrix_kernel_core::mm::check::sweep_w_xor_x"},
    "x86_64": {"ferrix_kernel_core::panic::conclude", "ferrix_kernel_core::mm::check::sweep_w_xor_x"},
}

# Functions an exemption's reason rests on: each must be unreached from every root left
# after the exemptions, or the run fails by name (10563 R1 (a), R3, R4).
GUARDS = {
    "publish": (("src/kernel/src/device.rs", "publish"), ("aarch64", "x86_64"),
                "walks the finders, the Finder impls' only caller"),
    "start_secondaries": (("src/kernel/src/smp.rs", "start_secondaries"), ("aarch64", "x86_64"),
                          "starts the secondary processors"),
    "cpu_on": (("src/kernel/src/arch/aarch64/smp.rs", "start"), ("aarch64",),
               "asks PSCI's CPU_ON for one processor"),
    "sipi": (("src/kernel/src/arch/x86_64/smp.rs", "start"), ("x86_64",),
             "sends one processor its INIT and SIPI"),
}
# The guards every exemption resting on processor starts needs, per architecture.
START_GUARDS = {"aarch64": ("start_secondaries", "cpu_on"), "x86_64": ("start_secondaries", "sipi")}


def fn_lines(path: str, name: str) -> range:
    """The lines of the first `fn name` in `path`, by its braces: a guard is held by every
    function of the image any of whose instructions comes from them, so an inlined guard
    is its caller."""
    lines = Path(path).read_text(encoding="utf-8", errors="replace").splitlines()
    named = [at for at, line in enumerate(lines) if re.search(rf"\bfn {re.escape(name)}\b", line)]
    if len(named) > 1:
        raise SystemExit(f"fatal-alloc-boot: guard {path}::{name} names {len(named)} functions: ambiguous")
    for at, line in enumerate(lines):
        if re.search(rf"\bfn {re.escape(name)}\b", line):
            depth, opened = 0, False
            for end in range(at, len(lines)):
                depth += lines[end].count("{") - lines[end].count("}")
                opened = opened or "{" in lines[end]
                if opened and depth <= 0:
                    return range(at + 1, end + 2)
    return range(0)


# What runs after the seal by the hardware's own hand, each of which must be in the image by
# this name or the run fails (10834 L1): AArch64's vector table, whose sixteen entries all
# branch to ferrix_trap_save, which reaches trap::dispatch (inlined in a release image, so
# not a symbol of its own), and ferrix_trap_save itself; x86-64's system
# call entry (its interrupt stubs are roots by the IDT taking their addresses).
ENTRIES = {
    "aarch64": ("ferrix_vectors", "ferrix_trap_save"),
    "x86_64": ("ferrix_syscall_entry",),
}

# Sites with no code in an image, beyond another architecture's arch/ files, each with why
# (10834 L2): passed as absent, printed and counted; any other absent site fails.
ABSENT: dict[tuple[str, str, int], str] = {
    ("aarch64", "src/kernel/src/init.rs", 448): (
        "no instruction of the AArch64 release image carries this line in its inline chain: "
        "the Once's closure (Vec::from of the option) by reading, not traced further"),
    ("x86_64", "src/kernel/src/init.rs", 448): (
        "no instruction of the x86-64 release image carries this line in its inline chain: "
        "the Once's closure (Vec::from of the option) is folded with an identical one, by "
        "reading; AArch64's image holds it"),
    ("x86_64", "src/kernel/src/arch/x86_64/smp.rs", 93): (
        "the collect's code carries the iterator chain's lines (88-92) in the x86-64 release "
        "image, not this one, by reading; the same function's other sites map"),
}

HEAD = re.compile(r"^([0-9a-f]+) <(.+)>:$")
LINE = re.compile(r"^; (/.+\.rs):(\d+)")
ADRP = re.compile(r"\badrp\s+(x\d+), 0x([0-9a-f]+)")
ADR = re.compile(r"\badr\s+(x\d+), 0x([0-9a-f]+)")
LITERAL = re.compile(r"\bldr\s+x\d+, 0x([0-9a-f]+)")
ADD_LO = re.compile(r"\badd\s+(x\d+), (x\d+), #0x([0-9a-f]+)")
LEA = re.compile(r"\blea[q]?\s+-?0x[0-9a-f]+\(%rip\), %\w+\s+# 0x([0-9a-f]+)")


def module(name: str):
    spec = importlib.util.spec_from_file_location(name.replace("-", "_"), HERE / f"{name}.py")
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


def fatal_sites() -> tuple[list[tuple[str, int]], int]:
    """(repo path, line) of every FATAL-ALLOC site, and the marker count."""
    fallible = module("check-fallible-alloc")
    gate = fallible.load_gate()
    sites = []
    for rel, found in fallible.item_sites().items():
        path = repo_relative(str(gate.kernel_path(rel).resolve()))
        for site in found:
            if site["marker"] == "fatal":
                sites.append((path, site["line"]))
    return sorted(set(sites)), len(sites)


def is_check_file(rel: str) -> bool:
    """A check file by the tree's own definition: certification-item.json's
    test_file_patterns, as check-item-boundary.py reads them (10834 L4)."""
    boundary = module("check-item-boundary")
    manifest = boundary.load_manifest()
    return boundary.is_test_file(rel, manifest) or boundary.is_test_file(
        rel.split("/src/", 1)[-1], manifest)


def markers(root: Path) -> list[tuple[str, int, int]]:
    """Every `FATAL-ALLOC:` marker in a .rs file of the kernel's source roots: (file, its line, the code
    line after the comment it ends, or its own line if code stands before it)."""
    out = []
    every = sorted({path for source in kernel_roots.roots() for path in root.joinpath(source).rglob("*.rs")})
    for path in every:
        rel = repo_relative(str(path.resolve()))
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
        for number, line in enumerate(lines, 1):
            if "FATAL-ALLOC:" not in line:
                continue
            if not line.lstrip().startswith("//"):
                out.append((rel, number, number))
                continue
            after = number
            while after < len(lines) and lines[after].lstrip().startswith("//"):
                after += 1
            out.append((rel, number, after + 1))
    return out


def marker_sites(root: Path, scanned: list[tuple[str, int]]) -> tuple[list, list[str], list[str]]:
    """The sites a marker makes (10588 G5: a marker is a site by its `fatal!`, wherever the
    allocation stands): a marker in an item file over no scanned site becomes a site at its
    code line. Markers in check files and in files outside the item are classed apart."""
    at = set(scanned)
    added, apart, outside = [], [], []
    for rel, number, code in markers(root):
        if (rel, number) in at or (rel, code) in at:
            continue
        if is_check_file(rel):
            apart.append(f"{rel}:{number}")
        elif rel.startswith(f"{kernel_roots.parts_place()}/"):
            outside.append(f"{rel}:{number}")
        else:
            added.append((rel, code))
    return added, apart, outside


def repo_relative(path: str) -> str:
    """A source path from its checkout's `src/` on, so a slot's build and this tree agree."""
    path = os.path.normpath(path)
    at = path.find("/src/")
    return path[at + 1 :] if at >= 0 else path


def lines_to_functions(text: str) -> dict[tuple[str, int], set[str]]:
    """Each (source file, line) of the line table, and the functions holding its code."""
    held: dict[tuple[str, int], set[str]] = {}
    current = None
    for line in text.splitlines():
        head = HEAD.match(line)
        if head:
            current = head.group(2)
            continue
        found = LINE.match(line)
        if found and current is not None:
            key = (repo_relative(found.group(1)), int(found.group(2)))
            held.setdefault(key, set()).add(current)
    return held


INSN = re.compile(r"^\s*([0-9a-f]+):")


def inline_lines(text: str, elf_path: str) -> dict[tuple[str, int], set[str]]:
    """Each (source file, line) of every instruction's inline chain, by `addr2line -i`,
    and the functions holding it: a site inlined into a caller is the caller's (H1)."""
    owner: dict[str, str] = {}
    current = None
    for line in text.splitlines():
        head = HEAD.match(line)
        if head:
            current = head.group(2)
            continue
        insn = INSN.match(line)
        if insn and current is not None:
            owner[insn.group(1)] = current
    answer = subprocess.run(
        ["addr2line", "-e", elf_path, "-i", "-a"],
        input="\n".join(f"0x{address}" for address in owner),
        capture_output=True, text=True, check=True,
    ).stdout
    held: dict[tuple[str, int], set[str]] = {}
    function = None
    for line in answer.splitlines():
        if line.startswith("0x"):
            function = owner.get(f"{int(line, 16):x}")
            continue
        if function is None or ":" not in line:
            continue
        path, _, number = line.rpartition(":")
        number = number.split(" ")[0]
        if number.isdigit():
            held.setdefault((repo_relative(path), int(number)), set()).add(function)
    return held


def starts_of(text: str) -> dict[int, str]:
    return {int(head.group(1), 16): head.group(2) for head in map(HEAD.match, text.splitlines()) if head}


def relocated_targets(elf: bytes, arch: str) -> set[int]:
    """Addresses the image's RELATIVE relocations write outside `.got`: function pointers in
    data, vtables among them. A `.got` slot is a call's (x86-64's calls between crates) or a
    load's, which [`address_taken`] reads at the load."""
    if elf[:4] != b"\x7fELF" or elf[4] != 2:
        return set()
    relative = {"x86_64": 8, "aarch64": 1027}[arch]
    shoff = struct.unpack_from("<Q", elf, 0x28)[0]
    shentsize, shnum, shstrndx = struct.unpack_from("<HHH", elf, 0x3A)
    headers = [struct.unpack_from("<IIQQQQIIQQ", elf, shoff + index * shentsize) for index in range(shnum)]
    names_at = headers[shstrndx][4]

    def name(header) -> str:
        start = names_at + header[0]
        return elf[start : elf.index(b"\0", start)].decode()

    got = [(header[3], header[3] + header[5]) for header in headers if name(header) == ".got"]
    out = set()
    for header in headers:
        if header[1] != 4:
            continue
        offset, size = header[4], header[5]
        for entry in range(offset, offset + size, 24):
            where, info, addend = struct.unpack_from("<QQq", elf, entry)
            if info & 0xFFFFFFFF != relative or any(low <= where < high for low, high in got):
                continue
            out.add(addend & 0xFFFFFFFFFFFFFFFF)
    return out


GOT_LOAD = re.compile(r"\bmovq\s+-?0x[0-9a-f]+\(%rip\), %\w+\s+# 0x([0-9a-f]+)")
AARCH64_GOT = re.compile(r"\bldr\s+(x\d+), \[(x\d+), #0x([0-9a-f]+)\]")


def address_taken(text: str, elf: bytes, arch: str, slots: dict[int, int]) -> set[str]:
    """Functions whose address the image takes, failing closed: in its code (AArch64's adr,
    adrp with an add or a load from a GOT slot, and a literal load; x86-64's lea, and every load of a GOT slot
    into a register, which may be a call through it or a pointer kept), and in its data: a
    RELATIVE relocation outside `.got`, or any aligned word equal to a function's start."""
    starts = starts_of(text)
    targets = set()
    pages: dict[str, int] = {}
    for line in text.splitlines():
        if HEAD.match(line):
            pages.clear()
            continue
        found = ADRP.search(line)
        if found:
            pages[found.group(1)] = int(found.group(2), 16)
            continue
        found = ADD_LO.search(line)
        if found and found.group(2) in pages:
            targets.add(pages[found.group(2)] + int(found.group(3), 16))
            continue
        found = ADR.search(line)
        if found:
            targets.add(int(found.group(2), 16))
            continue
        found = LITERAL.search(line)
        if found:
            word = word_at(elf, int(found.group(1), 16))
            if word is not None:
                targets.add(word)
            continue
        found = AARCH64_GOT.search(line)
        if found and found.group(2) in pages:
            slot = pages[found.group(2)] + int(found.group(3), 16)
            targets.add(slots.get(slot, slot))
            continue
        found = LEA.search(line) or GOT_LOAD.search(line)
        if found:
            address = int(found.group(1), 16)
            targets.add(slots.get(address, address))
    targets |= relocated_targets(elf, arch) | data_words(elf)
    return {starts[address] for address in targets if address in starts}


def sections(elf: bytes) -> list[tuple]:
    """Each section header: (name offset, type, flags, address, offset, size, ...)."""
    if elf[:4] != b"\x7fELF" or elf[4] != 2:
        return []
    shoff = struct.unpack_from("<Q", elf, 0x28)[0]
    shentsize, shnum = struct.unpack_from("<HH", elf, 0x3A)
    return [struct.unpack_from("<IIQQQQIIQQ", elf, shoff + index * shentsize) for index in range(shnum)]


def word_at(elf: bytes, address: int) -> int | None:
    """The eight bytes the image loads at `address`, if a section holds them."""
    for header in sections(elf):
        _, kind, _, at, offset, size = header[:6]
        if kind == 1 and at <= address and address + 8 <= at + size:
            return struct.unpack_from("<Q", elf, offset + address - at)[0]
    return None


def data_words(elf: bytes) -> set[int]:
    """Every aligned 8-byte word of the image's data sections (allocated, not executable):
    an absolute function pointer with no relocation beside it, as a static image has."""
    words = set()
    for header in sections(elf):
        _, kind, flags, _, offset, size = header[:6]
        if kind == 1 and flags & 0x2 and not flags & 0x4:
            for at in range(offset, offset + size - 7, 8):
                words.add(struct.unpack_from("<Q", elf, at)[0])
    return words


def got_slots(elf: bytes, arch: str) -> dict[int, int]:
    """Each GOT slot's target, from its RELATIVE relocation."""
    if elf[:4] != b"\x7fELF" or elf[4] != 2:
        return {}
    relative = {"x86_64": 8, "aarch64": 1027}[arch]
    shoff = struct.unpack_from("<Q", elf, 0x28)[0]
    shentsize, shnum = struct.unpack_from("<HH", elf, 0x3A)
    slots = {}
    for index in range(shnum):
        _, kind, _, _, offset, size, _, _, _, _ = struct.unpack_from("<IIQQQQIIQQ", elf, shoff + index * shentsize)
        if kind != 4:
            continue
        for entry in range(offset, offset + size, 24):
            where, info, addend = struct.unpack_from("<QQq", elf, entry)
            if info & 0xFFFFFFFF == relative:
                slots[where] = addend & 0xFFFFFFFFFFFFFFFF
    return slots


def start_takers(text: str, elf: bytes, arch: str) -> set[str]:
    """Every function that takes `_start`'s address or reads a data word holding it."""
    starts = {name: address for address, name in starts_of(text).items()}
    entry = starts.get("_start")
    if entry is None:
        return set()
    words = set()
    for header in sections(elf):
        _, kind, flags, at, offset, size = header[:6]
        if kind == 1 and flags & 0x2 and not flags & 0x4:
            for here in range(offset, offset + size - 7, 8):
                if struct.unpack_from("<Q", elf, here)[0] == entry:
                    words.add(at + here - offset)
    for header in sections(elf):
        if header[1] != 4:
            continue
        for here in range(header[4], header[4] + header[5], 24):
            where, info, addend = struct.unpack_from("<QQq", elf, here)
            if addend & 0xFFFFFFFFFFFFFFFF == entry:
                words.add(where)
    marks = {f"0x{entry:x} <_start>"} | {f"# 0x{word:x}" for word in words}
    takers, current = set(), None
    for line in text.splitlines():
        head = HEAD.match(line)
        if head:
            current = head.group(2)
            continue
        if current != "_start" and any(mark in line for mark in marks) and not re.search(r"\b(bl|b|callq?|jmpq?)\s", line):
            takers.add(current)
    return takers


def missing_entries(names: set[str], arch: str) -> list[str]:
    """The entries ENTRIES names that the image does not hold by that name."""
    return [entry for entry in ENTRIES[arch] if entry not in names]


def other_arch(name: str, arch: str) -> bool:
    """Whether an exemption is another architecture's by its path."""
    return any(f"::arch::{other}::" in name for other in ("aarch64", "x86_64", "armv7a") if other != arch)


def reach(roots: set[str], calls: dict[str, list[str]]) -> dict[str, str | None]:
    """Every function reached from `roots`, and the caller it was first reached from."""
    came: dict[str, str | None] = {root: None for root in roots}
    queue = deque(sorted(roots))
    while queue:
        name = queue.popleft()
        for callee in calls.get(name, ()):
            if callee not in came:
                came[callee] = name
                queue.append(callee)
    return came


def path_to(name: str, came: dict[str, str | None]) -> list[str]:
    out = [name]
    while came.get(out[-1]) is not None:
        out.append(came[out[-1]])
    return list(reversed(out))


def judge(sites, held, roots, calls, show_paths=False, exempt=None, guards=None, arch="aarch64"):
    """The lines to print and the problems: a held function reached from a root, and a
    guard an exemption rests on reached."""
    exempt = set() if exempt is None else exempt
    came = reach({root for root in roots if root not in exempt}, calls)
    lines, problems, absent = [], [], []
    holders: dict[str, list[tuple[str, int]]] = {}
    for site in sites:
        functions = held.get(site, set())
        if not functions:
            absent.append(site)
        for function in functions:
            holders.setdefault(function, []).append(site)
    for function in sorted(holders):
        if function in came:
            where = ", ".join(f"{Path(path).name}:{line}" for path, line in holders[function])
            problems.append(f"{function} ({where}) is reached after the seal")
            if show_paths:
                lines.append("    " + " -> ".join(path_to(function, came)))
    for key, ((path, name), arches, why) in (guards or {}).items():
        span = fn_lines(path, name) if Path(path).exists() else range(0)
        holding = {function for line in span for function in held.get((path, line), ())}
        if not holding:
            if arch in arches:
                problems.append(f"guard {key} ({path}::{name}, {why}) is not in its own image")
            else:
                lines.append(f"  guard {key}: not in this image, which is not its architecture's")
        for function in sorted(holding & set(came)):
            problems.append(f"{function} holds guard {key} ({why}) and is reached after the seal")
            if show_paths:
                lines.append("    " + " -> ".join(path_to(function, came)))
    lines.insert(0, (
        f"fatal-alloc-boot: {len(sites)} FATAL-ALLOC sites, {len(sites) - len(absent)} in this image "
        f"in {len(holders)} functions, {len(absent)} with no code here; {len(roots)} roots, "
        f"{len(exempt)} exempted, {len([key for key in ABSENT if key[0] == arch])} absent by ABSENT; "
        f"{len(problems)} problems"
    ))
    for path, line in absent:
        other = [name for name in ("aarch64", "x86_64", "armv7a") if name != arch]
        if any(path.startswith(f"src/kernel/src/arch/{name}/") for name in other):
            lines.append(f"  not in this image, another architecture's: {path}:{line}")
        elif (arch, path, line) in ABSENT:
            lines.append(f"  not in this image, by ABSENT: {path}:{line}: {ABSENT[(arch, path, line)]}")
        else:
            problems.append(f"{path}:{line} is a site no instruction of this image carries, and ABSENT does not say why")
    return lines, problems


def self_test() -> list[str]:
    failed = []
    calls = {"entry": ["a"], "a": ["held"], "boot": ["only"]}
    held = {("f.rs", 1): {"held"}, ("f.rs", 2): {"only"}, ("f.rs", 3): {"inlined_into"}}
    _, problems = judge([("f.rs", 1), ("f.rs", 2)], held, {"entry"}, calls)
    if len(problems) != 1 or "held" not in problems[0]:
        failed.append("a site's function reached from a root")
    _, problems = judge([("f.rs", 2)], held, {"entry", "only"}, calls)
    if not problems:
        failed.append("an address-taken root")
    _, problems = judge([("f.rs", 3)], held, {"inlined_into"}, calls)
    if not problems:
        failed.append("a site inlined into a root")
    _, problems = judge([("f.rs", 9)], held, set(), calls)
    if not problems:
        failed.append("a site with no code, not in ABSENT")
    lines, problems = judge([("src/kernel/src/arch/x86_64/a.rs", 9)], held, set(), calls, arch="aarch64")
    if problems or not any("another architecture's" in line for line in lines):
        failed.append("another architecture's site passed as absent")
    if not missing_entries({"other"}, "aarch64"):
        failed.append("an entry renamed")
    text = "0000000000001000 <f>:\n; /x/a.rs:7\n    1000: nop\n0000000000002000 <g>:\n    2000: adrp x1, 0x1000\n    2004: add x1, x1, #0x0\n"
    text = text.replace("/x/a.rs", "/x/src/a.rs")
    if lines_to_functions(text).get(("src/a.rs", 7)) != {"f"}:
        failed.append("the line table")
    if address_taken(text, b"", "aarch64", {}) != {"f"}:
        failed.append("adrp and add to a function")
    adr = "0000000000001000 <f>:\n    1000: nop\n0000000000002000 <g>:\n    2000: adr x8, 0x1000 <f>\n"
    if address_taken(adr, b"", "aarch64", {}) != {"f"}:
        failed.append("adr to a function")
    return failed


def main() -> int:
    failed = self_test()
    for what in failed:
        print(f"fatal-alloc-boot: self-test: {what} did not fire")
    if failed:
        return 1
    if "--self-test" in sys.argv:
        print("fatal-alloc-boot: self-test: 9 planted shapes FIRED")
        return 0
    unknown = [word for word in sys.argv[3:] if word != "--paths"]
    if len(sys.argv) < 3 or unknown:
        print(f"fatal-alloc-boot: usage: <kernel ELF> <x86_64|aarch64> [--paths]; unknown argument {unknown}")
        return 2
    elf_path, arch = sys.argv[1], sys.argv[2]
    stack = module("check-stack-depth")
    plain = subprocess.run([stack.objdump(), "-d", "-C", "--no-show-raw-insn", elf_path],
                           capture_output=True, text=True, check=True).stdout
    elf = Path(elf_path).read_bytes()
    slots = stack.got_targets(elf) if arch == "x86_64" else None
    _, calls, _, _ = stack.read_frames(plain, slots)
    names = set(starts_of(plain).values())
    entries = {name for name in names if name in ENTRIES[arch]}
    roots = set(entries) | address_taken(plain, elf, arch, got_slots(elf, arch))
    scanned, _ = fatal_sites()
    added, apart, outside = marker_sites(Path.cwd(), scanned)
    sites = sorted(set(scanned) | set(added))
    exempt = dict(EXEMPT)
    lines, problems = judge(sites, inline_lines(plain, elf_path), roots, calls, "--paths" in sys.argv,
                            {name for name in exempt if name in roots}, GUARDS, arch)
    for entry in missing_entries(names, arch):
        problems.append(f"entry {entry} is not in this image by that name (10834 L1)")
    beyond = start_takers(plain, elf, arch) - PINNED_START[arch]
    for taker in sorted(beyond):
        problems.append(f"{taker} takes or reads _start's address, beyond PINNED_START: its exemption does not hold")
    print("\n".join(lines))
    for name in sorted(name for name in exempt if name in roots):
        print(f"  exempt: {name}: {exempt[name][0]} (guard {exempt[name][1]})")
    for name in sorted(name for name in exempt if name not in roots and not other_arch(name, arch)):
        print(f"  stale exemption, no root in this image: {name}")
    every = markers(Path.cwd())
    print(
        f"fatal-alloc-boot: {len(every)} FATAL-ALLOC markers in the kernel's source roots' .rs files; "
        f"{len(scanned)} sites by check-fallible-alloc.py's scan, {len(added)} more by their markers; "
        f"{len(apart)} markers in check files and {len(outside)} outside the item, classed apart:"
    )
    for where in added:
        print(f"  a marker's site, no flagged call under it: {where[0]}:{where[1]}")
    for where in apart:
        print(f"  check code, classed apart: {where}")
    for where in outside:
        print(f"  outside the item, classed apart: {where}")
    for problem in problems:
        print(f"fatal-alloc-boot: {problem}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
