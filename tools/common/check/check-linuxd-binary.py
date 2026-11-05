#!/usr/bin/env python3
"""linuxd's binary holds neither an `%fs:` load nor any x87, MMX, SSE or AVX state (ledger 6464 Z3,
6093 (9)(d), 7216, 7249 N.2): K1's fast path leaves FS base unchanged and vector state lazy on the
promise that normal mode's code never reads them. Disassembles every executable section of the x86-64
ELF (`objdump -d`: what it marks code; data in a code section would read as `(bad)`, refused) and
refuses each, named by address: `%fs:`; `%xmm`, `%ymm`, `%zmm`, `%mm`, `%st`, `%k0-7`; the mnemonics
that touch that state with no such operand; and what the disassembler cannot read.

Usage:  check-linuxd-binary.py LINUXD_ELF [--self-test]
"""
import re
import subprocess
import sys

FS = re.compile(r"%fs:")
VECTOR = re.compile(r"%[xyz]mm[0-9]|%mm[0-7]\b|%st\b|%st\(|%k[0-7]\b")
# The mnemonic is the instruction's first word; `f...` and `x...` state with no register named.
STATE = re.compile(
    r"^\s*[0-9a-f]+:\s+(?:\S+\s+)?(fxsave|fxrstor|xsave|xrstor|ldmxcsr|stmxcsr|emms|femms|fninit|finit|"
    r"fnstcw|fldcw|fnstsw|fnsave|frstor|vzeroall|vzeroupper|vldmxcsr|vstmxcsr)[a-z0-9]*\b"
)
UNREAD = re.compile(r"\(bad\)")


def judge(lines: list[str]) -> list[str]:
    problems = []
    for line in lines:
        if FS.search(line):
            problems.append(f"an FS load in normal mode's code: {line.strip()}")
        elif VECTOR.search(line):
            problems.append(f"a vector register in normal mode's code: {line.strip()}")
        elif STATE.search(line):
            problems.append(f"vector state in normal mode's code: {line.strip()}")
        elif UNREAD.search(line):
            problems.append(f"an instruction the disassembler cannot read: {line.strip()}")
    return problems


def self_test() -> int:
    assert judge(["  1000:\tmov    %rax,%rbx"]) == []
    assert "FS load" in judge(["  1000:\tmov    %fs:0x28,%rax"])[0]
    assert "vector" in judge(["  1000:\tpxor   %xmm0,%xmm0"])[0]
    assert "vector" in judge(["  1000:\tvmovdqu %ymm3,(%rax)"])[0]
    assert "vector" in judge(["  1000:\tfld    %st(1)"])[0]
    assert "vector" in judge(["  1000:\tmovq   %mm0,%rax"])[0]
    assert "vector" in judge(["  1000:\tkmovw  %k1,%eax"])[0]
    for mnemonic in ("fxsave64 (%rax)", "xsaves (%rax)", "xrstor (%rax)", "ldmxcsr (%rax)", "emms", "fninit", "vzeroupper"):
        assert "vector state" in judge([f"  1000:\t{mnemonic}"])[0], mnemonic
    assert "cannot read" in judge(["  1000:\t(bad)"])[0]
    print("check-linuxd-binary: self-test passed")
    return 0


def main() -> int:
    if "--self-test" in sys.argv:
        return self_test()
    path = sys.argv[1]
    run = subprocess.run(["objdump", "-d", "--no-show-raw-insn", path],
                         capture_output=True, text=True, check=True)
    lines = run.stdout.splitlines()
    problems = judge(lines)
    for problem in problems[:20]:
        print(f"check-linuxd-binary: {problem}")
    if problems:
        print(f"check-linuxd-binary: FAILED, {len(problems)} instructions")
        return 1
    print(f"check-linuxd-binary: {len(lines)} lines of {path}, no FS load, no vector register")
    return 0


if __name__ == "__main__":
    sys.exit(main())
