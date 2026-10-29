//! Process-context identifiers: a `CR3` write that keeps the outgoing
//! address space's translations, tagged, where the processor has PCIDs
//! (`docs/PCID.md`).
//!
//! Switched on by `CPUID.1:ECX[17]` alone, never by vendor or part, and
//! only when every processor has it: the boot processor decides from its own
//! `CPUID` and sets `CR4.PCIDE` ([`init`]), before the speculation plan reads
//! the bit and before the secondaries copy its `CR4`; a secondary that lacks
//! PCIDs stops the boot before it writes that copy
//! ([`require_on_secondary`]). So one flag says for the whole machine
//! whether `CR3` writes are tagged. Without it every switch writes the bare
//! root, as before PCIDs.
//!
//! The allocator is Linux's x86 one: a few slots per processor, each holding
//! the id of the space it tags and that space's shootdown count as of the
//! last write that flushed the slot. PCID 0 is the kernel's own root and
//! nothing else; PCIDs 1 to [`SLOTS`] are user spaces. A processor's slots
//! are read and written only by that processor, with interrupts masked, so
//! relaxed atomics are enough and no table needs a lock.

use core::arch::x86_64::{__cpuid, __cpuid_count};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ferrix_sched::MAX_CPUS;

pub(crate) mod check;

use super::cpu;
use crate::console::println;

/// `CR4.PCIDE`.
pub(crate) const CR4_PCIDE: u64 = 1 << 17;

/// `CR3` bit 63: with `PCIDE` set, keep the new PCID's translations.
const NO_FLUSH: u64 = 1 << 63;

/// `CR3` bits 11 to 0: the PCID, with `PCIDE` set.
const PCID_MASK: u64 = 0xFFF;

/// User slots per processor: PCIDs 1 to 6, Linux's `TLB_NR_DYN_ASIDS`. A
/// slot costs TLB capacity and the search is linear, so six of the 4,095
/// the field holds.
pub(crate) const SLOTS: usize = 6;

/// Whether the boot processor turned PCIDs on: set once by [`init`], before
/// the speculation plan and the first secondary.
static ON: AtomicBool = AtomicBool::new(false);

/// Whether the boot processor reported INVPCID, for the boot line. Nothing
/// uses the instruction: every PCID-scoped flush is a `CR3` write.
static INVPCID: AtomicBool = AtomicBool::new(false);

/// Whether the boot processor's `CPUID` offered PCID, for the boot line.
static OFFERED: AtomicBool = AtomicBool::new(false);

/// One processor's slots.
struct Slots {
    /// The id of the space each slot tags, zero for none.
    space: [AtomicU64; SLOTS],
    /// The space's shootdown count read before the slot's last flushing
    /// write: every shootdown counted up to it is out of this TLB.
    synced: [AtomicU64; SLOTS],
    /// The slot the next miss takes.
    next: AtomicUsize,
    /// The value the last install wrote to `CR3`, bit 63 included, which
    /// reading `CR3` does not return: for the boot's check of every write.
    last: AtomicU64,
}

impl Slots {
    const fn new() -> Slots {
        Slots {
            space: [const { AtomicU64::new(0) }; SLOTS],
            synced: [const { AtomicU64::new(0) }; SLOTS],
            next: AtomicUsize::new(0),
            last: AtomicU64::new(0),
        }
    }
}

/// Every processor's slots, by logical number.
static TABLE: [Slots; MAX_CPUS] = [const { Slots::new() }; MAX_CPUS];

/// Whether this processor's `CPUID` offers PCID.
fn offered_here() -> bool {
    __cpuid(1).ecx & (1 << 17) != 0
}

/// Whether this processor's `CPUID` offers INVPCID.
fn invpcid_here() -> bool {
    __cpuid(0).eax >= 7 && __cpuid_count(7, 0).ebx & (1 << 10) != 0
}

/// Decide, on the boot processor, and set its `CR4.PCIDE` if it has
/// PCIDs. Called once, before `speculation::init` reads `CR4` for ERAPS,
/// before `CpuStarter::new` copies `CR4` for the secondaries, and before any
/// user space is installed.
///
pub(crate) fn init() {
    INVPCID.store(invpcid_here(), Ordering::Relaxed);
    OFFERED.store(offered_here(), Ordering::Relaxed);
    // Setting `PCIDE` with `CR3[11:0]` not zero is `#GP`. The kernel's
    // root is a frame, so they are; read rather than assumed.
    if !offered_here() || cpu::read_cr3() & PCID_MASK != 0 {
        return;
    }
    // SAFETY: (SYSREG) `CPUID` says this processor has PCIDs, it is in long
    // mode, and `CR3[11:0]` was just read as zero.
    unsafe { cpu::write_cr4(cpu::read_cr4() | CR4_PCIDE) };
    if cpu::read_cr4() & CR4_PCIDE != 0 {
        ON.store(true, Ordering::Release);
    }
}

/// Stop the boot if the boot processor turned PCIDs on and this secondary,
/// about to adopt the boot processor's `CR4`, has none: the machine-wide
/// flag would be false for it, and the bit `#GP` (`docs/PCID.md` §1).
pub(crate) fn require_on_secondary(boot_cr4: u64) {
    if boot_cr4 & CR4_PCIDE != 0 && !offered_here() {
        crate::panic::fatal!(
            crate::panic::catalog::SECONDARY_WITHOUT_PCID,
            "processor with APIC ID {}: no PCID (CPUID.1:ECX[17] clear), and the boot processor \
             turned PCIDs on",
            __cpuid(1).ebx >> 24
        );
    }
}

/// Whether the boot processor turned PCIDs on, for the whole machine.
pub(crate) fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// What one install wrote to `CR3`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RootWrite {
    /// The bare root, on a machine without PCIDs: every non-global
    /// translation dropped, as before PCIDs.
    Untagged,
    /// The space's slot with the no-flush bit: its translations kept.
    Kept,
    /// The space's slot, flushed: a new slot, or one a shootdown of the
    /// space has begun since it was last flushed.
    Flushed,
}

/// Write `CR3` for the space with id `space`, whose tables are at `root`
/// and whose shootdown count, read after this processor joined its set, is
/// `begun`, on processor `logical`.
///
/// # Safety
///
/// (TRANSLATE) As [`cpu::write_cr3`]: `root` maps the kernel's half. And
/// interrupts masked, with `logical` the processor running this: the slots
/// are this processor's alone.
pub(crate) unsafe fn install(root: u64, space: u64, begun: u64, logical: usize) -> RootWrite {
    let Some(slots) = TABLE.get(logical) else {
        // No record to keep for a processor past the table, which the
        // scheduler never starts; the bare root, as below.
        //
        // SAFETY: (TRANSLATE) the caller's guarantee.
        unsafe { cpu::write_cr3(root) };
        return RootWrite::Untagged;
    };
    if !on() {
        // Without PCIDs, the bare root, which drops every non-global
        // translation: the write every switch made before PCIDs.
        //
        // SAFETY: (TRANSLATE) the caller's guarantee.
        unsafe { cpu::write_cr3(root) };
        slots.last.store(root, Ordering::Relaxed);
        return RootWrite::Untagged;
    }
    let (slot, keep) = slots.choose(space, begun);
    let value = tagged_root(root, slot, keep);
    // SAFETY: (TRANSLATE) the caller's guarantee; `PCIDE` is set on every
    // processor while `on()`, and `pcid` is in 1..=6, so bit 63 and the low
    // bits are legal. Without bit 63 the write drops every non-global
    // translation tagged `pcid`.
    unsafe { cpu::write_cr3(value) };
    slots.last.store(value, Ordering::Relaxed);
    if keep {
        RootWrite::Kept
    } else {
        slots.flushed(slot, begun);
        RootWrite::Flushed
    }
}

impl Slots {
    /// Which slot the space with id `space` and shootdown count `begun`
    /// takes, and whether its write keeps the slot's translations.
    fn choose(&self, space: u64, begun: u64) -> (usize, bool) {
        let held = self
            .space
            .iter()
            .position(|slot| slot.load(Ordering::Relaxed) == space);
        match held {
            // A hit keeps the slot's translations only if no shootdown of the
            // space has begun since the slot was last flushed.
            Some(slot) => (
                slot,
                self.synced
                    .get(slot)
                    .is_some_and(|synced| synced.load(Ordering::Relaxed) >= begun),
            ),
            // A miss takes the next slot round robin, whoever held it, and
            // flushes it: the evicted space's translations go with the flush.
            None => {
                let slot = self.next.load(Ordering::Relaxed) % SLOTS;
                self.next.store((slot + 1) % SLOTS, Ordering::Relaxed);
                if let Some(owner) = self.space.get(slot) {
                    owner.store(space, Ordering::Relaxed);
                }
                (slot, false)
            }
        }
    }

    /// Slot `slot` was flushed by a write made after its space's shootdown
    /// count was read as `begun`.
    fn flushed(&self, slot: usize, begun: u64) {
        if let Some(synced) = self.synced.get(slot) {
            synced.store(begun, Ordering::Relaxed);
        }
    }
}

/// The `CR3` value for `root` in slot `slot`: the slot's PCID, one more
/// than its index, and bit 63 where the slot's translations are kept.
const fn tagged_root(root: u64, slot: usize, keep: bool) -> u64 {
    let value = root | (slot as u64 + 1);
    if keep { value | NO_FLUSH } else { value }
}

/// The PCID the running processor's `CR3` holds now.
pub(crate) fn tag_now() -> u64 {
    cpu::read_cr3() & PCID_MASK
}

/// The value the last install on processor `logical` wrote to `CR3`, bit
/// 63 included.
pub(crate) fn last_write(logical: usize) -> u64 {
    TABLE
        .get(logical)
        .map_or(0, |slots| slots.last.load(Ordering::Relaxed))
}

/// The boot line: whether PCIDs are on, and on how many processors.
pub(crate) fn report() {
    let invpcid = if INVPCID.load(Ordering::Relaxed) {
        "INVPCID offered, unused"
    } else {
        "no INVPCID"
    };
    if on() {
        println!(
            "  cpu      pcid: on, the boot processor's CPUID.1:ECX[17], required of every \
             processor; {SLOTS} slots each; {invpcid}"
        );
    } else if OFFERED.load(Ordering::Relaxed) {
        println!("  cpu      pcid: off, the boot processor's CR3[11:0] was not zero; {invpcid}");
    } else {
        println!("  cpu      pcid: off, CPUID.1:ECX[17] clear on the boot processor; {invpcid}");
    }
}
