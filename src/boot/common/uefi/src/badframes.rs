//! Pages kept out of the kernel's use as bad (`docs/MACHINE-CHECK-RESTART.md`
//! §2.5-2.7): the pages uncorrected records left in the machine-check banks
//! name, read first, before any of the loader's own allocations; and the
//! frames the boot before retired, from the pstore header's bad-frame table,
//! as `pstore::take` reads it; and the event log's set, which outlives a
//! power cycle (§9), read right after the banks.
//!
//! A page is kept out by allocating exactly it from firmware with the
//! loader's own type for it (`MemoryType::FERRIX_BAD_FRAME`), which the map
//! hands the kernel as `MemKind::BadFrame`: never free, not RAM, not in the
//! direct map. A refusal -- firmware has the page, or another of the
//! loader's allocations does -- is one line, and the page goes as it would
//! have. The loader marks exactly these pages and never drops one (the
//! assumption of use, §3); it reads no bank but to decode it, and clears
//! none: the kernel's bring-up does.

use crate::console::println;
use crate::services::Services;
use crate::uefi::tables::MemoryType;

/// A page.
const PAGE: u64 = 4096;

/// Pages kept out at most: the banks' 32, the table's 64 and the event
/// log's 64.
const MOST: usize = 160;

/// Frames the banks and the table named, at most: what the event log's set
/// is given (§9.2).
const NAMED: usize = 96;

/// The pages kept out so far this boot.
pub(crate) struct Kept {
    /// Their frame numbers, the first [`Kept::count`] of them.
    frames: [u32; MOST],
    /// How many.
    count: usize,
    /// Of them, the ones the banks named.
    from_banks: usize,
    /// The frames the banks and the table named this boot, kept out or
    /// not, the first [`Kept::named_count`]: the set records what the
    /// evidence named, the map what could be done about it.
    named: [u32; NAMED],
    /// How many.
    named_count: usize,
}

impl Kept {
    /// Whether the page at physical address `address` is one the banks
    /// named: memory the loader must not read (§2.6).
    pub(crate) fn names_bank_page(&self, address: u64) -> bool {
        let frame = address / PAGE;
        self.frames
            .iter()
            .take(self.from_banks)
            .any(|&kept| u64::from(kept) == frame)
    }

    /// The frames the banks and the table named this boot.
    pub(crate) fn named(&self) -> &[u32] {
        self.named.get(..self.named_count).unwrap_or_default()
    }

    /// Whether frame `frame` is already kept out.
    fn has(&self, frame: u32) -> bool {
        self.frames
            .iter()
            .take(self.count)
            .any(|&kept| kept == frame)
    }
}

/// Read the banks and keep out the page of every uncorrected record left
/// there with a usable address: first thing, before the loader allocates.
pub(crate) fn from_banks(services: &Services) -> Kept {
    let mut kept = Kept {
        frames: [0; MOST],
        count: 0,
        from_banks: 0,
        named: [0; NAMED],
        named_count: 0,
    };
    let mut pages = [0_u32; 32];
    let found = crate::arch::left_bank_pages(&mut pages);
    for &frame in pages.iter().take(found) {
        keep(services, &mut kept, frame, "left");
    }
    kept.from_banks = kept.count;
    kept
}

/// Keep frame `frame` out, found from `source` (`left`, `log` or `table`),
/// unless it is already; say it either way. A frame the banks or the table
/// named is remembered for the event log's set, whatever became of it.
pub(crate) fn keep(services: &Services, kept: &mut Kept, frame: u32, source: &str) {
    let address = u64::from(frame) * PAGE;
    if source != "log"
        && let Some(slot) = kept.named.get_mut(kept.named_count)
    {
        *slot = frame;
        kept.named_count += 1;
    }
    if kept.has(frame) {
        println!("  badframe {address:#x} kept out ({source}, as already)");
        return;
    }
    let Some(slot) = kept.frames.get_mut(kept.count) else {
        println!("  badframe {address:#x} not kept out ({source}): {MOST} pages are");
        return;
    };
    if services
        .allocate_at(address, PAGE, MemoryType::FERRIX_BAD_FRAME)
        .is_some()
    {
        *slot = frame;
        kept.count += 1;
        println!("  badframe {address:#x} kept out ({source})");
    } else {
        println!(
            "  badframe {address:#x} could not be kept out ({source}): it is in use (firmware or \
             the loader's earlier allocation)"
        );
    }
}
