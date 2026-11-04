#!/usr/bin/env python3
"""Write docs/microkernel/M5-SHIM-MAP.md: what each core and item item the
load names maps to in linuxd's shim (docs/microkernel/M5-LIFT.md §3).

The items come from microkernel-surface.py's own resolver, so the map moves
with the tree: an item the load starts to name and no rule places fails
`--check`. Usage: m5-shim-map.py [--check]
"""

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
OUT = ROOT / "docs/microkernel/M5-SHIM-MAP.md"

# Longest prefix wins. Classes: S (the same source, compiled into linuxd),
# R (reimplemented in user space, same signature), H (a wrapper over a native
# call on a handle), L (leaves with the load; the name moves to linuxd),
# N (not reachable: board glue, a check, a boot-only path; a build error if
# named under personality=user), P (a stub until its row lands).
RULES = [
    ("arch", "R", "the architecture module of the shim: a few unprivileged items"),
    ("arch::CAN_MOVE_INTERRUPTS", "S", "constant"),
    ("arch::Generic", "S", "Linux stat layouts: Linux ABI data, moves to linuxd (§2.1)"),
    ("arch::Legacy", "S", "the same"),
    ("arch::Stat64", "S", "the same"),
    ("arch::StatLayout", "S", "the same"),
    ("arch::aarch64", "S", "as arch::x86_64 once AArch64 is lifted (milestone 6)"),
    ("arch::x86_64", "S", "as below"),
    ("arch::cpuid", "H", "CPUID is unprivileged: the instruction, from ring 3"),
    ("arch::midr_of", "R", "from the AT_HWCAP word the kernel hands linuxd at start"),
    ("arch::percpu", "R", "a counter of linuxd's own, per thread"),
    ("arch::render_machine_check", "P", "/proc text; an empty answer until a native read exists"),
    ("arch::sigpage_code", "S", "linuxd's image (§2.1), ARMv7-A only, not lifted: native-only (Q7)"),
    ("arch::thread_core", "R", "THREAD_INFO on the thread, for /proc"),
    ("arch::user_extended_state", "R", "K1's context carries the vector state; linuxd reads its size"),
    ("audit", "H", "the kernel's audit stays for its own events; linuxd's records go to the debug log (§6.1: outside the TSF)"),
    ("claim", "S", "the served calls' claim table (pure data), for the device cores"),
    ("console::input", "H", "CONSOLE_READ with the console's event, until M6 moves the console"),
    ("console::log", "R", "linuxd's own ring for /dev/kmsg, fed by the debug log"),
    ("console::println", "R", "the debug log"),
    ("console::pstore", "N", "stays the kernel's panic path"),
    ("console::screen", "N", "stays the kernel's until M6"),
    ("console::write", "H", "DEBUG_WRITE"),
    ("console", "H", "DEBUG_WRITE"),
    ("crate::Live", "S", "the settings' seam, in linuxd"),
    ("crate::build_flag_word", "S", "the macro, compiled in linuxd's crates"),
    ("crate::flag_word", "S", "the same"),
    ("crate", "L", "the load's own root"),
    ("device", "H", "DEVICE_INFO; /sys's view of what the kernel published, over devmgr's channel"),
    ("discovery::board", "N", "board glue: M6"),
    ("discovery::devmgr", "H", "the channel to devmgr (§5); the kernel starts devmgr"),
    ("discovery::fdt", "H", "the read-only firmware VMO (§2.1)"),
    ("discovery::smbios", "H", "the read-only firmware VMO (§2.1)"),
    ("fallible", "S", "pure: the same file, by position"),
    ("hooks", "S", "the hook list's type, for linuxd's own lists"),
    ("init", "R", "linuxd's boot: init's three inputs as VMOs (§5); Launcher is called by linuxd's main, not the kernel"),
    ("iommu", "N", "the kernel's; a /proc line through DEVICE_ISOLATION"),
    ("irq", "P", "/proc/interrupts: empty until a native read exists"),
    ("mm::scrub", "P", "/proc text"),
    ("mm::zero_frame", "R", "a read-only zero VMO of linuxd's"),
    ("mm::direct_map", "R", "no direct map: VMO mappings in the shared region"),
    ("mm::with_initrd", "R", "the initramfs VMO (§5)"),
    ("mm::keep_initrd", "R", "the initramfs VMO (§5)"),
    ("mm::give_back_initrd", "R", "closes the initramfs VMO handle"),
    ("mm", "R", "frames become pages of linuxd's own VMOs; the counters come from JOB_GET_QUOTA"),
    ("mmio", "N", "board glue"),
    ("object::job", "H", "the cgroup tree: JOB_CREATE, JOB_KILL, JOB_SET_LIMIT, JOB_GET_QUOTA on handles; the node data stays linuxd's"),
    ("object::quota", "H", "the same; constants the same source"),
    ("object::process", "R", "linuxd's process list and pids (§4); Host is implemented by the load's Process on the shim's Process"),
    ("object::pin", "H", "VMO_PIN, for the device cores"),
    ("object::channel", "H", "CHANNEL_* on a handle"),
    ("object::port", "H", "PORT_*; Observer through a reader thread (§4.4)"),
    ("object::oom", "P", "the job's limit answers ENOMEM; the kernel's OOM kill stays"),
    ("object", "H", "handles: each Object variant wraps a kernel handle"),
    ("panic::catalog", "S", "the codes; a linuxd fatal ends the partition (K13's row)"),
    ("panic::fatal", "R", "print, then PROCESS_EXIT: a Linux-layer panic is a process fault (§6.4 (d))"),
    ("power", "H", "POWER_* plus linuxd's own flush list, run before asking the kernel"),
    ("random", "H", "RANDOM_FILL"),
    ("sched::wait", "R", "wait queues in the shared region, on K5's ADDRESS_WAIT"),
    ("sched::work", "R", "the per-thread work word; a post kicks (K1) or wakes"),
    ("sched::task", "R", "the shim's Task, one per kernel thread"),
    ("sched::affinity", "H", "THREAD_AFFINITY"),
    ("sched::borrow", "R", "the thread's Task, from the aligned stack"),
    ("sched::inspect", "P", "/proc/<pid>/stack: empty"),
    ("sched::trip", "N", "the kernel's ring-trip measure"),
    ("sched::vtime", "P", "virtual time accounting is the kernel's"),
    ("sched::preempt", "P", "/proc text"),
    ("sched::spawn", "H", "THREAD_CREATE in linuxd's process"),
    ("sched::sleep", "H", "ADDRESS_WAIT with a deadline"),
    ("sched::yield_now", "H", "THREAD_YIELD"),
    ("sched::current", "R", "the aligned stack"),
    ("sched", "R", "per-thread state in the shim; times from THREAD_INFO"),
    ("signal_frame", "S", "§2.1: linuxd writes frames itself; the same file"),
    ("smp::service", "N", "the kernel's IPI service"),
    ("smp", "R", "the processor count from the boot page"),
    ("sync", "R", "a test-and-set lock that yields; no interrupt state in ring 3"),
    ("syscall::native", "H", "the device cores' served calls end with M5's retired calls (§3): stubbed ENOSYS, then channels"),
    ("syscall::program", "S", "pure helpers"),
    ("syscall::uaccess", "R", "SPACE_READ/SPACE_WRITE, then direct copies under K1's layout"),
    ("syscall::Personality", "S", "the trait, implemented by the load's Linux"),
    ("syscall::unanswered", "S", "the same"),
    ("syscall", "L", "the load's syscall module"),
    ("timer", "R", "the time page VMO and the counter (§2.1)"),
    ("trap::ReturnPath", "R", "linuxd's restricted-exit loop calls these directly"),
    ("trap::set_return_path", "R", "the same"),
    ("trap::attention_due", "R", "the same"),
    ("trap", "S", "the Outcome types: the same declarations"),
    ("user::cache", "R", "linuxd's page cache accounting; pages are VMOs with linuxd as pager (K3)"),
    ("user::memory_type", "H", "MAP on an I/O mapping; kernel's"),
    ("user::space", "H", "K2's SPACE_* on the process's space handle; layout lock and the VMA list are linuxd's"),
    ("user::vmo", "H", "VMO_* on a handle; Filler becomes K3's pager"),
    ("user", "H", "as above"),
    ("vmap", "R", "a VMO mapped into the shared region"),
]
X86 = {
    "signal": "S, §2.1: linuxd writes frames; the same file",
    "trap::UserRegs": "S, K1's register block",
    "trap::enter_user": "H, K1's RESTRICTED_ENTER",
    "trap::resume_user": "H, K1's RESTRICTED_ENTER",
    "trap::stack_pointer": "S",
    "switch": "R, K1's context",
    "decode": "S, §2.1: the decode goes to linuxd",
    "set_thread_area": "H, K1's context (the kernel's sanitiser)",
    "thread_area": "H, K1's context",
    "vdso": "S, §2.1: the vDSO's code is linuxd's image",
    "cpu": "S, unprivileged cache maintenance",
}

def items():
    out = subprocess.run(
        [sys.executable, str(ROOT / "tools/common/gen/microkernel-surface.py"), "surface", "--list"],
        capture_output=True, text=True, check=True).stdout
    return sorted({l.strip() for l in out.splitlines() if l.startswith("  ")})

def place(item):
    best = None
    for prefix, cls, why in RULES:
        if item == prefix or item.startswith(prefix + "::") or (
            prefix.endswith(("::write", "::sleep", "::current")) and item.startswith(prefix)
        ):
            if best is None or len(prefix) > len(best[0]):
                best = (prefix, cls, why)
    return best

def render():
    lines = ["# M5's shim, item by item", "",
             "Generated by `tools/common/gen/m5-shim-map.py` from the item gate's resolver;",
             "`--check` fails when the load names an item no rule places. Classes and the",
             "reasoning are in [M5-LIFT.md](M5-LIFT.md) §3: **S** the same source compiled into",
             "linuxd, **R** reimplemented in user space with the same signature, **H** a wrapper",
             "over native calls on a handle, **L** the load's own, **N** not named under",
             "`personality=user`, **P** a stub until a named row lands.", "",
             "| Item | Class | Maps to |", "|---|---|---|"]
    counts, missing = {}, []
    for item in items():
        p = place(item)
        if p is None:
            missing.append(item)
            continue
        cls, why = p[1], p[2]
        if item.startswith(("arch::aarch64", "arch::x86_64")):
            for key, text in X86.items():
                if key in item:
                    cls, why = text.split(",")[0], text
                    break
        counts[cls] = counts.get(cls, 0) + 1
        lines.append(f"| `{item}` | {cls} | {why} |")
    summary = ", ".join(f"{k} {v}" for k, v in sorted(counts.items()))
    lines[3:3] = [f"Counts: {summary}.", ""]
    return "\n".join(lines) + "\n", missing

def main():
    text, missing = render()
    if missing:
        sys.exit("no rule places: " + ", ".join(missing))
    if "--check" in sys.argv:
        if not OUT.exists() or OUT.read_text() != text:
            sys.exit(f"{OUT} differs from the tree: run m5-shim-map.py")
        return
    OUT.write_text(text)
    print(text.splitlines()[3])

main()
