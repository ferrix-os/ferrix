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

pub(crate) use ferrix_eventlog::badframes::Mode;

use crate::console::println;
#[cfg(ferrix_bad_frames = "choice")]
use crate::services::Allocation;
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

/// This boot's bad-frame mode (`docs/MACHINE-CHECK-RESTART.md` §10): fixed
/// by a `bad_frames = persist` build, which compiles no read of the boot
/// word.
#[cfg(ferrix_bad_frames = "persist")]
pub(crate) fn mode(_cmdline: &str) -> Mode {
    println!("  badframe mode persist (fixed by this build)");
    Mode::Persist
}

/// This boot's bad-frame mode: a `bad_frames = choice` build reads it from
/// the command line, absent or not understood `persist`, said once.
#[cfg(ferrix_bad_frames = "choice")]
pub(crate) fn mode(cmdline: &str) -> Mode {
    use ferrix_eventlog::badframes::{Choice, OPTION, choice};
    let read = choice(cmdline);
    let mode = read.mode();
    match read {
        Choice::Absent => println!("  badframe mode {} (the word absent)", mode.word()),
        Choice::Chosen(_) => println!("  badframe mode {} ({OPTION})", mode.word()),
        Choice::NotUnderstood(value) => {
            println!("  badframe {OPTION}={value} is not understood, so persist");
        }
    }
    mode
}

/// Release what `mode` keeps out no longer (§10.3): after `pstore::take`,
/// so that the banks' pages stay held while it reads the region, and before
/// the kernel is staged. A frame firmware will not take back stays out.
#[cfg(ferrix_bad_frames = "choice")]
pub(crate) fn release(services: &Services, kept: &mut Kept, mode: Mode) {
    use ferrix_eventlog::badframes::{OPTION, released};
    let named = kept.named;
    let named = named.get(..kept.named_count).unwrap_or_default();
    let mut index = 0;
    while index < kept.count {
        let Some(&frame) = kept.frames.get(index) else {
            break;
        };
        let address = u64::from(frame) * PAGE;
        if !released(mode, frame, named) {
            index += 1;
            continue;
        }
        let page = Allocation { address, len: PAGE };
        if services.free("releasing a bad frame", page).is_err() {
            println!(
                "  badframe {address:#x} not released ({OPTION}={}): firmware refused",
                mode.word()
            );
            index += 1;
            continue;
        }
        println!(
            "  badframe {address:#x} released ({OPTION}={})",
            mode.word()
        );
        kept.frames.copy_within(index + 1..kept.count, index);
        kept.count -= 1;
        if index < kept.from_banks {
            kept.from_banks -= 1;
        }
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
