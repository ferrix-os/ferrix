//! The boot's check of the ASID slot rule (`docs/microkernel/K1.md` §8.1), on a table
//! of its own for roots described rather than installed, as `pcid::check`, so that it
//! runs on every boot. TCG models neither a TLB's tags nor their timing: QEMU drops
//! everything when an ASID changes, so a stale read cannot show, and the decision is
//! what is checked here. A real core's is the board row's.

use core::sync::atomic::Ordering;

use super::{ASID_SHIFT, SLOTS, Slots, asid_of, tagged_root};

/// The rule: a new root takes a slot and invalidates, a hit keeps, and a seventeenth
/// root takes a slot round robin whose owner invalidates when it comes back; every
/// ASID is in 1 to 16, in bits 63:48 of the root's value.
///
/// Verifies: L.aarch64.75, L.aarch64.76, L.aarch64.77, L.aarch64.79, L.aarch64.81
///
/// # Errors
///
/// The first step that did not hold.
pub(crate) fn check_slot_rule() -> Result<(), &'static str> {
    let slots = Slots::new();
    let install = |space: u64| {
        let (slot, kept) = slots.choose(space);
        let asid = asid_of(slot);
        if asid == 0 || asid > SLOTS as u64 {
            return Err("a root took an ASID out of range");
        }
        let value = tagged_root(0x1000, asid);
        if value >> ASID_SHIFT != asid || value & 0xFFFF_FFFF_FFFF != 0x1000 {
            return Err("a tagged root carried another root or its ASID out of place");
        }
        Ok(kept)
    };
    if install(1)? {
        return Err("a root new to the processor kept a slot's translations");
    }
    if !install(1)? {
        return Err("a root installed again did not keep its tag");
    }
    if install(2)? {
        return Err("a second root shared a first root's tag");
    }
    if !install(1)? {
        return Err("a root lost its tag to a second while a slot was free");
    }
    for space in 3..=(SLOTS as u64 + 1) {
        if install(space)? {
            return Err("a root new to the processor kept a slot's translations");
        }
    }
    if install(1)? {
        return Err("a root whose slot was taken found it again without an invalidation");
    }
    // Nothing of the rule's table is the running processor's.
    if slots.next.load(Ordering::Relaxed) >= SLOTS {
        return Err("the next slot ran past the table");
    }
    Ok(())
}
