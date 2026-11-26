#!/usr/bin/env python3
"""Each flag value's own code is in the binary built with it and out of the one built with another
(ledger 7016 B.2, B.4-B.6): by `nm`, on the kernel and on linuxd of one build, against the values
that build was given.

A symbol below belongs to one value of one flag, on the architectures named and while the flags in
`needs` have their values. In a build where all of that holds, the symbol must be in the binary; in
any other build it must be out. A value whose own code is inline (`exit`'s barrier at
`restricted_left`, `off`'s general way, `fault_ahead=off`'s fault of one page) has no symbol of its
own: what this shows for it is the other value's code out, and its own rows show the rest.

An optimised build may inline a function into its one caller, so a symbol's absence proves its code
out only where the same build shows it in under its own value. A release build (`--release`, LTO)
is judged for "out" alone, and says so; "in" is read on the development build.

Usage:  check-value-symbols.py --arch ARCH --words "flag=value ..." --kernel ELF [--linuxd ELF] [--release]
        check-value-symbols.py --self-test
Exit 1 names each symbol in where it must be out, or out where it must be in.
"""
import argparse
import glob
import os
import re
import subprocess
import sys

# (flag, the value whose code it is, binary, the symbol's text after demangling with the angle
# brackets taken out, the architectures that value builds it on, flags it also needs).
VALUE_SYMBOLS = (
    # K1's fast path: x86-64's alone (build.rs's `ferrix_fast_restricted`), its check with it.
    ("restricted_fastpath", "on", "kernel", "ferrix_syscall_fast", ("x86_64",), ()),
    ("restricted_fastpath", "on", "kernel", "fast_check::restricted_fast_counts", ("x86_64",),
     (("checks", "on"),)),
    # A partition switch's word, which region_check's domain case reads (ledger 7249 N.1).
    ("partition_speculation", "partition", "kernel", "speculation_check::switch_barrier_offered",
     ("x86_64", "aarch64"), (("checks", "on"),)),
    # linuxd's fault that brings neighbours (lift/core/fill.rs).
    ("fault_ahead", "on", "linuxd", "AddressSpace::fault_around", ("x86_64", "aarch64"), ()),
    ("fault_ahead", "on", "linuxd", "AddressSpace::write_ahead", ("x86_64", "aarch64"), ()),
    # K16.9 (L.object.339, ledger 10505 C6, 10548 Z6 (ii)): ring 0's growing globals' growing
    # paths are built under kernel_memory=heap alone, and the fixed form under retyped alone.
    ("kernel_memory", "heap", "kernel", "user::address_wait::grow", ("x86_64", "aarch64"), ()),
    ("kernel_memory", "retyped", "kernel", "object::process::fixed_entries", ("x86_64", "aarch64"),
     (("checks", "on"),)),
)


def values_of(words: str) -> dict[str, str]:
    return dict(word.split("=", 1) for word in words.split() if "=" in word)


HASH = re.compile(r"::h[0-9a-f]{16}$")


def defined(text: str) -> set[str]:
    """The names nm printed, each whole: a line's third field on, Rust's `::h<hash>` taken off."""
    names = set()
    for line in text.splitlines():
        fields = line.split(None, 2)
        if len(fields) == 3:
            names.add(HASH.sub("", fields[2].replace("<", "").replace(">", "")))
        elif len(fields) == 2:
            names.add(HASH.sub("", fields[1].replace("<", "").replace(">", "")))
    return names


def holds(names: set[str], symbol: str) -> bool:
    """Whether `symbol` is a name of `names` whole, or the last path segments of one."""
    return any(name == symbol or name.endswith(f"::{symbol}") for name in names)


def judge(arch: str, values: dict[str, str], names: dict[str, set[str] | None], release: bool,
          table: tuple = VALUE_SYMBOLS) -> tuple[list[str], list[str], int]:
    """What was read (lines to print), what breaks (problems), and how many symbols were judged."""
    read, problems, judged = [], [], 0
    for flag, value, binary, symbol, arches, needs in table:
        missing = [name for name in (flag, *(name for name, _ in needs)) if name not in values]
        if missing:
            problems.append(f"{arch}: `{symbol}`: the build's words name no {', '.join(missing)}")
            continue
        text = names.get(binary)
        if text is None:
            read.append(f"{arch}: `{symbol}`: NOT READ, no {binary} given")
            continue
        judged += 1
        wanted = (
            values[flag] == value
            and arch in arches
            and all(values.get(name) == want for name, want in needs)
        )
        found = holds(text, symbol)
        where = f"{arch}: `{symbol}` ({flag}={value}'s) under {flag}={values[flag]}"
        if found and not wanted:
            problems.append(f"{where}: in the {binary}, where it must be out")
        elif wanted and not found and not release:
            problems.append(f"{where}: out of the {binary}, where it must be in")
        elif wanted and not found:
            read.append(f"{where}: out of the release {binary}; not judged, LTO may inline it")
        else:
            read.append(f"{where}: {'in' if found else 'out'}, as the value says")
    return read, problems, judged


def nm_tool() -> list[str]:
    """The pinned toolchain's llvm-nm (rust-toolchain.toml's llvm-tools), which reads all three
    architectures alike; PATH's nm only where it is not installed, and said."""
    run = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True, text=True, check=False)
    found = sorted(glob.glob(os.path.join(run.stdout.strip(), "lib", "rustlib", "*", "bin", "llvm-nm")))
    if run.returncode == 0 and found:
        return [found[0], "--demangle"]
    print("check-value-symbols: no llvm-nm in the pinned toolchain, PATH's nm reads instead")
    return ["nm", "--demangle=rust"]


def symbols(tool: list[str], path: str) -> set[str]:
    run = subprocess.run([*tool, path], capture_output=True, text=True, check=False)
    names = defined(run.stdout) if run.returncode == 0 else set()
    if not names:
        raise SystemExit(f"check-value-symbols: {tool[0]} read nothing of {path}")
    return names


def self_test() -> int:
    five = VALUE_SYMBOLS[:5]
    fast = {"restricted_fastpath": "on", "partition_speculation": "partition", "fault_ahead": "on", "checks": "on"}
    slow = {"restricted_fastpath": "off", "partition_speculation": "exit", "fault_ahead": "off", "checks": "on"}
    kernel_in = (
        "ffffffff80001000 T ferrix_syscall_fast\n"
        "ffffffff80002000 t ferrix_kernel_core::arch::fast_check::restricted_fast_counts::h0123456789abcdef\n"
        "ffffffff80003000 T ferrix_kernel_core::arch::speculation_check::switch_barrier_offered\n"
    )
    linuxd_in = (
        "0000710000398680 t <ferrix_kernel_core::user::space::AddressSpace>::fault_around\n"
        "00007100003982e4 t <ferrix_kernel_core::user::space::AddressSpace>::write_ahead\n"
    )
    full = {"kernel": defined(kernel_in), "linuxd": defined(linuxd_in)}
    empty = {"kernel": defined("ffffffff80001000 T ferrix_syscall_stub\n"), "linuxd": defined("0000000000001000 T _start\n")}
    assert judge("x86_64", fast, full, False, five)[1] == []
    assert judge("x86_64", fast, full, False, five)[2] == 5
    assert judge("x86_64", slow, empty, False, five)[1] == []
    assert len(judge("x86_64", slow, full, False, five)[1]) == 5
    assert len(judge("x86_64", fast, empty, False, five)[1]) == 5
    # Release: "in" is not judged, "out" is.
    assert judge("x86_64", fast, empty, True, five)[1] == []
    assert len(judge("x86_64", slow, full, True, five)[1]) == 5
    # AArch64 has no fast path under either value: the two symbols must be out under `on` too.
    arm = {"kernel": full["kernel"] - {"ferrix_syscall_fast"}, "linuxd": full["linuxd"]}
    assert "restricted_fast_counts" in judge("aarch64", fast, arm, False, five)[1][0]
    no_fast = {"kernel": defined("ffffffff80003000 T ferrix_kernel_core::arch::speculation_check::switch_barrier_offered\n"),
               "linuxd": full["linuxd"]}
    assert judge("aarch64", fast, no_fast, False, five)[1] == []
    # A check-only symbol is out where the checks are.
    assert judge("x86_64", dict(fast, checks="off"), {"kernel": defined("1 T ferrix_syscall_fast\n"), "linuxd": full["linuxd"]}, False, five)[1] == []
    # No linuxd: its symbols are named NOT READ, never passed silently, and not counted as judged.
    read, problems, judged = judge("x86_64", slow, {"kernel": empty["kernel"]}, False, five)
    assert problems == [] and judged == 3 and sum("NOT READ" in line for line in read) == 2
    # X1 (ledger 10144): a flag of the table, or one a symbol needs, absent from the words fails by name.
    _, problems, _ = judge("x86_64", {k: v for k, v in slow.items() if k != "fault_ahead"}, empty, False, five)
    assert len(problems) == 2 and all("name no fault_ahead" in problem for problem in problems)
    _, problems, _ = judge("x86_64", {k: v for k, v in fast.items() if k != "checks"}, full, False, five)
    assert sum("name no checks" in problem for problem in problems) == 2
    # X2: a longer name that begins with a symbol's text is not that symbol, in or out.
    longer = {"kernel": defined("1 T ferrix_syscall_fast_unrelated\n2 t a::fast_check::restricted_fast_counts_of\n"),
              "linuxd": defined("3 t ferrix_kernel_core::user::space::AddressSpace::fault_around_count\n")}
    assert judge("x86_64", slow, longer, False, five)[1] == []
    assert len(judge("x86_64", fast, longer, False, five)[1]) == 5
    # K16.9: the growing paths under heap, the fixed form under retyped, each alone.
    k169 = VALUE_SYMBOLS[5:]
    grows = defined("1 T ferrix_kernel_core::user::address_wait::grow\n2 T ferrix_kernel_core::user::address_wait::shrink\n")
    fixed = defined("3 T ferrix_kernel_core::object::process::fixed_entries\n")
    heap, retyped = {"kernel_memory": "heap", "checks": "on"}, {"kernel_memory": "retyped", "checks": "on"}
    assert judge("x86_64", heap, {"kernel": grows}, False, k169)[1] == []
    assert judge("aarch64", retyped, {"kernel": fixed}, False, k169)[1] == []
    assert len(judge("x86_64", retyped, {"kernel": grows}, False, k169)[1]) == 2
    assert len(judge("x86_64", heap, {"kernel": fixed}, False, k169)[1]) == 2
    print("check-value-symbols: self-test passed")
    return 0


def main() -> int:
    if "--self-test" in sys.argv:
        return self_test()
    parser = argparse.ArgumentParser()
    parser.add_argument("--arch", required=True)
    parser.add_argument("--words", required=True)
    parser.add_argument("--kernel", required=True)
    parser.add_argument("--linuxd")
    parser.add_argument("--release", action="store_true")
    args = parser.parse_args()
    tool = nm_tool()
    names = {"kernel": symbols(tool, args.kernel), "linuxd": symbols(tool, args.linuxd) if args.linuxd else None}
    read, problems, judged = judge(args.arch, values_of(args.words), names, args.release)
    for line in read:
        print(f"check-value-symbols: {line}")
    for problem in problems:
        print(f"check-value-symbols: {problem}")
    if problems:
        print(f"check-value-symbols: FAILED, {len(problems)} symbols")
        return 1
    print(
        f"check-value-symbols: {args.arch}: {judged} of {len(VALUE_SYMBOLS)} symbols judged, "
        "each in or out as its build's values say"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
