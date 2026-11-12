#!/usr/bin/env python3
"""A native program's deepest path against its stack (the Intel driver, 2026-11-12).

    check-native-stacks.py <program ELF>... [--chains] [--self-test]

A native process starts on `NATIVE_STACK_SIZE` bytes of stack (`src/lib/proto/native-abi/src/types.rs`,
8 MiB), with a guard below. On 2026-11-12 the Intel GPU's driver `intel` built its front end's tables
(about 3 MiB) by value: `Tables::new`'s frame was 4.5 MiB and its caller's 3.7 MiB, so the walk from
`_start` to that frame's stack probe was 8.24 MiB, and the probe faulted below the stack on the N150 at
step 23. No row booted it: QEMU has no Intel GPU, so `test-intel-uapi` and `test-restart --boot intel`
boot `intel-fake` and the keeper in its place, whose same tables sat at 6.8 and 7.4 MiB.

This reads each function's frame and its calls with `check-stack-depth`'s reader and walk (check-frames'
probe-aware frame reader under it), and fails when a program's deepest chain is more than `BUDGET` of its
stack: what a lower bound leaves out (a call through a `dyn` or a function pointer is not followed, nor
is anything a signal handler adds) gets the rest. The chain starts at `_start` and also at the program's
own `main`, which the runtime calls through a pointer in a build with no LTO, below the runtime's own
frames. A GOT slot of a fixed-address program holds its target itself (no relocation), and is read from
the image. A program with no `_start` or no `main` fails: a renamed path is walked again, not skipped.

Like `check-stack-depth`, a lower bound, and read as one.
"""
import importlib.util
import re
import struct
import subprocess
import sys
from pathlib import Path

# The share of the stack the deepest walked chain may take. A quarter: `intel` read 0.85 MiB after its
# front end moved off the stack, 8.24 MiB before, and 7.92 MiB the day before the change that faulted.
BUDGET = (1, 4)
# The runtime's way to a program's `main` (`ferrix_rt::entry!`): what is on the stack under it.
RUNTIME = ("_start", "ferrix_rt_start", "ferrix_rt::start")
# What an x86-64 call puts on the stack beyond the callee's frame: its return address.
CALL_BYTES = 8


def stack_depth():
    """`check-stack-depth`: its objdump, frame and call reader, and walk."""
    path = Path(__file__).with_name("check-stack-depth.py")
    spec = importlib.util.spec_from_file_location("check_stack_depth", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def native_stack_bytes(source=None):
    """`NATIVE_STACK_SIZE`: the stack a native process starts on."""
    source = source or Path("src/lib/proto/native-abi/src/types.rs").read_text(encoding="utf-8")
    found = re.search(r"pub const NATIVE_STACK_SIZE: u64 = ([0-9_ *]+);", source)
    if not found:
        raise SystemExit("check-native-stacks: no NATIVE_STACK_SIZE in native-abi's types.rs")
    value = 1
    for factor in found.group(1).replace("_", "").split("*"):
        value *= int(factor.strip())
    return value


class Slots:
    """x86-64: each GOT slot's target. A position-independent image's comes from its relocations
    (`check-stack-depth.got_targets`); a fixed-address program's slot holds the address itself."""

    def __init__(self, elf, relocated):
        self.relocated, self.sections = relocated, []
        if elf[:4] != b"\x7fELF" or elf[4] != 2 or elf[5] != 1:
            return
        shoff = struct.unpack_from("<Q", elf, 0x28)[0]
        shentsize, shnum = struct.unpack_from("<HH", elf, 0x3A)
        for index in range(shnum):
            _, kind, _, address, offset, size, _, _, _, _ = struct.unpack_from(
                "<IIQQQQIIQQ", elf, shoff + index * shentsize
            )
            if kind == 1 and address:  # SHT_PROGBITS, loaded
                self.sections.append((address, elf[offset : offset + size]))

    def get(self, slot):
        if slot in self.relocated:
            return self.relocated[slot]
        for address, data in self.sections:
            if address <= slot and slot + 8 <= address + len(data):
                return struct.unpack_from("<Q", data, slot - address)[0]
        return None


def program_main(frames, name):
    """The program's `main`: `<crate>::main`, the crate its binary's name with `-` as `_`."""
    wanted = f"{name.replace('-', '_')}::main"
    return wanted if wanted in frames else None


def judge(frames, calls, name, stack, show_chains=False, unresolved=0, unfollowed=0):
    """Lines to print and problems, for one program."""
    depth = stack_depth()
    walk = depth.Walk(frames, calls, lambda _: False, CALL_BYTES)
    lines, problems = [], []
    main = program_main(frames, name)
    for part, found in (("_start", "_start" in frames), (f"{name.replace('-', '_')}::main", main)):
        if not found:
            problems.append(f"{name}: no `{part}`: the path to the program was renamed, walk it again")
    if problems:
        return lines, problems
    best = (walk.depth("_start"), walk.chain("_start"))
    under = [part for part in RUNTIME if part in frames]
    through_main = sum(frames[part] + CALL_BYTES for part in under) + walk.depth(main)
    if through_main > best[0]:
        best = (through_main, tuple(under) + walk.chain(main))
    budget = stack * BUDGET[0] // BUDGET[1]
    largest = max(frames.items(), key=lambda item: (item[1], item[0]))
    lines.append(
        f"{name}: deepest chain {best[0]} bytes ({best[0] / 1048576:.2f} MiB) of a {stack >> 20} MiB stack, "
        f"budget {budget} ({BUDGET[0]}/{BUDGET[1]}); largest frame {largest[1]} bytes, {largest[0][:100]}; "
        f"{unresolved} calls through a slot unresolved, {unfollowed} through a register or memory not followed"
    )
    if show_chains or best[0] > budget:
        for part in best[1]:
            lines.append(f"    {frames[part]:>10}  {part[:140]}")
    if best[0] > budget:
        problems.append(
            f"{name}: its deepest chain is {best[0]} bytes, over its budget of {budget} "
            f"({BUDGET[0]}/{BUDGET[1]} of its {stack}-byte stack): a table built by value, or a large "
            "local on the way to it; build it where it lives (the Intel driver's `serve::Placed`)"
        )
    return lines, problems


def check(elf, show_chains):
    depth = stack_depth()
    data = Path(elf).read_bytes()
    text = subprocess.run(
        [depth.objdump(), "-d", "-C", "--no-show-raw-insn", elf], capture_output=True, text=True, check=True
    ).stdout
    frames, calls, unresolved, unfollowed = depth.read_frames(text, Slots(data, depth.got_targets(data)))
    return judge(frames, calls, Path(elf).name, native_stack_bytes(), show_chains, unresolved, unfollowed)


def self_test():
    depth = stack_depth()
    stack = native_stack_bytes("pub const NATIVE_STACK_SIZE: u64 = 8 * 1024 * 1024;")
    assert stack == 8 << 20, stack
    # The 2026-11-12 shape: the runtime calls `main` through a pointer, and the tables are built by value
    # through two frames of 3.5 and 4.5 MiB, each with the probe loop rustc writes.
    text = (
        "1000 <_start>:\n  callq 0x1010 <ferrix_rt_start>\n"
        "1010 <ferrix_rt_start>:\n  pushq %rbp\n  leaq -0x10(%rip), %rsi     # 0x1030\n"
        "  callq *0x100(%rip)          # 0x3000\n"
        "1020 <ferrix_rt::start>:\n  pushq %rbp\n  subq $0x48, %rsp\n  callq *%rsi\n"
        "1030 <prog::main>:\n  pushq %rbp\n  subq $0x1000, %rsp\n  callq 0x1040 <prog::serve>\n"
        "1040 <prog::serve>:\n  pushq %rbp\n  movq %rsp, %r11\n  subq $0x380000, %r11\n  subq $0x1000, %rsp\n"
        "  movq $0x0, (%rsp)\n  cmpq %r11, %rsp\n  callq 0x1050 <Tables::new>\n"
        "1050 <Tables::new>:\n  pushq %rbp\n  movq %rsp, %r11\n  subq $0x480000, %r11\n  subq $0x1000, %rsp\n"
        "  movq $0x0, (%rsp)\n  cmpq %r11, %rsp\n"
    )
    image = struct.pack("<Q", 0x1020)
    slots = Slots(b"", {})
    slots.sections.append((0x3000, image))
    frames, calls, unresolved, _ = depth.read_frames(text, slots)
    assert unresolved == 0 and calls["ferrix_rt_start"] == ["ferrix_rt::start"], calls
    assert frames["Tables::new"] == 8 + 0x480000, frames["Tables::new"]
    _, problems = judge(frames, calls, "prog", stack)
    assert len(problems) == 1 and "over its budget" in problems[0], problems
    # The same program with its tables in their own pages: the walk through `main` is what counts.
    built = text.replace("subq $0x380000, %r11", "subq $0x8000, %r11").replace(
        "subq $0x480000, %r11", "subq $0x2000, %r11"
    )
    frames, calls, _, _ = depth.read_frames(built, slots)
    lines, problems = judge(frames, calls, "prog", stack, show_chains=True)
    assert not problems, problems
    assert "prog::main" in "\n".join(lines) and "Tables::new" in "\n".join(lines), lines
    # A program whose `main` was renamed is not passed.
    _, problems = judge(frames, calls, "other", stack)
    assert len(problems) == 1 and "renamed" in problems[0], problems


def main():
    self_test()
    if "--self-test" in sys.argv:
        print("check-native-stacks: self-test passed")
        return 0
    elves = [arg for arg in sys.argv[1:] if not arg.startswith("--")]
    if not elves:
        print(__doc__)
        return 2
    failed = False
    for elf in elves:
        lines, problems = check(elf, "--chains" in sys.argv)
        for line in lines:
            print(f"check-native-stacks: {line}")
        for problem in problems:
            print(f"check-native-stacks: {problem}")
        failed |= bool(problems)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
