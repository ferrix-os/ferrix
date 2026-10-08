# Assembly

This OS is written in Rust. This document is the complete argument for every
place it is not, and `tools/common/check/check-asm-budget.py` fails the build on any
assembly site that is not listed in `tools/common/data/asm-allowlist.json`.

## The test for admission

> The construct is defined by what the **machine** does to registers around it,
> before or after the first instruction of a Rust function could run.

An exception vector qualifies: the CPU jumps to a fixed offset with the
interrupted program's registers still live, and no Rust function can be entered
without a prologue that would destroy them. A scheduler does not qualify:
nothing about picking the next task is architectural once the registers have
been saved.

"Convenient in assembly" is not a reason. "Faster in assembly" is not a reason
either — measure it, then argue it in the allow-list entry.

## Boot: none

Every architecture boots via UEFI, so firmware calls a Rust `efi_main` with a
stack set up and the MMU on — EDK2 on the 64-bit pair, U-Boot on ARMv7-A. There
is no bootstrap assembly on any of them, which is unusual and is the direct
result of choosing UEFI: over Multiboot on x86-64, whose kernel is entered in
32-bit protected mode and has to reach long mode by hand, and over a bare
`-kernel` boot on ARMv7-A, which is entered with the MMU off and has to build
page tables before a Rust function could safely run.

The assembly the loader does contain is the page-table switch at the very end of
its life, after `ExitBootServices`: writing `CR3`; or programming `MAIR_EL1`,
`TCR_EL1`, `TTBR0/1_EL1` and `SCTLR_EL1`; or, on ARMv7-A, `MAIR0/1`, `TTBCR`,
`TTBR0/1` through `mcrr` and `SCTLR` — and then jumping to a virtual address
that did not exist a moment earlier. That sequence cannot be a Rust function
call, because the return address would be in the old address space.

On ARMv7-A that sequence is also *copyable*. Firmware may load the loader where
neither translation tree can map it at its own address while the MMU comes back
on — QEMU's `virt` with 2 GiB puts it inside the direct map's virtual range —
and then the loader copies the switch to a page below the split and runs it
there. So the sequence is a block between `ferrix_switch_start` and
`ferrix_switch_end`, register-only and position-independent, and
`tools/common/xtask/src/pe.rs` refuses a loader whose block is over a page or holds a
relocation. It is the same instructions, not more of them; the product owner
admitted the block on 2026-09-13 on exactly those conditions.

### The one loader without UEFI: the Pixel 7

`src/boot/vendor/google/pixel7/` is the exception, because the phone offers no UEFI. Its
signed bootloader, ABL, boots an Android boot image under the Linux arm64 boot
protocol: it jumps to the image's first byte **at EL2, with the MMU off and no
stack**, the device tree's address in `x0`. What UEFI does for `src/boot/common/uefi/`, that
loader has to do itself before any Rust runs, and each piece is defined by the
machine or by ABL rather than by choice:

* the 64-byte arm64 `Image` header at the start of the image, whose branch,
  sizes, flags and magic sit at offsets ABL reads;
* setting the stack pointer and zeroing `.bss`;
* the drop from EL2 to EL1 -- `HCR_EL2`, `CNTHCTL_EL2`, `CPTR_EL2`,
  `ICC_SRE_EL2`, then `ELR_EL2`/`SPSR_EL2` and `eret` -- so the kernel runs at
  the level it is written for;
* a `VBAR_EL1` table, sixteen entries at fixed 128-byte offsets, so a fault in
  the loader is reported rather than hung on;
* and at the other end the switch into the kernel, which is `src/boot/common/uefi/`'s less the
  half that turns the MMU off, since it never came on; with it `dc ivac`,
  because everything the loader writes goes to RAM with the caches off, and a
  line ABL left cached must be discarded rather than cleaned over it.

All of it is in one file, `src/boot/vendor/google/pixel7/src/entry.rs`, allow-listed with
a budget of 130 lines.

## The list

Each entry names why Rust cannot express it. Entries are added to
`tools/common/data/asm-allowlist.json` in the same commit as the code.

### Every architecture

| Site | Why |
|---|---|
| Context switch | Saves and restores the callee-saved set and the stack pointer *between two different stacks*. The function returns onto a stack that belongs to another task; Rust has no way to say that. On x86-64 it is a naked function (`#[unsafe(naked)]`, `naked_asm!`) rather than a `global_asm!` symbol, so that the kernel, built position-independent for KASLR, calls it directly rather than through the GOT (`docs/OPAQUE-KERNEL.md` §9.11, Q8); the trampoline a new task starts on, `ferrix_task_entry`, stays in `global_asm!`, since nothing calls it: only its address is pushed. |
| CPU primitives | Single instructions with no Rust spelling: reading a control or system register (`mrs`, or `mrc`/`mrrc` on `cp15`), invalidating a TLB entry, memory barriers, `wfi`/`hlt`, `sti`/`msr daifclr`/`cpsie`, `rdtsc` and the generic timer's comparator, `cpuid`, and the `hvc`/`smc` a PSCI call is made with. Each is one instruction, or one short fixed sequence, wrapped in one `#[inline]` function. |
| Side-channel primitives (`speculation.rs`) | The Spectre v1 clamp: a compare and a conditional select (`sbb` on x86-64, `csel` or `movhs` then `csdb` on the Arm pair) that bounds a program's index by a data dependency rather than a branch. Written in Rust the compiler folds it into the bounds check it follows and nothing is left; `csdb` has no Rust spelling at all. Beside it, the predictor barriers a switch of address space issues (x86-64's return stack refill, ARMv7-A's `BPIALL`/`ICIALLU`), x86-64's `VERW`, and the Arm ID and control register accesses the plan is made from. `docs/certification/SPECULATION.md`. |
| Device register access (`arch/aarch64/mmio.rs`, `arch/armv7a/mmio.rs`) | One `ldrb`/`ldrh`/`ldr` or `strb`/`strh`/`str` with a bare base register for each access. Under KVM a register access traps, and the hypervisor emulates it from the fault's syndrome, which a load or store with writeback, or a pair, does not carry: `KVM_RUN` fails and the guest stops. Volatile says the access happens, not which instruction does it, and LLVM folds repeated reads at one offset into a pre-indexed `ldrh`, which stopped the Pixel 7's VM in stage 10. Linux's `readl` and `writel` are assembly for the same reason. x86-64 keeps volatile, since KVM there decodes the instruction. |

### x86-64

| Site | Why |
|---|---|
| Interrupt and exception stubs | The CPU pushes a hardware frame and, for some vectors, an error code — a difference of one word between vectors that Rust's calling convention cannot model. The stubs normalise this and call into Rust. |
| Paranoid entry | The NMI, `#DB`, `#MC` and the double fault arrive on any instruction, including the `SYSCALL` trampoline's ring-0 stretches on the program's stack and `GS`, where the saved `CS` does not say which way round `GS` is. Before any Rust can run, the entry has to read `GS_BASE` with `rdmsr` and `swapgs` only if it is not the kernel's, clear `DR7` so no breakpoint fires on its IST stack, and move a ring-3 `#DB` off that stack onto the task's; and after Rust returns, undo exactly what it did. `swapgs`, `mov %dr7` and the stack switch are side effects on how the next instruction runs, which no function can hold. |
| `SYSCALL` entry | `syscall` leaves the return address in `rcx`, flags in `r11`, and *does not switch the stack*. Entry has to swap to the kernel stack via `swapgs` and `%gs`-relative addressing before anything can be pushed. |
| Per-processor word through `GS` (`cpu.rs`, `read_gs_at` and `gs_xadd`) | One `mov` from, and one `xadd` without `lock` to, a word of this processor's record addressed as `gs:[offset]`: the preemption count every kernel spin lock raises and lowers (`sched/preempt.rs`, OPAQUE-KERNEL.md §9.8 2b). The single instruction is the point. The processor finds its record and changes it in one step, so an interrupt is taken before or after it and a task cannot be moved between finding its processor's record and changing it -- the bug the old masked window existed to close, at the cost of an interrupt save and restore and two locked operations on every lock taken. Rust has no segment-relative addressing, and with a pointer read first it reopens that window. Linux's `this_cpu_add` and `raw_cpu_read` are assembly for the same reason. Added 2026-10-03, two lines, by the consultant's Q2 of 2026-10-02. |
| AP trampoline | Application processors start in 16-bit real mode at a page-aligned physical address below 1 MiB. Rust has no 16-bit real-mode target. This is the only 16-bit code in the project. |

### AArch64

| Site | Why |
|---|---|
| `VBAR_EL1` vector table | The architecture defines a table of sixteen entries at fixed 128-byte offsets. The layout is the interface; a Rust `static` of function pointers is not what the CPU reads. |
| EL0 entry and return | Dropping to EL0 is an `eret` into a program that has never run, so its state — `SP_EL0`, `ELR_EL1`, `SPSR_EL1`, and every general register zeroed so nothing of the kernel's leaks — has to be built first. And the stack pointer has to move to a dedicated entry stack *between* the last Rust frame and the `eret`: at EL1, `sp` is `SP_EL1`, the stack every exception from EL0 lands on, so leaving it where the kernel parked its own registers means the program's first fault pushes a frame over them. The return, `ferrix_leave_user`, restores that parked stack from inside a system call and abandons the call's frame. |
| EL2 → EL1 drop | Firmware may hand off at EL2. Lowering to EL1 is an `eret` into a context that has to be constructed first, so the "return" goes somewhere the compiler cannot know about. |
| Secondary core entry | PSCI `CPU_ON` starts a core at a physical address with its MMU and caches off and no stack. Installing the translation regime and turning the MMU on while executing at an address only an identity map makes meaningful is the loader's switch again, once per core. |

### ARMv7-A

| Site | Why |
|---|---|
| The vector table and its `srs`/`rfe` path | Eight one-instruction entries at `VBAR`, and an exception model that delivers each exception into one of five processor modes, every one with its own banked stack pointer and link register. Each stub corrects the banked `lr` by an offset the architecture fixes per exception, stores it and the saved status on the *SVC* stack with `srsdb`, and switches to SVC mode with `cps`; the return is `rfeia`, which loads the program counter and the status in one instruction. No Rust function can be entered in a mode whose stack pointer nothing ever set — which is the point: no mode but SVC is given one. |
| USR entry and return | AArch64's argument, in ARMv7-A's modes. The program's stack pointer is banked and can only be set from a mode that shares it, so it is set from System mode. The SVC stack pointer — where every exception from USR is stored with `srsdb` — has to move to a dedicated entry stack between the last Rust frame and the drop, or the program's first fault lands on the registers the kernel parked. And the drop is an `rfeia` of a return address and status built for a program that has never run. |
| Secondary core entry | AArch64's argument again: PSCI `CPU_ON` starts a core at a physical address with its MMU and caches off and no stack. The entry checks it was started in SVC mode, loads its whole start block in one `ldm` while nothing is mapped, invalidates the instruction cache and branch predictor as the loader does, installs `MAIR0/1`, `TTBCR`, both `TTBR`s and the boot core's `SCTLR`, and turns the MMU on while executing through an identity map. |

The loader's switch is argued under *Boot*, and the `cp15` accessors are the
CPU primitives above. What ARMv7-A does not need is AArch64's EL2 → EL1 drop:
the loader refuses to start in HYP mode rather than leaving it, because U-Boot
enters in SVC mode unless the machine was built with virtualisation.

### A native program, on every architecture

`src/user/system/native/rt`, the runtime a native program links, has one file per architecture
under `src/user/system/native/rt/src/arch/`, and each holds the same three things.

| Site | Why |
|---|---|
| `_start` | A process is *entered*, not called. The kernel drops to user mode with `SYSRET`, `eret` or `rfeia`, so there is no return address and no caller's frame. The stack is aligned for a program entry, not for a callee: on x86-64 that is eight bytes off what a function prologue assumes. Two to five instructions zero the frame pointer (and link register), align the stack, and call `ferrix_rt_start`, leaving the bootstrap handle in the first argument register. |
| The trap | `syscall` or `svc #0`, and the register each argument goes in. This is the kernel's system call entry, read from the other side, and it has no Rust spelling. It is one instruction in an `asm!` block. Each register is named as an operand, and the budget counts those lines too, because the register assignment *is* the contract. |
| `exit_group` | The same trap, with nothing to return to. |

Nothing a runtime grows later belongs here: not threads, thread-local storage
or an allocator.

## The budget

`max_total_lines` in the allow-list is an **absolute** cap, not a percentage,
because assembly here is a fixed cost. The list above is meant to be finished
once every architecture boots: a scheduler, a filesystem or a driver must add
nothing to it. Raising the cap is a commit whose message explains what the
machine made unavoidable.

What counts is every line of assembly text: each line holding an ordinary
string literal in an `asm!`, `naked_asm!` or `global_asm!`, and each line of a
raw string (`r#"..."#`) that holds an instruction, a label or a directive,
blank lines and assembler comments aside. Until 2026-09-27 a raw string counted
as the line or two its quotes were on, so the tree's trap tables, context
switches and secondary entries passed for 733 lines when they were 1569. The
cap went from 800 to 1600 and nine budgets to their true counts in the commit
that fixed the count. Only one of the nine had grown meanwhile, AArch64's
secondary entry, by the EL2 drop the Pixel 7 needs.

The Rust percentage is reported on every run and is the project's founding
claim, but read it as a trend rather than a gate. With assembly held flat, the
share rises as the operating system is written — which is the honest way to get
there. A ratio ceiling tight enough to bind today would only be measuring how
young the tree is, and the way to satisfy it would be to write less OS.
