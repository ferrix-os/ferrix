#!/usr/bin/env python3
"""The ELF half of docs/KERNEL-CRATES.md section 6: whether a removable crate
is in a linked kernel.

A removable crate (`composition_root.removable` in
tools/common/data/certification-item.json) is linked only into a kernel whose
composition root names it, under one value of its flag. Cargo lists it in
every build, so the dependency of record is the linked binary itself
(ledger 3558 (11), 3623 (R1)): a kernel built with any other value holds no
symbol of the crate, nor any of the load's mechanisms only the crate calls
(`absent_symbols`); one built with the present value holds at least one of
the crate's. The symbols are read with `nm --demangle` from the ELF the
build left, release, on each architecture (`cargo xtask test-removable`).

    check-removable-symbols.py --elf KERNEL --crate ferrix-kernel-settings --value fixed
    check-removable-symbols.py --self-test
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent.parent
MANIFEST = ROOT / "tools" / "common" / "data" / "certification-item.json"


def judge(names: list[str], entry: dict, value: str) -> list[str]:
    """What is wrong with a kernel whose demangled symbol `names` were built
    with `value` of `entry`'s flag."""
    crate = entry.get("symbol", "")
    mine = [name for name in names if f"{crate}::" in name or name.startswith(f"{crate}::")]
    if value == entry.get("present_when"):
        if not mine:
            return [f"built with {entry['flag']}={value}, and no symbol of {crate} is in it: "
                    f"the composition root does not link the crate"]
        return []
    problems = [f"built with {entry['flag']}={value}, and it holds {len(mine)} symbol(s) of {crate}, "
                f"the first {mine[0]}" ] if mine else []
    for absent in entry.get("absent_symbols", []):
        held = [name for name in names if name == absent or name.startswith(absent + "::")]
        if held:
            problems.append(f"built with {entry['flag']}={value}, and it holds {held[0]}, which only {crate} calls")
    return problems


def seam_problems(elf: Path, names: list[str], entry: dict, value: str) -> list[str]:
    """The seam's symbols in a kernel built with `value` (ledger 3934 (4)):
    each `__ferrix_*` symbol the record lists defined exactly once, and in a
    build with the present value each one's code calling into the crate (its
    implementation's methods are out of line for this); in another build the
    load's `Absent`, inlined or not, and nothing of the crate (`judge`)."""
    problems = []
    crate = entry.get("symbol", "")
    for symbol in entry.get("seam_symbols", []):
        count = sum(1 for name in names if name == symbol)
        if count != 1:
            problems.append(f"{symbol} is defined {count} time(s), not once")
            continue
        if value == entry.get("present_when") and not reaches(elf, symbol, crate):
            problems.append(f"built with {entry['flag']}={value}, and {symbol} does not call into {crate}")
    return problems


def reaches(elf: Path, symbol: str, crate: str) -> bool:
    """Whether `symbol`'s code in `elf` calls or jumps to a symbol of `crate`."""
    tool = llvm_objdump()
    out = subprocess.run([tool, "-d", "-C", f"--disassemble-symbols={symbol}", str(elf)],
                         capture_output=True, text=True, check=False).stdout
    return any(f"<{crate}::" in line or f"<<{crate}::" in line for line in out.splitlines())


def llvm_objdump() -> str:
    """LLVM's objdump, which reads every architecture's ELF: on PATH, or the
    toolchain's `llvm-tools` component."""
    found = shutil.which("llvm-objdump")
    if found:
        return found
    sysroot = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True, text=True,
                             check=True).stdout.strip()
    for candidate in sorted(Path(sysroot).glob("lib/rustlib/*/bin/llvm-objdump")):
        return str(candidate)
    raise SystemExit("removable-symbols: no llvm-objdump (rustup component add llvm-tools)")


def symbols(elf: Path) -> list[str]:
    tool = shutil.which("llvm-nm") or shutil.which("nm")
    if tool is None:
        raise SystemExit("removable-symbols: neither llvm-nm nor nm is on PATH")
    out = subprocess.run([tool, "--demangle", str(elf)], capture_output=True, text=True, check=True).stdout
    names = []
    for line in out.splitlines():
        parts = line.split(" ", 2)
        if len(parts) == 3:
            names.append(parts[2].strip())
    if not names:
        raise SystemExit(f"removable-symbols: {elf} has no symbol table; the check cannot establish an absence")
    return names


def self_test() -> list[str]:
    entry = {"flag": "runtime_settings", "present_when": "live", "symbol": "ferrix_kernel_settings",
             "absent_symbols": ["<ferrix_kernel_load::syscall::system::UtsNamespace>::set_hostname"]}
    live = ["<ferrix_kernel_settings::Live as ferrix_kernel_load::setters::Setters>::sethostname",
            "__ferrix_setters_sethostname"]
    fixed = ["__ferrix_setters_sethostname"]
    failures = []
    if judge(live, entry, "live") or judge(fixed, entry, "fixed"):
        failures.append("a live kernel with the crate or a fixed one without it was refused")
    if not judge(fixed, entry, "live"):
        failures.append("a live kernel without the crate passed")
    if not judge(live, entry, "fixed"):
        failures.append("a fixed kernel with the crate passed")
    leaked = fixed + ["<ferrix_kernel_load::syscall::system::UtsNamespace>::set_hostname"]
    if not judge(leaked, entry, "fixed"):
        failures.append("a fixed kernel holding a mechanism only the crate calls passed")
    seam = dict(entry, seam_symbols=["__ferrix_setters_sethostname"])
    if seam_problems(Path("/nonexistent"), fixed, seam, "fixed"):
        failures.append("a fixed kernel with its seam symbol once was refused")
    if not seam_problems(Path("/nonexistent"), fixed + ["__ferrix_setters_sethostname"], seam, "fixed"):
        failures.append("a seam symbol defined twice passed")
    if not seam_problems(Path("/nonexistent"), [], seam, "fixed"):
        failures.append("a seam symbol not defined passed")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--elf", type=Path)
    parser.add_argument("--crate")
    parser.add_argument("--value")
    args = parser.parse_args()
    failures = self_test()
    if failures:
        for failure in failures:
            print(f"removable-symbols: self-test: {failure}", file=sys.stderr)
        return 1
    if args.self_test:
        print("removable-symbols: self-tests pass")
        return 0
    if not (args.elf and args.crate and args.value):
        parser.error("--elf, --crate and --value are needed")
    with MANIFEST.open(encoding="utf-8") as handle:
        removable = json.load(handle).get("composition_root", {}).get("removable", {})
    entry = removable.get(args.crate)
    if entry is None:
        print(f"removable-symbols: {args.crate} is not in composition_root.removable", file=sys.stderr)
        return 1
    names = symbols(args.elf)
    problems = judge(names, entry, args.value) + seam_problems(args.elf, names, entry, args.value)
    if problems:
        for problem in problems:
            print(f"removable-symbols: {args.elf}: {problem}", file=sys.stderr)
        return 1
    held = sum(1 for name in names if f"{entry['symbol']}::" in name)
    print(f"removable-symbols: {args.elf}: {entry['flag']}={args.value}, {held} symbol(s) of {entry['symbol']}"
          f"{'' if held else ' and none of the ' + str(len(entry.get('absent_symbols', []))) + ' mechanisms only it calls'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
