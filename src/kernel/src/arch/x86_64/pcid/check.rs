//! The boot's check of the PCID slot rule (`docs/PCID.md` §5, K1), on a
//! table of its own for spaces described rather than installed, so that it
//! runs on every boot whatever the processor offers: QEMU's TCG has no
//! PCIDs, and nazuna's KVM has none to offer.

use core::sync::atomic::Ordering;

use super::{NO_FLUSH, OFFERED, PCID_MASK, SLOTS, Slots, on, tagged_root};

/// The decision and the slot rule.
///
/// The decision: PCIDs on exactly where the boot processor's CPUID offers
/// them (its `CR3`'s low bits are zero on every boot this kernel makes).
///
/// The rule: a new space flushes, a hit with no shootdown since keeps, a hit
/// after one flushes and is kept again after, a seventh space takes a slot
/// round robin and its owner flushes when it comes back, and every value
/// written carries the slot's PCID in 1..=6 with bit 63 exactly when kept.
/// With the hit's comparison taken out, or the miss not flushing, this
/// fails.
///
/// Verifies: `L.x86_64.214`, `L.x86_64.216`, `L.x86_64.217`
pub(crate) fn check_slot_rule() -> Result<(), &'static str> {
    if on() != OFFERED.load(Ordering::Relaxed) {
        return Err(
            "PCIDs are on where the boot processor's CPUID does not offer them, or off where it does",
        );
    }
    let slots = Slots::new();
    let install = |space: u64, begun: u64| {
        let (slot, keep) = slots.choose(space, begun);
        if !keep {
            slots.flushed(slot, begun);
        }
        let value = tagged_root(0x1000, slot, keep);
        let tag = value & PCID_MASK;
        if value & !NO_FLUSH & !PCID_MASK != 0x1000 || tag == 0 || tag > SLOTS as u64 {
            return Err("a tagged root carried another root or a PCID out of range");
        }
        if (value & NO_FLUSH != 0) != keep {
            return Err("a tagged root's bit 63 disagreed with the slot rule");
        }
        Ok(keep)
    };
    let steps: [(u64, u64, bool, &'static str); 10] = [
        (
            1,
            0,
            false,
            "a space new to the processor kept a slot's translations",
        ),
        (
            1,
            0,
            true,
            "a space with no shootdown since did not keep its slot",
        ),
        (
            2,
            5,
            false,
            "a second space new to the processor kept a slot",
        ),
        (1, 0, true, "a space lost its slot while one was free"),
        (
            1,
            1,
            false,
            "a space with a shootdown since its slot's flush kept it",
        ),
        (
            1,
            1,
            true,
            "a space flushed after a shootdown did not keep its slot after",
        ),
        (2, 5, true, "a second space did not keep its slot"),
        (3, 0, false, "a third space kept a slot"),
        (4, 0, false, "a fourth space kept a slot"),
        (5, 0, false, "a fifth space kept a slot"),
    ];
    for (space, begun, kept, what) in steps {
        if install(space, begun)? != kept {
            return Err(what);
        }
    }
    // Six spaces fill the six slots; three more take the slots of 1, 2
    // and 3 round robin, and 1 must flush when it comes back.
    for (space, what) in [
        (6, "a sixth space kept a slot"),
        (7, "a seventh space kept a slot"),
        (8, "an eighth space kept a slot"),
        (9, "a ninth space kept a slot"),
    ] {
        if install(space, 0)? {
            return Err(what);
        }
    }
    if install(1, 1)? {
        return Err("a space whose slot was taken found it again without a flush");
    }
    Ok(())
}
