#!/usr/bin/env python3
"""A kernel stack's worst paths, against `vmap::STACK_PAGES` (F-100, F-101; ledger 6623; F-107, F-110).

    check-stack-depth.py <kernel ELF> <x86_64|aarch64|armv7a> [--chains] [--self-test]

Reads the frames from the disassembly and adds them along named paths: a system call down to a
process made, under it the deepest of what that calls, and on top of all a timer's interrupt. It
fails when a path passes the stack, or when a name it walks is gone: a path renamed must be walked
again, not skipped. `--chains` prints each row's deepest chain.

It reads the architecture's own `KERNEL_STACK_PAGES` and wants `FLOOR` bytes left. Every direct call
below `sys_clone` counts as possible, a network namespace's first frames among them, but what cannot be
reached from there, said at `CLONE_CANNOT`. What a timer's interrupt cannot reach is said at
`INTERRUPT_CANNOT`, each with the test that refuses it there, its requirement and the control that
fired without the test.

THE WALK (F-107, ledger 8665): below a function, the deepest chain in which no function appears twice.
Callees are taken in name order, and the result does not depend on that order: the graph is cut into
its strongly connected parts, the parts walked callees first, and inside a part every chain that
repeats no function is read, so a cycle counts each of its functions once on the deepest way round.
A part with more than `CYCLE_CHAINS` such chains is refused rather than cut by order. Ties between
chains of one depth go to the first by name. So one kernel reads one figure on every run.

THE CALLS (F-110): a direct call or tail call on every architecture, ARMv7-A's with the immediate
objdump prints after its target (before F-110 that line went unread, so ARMv7-A's walk reached
nothing); and x86-64's calls through the GOT (`callq *off(%rip)`, a position-independent kernel's
calls between crates, most of its calls), followed by the slot's `R_X86_64_RELATIVE` relocation, as
are its calls through a register the function last loaded from a slot or with a function's address
(`movq off(%rip), %reg` / `leaq`, then `callq *%reg`). x86-64's return addresses, 8 bytes a call, are
counted. Each architecture prints how many calls and jumps through a register or memory it does not
follow. An image with a call through a long-branch thunk or through a GOT slot that resolves to no
function is not judged, and the row fails on it.

A LOWER BOUND, and read as one: calls through a register or memory (a `dyn` or a function pointer)
are not followed, nor failure paths, formatting and drops; `job::notify`'s `Once` is left out below
`with_context` (its first use is at boot). A tail call counts as a call, its caller's frame kept. It
catches a by-value copy back on a path it walks; it does not prove no other path is deeper.
"""
import importlib.util
import re
import shutil
import struct
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
# What a call itself puts on the stack beyond the callee's frame: x86-64's return address. An Arm
# call's link register is saved by the callee's own frame, which `check-frames`' walk reads.
CALL_BYTES = {"x86_64": 8, "aarch64": 0, "armv7a": 0}
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
# A kernel with no load in it (`personality=user`, FLIP.md LV.5): no Linux call runs on a kernel
# stack there, as `linuxd`'s are its own, so its deepest named call is a region's process made.
USER_NATIVE = COMMON + ["syscall::native::region::process_create_shared"]
USER_SUBTREE = "ferrix_kernel_core::object::process::Process>::new"
# What this row asks to be left on each architecture's stack, for what a lower bound leaves out
# (ledger 7033 D2 for AArch64; x86-64 and ARMv7-A the same since their walks are judged, F-110).
FLOOR = {"x86_64": 4096, "aarch64": 4096, "armv7a": 4096}
# An architecture whose figures judge nothing, and why, beside what `not_judged` finds in an image.
NOT_JUDGED = {}
# `Vmo::retire` passes `cut` false, and `ShadowCopies` exists only with `cut`, which only `cut_mappings`
# passes, so nothing below `sys_clone` reaches this; a truncation does, not through clone.
CLONE_CANNOT = "ShadowCopies>::finish"
# What a timer's interrupt cannot reach, though a call leads there: each is begun only behind a test
# that refuses it in an interrupt, which a call graph cannot read (ledger 8665, 8738 C1-C3). Each names
# its guard, the requirement that makes it so and the control that fired when the guard went, and
# says where the guard stands. "callers": in every caller, so the function is cut whole. "inside": in
# the function itself, so its frame and its guard's call stay, and only its other callees are cut;
# the regex names the callees kept. A name is matched whole (`purge`, not `purge_held`). One gone from
# the image cuts nothing, so the walk reads deeper, never less: the line under the timer says which
# were found.
INTERRUPT_CANNOT = {
    "ferrix_kernel_core::mm::large::purge": (
        "inside",
        r"sched::may_block",
        "`large::purge` returns at once where `sched::may_block` says no, as it does in every "
        "interrupt's service, whose interrupts are masked (L.mm.145, L.mm.143): control "
        "ef30-red3-c1, the test removed, a masked request's purge panics",
    ),
    "ferrix_kernel_core::object::oom::for_kernel_heap": (
        "callers",
        None,
        "the kernel heap's kill is asked only where `mm::heap_may_reclaim` says yes (`mm::heap_or_kill`, "
        "`mm::reservation_grow`): its `sched::may_block` says no in an interrupt's service, whose "
        "interrupts are masked, and its `irq::in_interrupt` test says no there too (L.mm.138, "
        "L.mm.136's case): control f107-irq-c1, the interrupt test removed",
    ),
}
# What `Drop for Task` hands the reaper where it may not sleep, and so does not reach there; and what
# its two slots hold, nothing at a drop (FX-0534's rule): a slot's payload is reached by no last drop.
HANDED_OVER = re.compile(
    r"panic|fmt::|leftovers::hand_over|Leftovers"
    r"|object::process::Process|user::space::AddressSpace|UserThread|UserState|RestrictedState"
    r"|ferrix_sched::tree::Slot<|ferrix_sched::tree::Node<|ferrix_sched::Entity<"
)
# What sleeps, or waits on another processor. A way to any of these from what is left is F-104 again.
MAY_SLEEP = re.compile(r"WaitQueue>::wait|sched::block|sched::schedule|sched::sleep|flush_tlb_everywhere|flush_tlb_pages")
# L.object.374 (F-107's B, docs/OOM-REAPER.md, ledger 8784, 8814): no OOM kill unmaps a space where
# it was asked -- the emptier does, a task of its own -- and the hand-off allocates nothing. Each rule:
# where the walk starts, what it must not reach, and what reaching it would mean.
EMPTIES_NOTHING = (
    (
        "ferrix_kernel_core::object::oom::kill_in",
        re.compile(r"user::space::AddressSpace>::unmap$"),
        "unmaps a space where it was asked",
    ),
    (
        "ferrix_kernel_core::object::oom::hand_over",
        re.compile(r"^__rustc::__rust_alloc$|^__rustc::__rust_realloc$|GlobalAlloc>::alloc$|GlobalAlloc>::realloc$"),
        "allocates in its hand-off",
    ),
)
# What those walks do not follow, each for its reason (ledger 8856 A5):
# * a failure path (a panic stops the machine);
# * a switch (`schedule`, a block, a wait, a sleep): what runs after a switch belongs to another task;
# * a task's last reference let go (`Arc<Task>`'s `drop_slow`): in the hand-off's wake,
#   `EMPTIER_WAIT`'s only waiter is the emptier, a kernel task that never ends, so its reference is
#   never the last; anywhere else on `kill_in`'s walk, a task's last drop is what `drop_rest`'s row
#   walks and bounds (F-104, L.mm.143).
EMPTIES_NOTHING_STOPS = re.compile(
    r"panic|rust_begin_unwind|unwrap_failed|expect_failed|slice_.*fail|alloc_error"
    r"|sched::schedule$|sched::schedule_from|sched::block|WaitQueue>::wait|sched::sleep"
    r"|Arc<ferrix_kernel_core::sched::task::Task>>::drop_slow"
)
# The emptier's own row: its chunk under a timer's interrupt, on a kernel task's stack (8784 C6).
EMPTIER = "ferrix_kernel_core::object::oom::emptier"
# The way out of a system call (F-107, the PO's 2026-11-12 order after a real overflow on x86-64's
# exit with tens of thousands of regions): `deliver::return_to_user`, which builds a pending
# signal's frame on the user stack, or ends the calling thread (`leave_current`) and with the last
# one releases its process and unmaps the whole space, under the way in and a timer's interrupt.
EXIT = "ferrix_kernel_load::syscall::deliver::return_to_user"
# The stack `ferrix_stack_stop` says its one line on: `trap::STACK_STOP_SIZE`.
STOP_STACK = 4096
LEAVE_OUT = re.compile(
    r"panic|rust_begin_unwind|unwrap_failed|expect_failed|slice_.*fail|alloc_error|fmt::|drop_slow"
    r"|drop_glue|Drop>::drop|ops::drop|job::notify|Once<"
)
TIMER = "ferrix_kernel_core::sched::fire_timers"
TIMER_ABOVE = "ferrix_kernel_core::sched::timer_expired"
# The most chains with no function twice that one strongly connected part may hold before the walk
# refuses it: a part that large is named and cut, not read by an order.
CYCLE_CHAINS = 1_000_000
# A linker's long-branch thunk or veneer: a call through one is not followed.
THUNK = re.compile(r"Thunk|[Vv]eneer")

HEAD = re.compile(r"^([0-9a-f]+) <(.*)>:")
# A direct call or tail call: x86-64's `callq`/`jmp`, Arm's `bl`/`blx`/`b`. ARMv7-A's objdump ends
# the line with the immediate (`@ imm = #...`).
CALL = re.compile(
    r"\b(callq?|jmpq?|bl|blx|b)\s+(?:0x)?[0-9a-f]+ <(.*?)(\+0x[0-9a-f]+)?>(?:\s+@ imm = #-?(?:0x)?[0-9a-f]+)?$"
)
# x86-64's call or tail call through a GOT slot, with the slot's address objdump works out.
GOT_CALL = re.compile(r"\b(?:callq?|jmpq?)\s+\*-?0x[0-9a-f]+\(%rip\)\s+# 0x([0-9a-f]+)")


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


def frame_reader():
    """`check-frames`' walk: one reader of a frame, its probe loop's size among them (ledger 7275 L3.1)."""
    path = Path(__file__).with_name("check-frames.py")
    spec = importlib.util.spec_from_file_location("check_frames", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.walk


def got_targets(elf):
    """x86-64: each GOT slot's target address, from the image's `R_X86_64_RELATIVE` relocations, whose
    addend is the address (a static PIE kernel has no other kind). {} for any other image."""
    if elf[:4] != b"\x7fELF" or elf[4] != 2 or elf[5] != 1:
        return {}
    shoff = struct.unpack_from("<Q", elf, 0x28)[0]
    shentsize, shnum = struct.unpack_from("<HH", elf, 0x3A)
    slots = {}
    for index in range(shnum):
        _, kind, _, _, offset, size, _, _, _, _ = struct.unpack_from("<IIQQQQIIQQ", elf, shoff + index * shentsize)
        if kind != 4:  # SHT_RELA
            continue
        for entry in range(offset, offset + size, 24):
            where, info, addend = struct.unpack_from("<QQq", elf, entry)
            if info & 0xFFFFFFFF == 8:  # R_X86_64_RELATIVE
                slots[where] = addend & 0xFFFFFFFFFFFFFFFF
    return slots


# x86-64: a register loaded from a GOT slot (`movq off(%rip), %reg # 0xSLOT`), or with a function's
# own address (`leaq off(%rip), %reg # 0xADDR`), and a call or tail call through a register.
GOT_LOAD = re.compile(r"\bmovq\s+-?0x[0-9a-f]+\(%rip\), %(\w+)\s+# 0x([0-9a-f]+)")
ADDRESS_LOAD = re.compile(r"\bleaq\s+-?0x[0-9a-f]+\(%rip\), %(\w+)\s+# 0x([0-9a-f]+)")
REGISTER_CALL = re.compile(r"\b(?:callq?|jmpq?)\s+\*%(\w+)\s*$")
# A call through a register or memory the walk does not follow: x86-64's through memory (not the GOT)
# or a register it holds no load of; AArch64's `blr`; ARMv7-A's `blx` to a register.
OTHER_INDIRECT = re.compile(
    r"\b(?:callq?|jmpq?)\s+\*-?(?:0x[0-9a-f]+)?\((?!%rip)|\bblr\s+x\d+|\bblx\s+r\d+"
)
# What an x86-64 call leaves undefined: the System V caller-saved registers.
CALLER_SAVED = ("rax", "rcx", "rdx", "rsi", "rdi", "r8", "r9", "r10", "r11")
# Instructions that write registers they do not name.
IMPLICIT = {"cpuid": ("rax", "rbx", "rcx", "rdx"), "rdtsc": ("rax", "rdx"), "rdtscp": ("rax", "rcx", "rdx"),
            "rdmsr": ("rax", "rdx"), "syscall": ("rcx", "r11")}


def register(name):
    """An x86-64 register's 64-bit name: `eax`, `ax`, `al` are `rax`; `r15d` is `r15`."""
    name = name.lower()
    numbered = re.fullmatch(r"(r(?:8|9|1[0-5]))[dwb]?", name)
    if numbered:
        return numbered.group(1)
    for full, parts in (
        ("rax", ("rax", "eax", "ax", "al", "ah")), ("rbx", ("rbx", "ebx", "bx", "bl", "bh")),
        ("rcx", ("rcx", "ecx", "cx", "cl", "ch")), ("rdx", ("rdx", "edx", "dx", "dl", "dh")),
        ("rsi", ("rsi", "esi", "si", "sil")), ("rdi", ("rdi", "edi", "di", "dil")),
        ("rbp", ("rbp", "ebp", "bp", "bpl")), ("rsp", ("rsp", "esp", "sp", "spl")),
    ):
        if name in parts:
            return full
    return name


def read_frames(text, slots=None):
    """Each function's frame in bytes, what it calls, in name order, the calls through a GOT slot that
    resolved to no function, and the calls through a register or memory not followed.

    `slots` maps a GOT slot to its target address (`got_targets`), for x86-64: a call through a slot
    is followed to its target, and so is a call through a register this function last loaded from a
    slot or with a function's address, in the order its instructions are written. A load is dropped
    when an instruction writes the register, and a call drops the caller-saved ones; a register held
    by no load is a call not followed."""
    frames = {name: frame for name, (frame, _) in frame_reader()(text).items()}
    starts = {}
    if slots is not None:
        for line in text.splitlines():
            head = HEAD.match(line)
            if head:
                starts[int(head.group(1), 16)] = head.group(2)
    calls, current, unresolved, unfollowed, held = {}, None, 0, 0, {}
    for line in text.splitlines():
        head = HEAD.match(line)
        if head:
            current = head.group(2)
            calls[current] = set()
            held = {}
            continue
        if current is None:
            continue
        call = CALL.search(line)
        callee = call.group(2) if call else None
        # A branch inside the function is no call; a call (not a jump) to its own start is a
        # recursion, kept as an edge for `Walk.recursions` to name (a walk counts it once).
        if callee == current and call.group(1) in ("call", "callq", "bl", "blx") and not call.group(3):
            calls[current].add(callee)
        called = callee is not None and callee != current
        if callee is None and slots is not None:
            through = GOT_CALL.search(line)
            loaded = GOT_LOAD.search(line)
            address = ADDRESS_LOAD.search(line)
            by_register = REGISTER_CALL.search(line)
            if through:
                called = True
                callee = starts.get(slots.get(int(through.group(1), 16)))
                unresolved += callee is None
            elif by_register:
                called = True
                target = held.get(register(by_register.group(1)), "")
                if target is None:
                    unresolved += 1
                elif target:
                    callee = target
                else:
                    unfollowed += 1
            elif loaded:
                # A slot whose target is no function's start is data: None, a call through it unresolved.
                held[register(loaded.group(1))] = starts.get(slots.get(int(loaded.group(2), 16)))
                continue
            elif address and int(address.group(2), 16) in starts:
                held[register(address.group(1))] = starts[int(address.group(2), 16)]
                continue
            else:
                fields = line.split("#", 1)[0].split("\t")
                mnemonic = fields[1].strip() if len(fields) > 1 else ""
                operands = fields[2].strip() if len(fields) > 2 else ""
                written = [register(r) for r in IMPLICIT.get(mnemonic, ())]
                last = operands.rsplit(",", 1)[-1].strip()
                if last.startswith("%"):
                    written.append(register(last[1:]))
                if mnemonic.startswith(("xchg", "xadd", "cmpxchg")):
                    written += [register(r) for r in re.findall(r"%(\w+)", operands)]
                for name in written:
                    held.pop(name, None)
        if callee is None and not called and OTHER_INDIRECT.search(line):
            unfollowed += 1
            called = True
        if called:
            for name in CALLER_SAVED:
                held.pop(name, None)
        if callee is not None and callee != current:
            calls[current].add(callee)
    return frames, {name: sorted(callees) for name, callees in calls.items()}, unresolved, unfollowed


def named(frames, part, problems):
    """The shortest name holding `part`, the first by name among those as short, or a problem said."""
    found = [name for name in frames if part in name]
    if not found:
        problems.append(f"no function holds `{part}`: the path was renamed, walk it again")
        return None
    return min(found, key=lambda name: (len(name), name))


class CycleTooLarge(Exception):
    pass


class Walk:
    """The deepest chain below a function in which no function appears twice, over the calls whose
    callee has a frame and is not `leave`'s; a function weighs its frame and `call_bytes`.

    Order-free by construction: Tarjan's strongly connected parts come out callees first; a part's
    function reads, for every chain inside the part from it that repeats nothing, the chain's weight
    and the deepest way out of its last function, and keeps the deepest. Out of a part is a part
    already read, which cannot lead back, so its figure holds whatever called it."""

    def __init__(self, frames, calls, leave, call_bytes=0, inside=None):
        self.frames, self.calls, self.leave, self.call_bytes = frames, calls, leave, call_bytes
        # name -> the regex of the only callees it keeps: a guard inside the function (`INTERRUPT_CANNOT`).
        self.inside = inside or {}
        self.best = {}  # name -> (depth, chain)
        # Each strongly connected part with a cycle in it, as a sorted tuple of its functions: a
        # recursion, whose depth this walk does not bound (it counts each function once).
        self.cycles = {}  # name -> its part, for every function in a cyclic part

    def weight(self, name):
        return self.frames[name] + self.call_bytes

    def recursions(self, start):
        """The cyclic parts reachable from `start` by the calls this walk follows, each a sorted
        tuple of its functions, in name order."""
        self.read(start)
        seen, todo, found = {start}, [start], set()
        while todo:
            name = todo.pop()
            if name in self.cycles:
                found.add(self.cycles[name])
            for callee in self.callees(name):
                if callee not in seen:
                    seen.add(callee)
                    todo.append(callee)
        return sorted(found)

    def callees(self, name):
        kept = self.inside.get(name)
        return [
            c
            for c in self.calls.get(name, ())
            if c in self.frames and c != name and not self.leave(c) and (kept is None or re.search(kept, c))
        ]

    def depth(self, start):
        self.read(start)
        return self.best[start][0]

    def chain(self, start):
        self.read(start)
        return self.best[start][1]

    def read(self, start):
        if start in self.best:
            return
        index, low, on, stack, counter = {}, {}, set(), [], 0
        work = [(start, iter(self.callees(start)))]
        index[start] = low[start] = counter
        counter += 1
        stack.append(start)
        on.add(start)
        while work:
            name, rest = work[-1]
            for callee in rest:
                if callee in self.best:
                    continue
                if callee not in index:
                    index[callee] = low[callee] = counter
                    counter += 1
                    stack.append(callee)
                    on.add(callee)
                    work.append((callee, iter(self.callees(callee))))
                    break
                if callee in on:
                    low[name] = min(low[name], index[callee])
            else:
                work.pop()
                if work:
                    low[work[-1][0]] = min(low[work[-1][0]], low[name])
                if low[name] == index[name]:
                    part = []
                    while True:
                        member = stack.pop()
                        on.discard(member)
                        part.append(member)
                        if member == name:
                            break
                    self.read_part(part)

    def way_out(self, name, inside):
        """The deepest callee outside `inside`, already read: (depth, chain), the first by name on a tie."""
        best = (0, ())
        for callee in self.callees(name):
            if callee in inside:
                continue
            depth, chain = self.best[callee]
            if depth > best[0] or (depth == best[0] and depth and chain < best[1]):
                best = (depth, chain)
        return best

    def read_part(self, part):
        inside = set(part)
        if len(part) > 1 or part[0] in self.calls.get(part[0], ()):
            cycle = tuple(sorted(part))
            for name in part:
                self.cycles[name] = cycle
        out = {name: self.way_out(name, inside) for name in part}
        inner = {name: [c for c in self.callees(name) if c in inside] for name in part}
        chains = 0
        for first in sorted(part):
            best = (-1, ())
            # Every chain inside the part from `first` that repeats no function, as a stack of
            # (function, its callees inside not yet tried); each chain read ends where it stands.
            path, weight, seen = [first], self.weight(first), {first}
            trail = [iter(inner[first])]
            while True:
                chains += 1
                if chains > CYCLE_CHAINS:
                    raise CycleTooLarge(
                        f"a cycle of {len(part)} functions, `{min(part)[:80]}` among them, holds more than "
                        f"{CYCLE_CHAINS} chains with no function twice: name and cut it"
                    )
                depth, chain = out[path[-1]]
                candidate = (weight + depth, tuple(path) + chain)
                if candidate[0] > best[0] or (candidate[0] == best[0] and candidate[1] < best[1]):
                    best = candidate
                step = next((c for c in trail[-1] if c not in seen), None)
                while step is None:
                    trail.pop()
                    if not trail:
                        break
                    gone = path.pop()
                    seen.discard(gone)
                    weight -= self.weight(gone)
                    step = next((c for c in trail[-1] if c not in seen), None)
                if not trail:
                    break
                path.append(step)
                seen.add(step)
                weight += self.weight(step)
                trail.append(iter(inner[step]))
            self.best[first] = best


def drop_rest(frames, calls, problems):
    """What a task's last drop still does where it may not sleep (F-104, ledger 7103 (8)): its deepest
    chain, with the fields it hands the reaper left out, and a refusal of any way from there to a wait
    (no `INTERRUPT_CANNOT` here: a way behind a run-time test is still a way). Answers the depth, or 0
    if the image has no such drop."""
    start = next((name for name in sorted(frames) if "drop_glue::<ferrix_kernel_core::sched::task::Task>" in name), None)
    if start is None:
        return 0
    found, seen, todo = set(), {start}, [start]
    while todo:
        name = todo.pop()
        for callee in calls.get(name, ()):
            if callee not in frames or HANDED_OVER.search(callee):
                continue
            if MAY_SLEEP.search(callee):
                found.add(f"a task's last drop reaches `{callee[:90]}` by way of `{name[:70]}` (F-104)")
            elif callee not in seen:
                seen.add(callee)
                todo.append(callee)
    problems.extend(sorted(found))
    walk = Walk(frames, calls, lambda name: bool(HANDED_OVER.search(name) or MAY_SLEEP.search(name)))
    return walk.depth(start)


def empties_nothing(frames, calls, problems):
    """L.object.374's walks: from each rule's start by the calls the walk follows, the functions
    reached, and a problem for each way to what it must not reach. Answers a line's text, or None for
    an image with no OOM kill."""
    if not any(start in frames for start, _, _ in EMPTIES_NOTHING):
        return None
    counts = []
    for start, target, what in EMPTIES_NOTHING:
        if start not in frames:
            problems.append(f"no function `{start}`: L.object.374's walk was renamed, walk it again")
            continue
        found, seen, todo = set(), {start}, [start]
        while todo:
            name = todo.pop()
            for callee in calls.get(name, ()):
                if callee not in frames or EMPTIES_NOTHING_STOPS.search(callee):
                    continue
                if target.search(callee):
                    found.add(f"the OOM kill {what}: `{callee[:80]}` by way of `{name[:70]}` (L.object.374)")
                elif callee not in seen:
                    seen.add(callee)
                    todo.append(callee)
        problems.extend(sorted(found))
        counts.append(f"`{start.rsplit('::', 1)[-1]}` reaches {len(seen)}")
    return ", ".join(counts)


def not_judged(arch, calls, unresolved):
    """Why this image's figures judge nothing, or None: a call through a thunk or veneer, or through a
    GOT slot that resolved to no function, is a call the walk cannot follow (F-110). The row fails on
    it (ledger 8738 B2): an image the walk cannot read is never passed."""
    thunks = sum(1 for callees in calls.values() for callee in callees if THUNK.search(callee))
    if thunks:
        return f"not judged: {thunks} calls through a long-branch thunk are not followed"
    if unresolved:
        return f"not judged: {unresolved} calls through the GOT resolve to no function"
    return NOT_JUDGED.get(arch)


def rows_named(frames, user, problems):
    """The way out's and the emptier's lines fail closed (ledger 9096 H1): in an image with the
    syscall layer, a start the walk does not find is a problem, as `named` says one, not a line left
    out. A user kernel has no load and is excepted."""
    if user:
        return
    for start, what in ((EXIT, "the way out"), (EMPTIER, "the OOM emptier")):
        if start not in frames:
            problems.append(f"no function `{start}`: {what}'s line was renamed, walk it again")


def judge(frames, calls, arch, stack_bytes, unresolved=0, show_chains=False, unfollowed=0):
    """Lines to print and the problems."""
    problems, lines = [], []
    frame = FRAMES[arch]
    floor = FLOOR.get(arch, 0)
    why_not = not_judged(arch, calls, unresolved)
    remark = f" ({why_not})" if why_not else ""
    if why_not:
        problems.append(f"{arch}: {why_not}")
    each = CALL_BYTES[arch]
    plain = Walk(frames, calls, LEAVE_OUT.search, each)
    whole = {name for name, (kind, _, _) in INTERRUPT_CANNOT.items() if kind == "callers"}
    inside = {name: kept for name, (kind, kept, _) in INTERRUPT_CANNOT.items() if kind == "inside"}
    under_irq = Walk(frames, calls, lambda name: bool(LEAVE_OUT.search(name)) or name in whole, each, inside)
    clone_walk = Walk(frames, calls, lambda name: bool(LEAVE_OUT.search(name)) or CLONE_CANNOT in name, each)

    def chain_line(label, chain):
        if show_chains:
            lines.append(f"stack-depth: {arch}: {label}'s deepest: " + " > ".join(chain))

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
    above = next((n for n in sorted(frames) if TIMER_ABOVE in n), None)
    rest = drop_rest(frames, calls, problems)
    lines.append(f"stack-depth: {arch}: what a task's last drop leaves, under an interrupt: {rest}{remark}")
    kill = empties_nothing(frames, calls, problems)
    if kill:
        lines.append(
            f"stack-depth: {arch}: the OOM kill unmaps nothing where it was asked, and its hand-off "
            f"allocates nothing (L.object.374): {kill}"
        )
    lines.append(
        f"stack-depth: {arch}: calls and jumps through a register or memory not followed: {unfollowed} "
        "(what makes the row a lower bound)"
    )
    cut = sorted(
        f"`{name.rsplit('::', 1)[-1]}`" + (" but its guard" if kind == "inside" else "")
        for name, (kind, _, _) in INTERRUPT_CANNOT.items()
        if name in frames
    )
    lines.append(
        f"stack-depth: {arch}: under a timer's interrupt, not walked (INTERRUPT_CANNOT): "
        + (", ".join(cut) or "none in this image")
    )
    user = not any("ferrix_kernel_load::" in name for name in frames)
    rows_named(frames, user, problems)
    paths = (
        (("a region's process made", way_in + USER_NATIVE, USER_SUBTREE),)
        if user
        else (
            ("a process made", way_in + NATIVE, SUBTREE),
            ("clone to Process::new", way_in + LINUX, LINUX_SUBTREE),
        )
    )
    if user:
        lines.append(f"stack-depth: {arch}: a user kernel: no load, so a Linux call's paths are linuxd's")
    try:
        deep_timer = None
        for label, chain, subtree in paths:
            missing = []
            names = [named(frames, part, missing) for part in chain]
            below = named(frames, subtree, missing)
            if missing or timer is None:
                return lines, problems + missing
            if deep_timer is None:
                top = max((timer, above or timer), key=lambda name: (under_irq.depth(name), name))
                deep_timer = under_irq.depth(top) + rest
                chain_line("a timer's interrupt", under_irq.chain(top))
            total = frame["entry"] + sum(frames[name] + each for name in names)
            depth = plain.depth(below)
            worst = total + depth + frame["interrupt"] + deep_timer
            lines.append(
                f"stack-depth: {arch}: {label}: {total} + {depth} + {frame['interrupt']} + "
                f"{deep_timer} the timer's = {worst} of {stack_bytes}, a lower bound{remark}"
            )
            chain_line(label, plain.chain(below))
            if worst > stack_bytes - floor:
                problems.append(
                    f"{arch}: {label} under a timer's interrupt reaches {worst} of {stack_bytes} bytes, {floor} to be left"
                )
        # All of what direct calls reach below `sys_clone`, as possible, but what `CLONE_CANNOT` says.
        clone = None if user else named(frames, "family::sys_clone", problems)
        if clone:
            chain = sum(frames[named(frames, part, problems)] + each for part in way_in + LINUX[:-1])
            below = clone_walk.depth(clone)
            worst = frame["entry"] + chain + below + frame["interrupt"] + deep_timer
            lines.append(
                f"stack-depth: {arch}: clone by every direct call: {frame['entry'] + chain} + {below} + "
                f"{frame['interrupt']} + {deep_timer} the timer's = {worst} of {stack_bytes}, a lower bound{remark}"
            )
            chain_line("clone by every direct call", clone_walk.chain(clone))
            if worst > stack_bytes - floor:
                problems.append(
                    f"{arch}: clone by every direct call reaches {worst} of {stack_bytes} bytes, {floor} to be left"
                )
        # The way out of a system call: a signal's frame, or a thread's end with its process
        # released and its space unmapped, under the way in and a timer's interrupt.
        if not user and EXIT in frames:
            ways = [named(frames, part, problems) for part in way_in]
            total = frame["entry"] + sum(frames[name] + each for name in ways if name)
            depth = plain.depth(EXIT)
            worst = total + depth + frame["interrupt"] + deep_timer
            lines.append(
                f"stack-depth: {arch}: the way out (a signal's frame, a thread's end): {total} + {depth} + {frame['interrupt']} + "
                f"{deep_timer} the timer's = {worst} of {stack_bytes}, a lower bound{remark}"
            )
            chain_line("the way out", plain.chain(EXIT))
            if worst > stack_bytes - floor:
                problems.append(
                    f"{arch}: the way out of a system call under a timer's interrupt reaches {worst} of "
                    f"{stack_bytes} bytes, {floor} to be left"
                )
        # The OOM kill's emptier, a kernel task: its chunk under a timer's interrupt (L.object.376).
        if EMPTIER in frames:
            depth = plain.depth(EMPTIER)
            worst = depth + frame["interrupt"] + deep_timer
            lines.append(
                f"stack-depth: {arch}: the OOM emptier: {depth} + {frame['interrupt']} + {deep_timer} the "
                f"timer's = {worst} of {stack_bytes}, a lower bound{remark}"
            )
            chain_line("the OOM emptier", plain.chain(EMPTIER))
            if worst > stack_bytes - floor:
                problems.append(
                    f"{arch}: the OOM emptier under a timer's interrupt reaches {worst} of {stack_bytes} bytes, "
                    f"{floor} to be left"
                )
        # The report of an overflow, on its emergency stack of the same size, with the machine stopping.
        overflow = next((name for name in sorted(frames) if "trap::stack_overflow" in name), None)
        if overflow:
            depth = plain.depth(overflow)
            lines.append(f"stack-depth: {arch}: an overflow's report: {depth} of {stack_bytes}, a lower bound{remark}")
            if depth > stack_bytes:
                problems.append(f"{arch}: an overflow's report reaches {depth} of its stack's {stack_bytes} bytes")
        stop = next((name for name in sorted(frames) if "trap::stack_stop_report" in name), None)
        if stop:
            depth = plain.depth(stop)
            lines.append(
                f"stack-depth: {arch}: an overflow with no stack left: {depth} of {STOP_STACK}, a lower bound{remark}"
            )
            if depth > STOP_STACK:
                problems.append(f"{arch}: an overflow's last line reaches {depth} of its stack's {STOP_STACK} bytes")
    except CycleTooLarge as refused:
        problems.append(f"{arch}: {refused}")
    return lines, problems


def stack_bytes(arch, source=None):
    """`vmap::STACK_PAGES` for this architecture: its module's constant."""
    source = source or Path(f"src/kernel/src/arch/{arch}/mod.rs").read_text(encoding="utf-8")
    return int(re.search(r"const KERNEL_STACK_PAGES: u64 = (\d+);", source).group(1)) * PAGE


def old_deepest(start, frames, calls, memo, stack=()):
    """The walk before F-107, kept for the self-test alone: a memo under a cycle cut, read in the order
    the callees come, so a cycle reads two ways."""
    if start in stack:
        return 0
    if start in memo:
        return memo[start]
    below = [old_deepest(callee, frames, calls, memo, stack + (start,)) for callee in calls.get(start, ())]
    memo[start] = frames.get(start, 0) + max(below, default=0)
    return memo[start]


def self_test():
    text = (
        "0000 <a>:\n  sub sp, sp, #0x10\n  bl 0x20 <b>\n"
        "0020 <b>:\n  stp x29, x30, [sp, #-32]!\n  bl 0x20 <b>\n  bl 0x40 <panic_x>\n"
        "0040 <panic_x>:\n  sub sp, sp, #0x1000\n"
        "0060 <c>:\n  subq $0x18, %rsp\n  pushq %rbx\n  callq 0x70 <d>\n"
        "0070 <d>:\n  push {r4, r5, lr}\n  vpush {d8, d9}\n  bl 0x80 <e> @ imm = #0x8\n"
        "0080 <e>:\n  sub sp, sp, #8\n"
    )
    frames, calls, _, _ = read_frames(text)
    assert frames == {"a": 16, "b": 32, "panic_x": 4096, "c": 32, "d": 28, "e": 8}, frames
    assert calls["d"] == ["e"], "ARMv7-A's call, its immediate after the name, is followed"
    loops = (
        "0000 <x>:\n  movq %rsp, %r11\n  subq $0x8000, %r11\n  subq $0x1000, %rsp\n  movq $0x0, (%rsp)\n"
        "  cmpq %r11, %rsp\n  jne 0x0 <x>\n"
        "0040 <y>:\n  sub x9, sp, #0x8, lsl #12\n  sub sp, sp, #0x1, lsl #12\n  cmp sp, x9\n  str xzr, [sp]\n"
        "  b.ne 0x40 <y>\n"
    )
    sized, _, _, _ = read_frames(loops)
    assert sized["x"] == 0x8000 and sized["y"] == 0x8000, sized
    leave = LEAVE_OUT.search
    assert Walk(frames, calls, leave).depth("a") == 48, "recursion and panics are left out"
    assert Walk(frames, calls, leave).depth("c") == 68
    assert Walk(frames, calls, leave, 8).depth("c") == 92, "a call's return address counted"
    # One cycle read in both orders (F-107): r calls p and q, p and q call each other, q calls z. The
    # deepest chain with no function twice is r > p > q > z, 1 + 10 + 100 + 1000. The walk before
    # read 1111 with p first and 1101 with q first: q's figure, memoised under p's cut, came back as
    # p's callee without p. Both orders now read 1111, and the same chain.
    cycle = {"r": 1, "p": 10, "q": 100, "z": 1000}
    one = {"r": ["p", "q"], "p": ["q"], "q": ["p", "z"], "z": []}
    other = {name: list(reversed(callees)) for name, callees in one.items()}
    assert old_deepest("r", cycle, one, {}) != old_deepest("r", cycle, other, {}), "the old walk read two ways"
    for order in (one, other):
        walk = Walk(cycle, order, lambda name: False)
        assert walk.depth("r") == 1111, walk.depth("r")
        assert walk.chain("r") == ("r", "p", "q", "z"), walk.chain("r")
        assert Walk(cycle, order, lambda name: False).depth("q") == 1100
        assert Walk(cycle, order, lambda name: False).depth("p") == 1110
    # A tie goes to the first chain by name, in either order.
    tie = {"s": 1, "m": 5, "n": 5}
    for order in (["m", "n"], ["n", "m"]):
        assert Walk(tie, {"s": order}, lambda name: False).chain("s") == ("s", "m")
    # A part too large to read is refused, not cut by order.
    global CYCLE_CHAINS
    kept, CYCLE_CHAINS = CYCLE_CHAINS, 3
    try:
        Walk(cycle, one, lambda name: False).depth("r")
        raise AssertionError("a part over CYCLE_CHAINS was read")
    except CycleTooLarge:
        pass
    finally:
        CYCLE_CHAINS = kept
    # x86-64's call through a GOT slot, resolved by the slot's relocation.
    got = "0000 <f>:\n  subq $0x8, %rsp\n  callq *0x10(%rip) # 0x100 <x+0x4>\n0040 <g>:\n  pushq %rbx\n"
    _, through, unresolved, _ = read_frames(got, {0x100: 0x40})
    assert through["f"] == ["g"] and unresolved == 0, through
    _, through, unresolved, _ = read_frames(got, {})
    assert through["f"] == [] and unresolved == 1, "an unresolved slot is counted"
    # And through a register loaded from a slot (ledger 8738 B1): a callee-saved register keeps its
    # load across a call, a caller-saved one does not, a write drops it, a function's own address
    # loaded with `leaq` is followed, and a data slot called through is unresolved.
    held = (
        "0000 <f>:\n"
        "1:  \tmovq\t0x10(%rip), %r15 # 0x100 <x>\n"
        "2:  \tmovq\t0x10(%rip), %rax # 0x108 <x>\n"
        "3:  \tcallq\t*%r15\n"
        "4:  \tcallq\t*%r15\n"
        "5:  \tcallq\t*%rax\n"
        "6:  \tmovq\t0x10(%rip), %rbx # 0x108 <x>\n"
        "7:  \tmovl\t$0x1, %ebx\n"
        "8:  \tcallq\t*%rbx\n"
        "9:  \tleaq\t0x10(%rip), %r12 # 0x60 <h>\n"
        "a:  \tjmpq\t*%r12\n"
        "b:  \tmovq\t0x10(%rip), %r13 # 0x110 <x>\n"
        "c:  \tcallq\t*%r13\n"
        "d:  \tcallq\t*0x8(%rax)\n"
        "0040 <g>:\n\tpushq\t%rbx\n0050 <k>:\n\tpushq\t%rbx\n0060 <h>:\n\tpushq\t%rbx\n"
    )
    _, through, unresolved, unfollowed = read_frames(held, {0x100: 0x40, 0x108: 0x50, 0x110: 0x999})
    assert through["f"] == ["g", "h"], through
    assert unresolved == 1 and unfollowed == 3, (unresolved, unfollowed)
    _, _, _, arm = read_frames("0000 <f>:\n\tblr\tx8\n0040 <g>:\n\tblx\tr3\n\tbx\tlr\n")
    assert arm == 2, "AArch64's blr and ARMv7-A's blx to a register are counted, a return not"
    rela = bytearray(0x40 + 64 * 2 + 24)
    rela[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<Q", rela, 0x28, 0x40)
    struct.pack_into("<HH", rela, 0x3A, 64, 2)
    struct.pack_into("<IIQQQQIIQQ", rela, 0x40 + 64, 0, 4, 0, 0, 0xC0, 24, 0, 0, 8, 24)
    struct.pack_into("<QQq", rela, 0xC0, 0x100, 8, -0x80000000)
    assert got_targets(bytes(rela)) == {0x100: 0xFFFFFFFF80000000}, got_targets(bytes(rela))
    # An exclusion under the interrupt is a name, not a part of one: `purge` is cut, `purge_held` not.
    assert "ferrix_kernel_core::mm::large::purge_held" not in INTERRUPT_CANNOT
    assert all(kind in ("callers", "inside") for kind, _, _ in INTERRUPT_CANNOT.values())
    # A guard in every caller cuts the function whole; a guard inside keeps the function's frame and
    # its guard's call, and cuts its other callees (ledger 8738 C2).
    guarded = {"t": 1, "kill": 10, "purge": 20, "may_block": 2, "flush": 500, "victim": 300}
    edges = {"t": ["kill", "purge"], "kill": ["victim"], "purge": ["flush", "may_block"]}
    whole = Walk(guarded, edges, lambda name: name == "kill", 0, {"purge": "may_block"})
    assert whole.depth("t") == 23 and whole.chain("t") == ("t", "purge", "may_block"), whole.chain("t")
    assert Walk(guarded, edges, lambda name: False).depth("t") == 521
    # An image the walk cannot read fails the row, whatever its figures (ledger 8738 B2).
    assert not_judged("aarch64", {"f": ["__AArch64AbsLongThunk_g"]}, 0)
    assert not_judged("x86_64", {"f": []}, 1)
    assert not_judged("x86_64", {"f": []}, 0) is None
    _, refused = judge({"f": 8}, {"f": ["__ARMv7ABSLongThunk_g"]}, "armv7a", 16384)
    assert any("not judged" in problem for problem in refused), refused
    # L.object.374: the kill reaching an unmap, or its hand-off the allocator, is a problem; a way
    # through a switch or a task's last drop in a wake is not followed.
    kill, hand = EMPTIES_NOTHING[0][0], EMPTIES_NOTHING[1][0]
    unmap = "<ferrix_kernel_core::user::space::AddressSpace>::unmap"
    alloc = "__rustc::__rust_alloc"
    switch, last = "ferrix_kernel_core::sched::schedule", "<alloc::sync::Arc<ferrix_kernel_core::sched::task::Task>>::drop_slow"
    weights = {name: 8 for name in (kill, hand, unmap, alloc, switch, last, "ended", "wake")}
    clean = {kill: ["ended"], "ended": [hand], hand: ["wake"], "wake": [switch, last], switch: [alloc], last: [alloc, unmap]}
    found = []
    assert empties_nothing(weights, clean, found) and not found, found
    dirty = dict(clean, ended=[hand, unmap], wake=[switch, last, alloc])
    found = []
    empties_nothing(weights, dirty, found)
    assert len(found) == 2 and any("unmaps a space" in f for f in found) and any("allocates" in f for f in found), found
    assert empties_nothing({"x": 8}, {}, []) is None, "an image with no OOM kill has no such line"
    problems = []
    assert named(frames, "nothing", problems) is None and problems
    # The way out and the emptier fail closed in an image with the syscall layer (ledger 9096 H1).
    load = {"ferrix_kernel_load::x": 8, EXIT: 8, EMPTIER: 8}
    problems = []
    rows_named(load, False, problems)
    assert not problems, problems
    for gone in (EXIT, EMPTIER):
        problems = []
        rows_named({name: 8 for name in load if name != gone}, False, problems)
        assert len(problems) == 1 and gone in problems[0] and "renamed" in problems[0], problems
    problems = []
    rows_named({"x": 8}, True, problems)
    assert not problems, "a user kernel is excepted"
    _, refused = judge({"ferrix_kernel_load::x": 8}, {}, "x86_64", 20480)
    assert any(EXIT in problem for problem in refused) and any(EMPTIER in problem for problem in refused), refused
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
    slots = got_targets(Path(elf).read_bytes()) if arch == "x86_64" else None
    frames, calls, unresolved, unfollowed = read_frames(text, slots)
    lines, problems = judge(frames, calls, arch, stack_bytes(arch), unresolved, "--chains" in sys.argv, unfollowed)
    print("\n".join(lines))
    for problem in problems:
        print(f"check-stack-depth: {problem}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
