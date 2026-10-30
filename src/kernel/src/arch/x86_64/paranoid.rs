//! The paranoid entries: the NMI, the debug exception, the machine check and
//! the double fault.
//!
//! # Why these four cannot use the ordinary entry
//!
//! The ordinary stub decides whether to `swapgs` from the saved `CS`: ring 3
//! means `GS` is the program's, ring 0 the kernel's. That holds for everything
//! the kernel can hold off. An interrupt waits for `sti`, and every stretch of
//! ring 0 that runs with the program's `GS` or stack runs under `cli`; a fault
//! comes from an instruction, and those stretches hold none that fault.
//!
//! These four hold off for nothing. The `SYSCALL` trampoline has four ring-0
//! states in which the saved `CS` lies about `GS`, or the stack is the
//! program's:
//!
//! ```text
//!   ferrix_syscall_stub:  swapgs                 GS program's, RSP program's
//!                         movq %rsp, %gs:16      GS kernel's,  RSP program's
//!                         movq %gs:8, %rsp
//!   ...
//!                         popq %rsp
//!                         swapgs                 GS kernel's,  RSP program's
//!                         sysretq                GS program's, RSP program's
//! ```
//!
//! and `ferrix_enter_user`, `ferrix_resume_user` and the trap stub's own return
//! have the same shape. An NMI or a hardware breakpoint there, decided by `CS`,
//! runs its handler through a per-CPU pointer the program chose.
//!
//! # What the entry does
//!
//! `ferrix_paranoid_common` in `trap.rs` builds the ordinary frame on the
//! vector's own IST stack (`gdt.rs`), then:
//!
//! * **`GS` from `GS_BASE`.** It reads the register: a kernel `GS` base is a
//!   per-CPU record, a heap address in the upper half with its sign bit set, and
//!   no program can hold one -- a program starts with a `GS` base of zero
//!   (`syscall::init`), `arch_prctl(ARCH_SET_GS)` refuses every address that is
//!   not a user one (`syscall::arch_prctl`), `CR4.FSGSBASE` stays clear, so
//!   `WRGSBASE` is `#UD` in ring 3, and loading a selector in long mode sets a
//!   32-bit base. Anything else is the program's, so it swaps, and remembers in
//!   `EBX` to swap back on the way out. Linux's `paranoid_entry` made the same
//!   decision the same way until it enabled `FSGSBASE`. Turning `FSGSBASE` on
//!   for programs would end this rule: the entry would have to find its
//!   per-CPU record without `GS` (`docs/WINE.md` §3).
//! * **`DR7` saved and cleared** for the handler's whole run, and restored on
//!   the way out.
//! * **Each stack counts its occupants** in the word above the frame (the IST
//!   entry is sixteen bytes below the top). A second is reported, as FX-9006,
//!   rather than returned from.
//! * **A `#DB` or `#MC` from ring 3 leaves the IST stack.** A `#DB` is the
//!   program's own -- a trap flag, `SIGTRAP` -- and delivering it may block,
//!   switch or end the task, none of which can happen on a stack the next
//!   `#DB` on this processor starts from. A machine check from ring 3 may end
//!   in `SIGBUS` for the same program (`docs/MACHINE-CHECK.md` §3.3, as
//!   Linux's `exc_machine_check_user`). Either frame is copied to the task's
//!   kernel stack, where the processor would have pushed it without the IST,
//!   and takes the ordinary path, whose `CS` test is right for a ring-3 frame;
//!   a machine check's `MCIP` stays set until its banks are read there. Only
//!   vectors 1 and 18 are moved. The other two stay: an NMI from ring 3 is
//!   counted and returned from, and a double fault stops. A machine check
//!   from ring 0 never leaves its IST stack.
//!
//! # Nesting
//!
//! A handler on an IST stack that is entered again from its own vector starts
//! at the same top and overwrites itself. The scheme is to make that
//! impossible, and to catch it if a rule below is broken.
//!
//! * **`#DB` cannot nest.** The interrupt gate clears `TF`, so a handler does
//!   not single-step itself. `DR7` is cleared before any handler code runs,
//!   so no breakpoint fires while one is on its stack -- and an NMI that
//!   arrives in between clears it for its own run too. What is left is a
//!   breakpoint on the entry's own instructions before the clear, or on the IST
//!   stacks: nothing may arm one there. The boot check (`paranoid/check.rs`)
//!   is the only code that arms breakpoints; an interface that lets anything
//!   else must refuse `ferrix_paranoid_stubs` and the stacks, as Linux refuses
//!   `noinstr` text.
//!   This is Linux's scheme since 5.10, which retired its IST shift for it.
//! * **An NMI cannot nest.** The processor delivers no NMI after one until the
//!   next `iretq`, and the handler's own is its last instruction. An exception
//!   inside the handler that returned would be an earlier `iretq`, so the
//!   handler takes none: it touches statics and per-CPU records mapped since
//!   boot, never the demand window, prints nothing, and `DR7` is clear. Linux's
//!   nested-NMI frame juggling exists because its NMI handlers do fault.
//! * **`#DF` never returns, and `#MC` returns only with `MCIP` clear.** A
//!   ring-0 machine check returns only when every bank lets the kernel go on
//!   (`mce::on_kernel_stack`), having cleared `MCIP` as its last act: until
//!   then a second machine check shuts the processor down rather than
//!   entering this stack, and after it the handler touches the stack no more
//!   than the `iretq` does. Otherwise a second on the same stack is the
//!   report failing.

use core::sync::atomic::{AtomicU64, Ordering};

use super::cpu;
use super::trap::TrapFrame;
use crate::panic::catalog;

/// `#DB`.
const DEBUG: u64 = 1;
/// The NMI.
const NMI: u64 = 2;
/// `#MC`.
const MACHINE_CHECK: u64 = 18;

/// `DR6` with no condition recorded: the fixed-one bits set, the rest clear.
/// Linux's `DR6_RESERVED`.
const DR6_CLEAR: u64 = 0xFFFE_0FF0;
/// `DR6`'s B0 to B3: which hardware breakpoint matched.
const DR6_BREAKPOINTS: u64 = 0xF;
/// `RFLAGS.RF`: the instruction returned to runs without matching an
/// instruction breakpoint.
const RFLAGS_RF: u64 = 1 << 16;

// CHECKS-ONLY: arch/x86_64/paranoid/check.rs
/// NMIs taken.
#[doc(hidden)]
pub static NMIS: AtomicU64 = AtomicU64::new(0);

/// NMIs taken since boot: for interrupt remapping's check R2, that a forged
/// NMI-mode message arrived nowhere.
pub(crate) fn nmis() -> u64 {
    NMIS.load(Ordering::Relaxed)
}
// CHECKS-ONLY: arch/x86_64/paranoid/check.rs
/// Kernel `#DB`s taken for each hardware breakpoint, by the slot DR6 named.
#[doc(hidden)]
pub static BREAKPOINTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
// CHECKS-ONLY: arch/x86_64/paranoid/check.rs
/// Paranoid handlers that found `GS` naming no processor's record.
#[doc(hidden)]
pub static FOREIGN_GS: AtomicU64 = AtomicU64::new(0);
// CHECKS-ONLY: arch/x86_64/paranoid/check.rs
/// NMIs taken from ring 3, of [`NMIS`]: the ones whose entry found a
/// program's `GS` base in the register and swapped.
#[doc(hidden)]
pub static USER_NMIS: AtomicU64 = AtomicU64::new(0);

// CHECKS-ONLY: arch/x86_64/paranoid/check.rs
/// Runs of [`debug_hook`].
#[doc(hidden)]
pub static HOOK_RUNS: AtomicU64 = AtomicU64::new(0);

/// Where `ferrix_paranoid_common` hands over, on the vector's own stack, with
/// the kernel's `GS` and `DR7` clear.
///
/// `occupants` is the count of exceptions on this stack, this one included.
#[unsafe(no_mangle)]
extern "C" fn ferrix_paranoid_entry(frame: &mut TrapFrame, occupants: u64) {
    if occupants != 1 {
        crate::trap::fatal(
            frame,
            "an exception nested on its own interrupt stack",
            &catalog::NESTED_INTERRUPT_STACK,
        );
    }
    match frame.vector {
        DEBUG => debug(frame),
        NMI => {
            note_gs();
            let _ = NMIS.fetch_add(1, Ordering::Relaxed);
            if frame.cs & 3 == 3 {
                let _ = USER_NMIS.fetch_add(1, Ordering::Relaxed);
            }
        }
        // From ring 0 only: a ring-3 one left this stack in the stub. Returns
        // when every bank lets the kernel go on, with `MCIP` clear; stops as
        // FX-9005 otherwise (`docs/MACHINE-CHECK.md` §3.3).
        MACHINE_CHECK => super::mce::on_kernel_stack(frame),
        vector => crate::trap::fatal(
            frame,
            super::trap::vector_name(vector),
            &catalog::UNEXPECTED_EXCEPTION,
        ),
    }
}

/// A `#DB` taken in the kernel: a hardware breakpoint is counted and returned
/// from; anything else -- a single step, a general detect -- is nothing the
/// kernel asked for, and stops it.
fn debug(frame: &mut TrapFrame) {
    let status = take_debug_status();
    if status & DR6_BREAKPOINTS == 0 {
        crate::trap::fatal(
            frame,
            "a debug exception in the kernel that no hardware breakpoint raised",
            &catalog::UNEXPECTED_EXCEPTION,
        );
    }
    for (slot, count) in BREAKPOINTS.iter().enumerate() {
        if status & (1 << slot) != 0 {
            let _ = count.fetch_add(1, Ordering::Relaxed);
        }
    }
    note_gs();

    // An instruction breakpoint is a fault: the saved RIP is the instruction
    // that matched, and returning to it without RF matches it again, for ever.
    // RF lets exactly that instruction run. A data breakpoint is a trap, which
    // needs none, and RF costs it nothing.
    frame.rflags |= RFLAGS_RF;
    debug_hook();
}

/// Read and clear `DR6`. Sticky, so a `#DB` that left it set would be read
/// again by the next one.
pub(super) fn take_debug_status() -> u64 {
    let status = cpu::read_dr6();
    // SAFETY: (SYSREG) the value DR6 holds with nothing recorded, reserved bits as the
    // processor defines them.
    unsafe { cpu::write_dr6(DR6_CLEAR) };
    status
}

/// Count a handler that finds `GS` naming no processor's record -- which, once
/// the records exist, only a wrong `swapgs` decision can do.
///
/// Compares the register against every record rather than following it: that
/// is what `this_cpu_for_report` is for, and following a program's `GS` from
/// here would fault inside a handler that must not.
fn note_gs() {
    if crate::smp::topology().is_some() && crate::smp::this_cpu_for_report().is_err() {
        let _ = FOREIGN_GS.fetch_add(1, Ordering::Relaxed);
    }
}

// CHECKS-ONLY: arch/x86_64/paranoid/check.rs
/// Called by every kernel `#DB` handler run, so that the nesting check can put
/// a breakpoint on code the handler executes.
#[inline(never)]
#[doc(hidden)]
pub fn debug_hook() {
    let _ = HOOK_RUNS.fetch_add(1, Ordering::Relaxed);
}
