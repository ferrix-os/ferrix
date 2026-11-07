//! Address space identifiers on `AArch64` (`docs/microkernel/K1.md` §8.1): a table
//! per processor of sixteen slots, each the id of the root it tags, so that a switch
//! back to a root in a slot keeps its translations.
//!
//! The tag sites, every one: [`tagged_root`] writes it to bits 63:48 of `TTBR0_EL1`,
//! [`super::cpu::write_ttbr0_invalidating`] to the same bits of `TLBI ASIDE1`'s
//! operand, and `disable_ttbr0` writes ASID 0, which no user translation is ever
//! tagged with. `TCR_EL1.AS` is 0 and `A1` is 0 on every loader, so eight bits are
//! the ASID, and one write of `TTBR0_EL1` changes root and ASID at once.
//!
//! A hit invalidates nothing, and that is safe because every shootdown here is
//! broadcast and names no ASID (`tlbi vaae1is`, `tlbi vmalle1is`,
//! `tools/common/check/check-asid-tlbi.py`), so it has reached this TLB under every
//! tag, whether or not the space was on this processor. Ids are never reused, so a
//! space that has gone is never hit again, and its entries go with the next miss
//! that takes its slot.

pub(crate) mod check;

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::cpu;
use ferrix_sched_class::MAX_CPUS;

/// How many tags a processor has: ASIDs 1 to 16.
pub(crate) const SLOTS: usize = 16;

/// Where `TTBR0_EL1` and `TLBI`'s operand hold the ASID.
const ASID_SHIFT: u32 = 48;

/// In [`Slots::last`] alone, never in the register: the write kept the slot's
/// translations. `TTBR0_EL1` has no such bit, so the boot's check reads this record.
const KEPT: u64 = 1 << 63;

/// In [`Slots::last`] alone: the low bits are free in an aligned root, and hold the
/// ASID where the tag cases read a tag.
const fn record(root: u64, asid: u64, kept: bool) -> u64 {
    let value = root | asid;
    if kept { value | KEPT } else { value }
}

/// One processor's slots.
struct Slots {
    /// The id of the root each slot tags, zero for none.
    space: [AtomicU64; SLOTS],
    /// The slot the next miss takes.
    next: AtomicUsize,
    /// The record of the last write, for the boot's check: see [`record`].
    last: AtomicU64,
}

impl Slots {
    const fn new() -> Slots {
        Slots {
            space: [const { AtomicU64::new(0) }; SLOTS],
            next: AtomicUsize::new(0),
            last: AtomicU64::new(0),
        }
    }

    /// Which slot the root with id `space` takes, and whether its translations are
    /// kept. At most [`SLOTS`] compares.
    fn choose(&self, space: u64) -> (usize, bool) {
        let held = self
            .space
            .iter()
            .position(|slot| slot.load(Ordering::Relaxed) == space);
        if let Some(slot) = held {
            return (slot, true);
        }
        // A miss takes the next slot round robin, whoever held it.
        let slot = self.next.load(Ordering::Relaxed) % SLOTS;
        self.next.store((slot + 1) % SLOTS, Ordering::Relaxed);
        if let Some(owner) = self.space.get(slot) {
            owner.store(space, Ordering::Relaxed);
        }
        (slot, false)
    }
}

/// Every processor's slots, by logical number.
static TABLE: [Slots; MAX_CPUS] = [const { Slots::new() }; MAX_CPUS];

/// The ASID of slot `slot`: one more than its index, so never ASID 0.
const fn asid_of(slot: usize) -> u64 {
    slot as u64 + 1
}

/// The value of `TTBR0_EL1` for `root` under `asid`.
const fn tagged_root(root: u64, asid: u64) -> u64 {
    root | (asid << ASID_SHIFT)
}

/// Make `root`, whose id is `space`, this processor's lower half. A hit writes the
/// root with its ASID; a miss invalidates the ASID before any walk can use it.
///
/// # Safety
///
/// (TRANSLATE) `root` must root a live set of tables for the lower half, and they
/// must stay live until another root replaces them on this processor. And interrupts
/// masked, with `logical` the processor running this: the slots are its alone.
pub(crate) unsafe fn install(root: u64, space: u64, logical: usize) {
    let Some(slots) = TABLE.get(logical) else {
        // No record for a processor past the table, which the scheduler never
        // starts: the first tag, invalidated every time.
        //
        // SAFETY: (TRANSLATE) the caller's guarantee.
        unsafe { cpu::write_ttbr0_invalidating(tagged_root(root, asid_of(0)), asid_of(0)) };
        return;
    };
    let (slot, kept) = slots.choose(space);
    let asid = asid_of(slot);
    let value = tagged_root(root, asid);
    if kept {
        // SAFETY: (TRANSLATE) the caller's guarantee.
        unsafe { cpu::write_ttbr0(value) };
    } else {
        // SAFETY: (TRANSLATE) the caller's guarantee.
        unsafe { cpu::write_ttbr0_invalidating(value, asid) };
    }
    slots
        .last
        .store(record(root, asid, kept), Ordering::Relaxed);
}

/// The ASID the running processor's `TTBR0_EL1` holds now.
pub(crate) fn tag_now() -> u64 {
    cpu::read_ttbr0() >> ASID_SHIFT
}

/// The record of the last install on processor `logical`, and the bit that says it
/// kept: what `user::check`'s tag cases read.
pub(crate) fn last_write(logical: usize) -> Option<(u64, u64, bool)> {
    TABLE
        .get(logical)
        .map(|slots| (slots.last.load(Ordering::Relaxed), KEPT, true))
}
