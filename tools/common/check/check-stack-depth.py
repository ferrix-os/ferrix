#!/usr/bin/env python3
"""A kernel stack's worst paths, against `vmap::STACK_PAGES` (F-100, F-101; ledger 6623).

    check-stack-depth.py <kernel ELF> <x86_64|aarch64|armv7a> [--self-test]

Reads the frames from the disassembly and adds them along named paths: a system call down to a
process made, under it the deepest of what that calls, and on top of all a timer's interrupt. It
fails when a path passes the stack, or when a name it walks is gone: a path renamed must be walked
again, not skipped.

It reads the architecture's own `KERNEL_STACK_PAGES`, and on AArch64 wants 4,096 bytes left. Every
direct call below `sys_clone` counts as possible, a network namespace's first frames among them, but
what cannot be reached from there, said at `CLONE_CANNOT`.

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
# What this row asks to be left on AArch64's stack, whose figures are the real ones (ledger 7033 D2).
FLOOR = {"aarch64": 4096}
# ARMv7-A's calls through a long-branch thunk are not followed: its figures are not figures (D4).
NOT_JUDGED = {"armv7a": "not a figure: its long-branch thunks are not followed"}
# `Vmo::retire` passes `cut` false, and `ShadowCopies` exists only with `cut`, which only `cut_mappings`
# passes, so nothing below `sys_clone` reaches this; a truncation does, not through clone.
CLONE_CANNOT = "ShadowCopies>::finish"
# What `Drop for Task` hands the reaper where it may not sleep, and so does not reach there; and what
# its two slots hold, nothing at a drop (FX-0534's rule): a slot's payload is reached by no last drop.
HANDED_OVER = re.compile(
    r"panic|fmt::|leftovers::hand_over|Leftovers"
    r"|object::process::Process|user::space::AddressSpace|UserThread|UserState|RestrictedState"
    r"|ferrix_sched::tree::Slot<|ferrix_sched::tree::Node<|ferrix_sched::Entity<"
)
# What sleeps, or waits on another processor. A way to any of these from what is left is F-104 again.
MAY_SLEEP = re.compile(r"WaitQueue>::wait|sched::block|sched::schedule|sched::sleep|flush_tlb_everywhere|flush_tlb_pages")
# The stack `ferrix_stack_stop` says its one line on: `trap::STACK_STOP_SIZE`.
STOP_STACK = 4096
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


def deepest(start, frames, calls, memo, stack=(), without=None):
    """The deepest direct-call chain below `start`, itself included."""
    if start in stack:
        return 0
    if start in memo:
        return memo[start]
    below = [
        deepest(callee, frames, calls, memo, stack + (start,), without)
        for callee in calls.get(start, ())
        if callee in frames and not LEAVE_OUT.search(callee) and not (without and without in callee)
    ]
    memo[start] = frames.get(start, 0) + max(below, default=0)
    return memo[start]


def drop_rest(frames, calls, problems):
    """What a task's last drop still does where it may not sleep (F-104, ledger 7103 (8)): its deepest
    direct chain, with the fields it hands the reaper left out, and a refusal of any way from there to
    a wait. Answers the depth, or 0 if the image has no such drop."""
    start = next((name for name in frames if "drop_glue::<ferrix_kernel_core::sched::task::Task>" in name), None)
    if start is None:
        return 0

    memo = {}

    def below(name, stack):
        if name in stack:
            return 0, ()
        if name in memo:
            return memo[name]
        best = (0, ())
        for callee in calls.get(name, ()):
            if callee not in frames or HANDED_OVER.search(callee):
                continue
            if MAY_SLEEP.search(callee):
                problems.append(f"a task's last drop reaches `{callee[:90]}` by way of `{name[:70]}` (F-104)")
                continue
            depth, chain = below(callee, stack + (name,))
            if depth > best[0]:
                best = (depth, chain)
        memo[name] = (frames.get(name, 0) + best[0], (name,) + best[1])
        return memo[name]

    return below(start, ())[0]


def judge(frames, calls, arch, stack_bytes):
    """Lines to print and the problems."""
    problems, lines, memo = [], [], {}
    frame = FRAMES[arch]
    floor = FLOOR.get(arch, 0)
    remark = f" ({NOT_JUDGED[arch]})" if arch in NOT_JUDGED else ""
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
    rest = drop_rest(frames, calls, problems)
    lines.append(f"stack-depth: {arch}: what a task's last drop leaves, under an interrupt: {rest}{remark}")
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
        ) + rest
        worst = total + depth + frame["interrupt"] + deep_timer
        lines.append(
            f"stack-depth: {arch}: {label}: {total} + {depth} + {frame['interrupt']} + "
            f"{deep_timer} the timer's = {worst} of {stack_bytes}, a lower bound{remark}"
        )
        if worst > stack_bytes - floor:
            problems.append(
                f"{arch}: {label} under a timer's interrupt reaches {worst} of {stack_bytes} bytes, {floor} to be left"
            )
    # All of what direct calls reach below `sys_clone`, as possible, but what `CLONE_CANNOT` says.
    clone = named(frames, "family::sys_clone", problems)
    if clone:
        chain = sum(frames[named(frames, part, problems)] for part in way_in + LINUX[:-1])
        below = deepest(clone, frames, calls, {}, without=CLONE_CANNOT)
        worst = frame["entry"] + chain + below + frame["interrupt"] + deep_timer
        lines.append(
            f"stack-depth: {arch}: clone by every direct call: {frame['entry'] + chain} + {below} + "
            f"{frame['interrupt']} + {deep_timer} the timer's = {worst} of {stack_bytes}, a lower bound{remark}"
        )
        if worst > stack_bytes - floor:
            problems.append(
                f"{arch}: clone by every direct call reaches {worst} of {stack_bytes} bytes, {floor} to be left"
            )
    # The report of an overflow, on its emergency stack of the same size, with the machine stopping.
    overflow = next((name for name in frames if "trap::stack_overflow" in name), None)
    if overflow:
        depth = deepest(overflow, frames, calls, {})
        lines.append(f"stack-depth: {arch}: an overflow's report: {depth} of {stack_bytes}, a lower bound{remark}")
        if depth > stack_bytes:
            problems.append(f"{arch}: an overflow's report reaches {depth} of its stack's {stack_bytes} bytes")
    stop = next((name for name in frames if "trap::stack_stop_report" in name), None)
    if stop:
        depth = deepest(stop, frames, calls, {})
        lines.append(f"stack-depth: {arch}: an overflow with no stack left: {depth} of {STOP_STACK}, a lower bound{remark}")
        if depth > STOP_STACK:
            problems.append(f"{arch}: an overflow's last line reaches {depth} of its stack's {STOP_STACK} bytes")
    return lines, problems


def stack_bytes(arch, source=None):
    """`vmap::STACK_PAGES` for this architecture: its module's constant."""
    source = source or Path(f"src/kernel/src/arch/{arch}/mod.rs").read_text(encoding="utf-8")
    return int(re.search(r"const KERNEL_STACK_PAGES: u64 = (\d+);", source).group(1)) * PAGE


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
    assert stack_bytes("aarch64", "pub(crate) const KERNEL_STACK_PAGES: u64 = 6;") == 6 * PAGE, "read"


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
    lines, problems = judge(frames, calls, arch, stack_bytes(arch))
    print("\n".join(lines))
    for problem in problems:
        print(f"check-stack-depth: {problem}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
