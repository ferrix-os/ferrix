//! ARMv7-A's side-channel defences.
//!
//! `docs/certification/SPECULATION.md` §5 argues the set; this applies it.
//!
//! | Hazard | Defence here | When |
//! |---|---|---|
//! | Spectre v1 | indices clamped with a conditional move and `csdb` | always |
//! | Spectre v2 | `BPIALL` (Cortex-A8, A9, A12, A17) or `ICIALLU` (Cortex-A15, Brahma-B15) when the processor switches address space | the core is one Arm lists as affected |
//! | Meltdown, store bypass, BHB | none needed on the reference core | -- |
//!
//! The reference machine's core, the Cortex-A7 of the STM32MP157 and of QEMU's
//! `cortex-a7`, is an in-order design that Arm lists as affected by none of
//! the variants, so all it gets is the clamp -- whose `csdb` is a hint it runs
//! as a no-op. The branch predictor barrier is here for the cores that do
//! need it, which the kernel boots on but the reference configuration does
//! not include; on the A8 and the A15 it works only where secure firmware set
//! `ACTLR.IBE`, which the boot log reports.

use core::arch::asm;
use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use super::cpu;
use crate::arch::speculation::{Defences, HARDENED, record_this_cpu};
use crate::console::println;

/// Which switch barrier this machine's cores need.
static BARRIER: AtomicU8 = AtomicU8::new(BARRIER_NONE);
/// None.
const BARRIER_NONE: u8 = 0;
/// `BPIALL`: invalidate the whole branch predictor.
const BARRIER_BPIALL: u8 = 1;
/// `ICIALLU`: invalidate the instruction cache, which on the Cortex-A15
/// invalidates the branch predictor with it once firmware has set
/// `ACTLR.IBE`.
const BARRIER_ICIALLU: u8 = 2;
/// The plan's defences, for the secondaries.
static PLAN_DEFENCES: AtomicU32 = AtomicU32::new(0);

/// What `MIDR` says the core is, as far as this module cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    /// Cortex-A7, and every core not named below: Arm lists it as affected
    /// by none of the variants.
    Unaffected,
    /// Cortex-A8, which needs `BPIALL` and `ACTLR.IBE` (bit 6).
    CortexA8,
    /// Cortex-A9, A12 or A17, which need `BPIALL`.
    Bpiall,
    /// Cortex-A15 or Brahma-B15, which need `ICIALLU` and `ACTLR.IBE`
    /// (bit 0).
    CortexA15,
}

impl Part {
    /// This core.
    fn read() -> Part {
        let midr = read_midr();
        let implementer = midr >> 24;
        let part = (midr >> 4) & 0xFFF;
        match (implementer, part) {
            (0x41, 0xC08) => Part::CortexA8,
            (0x41, 0xC09 | 0xC0D | 0xC0E) => Part::Bpiall,
            (0x41, 0xC0F) | (0x42, 0x00F) => Part::CortexA15,
            _ => Part::Unaffected,
        }
    }

    /// The barrier this part needs, and whether firmware set the `ACTLR` bit
    /// it needs to work.
    fn barrier(self) -> (u8, bool) {
        match self {
            Part::Unaffected => (BARRIER_NONE, true),
            Part::Bpiall => (BARRIER_BPIALL, true),
            Part::CortexA8 => (BARRIER_BPIALL, cpu::read_actlr() & (1 << 6) != 0),
            Part::CortexA15 => (BARRIER_ICIALLU, cpu::read_actlr() & 1 != 0),
        }
    }
}

/// Decide, apply on the boot processor, and say what was done.
pub(crate) fn init() {
    if !HARDENED {
        println!("  cpu      speculation defences off: built with --mitigations off");
        record_this_cpu(Defences::NONE);
        return;
    }
    let part = Part::read();
    let (barrier, enabled) = part.barrier();
    let mut plan = Defences::CLAMPED_INDICES;
    if barrier != BARRIER_NONE {
        BARRIER.store(barrier, Ordering::Relaxed);
        plan = plan.with(Defences::SWITCH_BARRIER);
    }
    PLAN_DEFENCES.store(plan.bits(), Ordering::Release);
    record_this_cpu(plan);
    println!("  cpu      speculation defences: {}", plan.names());
    let v2 = match (part, enabled) {
        (Part::Unaffected, _) => "not affected (Arm lists this core as unaffected)",
        (_, true) => "covered between programs",
        (_, false) => "NOT covered: firmware left ACTLR.IBE clear (AoU-11)",
    };
    println!(
        "  cpu      speculation exposure: Spectre v2 {v2}; Meltdown and store bypass not affected"
    );
}

/// Apply the boot processor's plan on a secondary, as it starts: there is
/// nothing to write to the core, only the plan to record.
pub(crate) fn apply_this_cpu() {
    let plan = if HARDENED {
        Defences::from_bits(PLAN_DEFENCES.load(Ordering::Acquire))
    } else {
        Defences::NONE
    };
    record_this_cpu(plan);
}

/// Issue the switch barrier, where the plan has one. Answers whether it did.
pub(crate) fn switch_barrier(_cpu: usize) -> bool {
    issue(BARRIER.load(Ordering::Relaxed))
}

/// Issue `barrier`. Answers whether there was one to issue.
fn issue(barrier: u8) -> bool {
    match barrier {
        BARRIER_BPIALL => {
            // SAFETY: (PROTECT) `BPIALL` invalidates the branch predictor, which only
            // costs time; the `isb` makes it take effect before the next
            // branch.
            unsafe {
                asm!("mcr p15, 0, {zero}, c7, c5, 6", "isb", zero = in(reg) 0_u32, options(nostack, preserves_flags));
            };
            true
        }
        BARRIER_ICIALLU => {
            // SAFETY: (PROTECT) `ICIALLU` invalidates the instruction cache (and, with
            // `ACTLR.IBE`, the predictor), which only costs time.
            unsafe {
                asm!("mcr p15, 0, {zero}, c7, c5, 0", "isb", zero = in(reg) 0_u32, options(nostack, preserves_flags));
            };
            true
        }
        _ => false,
    }
}

/// Nothing to add once every processor has started: `init` decided for all
/// of them, from the boot processor's `MIDR`, and printed the exposure then.
pub(crate) fn report_once_started() {}

/// What the boot check adds on this architecture: the barrier each part of
/// the table gets, for the cores the machine does not have, and the one
/// barrier no ARMv7-A core `virt` takes plans -- `BPIALL`, the Cortex-A9's --
/// issued once. It is an ARMv7-A instruction every core executes, and on
/// one whose predictor needs no invalidating it only costs time.
///
/// # Errors
///
/// A part given another barrier, or a barrier that was not issued.
pub(crate) fn check() -> Result<(), &'static str> {
    let parts = [
        (Part::Unaffected, BARRIER_NONE),
        (Part::Bpiall, BARRIER_BPIALL),
        (Part::CortexA8, BARRIER_BPIALL),
        (Part::CortexA15, BARRIER_ICIALLU),
    ];
    if parts
        .iter()
        .any(|&(part, barrier)| core::hint::black_box(part).barrier().0 != barrier)
    {
        return Err("a core was not given the switch barrier Arm lists for it");
    }
    if !issue(core::hint::black_box(BARRIER_BPIALL)) || issue(core::hint::black_box(BARRIER_NONE)) {
        return Err("a switch barrier was not issued as asked");
    }
    Ok(())
}

/// `index` if `index < len`, else zero: `movhs` on the flags `cmp` set, and
/// `csdb` so that no later instruction may use a predicted result of it.
#[inline(always)]
pub(crate) fn clamp_index(index: usize, len: usize) -> usize {
    if !HARDENED {
        return index;
    }
    let mut clamped = index;
    // SAFETY: (PROTECT) three register instructions; no memory, no stack.
    unsafe {
        asm!(
            "cmp {clamped}, {len}",
            "movhs {clamped}, #0",
            "csdb",
            clamped = inout(reg) clamped,
            len = in(reg) len,
            options(pure, nomem, nostack),
        );
    }
    clamped
}

/// `value` if `value < end`, else zero, for a 64-bit value in two registers:
/// the subtraction's borrow decides, as `cmp` does for one.
#[inline(always)]
pub(crate) fn clamp_below(value: u64, end: u64) -> u64 {
    if !HARDENED {
        return value;
    }
    let mut low = value as u32;
    let mut high = (value >> 32) as u32;
    // SAFETY: (PROTECT) five register instructions; no memory, no stack.
    unsafe {
        asm!(
            "subs {scratch}, {low}, {end_low}",
            "sbcs {scratch}, {high}, {end_high}",
            "movhs {low}, #0",
            "movhs {high}, #0",
            "csdb",
            low = inout(reg) low,
            high = inout(reg) high,
            end_low = in(reg) end as u32,
            end_high = in(reg) (end >> 32) as u32,
            scratch = out(reg) _,
            options(pure, nomem, nostack),
        );
    }
    (u64::from(high) << 32) | u64::from(low)
}

/// `MIDR`: who designed the core, and which part it is.
fn read_midr() -> u32 {
    let value: u32;
    // SAFETY: (SYSREG) an ID register read, legal at PL1 on every core.
    unsafe {
        asm!("mrc p15, 0, {}, c0, c0, 0", out(reg) value, options(nomem, nostack, preserves_flags));
    };
    value
}

/// The switch barrier between two programs of one speculation domain:
/// nothing. This architecture's barrier is the predictor invalidation alone,
/// which a domain leaves out (`docs/OPAQUE-KERNEL.md` §9.3a, A2).
pub(crate) fn switch_barrier_in_domain(_cpu: usize) -> bool {
    false
}

/// Whether [`switch_barrier_in_domain`] refills a return stack: there is
/// none to refill here.
pub(crate) const REFILL_IN_DOMAIN: bool = false;

/// No refill to want: see [`REFILL_IN_DOMAIN`].
pub(crate) const fn refill_wanted_in_domain() -> bool {
    REFILL_IN_DOMAIN
}
