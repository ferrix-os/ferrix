#!/usr/bin/env python3
"""Two kernels' symbols and sizes, for what is said to leave a build unchanged
(docs/microkernel/M6.md §4, ledger 5942 M.8): `present` and `desktop` against the base,
per architecture. A report, read by a person with the four boots beside it: a generic's
hash or an inlining moved is not a change of behaviour, and a name gone or a size moved
says where to look. Exit 1 on any difference of name or size, ignoring symbols' hashes.

Usage:  compare-kernel-symbols.py BASE_ELF HEAD_ELF [--self-test]
"""
import re
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


def symbols(path: str) -> dict[str, int]:
    run = subprocess.run(
        ["nm", "-S", "-C", "--defined-only", path], capture_output=True, text=True, check=True
    )
    return table(run.stdout.splitlines())


def self_test() -> int:
    base = table(["0 10 T a::b::h0123456789abcdef", "10 8 T c", "x"])
    assert base == {"a::b": 16, "c": 8}
    same = table(["20 10 T a::b::hfedcba9876543210", "30 8 T c"])
    assert not differences(base, same), "a hash alone is a difference"
    assert differences(base, table(["20 11 T a::b", "30 8 T c"])) == ["size: a::b: 16 -> 17"]
    assert differences(base, table(["0 10 T a::b"])) == ["gone: c"]
    assert differences(base, table(["0 10 T a::b", "10 8 T c", "20 1 T d"])) == ["new: d"]
    print("compare-kernel-symbols: self-test passed")
    return 0


def main() -> int:
    if "--self-test" in sys.argv:
        return self_test()
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    base, head = symbols(sys.argv[1]), symbols(sys.argv[2])
    found = differences(base, head)
    for line in found:
        print(line)
    print(f"compare-kernel-symbols: {len(base)} and {len(head)} symbols, {len(found)} differences")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
