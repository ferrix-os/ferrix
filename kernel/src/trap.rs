//! What happens when the CPU stops running the program and enters the kernel.
//!
//! The two architectures disagree about almost everything here — x86-64 has 256
//! vectors and an error code that exists for ten of them; AArch64 has sixteen
//! vector-table entries and a syndrome register — so each `arch` module
//! classifies its own frame into the [`Trap`] below, and the policy is written
//! once.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch;
use crate::console::println;

/// Why the kernel was entered, in terms both architectures share.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Trap {
    /// A translation fault: the address was not mapped, or was mapped without
    /// the permission the access needed.
    PageFault(PageFault),
    /// A debugger breakpoint. `int3` on x86-64, `brk` on AArch64.
    Breakpoint,
    /// An instruction the CPU refused to execute.
    IllegalInstruction,
    /// A device interrupt, by controller-assigned number.
    Interrupt(u32),
    /// A deliberate entry from user mode. Stage 7 gives this a body.
    SystemCall,
    /// Anything else, which at this stage means the kernel has a bug.
    Fault {
        /// The architecture's name for it.
        name: &'static str,
        /// The architecture's own code: a vector on x86-64, a syndrome on
        /// `AArch64`.
        code: u64,
    },
}

/// The details of a translation fault.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct PageFault {
    /// The address the access was to.
    pub(crate) address: u64,
    /// True if the access was a write.
    pub(crate) write: bool,
    /// True if it was an instruction fetch.
    pub(crate) execute: bool,
    /// True if the fault came from user mode.
    pub(crate) user: bool,
    /// True if a mapping existed but denied the access; false if there was no
    /// mapping at all. The difference decides whether this is a protection
    /// violation or a page that has not been faulted in yet.
    pub(crate) present: bool,
}

/// Breakpoints taken since boot.
///
/// Counted rather than merely handled, so the boot self-check can prove the
/// whole path ran — entry, save, dispatch, restore, return — rather than only
/// that it did not crash.
static BREAKPOINTS: AtomicU64 = AtomicU64::new(0);

/// Page faults the kernel resolved by mapping a page.
static FAULTS_HANDLED: AtomicU64 = AtomicU64::new(0);

/// How many breakpoints have been taken.
pub(crate) fn breakpoint_count() -> u64 {
    BREAKPOINTS.load(Ordering::Relaxed)
}

/// How many page faults have been resolved.
pub(crate) fn handled_fault_count() -> u64 {
    FAULTS_HANDLED.load(Ordering::Relaxed)
}

/// Where every trap arrives, from either architecture's entry stub.
pub(crate) fn dispatch(frame: &mut arch::TrapFrame) {
    match arch::classify(frame) {
        Trap::Breakpoint => {
            // The two architectures disagree about where the saved instruction
            // pointer lands: x86-64's `int3` pushes the address after itself,
            // AArch64's `brk` the address *of* itself. Returning without
            // asking would loop forever on one of them.
            arch::advance_past_breakpoint(frame);
            let _ = BREAKPOINTS.fetch_add(1, Ordering::Relaxed);
        }
        Trap::PageFault(fault) => handle_page_fault(frame, fault),
        // The controller, not the CPU, knows which interrupt arrived and how
        // it is acknowledged, and the two architectures disagree about both.
        // So the architecture claims, dispatches and retires; what crosses
        // back into generic code is a number.
        Trap::Interrupt(_) => arch::service_interrupts(frame, crate::irq::dispatch),
        Trap::SystemCall => fatal(frame, "system call before stage 7"),
        Trap::IllegalInstruction => fatal(frame, "illegal instruction"),
        Trap::Fault { name, .. } => fatal(frame, name),
    }
}

/// Resolve a translation fault, or report it and stop.
///
/// Stage 3 handles exactly one case — a kernel address inside the on-demand
/// window, which is mapped and the instruction retried. That is deliberately
/// the same shape the real handler will have: a fault is resolved by *making
/// the mapping true* and returning, never by stepping over the instruction.
/// Everything else is a bug in the kernel and is fatal.
fn handle_page_fault(frame: &mut arch::TrapFrame, fault: PageFault) {
    if !fault.present
        && !fault.user
        && crate::mm::is_demand_window(fault.address)
        && crate::mm::map_demand_page(fault.address).is_ok()
    {
        let _ = FAULTS_HANDLED.fetch_add(1, Ordering::Relaxed);
        return;
    }

    println!();
    println!(
        "FERRIX-PANIC page fault at {:#x} ({}{}{}{})",
        fault.address,
        if fault.user { "user " } else { "kernel " },
        if fault.write { "write" } else { "read" },
        if fault.execute { ", fetch" } else { "" },
        if fault.present {
            ", protection"
        } else {
            ", not mapped"
        },
    );
    fatal(frame, "unhandled page fault");
}

/// Report a trap the kernel cannot continue past, and stop the machine.
fn fatal(frame: &arch::TrapFrame, what: &str) -> ! {
    println!();
    println!("FERRIX-PANIC {what}");
    arch::report_trap(frame);
    arch::halt()
}
