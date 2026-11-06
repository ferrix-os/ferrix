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
"""
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
    print("compare-kernel-symbols: self-test passed")
    return 0


def main() -> int:
    if "--self-test" in sys.argv:
        return self_test()
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
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


if __name__ == "__main__":
    sys.exit(main())
