#!/usr/bin/env python3
"""Two kernels' symbols and sizes, for what is said to leave a build unchanged
(docs/microkernel/M6.md §4, ledger 5942 M.8): `present` and `desktop` against the base,
per architecture. A report, read by a person with the four boots beside it: a generic's
hash or an inlining moved is not a change of behaviour, and a name gone or a size moved
says where to look. Exit 1 on any difference of name or size, ignoring symbols' hashes.

The bodies' comparison masks every number, a displacement's sign with it: it sees a changed
instruction or register, and never a changed constant (ledger 7184 Q.3). Constants unseen by it
are BACKLOG's row, with M7.

Usage:  compare-kernel-symbols.py BASE_ELF HEAD_ELF [--self-test]

FERRIX_SYMBOLS_RENAME=OLD=NEW[,OLD=NEW...]: a move of a module into a crate of its own
(docs/microkernel/CRATE-SPLIT.md §3.3, the recorded crate-name rewrite) renames every path
under it; each OLD path prefix in the base's names (wherever it stands in a name, a
generic's argument included) is read as NEW before the comparison. Only whole `::`-ended
paths are taken, so a rename cannot hide anything but a path's own crate.

FERRIX_SYMBOLS_CRATE_MOVE=<why>: a recorded move of items into a crate of their own
(docs/microkernel/CORE-WITHOUT-EXTENSIONS.md E2, E4; ledger 9784 ruling C, C4). Every crate
that depends on the new one gets a new metadata hash, which moves which name identical-code
folding keeps for a shared body, the hashed local labels, the TypeId constants and the layout;
the names cannot match. Then the names and sizes are a report, and the verdict is the bodies':
every text symbol's body by llvm-objdump (which reads all three architectures), each keyed by
name and address, names ignored, compared as a multiset under the strong mask (`strong`): the
mask above, and also decimal immediates of four digits or more with objdump's immediate
annotations, a function's trailing padding, and AArch64's `adrp`+`add` written `nop`+`adr`.
Equal symbol counts, equal size multisets and equal bodies pass; anything else fails. The
opcodes that differ beyond spacing are printed, for the reader to class.
"""
import collections
import hashlib
import os
import re
import shutil
import subprocess
import sys


def table(lines: list[str]) -> dict[str, int]:
    out: dict[str, int] = {}
    for line in lines:
        fields = line.split(None, 3)
        if len(fields) != 4 or fields[2] not in "tTrRdDbB":
            continue
        name = re.sub(r"::h[0-9a-f]{16}$", "", fields[3].strip())
        out[name] = out.get(name, 0) + int(fields[1], 16)
    return out


def differences(base: dict[str, int], head: dict[str, int]) -> list[str]:
    found = [f"gone: {name}" for name in sorted(set(base) - set(head))]
    found += [f"new: {name}" for name in sorted(set(head) - set(base))]
    for name in sorted(set(base) & set(head)):
        if base[name] != head[name]:
            found.append(f"size: {name}: {base[name]} -> {head[name]}")
    return found


def renames(text: str) -> list[tuple[str, str]]:
    pairs = []
    for pair in filter(None, text.split(",")):
        old, sep, new = pair.partition("=")
        if not sep or not old.endswith("::") or not new.endswith("::"):
            raise SystemExit(f"compare-kernel-symbols: FERRIX_SYMBOLS_RENAME {pair}: OLD::=NEW::")
        pairs.append((old, new))
    return pairs


def renamed(names: dict[str, object], pairs: list[tuple[str, str]]) -> dict[str, object]:
    out = {}
    for name, value in names.items():
        for old, new in pairs:
            name = re.sub(r"(?<![A-Za-z0-9_])" + re.escape(old), new, name)
        out[name] = value
    return out


def symbols(path: str) -> dict[str, int]:
    run = subprocess.run(
        ["nm", "-S", "-C", "--defined-only", path], capture_output=True, text=True, check=True
    )
    return table(run.stdout.splitlines())


def objdump_for(path: str) -> str:
    """The objdump that reads this ELF: the host's does not read AArch64's (ledger 7232)."""
    with open(path, "rb") as elf:
        machine = int.from_bytes(elf.read(20)[18:20], "little")
    wanted = {0xB7: "aarch64-linux-gnu-objdump"}.get(machine)
    return wanted if wanted and shutil.which(wanted) else "objdump"


def bodies(path: str) -> dict[str, str]:
    """Each text symbol's disassembly, its addresses and displacements masked: what is left
    is what a layout's shift cannot move (ledger 7132 ED.2)."""
    run = subprocess.run([objdump_for(path), "-d", "-C", "--no-show-raw-insn", path],
                         capture_output=True, text=True, check=False)
    if run.returncode != 0:
        print(f"compare-kernel-symbols: this objdump does not read {path}: bodies not compared")
        return {}
    out: dict[str, list[str]] = {}
    name = None
    for line in run.stdout.splitlines():
        head = re.match(r"^[0-9a-f]+ <(.+)>:$", line)
        if head:
            name = re.sub(r"::h[0-9a-f]{16}$", "", head.group(1))
            out.setdefault(name, [])
        elif name is not None and "\t" in line:
            out[name].append(masked(line.split("\t", 1)[1]))
    return {key: hashlib.sha256("\n".join(value).encode()).hexdigest() for key, value in out.items()}


def masked(text: str) -> str:
    """An instruction with its addresses, displacements and numbers masked. objdump's annotation
    runs to the line's end, and a generic's name holds `>` of its own: the mask takes all of it
    (ledger 7319 L.1). AArch64 prints a page offset in decimal, so every number goes (7184 Q.3)."""
    text = re.sub(r"-?0x[0-9a-f]+|\b[0-9a-f]{6,}\b", "#", text)
    text = re.sub(r"#-?[0-9]+", "#", text)
    return re.sub(r"<.*>", "<>", text)


def llvm_objdump() -> str | None:
    """The toolchain's llvm-objdump (rustup's llvm-tools), which reads every target's ELF."""
    try:
        sysroot = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True, text=True,
                                 check=True).stdout.strip()
        host = next(line.split()[1] for line in subprocess.run(
            ["rustc", "-vV"], capture_output=True, text=True, check=True).stdout.splitlines()
            if line.startswith("host:"))
    except (OSError, subprocess.CalledProcessError, StopIteration):
        return None
    path = os.path.join(sysroot, "lib", "rustlib", host, "bin", "llvm-objdump")
    return path if os.path.exists(path) else None


def listed_bodies(path: str, objdump: str) -> list[tuple[str, list[str]]]:
    """Every text symbol with code, keyed by name and address, its instructions masked."""
    run = subprocess.run([objdump, "-d", "-C", "--no-show-raw-insn", path],
                         capture_output=True, text=True, check=True)
    out: list[tuple[str, list[str]]] = []
    for line in run.stdout.splitlines():
        head = re.match(r"^([0-9a-f]+) <(.+)>:$", line)
        if head:
            out.append((f"{head.group(2)} @{head.group(1)}", []))
        elif out and "\t" in line:
            out[-1][1].append(masked(line.split("\t", 1)[1]))
    return [(name, lines) for name, lines in out if lines]


def relax(lines: list[str]) -> list[str]:
    """AArch64's linker writes `adrp R, T; add R, R, :lo12:T` as `nop; adr R, T` where T is
    near: a moved layout swaps one for the other, the same address."""
    out, at = [], 0
    while at < len(lines):
        first = re.sub(r"\s+", " ", lines[at]).strip()
        second = re.sub(r"\s+", " ", lines[at + 1]).strip() if at + 1 < len(lines) else ""
        near = re.match(r"adr (\w+), # <>$", second)
        if first == "nop" and near:
            out += [f"adrp {near.group(1)}, # <>", f"add {near.group(1)}, {near.group(1)}, ##"]
            at += 2
        else:
            out.append(lines[at])
            at += 1
    return out


def strong(lines: list[str]) -> str:
    """A masked body, masked also of what a crate move moves without changing code."""
    lines = relax(lines)
    while lines and lines[-1].strip() in ("int3", "nop"):
        lines = lines[:-1]
    kept = []
    for line in lines:
        line = re.sub(r"\[(\w+)\]$", r"[\1, ##]", line)
        line = re.sub(r"\s*(?:[#@] imm = |// =).*$", "", line)
        line = re.sub(r"\$?-?\b\d{4,}\b", "N", line)
        kept.append(re.sub(r"\s+", " ", line).strip())
    return "\n".join(kept)


def moved_bodies(base: list[tuple[str, list[str]]], head: list[tuple[str, list[str]]]) -> tuple[bool, dict]:
    """Whether two kernels' bodies are equal as a multiset under the strong mask, names
    ignored, and the opcodes of the instructions that differ beyond spacing, pair by pair
    for the bodies one name holds once in each."""
    was = collections.Counter(strong(lines) for _, lines in base)
    now = collections.Counter(strong(lines) for _, lines in head)
    def single(listed: list[tuple[str, list[str]]]) -> dict[str, list[str]]:
        held = collections.Counter(name.rsplit(" @", 1)[0] for name, _ in listed)
        return {name.rsplit(" @", 1)[0]: lines for name, lines in listed if held[name.rsplit(" @", 1)[0]] == 1}
    once_was, once_now = single(base), single(head)
    opcodes: collections.Counter = collections.Counter()
    for name in set(once_was) & set(once_now):
        for x, y in zip(once_was[name], once_now[name]):
            if re.sub(r"\s+", " ", x) != re.sub(r"\s+", " ", y):
                opcodes[x.split()[0] if x.split() else ""] += 1
    return was == now, dict(opcodes)


def self_test() -> int:
    base = table(["0 10 T a::b::h0123456789abcdef", "10 8 T c", "x"])
    assert base == {"a::b": 16, "c": 8}
    same = table(["20 10 T a::b::hfedcba9876543210", "30 8 T c"])
    assert not differences(base, same), "a hash alone is a difference"
    assert differences(base, table(["20 11 T a::b", "30 8 T c"])) == ["size: a::b: 16 -> 17"]
    assert differences(base, table(["0 10 T a::b"])) == ["gone: c"]
    assert differences(base, table(["0 10 T a::b", "10 8 T c", "20 1 T d"])) == ["new: d"]
    # An adrp beside another generic is one instruction; a changed one is still a change (7319 L.1).
    beside = "adrp\tx9, 0xffff8000 <<a::B<c::D> as e::F>::g+0x10>"
    moved = "adrp\tx9, 0xffff9000 <<h::I<j::K> as e::F>::l+0x20>"
    assert masked(beside) == masked(moved), "an address's neighbour read as changed code"
    assert masked("add\tx0, x0, #1472") == masked("add\tx0, x0, #24")
    assert masked("ldr\tx1, [x0]") != masked("ldr\tx2, [x0]"), "a changed register was not seen"
    assert masked("add\tx0, x0, #8 <a<b>>") != masked("sub\tx0, x0, #8 <a<b>>"), "a changed opcode was not seen"
    pairs = renames("a::m::=b::m::")
    moved_out = renamed({"a::m::f": 1, "<a::m::T>::g": 2, "xa::m::h": 3, "a::n::f": 4}, pairs)
    assert moved_out == {"b::m::f": 1, "<b::m::T>::g": 2, "xa::m::h": 3, "a::n::f": 4}, moved_out
    assert not differences(renamed(table(["0 10 T a::m::f"]), pairs), table(["0 10 T b::m::f"])), \
        "a recorded rename was read as a difference"
    assert differences(table(["0 10 T a::m::f"]), table(["0 10 T b::m::f"])), "a rename unrecorded passed"
    assert relax(["nop", "adr\tx2, # <>"]) == ["adrp x2, # <>", "add x2, x2, ##"]
    near, far = ["mov\tx0, ##", "nop", "adr\tx8, # <>"], ["mov\tx0, ##", "adrp\tx8, # <>", "add\tx8, x8, ##"]
    assert strong(near) == strong(far), "a relaxed adrp read as changed code"
    assert strong(["movabsq\t$-1234567, %rcx # imm = 0xAB"]) == strong(["movabsq\t$-7654321, %rcx # imm = 0xCD"])
    assert strong(["ret", "int3", "int3"]) == strong(["ret"]), "padding read as code"
    assert strong(["ldr\tx1, [x0]"]) != strong(["ldr\tx2, [x0]"]), "a changed register was not seen"
    assert strong(["add\tx0, x0, #8"]) != strong(["sub\tx0, x0, #8"]), "a changed opcode was not seen"
    swapped_base = [("a::f @1", ["ret"]), ("b::g @2", ["nop", "adr\tx1, # <>"])]
    swapped_head = [("c::h @1", ["adrp\tx1, # <>", "add\tx1, x1, ##"]), ("a::f @2", ["ret"])]
    assert moved_bodies(swapped_base, swapped_head)[0], "names swapped by folding read as a change"
    assert not moved_bodies(swapped_base, [("a::f @1", ["ret"]), ("b::g @2", ["ret"])])[0], \
        "a changed body passed as a move"
    print("compare-kernel-symbols: self-test passed")
    return 0


def main() -> int:
    if "--self-test" in sys.argv:
        return self_test()
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    move = os.environ.get("FERRIX_SYMBOLS_CRATE_MOVE", "")
    if move:
        return crate_move(sys.argv[1], sys.argv[2], move)
    pairs = renames(os.environ.get("FERRIX_SYMBOLS_RENAME", ""))
    for old, new in pairs:
        print(f"compare-kernel-symbols: the base's {old} read as {new} (a recorded move)")
    base, head = renamed(symbols(sys.argv[1]), pairs), symbols(sys.argv[2])
    found = differences(base, head)
    for line in found:
        print(line)
    print(f"compare-kernel-symbols: {len(base)} and {len(head)} symbols, {len(found)} differences")
    # A report beside it, failing nothing: bodies that moved though their size did not.
    was, now = renamed(bodies(sys.argv[1]), pairs), bodies(sys.argv[2])
    moved = sorted(name for name in set(was) & set(now) if was[name] != now[name])
    for name in moved:
        print(f"body: {name}")
    print(f"compare-kernel-symbols: {len(was)} and {len(now)} bodies, {len(moved)} differ with addresses masked")
    return 1 if found else 0


def crate_move(base_elf: str, head_elf: str, why: str) -> int:
    """FERRIX_SYMBOLS_CRATE_MOVE's verdict (the docstring): the bodies', names a report."""
    print(f"compare-kernel-symbols: a recorded crate move ({why}): names reported, bodies judged")
    base, head = symbols(base_elf), symbols(head_elf)
    found = differences(base, head)
    sizes_equal = collections.Counter(base.values()) == collections.Counter(head.values())
    print(f"compare-kernel-symbols: {len(base)} and {len(head)} symbols, {len(found)} name or size "
          f"differences, sizes as a multiset {'equal' if sizes_equal else 'NOT equal'}")
    objdump = llvm_objdump()
    if objdump is None:
        print("compare-kernel-symbols: no llvm-objdump in the toolchain: bodies not compared")
        return 1
    was, now = listed_bodies(base_elf, objdump), listed_bodies(head_elf, objdump)
    equal, opcodes = moved_bodies(was, now)
    print(f"compare-kernel-symbols: {len(was)} and {len(now)} bodies; under the strong mask, names "
          f"ignored, {'EQUAL' if equal else 'NOT EQUAL'}; differing beyond spacing, by opcode: {opcodes}")
    return 0 if equal and sizes_equal and len(base) == len(head) and len(was) == len(now) else 1


if __name__ == "__main__":
    sys.exit(main())
