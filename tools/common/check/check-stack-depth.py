#!/usr/bin/env python3
"""A kernel stack's worst paths, against `vmap::STACK_PAGES` (F-100, F-101; ledger 6623).

    check-stack-depth.py <kernel ELF> <x86_64|aarch64|armv7a> [--self-test]

Reads the frames from the disassembly and adds them along named paths: a system call down to a
process made, under it the deepest of what that calls, and on top of all a timer's interrupt. It
fails when a path passes the stack, or when a name it walks is gone: a path renamed must be walked
again, not skipped.

A LOWER BOUND, and read as one: indirect calls are not followed, nor failure paths, formatting and
drops; ARMv7-A's calls through a linker's long-branch thunk are not followed, so its figures are the
least of the three; x86-64's return addresses, 8 bytes a call, are not counted; `job::notify`'s `Once` is left
out below `with_context` (its first use is at boot). It catches a by-value copy back on a path it
walks; it does not prove no other path is deeper.
"""
import re
import shutil
import subprocess
import sys
from pathlib import Path

PAGE = 4096
# What the entry and the interrupt each save before any function runs: `SyscallFrame` and each
# architecture's `TrapFrame`; AArch64's interrupt with its vector state, as ledger 6623's walk.
FRAMES = {
    "x86_64": {"entry": 128, "interrupt": 176, "first": "ferrix_syscall_entry"},
    "aarch64": {"entry": 304, "interrupt": 896, "first": "ferrix_kernel_core::trap::dispatch"},
    "armv7a": {"entry": 80, "interrupt": 80, "first": "ferrix_kernel_core::trap::dispatch"},
}
# After the architecture's way in. "system_call" is not on x86-64's way.
COMMON = [
    "ferrix_kernel_item::syscall::native_call",
    "ferrix_kernel_item::syscall::native::dispatch",
    "ticket::charging_for_call",
    "ferrix_kernel_item::syscall::native::answer",
]
NATIVE = COMMON + ["ferrix_kernel_load::syscall::launch::make_native"]
SUBTREE = "ferrix_kernel_load::syscall::exec::make_native_in"
LINUX = ["ferrix_kernel_item::syscall::dispatch_with", "syscall::linux::answer_as", "family::sys_clone"]
LINUX_SUBTREE = "process::Process>::new"
LEAVE_OUT = re.compile(
    r"panic|rust_begin_unwind|unwrap_failed|expect_failed|slice_.*fail|alloc_error|fmt::|drop_slow"
    r"|drop_glue|Drop>::drop|ops::drop|job::notify|Once<"
)
TIMER = "ferrix_kernel_core::sched::fire_timers"
TIMER_ABOVE = "ferrix_kernel_core::sched::timer_expired"


def objdump():
    found = shutil.which("llvm-objdump")
    if found:
        return found
    sysroot = subprocess.run(
        ["rustc", "--print", "sysroot"], capture_output=True, text=True, check=True
    ).stdout.strip()
    for candidate in sorted(Path(sysroot).glob("lib/rustlib/*/bin/llvm-objdump")):
        return str(candidate)
    raise SystemExit("check-stack-depth: no llvm-objdump (rustup component llvm-tools)")


def read_frames(text):
    """Each function's frame in bytes and what it calls directly, from a disassembly."""
    frames, calls, current = {}, {}, None
    for line in text.splitlines():
        head = re.match(r"^[0-9a-f]+ <(.*)>:", line)
        if head:
            current = head.group(1)
            frames[current], calls[current] = 0, set()
            continue
        if current is None:
            continue
        sub = re.search(r"\bsub\s+sp, (?:sp, )?#(0x[0-9a-f]+|\d+)(, lsl #12)?", line)
        if sub:
            frames[current] += int(sub.group(1), 0) << (12 if sub.group(2) else 0)
        pair = re.search(r"\b(?:stp|str)\s+\S+(?:, \S+)?, \[sp, #-(0x[0-9a-f]+|\d+)\]!", line)
        if pair:
            frames[current] += int(pair.group(1), 0)
        rsp = re.search(r"\bsubq?\s+\$(0x[0-9a-f]+|\d+), %rsp", line)
        if rsp:
            frames[current] += int(rsp.group(1), 0)
        if re.search(r"\bpushq?\s+%", line):
            frames[current] += 8
        push = re.search(r"\b(?:push|stmdb\s+sp!,)\s*\{([^}]*)\}", line)
        if push:
            frames[current] += 4 * (push.group(1).count(",") + 1)
        vpush = re.search(r"\bvpush\s*\{([^}]*)\}", line)
        if vpush:
            frames[current] += 8 * (vpush.group(1).count(",") + 1)
        call = re.search(r"\b(?:callq?|jmpq?|bl|blx|b)\s+(?:0x)?[0-9a-f]+ <(.*?)(?:\+0x[0-9a-f]+)?>$", line)
        if call and call.group(1) != current:
            calls[current].add(call.group(1))
    return frames, calls


def named(frames, part, problems):
    """The shortest name holding `part`, or a problem said."""
    found = [name for name in frames if part in name]
    if not found:
        problems.append(f"no function holds `{part}`: the path was renamed, walk it again")
        return None
    return min(found, key=len)


def deepest(start, frames, calls, memo, stack=()):
    """The deepest direct-call chain below `start`, itself included."""
    if start in stack:
        return 0
    if start in memo:
        return memo[start]
    below = [
        deepest(callee, frames, calls, memo, stack + (start,))
        for callee in calls.get(start, ())
        if callee in frames and not LEAVE_OUT.search(callee)
    ]
    memo[start] = frames.get(start, 0) + max(below, default=0)
    return memo[start]


def judge(frames, calls, arch, stack_bytes):
    """Lines to print and the problems."""
    problems, lines, memo = [], [], {}
    frame = FRAMES[arch]
    # The dispatcher and the architecture's own function may be one by inlining; one must exist.
    way_in = []
    if arch == "x86_64":
        way_in.append(frame["first"])
    else:
        ways = [frame["first"], f"ferrix_kernel_core::arch::{arch}::trap::system_call"]
        found = [way for way in ways if any(way in name for name in frames)]
        way_in += found or ways
        way_in.append("ferrix_kernel_core::trap::system_call")
    timer = named(frames, TIMER, problems)
    above = next((n for n in frames if TIMER_ABOVE in n), None)
    for label, chain, subtree in (
        ("a process made", way_in + NATIVE, SUBTREE),
        ("clone to Process::new", way_in + LINUX, LINUX_SUBTREE),
    ):
        total = frame["entry"]
        for part in chain:
            name = named(frames, part, problems)
            if name:
                total += frames[name]
        below = named(frames, subtree, problems)
        if problems:
            return lines, problems
        depth = deepest(below, frames, calls, memo)
        deep_timer = max(
            deepest(timer, frames, calls, memo), deepest(above or timer, frames, calls, memo)
        )
        worst = total + depth + frame["interrupt"] + deep_timer
        lines.append(
            f"stack-depth: {arch}: {label}: {total} + {depth} + {frame['interrupt']} + "
            f"{deep_timer} the timer's = {worst} of {stack_bytes}, a lower bound"
        )
        if worst > stack_bytes:
            problems.append(f"{arch}: {label} under a timer's interrupt reaches {worst} of {stack_bytes} bytes")
    clone = named(frames, "family::sys_clone", problems)
    if clone:
        lines.append(
            f"stack-depth: {arch}: not judged: {deepest(clone, frames, calls, memo)} below sys_clone "
            "by direct calls alone, whichever can happen (BACKLOG)"
        )
    return lines, problems


def stack_bytes():
    source = Path("src/kernel/src/vmap.rs").read_text(encoding="utf-8")
    return int(re.search(r"const STACK_PAGES: u64 = (\d+);", source).group(1)) * PAGE


def self_test():
    text = (
        "0000 <a>:\n  sub sp, sp, #0x10\n  bl 0x20 <b>\n"
        "0020 <b>:\n  stp x29, x30, [sp, #-32]!\n  bl 0x20 <b>\n  bl 0x40 <panic_x>\n"
        "0040 <panic_x>:\n  sub sp, sp, #0x1000\n"
        "0060 <c>:\n  subq $0x18, %rsp\n  pushq %rbx\n  callq 0x70 <d>\n"
        "0070 <d>:\n  push {r4, r5, lr}\n  vpush {d8, d9}\n"
    )
    frames, calls = read_frames(text)
    assert frames == {"a": 16, "b": 32, "panic_x": 4096, "c": 32, "d": 28}, frames
    assert deepest("a", frames, calls, {}) == 48, "recursion and panics are left out"
    assert deepest("c", frames, calls, {}) == 60
    problems = []
    assert named(frames, "nothing", problems) is None and problems


def main():
    self_test()
    if "--self-test" in sys.argv:
        print("check-stack-depth: self-test passed")
        return 0
    elf, arch = sys.argv[1], sys.argv[2]
    text = subprocess.run(
        [objdump(), "-d", "-C", "--no-show-raw-insn", elf], capture_output=True, text=True, check=True
    ).stdout
    frames, calls = read_frames(text)
    lines, problems = judge(frames, calls, arch, stack_bytes())
    print("\n".join(lines))
    for problem in problems:
        print(f"check-stack-depth: {problem}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
