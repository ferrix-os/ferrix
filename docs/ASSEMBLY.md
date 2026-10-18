# Assembly

This OS is written in Rust. This document is the complete argument for every
place it is not, and `scripts/check-asm-budget.py` fails the build on any
assembly site that is not listed in `scripts/asm-allowlist.json`.

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

Both architectures boot via UEFI, so firmware calls a Rust `efi_main` with the
CPU already in 64-bit mode, a stack set up and the MMU on. There is no bootstrap
assembly on either architecture, which is unusual and is the direct result of
choosing UEFI over Multiboot on x86-64 — a Multiboot kernel is entered in 32-bit
protected mode and has to reach long mode by hand.

The assembly the loader does contain is the page-table switch at the very end of
its life, after `ExitBootServices`: writing `CR3`, or programming `MAIR_EL1`,
`TCR_EL1`, `TTBR0/1_EL1` and `SCTLR_EL1`, and then jumping to a virtual address
that did not exist a moment earlier. That sequence cannot be a Rust function
call, because the return address would be in the old address space.

## The list

Each entry names why Rust cannot express it. Entries are added to
`scripts/asm-allowlist.json` in the same commit as the code.

### Both architectures

| Site | Why |
|---|---|
| Context switch | Saves and restores the callee-saved set and the stack pointer *between two different stacks*. The function returns onto a stack that belongs to another task; Rust has no way to say that. |
| CPU primitives | Single instructions with no Rust spelling: reading a control or system register, invalidating a TLB entry, memory barriers, `wfi`/`hlt`, `sti`/`msr daifclr`, `rdtsc` and the generic timer's comparator, `cpuid`. Each is one instruction wrapped in one `#[inline]` function. |

### x86-64

| Site | Why |
|---|---|
| Interrupt and exception stubs | The CPU pushes a hardware frame and, for some vectors, an error code — a difference of one word between vectors that Rust's calling convention cannot model. The stubs normalise this and call into Rust. |
| `SYSCALL` entry | `syscall` leaves the return address in `rcx`, flags in `r11`, and *does not switch the stack*. Entry has to swap to the kernel stack via `swapgs` and `%gs`-relative addressing before anything can be pushed. |
| AP trampoline | Application processors start in 16-bit real mode at a page-aligned physical address below 1 MiB. Rust has no 16-bit real-mode target. This is the only 16-bit code in the project. |

### AArch64

| Site | Why |
|---|---|
| `VBAR_EL1` vector table | The architecture defines a table of sixteen entries at fixed 128-byte offsets. The layout is the interface; a Rust `static` of function pointers is not what the CPU reads. |
| EL2 → EL1 drop | Firmware may hand off at EL2. Lowering to EL1 is an `eret` into a context that has to be constructed first, so the "return" goes somewhere the compiler cannot know about. |

## The budget

`max_total_lines` in the allow-list is an **absolute** cap, not a percentage,
because assembly here is a fixed cost. The list above is meant to be finished
once both architectures boot: a scheduler, a filesystem or a driver must add
nothing to it. Raising the cap is a commit whose message explains what the
machine made unavoidable.

The Rust percentage is reported on every run and is the project's founding
claim, but read it as a trend rather than a gate. With assembly held flat, the
share rises as the operating system is written — which is the honest way to get
there. A ratio ceiling tight enough to bind today would only be measuring how
young the tree is, and the way to satisfy it would be to write less OS.
