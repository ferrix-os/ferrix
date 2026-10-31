//! The kernel log, kept across a reset too: a mirror of every byte the log
//! records into a region of RAM the loader gave, which the next boot's loader
//! finds and reports (`docs/POSTMORTEM.md` §3, the format in
//! `ferrix_bootinfo::pstore`).
//!
//! # What the kernel does with the region, and what it does not
//!
//! It checks the region once, early, on the boot processor before interrupts
//! are on ([`attach`]), and uses it only if every check passes: the boot info
//! names it wholly inside one [`MemKind::Pstore`] entry of the map, its
//! length is a header page and a ring of an allowed power of two, it is RAM
//! the direct map covers, and its header is the one the loader left (valid,
//! nothing recorded). Anything else is one line and no mirror, and nothing is
//! written to the region before the checks pass. From then on the kernel
//! writes ring bytes and the one count, through the same lock-free
//! [`log::record_into`] the log itself records with, and never reads back or
//! parses anything there: what a previous boot left is the loader's to judge.
//!
//! The mirror records what the log records and nothing else, so the lines
//! that carry the kernel's layout (`console::write_unlogged`) are not in it
//! either.
//!
//! The panic path writes the region back from the caches ([`write_back`]),
//! since a reset need not.
//!
//! # How the boot stopped
//!
//! Whatever stops the machine on purpose records how in the header's stop
//! word ([`stop`], `docs/POSTMORTEM.md` §5): `reboot(2)`, `power::finish`,
//! and the first failure report ([`panicked`]). One store each, after the
//! caller's last line and before the architecture's stop, so the next boot's
//! loader can say how this one ended; a word of 0 there says nothing
//! stopped it on purpose.

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use ferrix_bootinfo::pstore::{
    self, HEADER_BYTES, STOP_OFFSET, Stop, TABLE_COUNT_OFFSET, TABLE_ENTRIES, TABLE_OFFSET,
    WRITTEN_OFFSET,
};
use ferrix_bootinfo::{BootView, MemKind, PAGE_SIZE};
use ferrix_sync::Once;

use super::log;
use crate::console::println;
use crate::{arch, mm};

/// The attached region: its ring, its count, and where it is.
struct Region {
    /// The ring's bytes.
    slots: &'static [AtomicU8],
    /// The header's count of bytes ever recorded.
    written: &'static AtomicU64,
    /// The header's stop word.
    stop: &'static AtomicU64,
    /// The bad-frame table's count of claimed slots (version 3).
    table_count: &'static AtomicU64,
    /// The bad-frame table's entries.
    table: &'static [AtomicU64],
    /// The region's physical address and length, for the reports and the
    /// boot check.
    phys: u64,
    len: u64,
    /// Its direct-map address, for the panic path's write-back.
    virt: u64,
}

/// The region, once [`attach`] has checked it.
static REGION: Once<Region> = Once::new();

/// Record `bytes` in the region, if one is attached: what [`log::record`]
/// calls with every byte it records.
pub(crate) fn record(bytes: &[u8]) {
    if let Some(region) = REGION.get() {
        for &byte in bytes {
            log::record_into(region.slots, region.written, byte);
        }
    }
}

/// Write the region back from the caches to memory, for a reset that may
/// discard them, and say whether there was one. The panic path's, last
/// before the screen; every other stop's goes through [`stop`]. Takes no
/// lock and allocates nothing.
pub(crate) fn write_back() -> bool {
    REGION.get().is_some_and(|region| {
        arch::write_back_for_reset(region.virt, region.len);
        true
    })
}

/// Record `stop` as how this boot stopped, then write the region back for
/// the stop to come ([`write_back`]); whether there was a region. For a stop
/// on purpose that does not return, after its caller's last line and before
/// the architecture's reset, power-off or halt: the lines those print, and
/// any another processor logs meanwhile, may miss the region's RAM.
///
/// A failure's word -- a panic's, a machine check's -- is never replaced: a
/// stop racing a failure report on another processor is not how the boot
/// ended. Takes no lock and allocates nothing.
pub fn stop(stop: Stop) -> bool {
    REGION.get().is_some_and(|region| {
        let word = stop.word();
        let _ = region
            .stop
            .fetch_update(Ordering::Release, Ordering::Relaxed, |now| {
                (!is_failure(now)).then_some(word)
            });
        arch::write_back_for_reset(region.virt, region.len);
        true
    })
}

/// Whether `word` records a failure: a panic or a machine check, which
/// nothing replaces.
fn is_failure(word: u64) -> bool {
    word == Stop::Panic.word() || word == Stop::MachineCheck.word()
}

/// Record how this boot stopped at its first failure report, as it begins
/// -- `stop`, a panic or a machine check (`L.console.58`) -- so that a
/// report that faults on its way still says so; a failure's word already
/// there stands. The report's own write-back ([`write_back`]) comes later.
/// One compare-and-swap, no lock, no allocation.
pub(crate) fn panicked(stop: Stop) {
    if let Some(region) = REGION.get() {
        let word = stop.word();
        let _ = region
            .stop
            .fetch_update(Ordering::Release, Ordering::Relaxed, |now| {
                (!is_failure(now)).then_some(word)
            });
    }
}

/// What [`record_bad_frame`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Recorded {
    /// The frame is in the table for the next boot's loader.
    Kept,
    /// The table's 64 slots were claimed already: the frame is not.
    Full,
    /// No region is attached: nothing is recorded across a reset.
    NoRegion,
}

/// Record frame `frame` in the bad-frame table for the next boot's loader,
/// which keeps it out of that boot (`L.mm.109`, `L.x86_64.254`,
/// `docs/MACHINE-CHECK-RESTART.md` §2.3): a slot claimed by one `fetch_add`
/// on the count, the entry stored after the claim, both written back from
/// the caches. Lock-free and allocation-free, so the machine check's own
/// stop may call it on its stack; the kernel reads only its own count.
pub(crate) fn record_bad_frame(frame: u64) -> Recorded {
    let Some(region) = REGION.get() else {
        return Recorded::NoRegion;
    };
    let Ok(frame) = u32::try_from(frame) else {
        return Recorded::Full;
    };
    let slot = region.table_count.fetch_add(1, Ordering::AcqRel);
    let Some(entry) = usize::try_from(slot)
        .ok()
        .and_then(|slot| region.table.get(slot))
    else {
        return Recorded::Full;
    };
    entry.store(pstore::table_entry(frame), Ordering::Release);
    arch::flush_cache_lines(core::ptr::from_ref(entry) as u64, 8);
    arch::flush_cache_lines(core::ptr::from_ref(region.table_count) as u64, 8);
    Recorded::Kept
}

/// The bad-frame table's count of claimed slots, if a region is attached:
/// for stage 2's check that no checked retirement reaches it.
pub(crate) fn table_claimed() -> Option<u64> {
    REGION
        .get()
        .map(|region| region.table_count.load(Ordering::Acquire))
}

/// The attached region's physical address and length: for the boot check
/// that no frame of it is free.
pub fn attached() -> Option<(u64, u64)> {
    REGION.get().map(|region| (region.phys, region.len))
}

/// Check the region the loader gave and, if every check passes, mirror the
/// log into it from here on, starting with what the log holds so far.
///
/// Once, on the boot processor, after the direct map's RAM is recorded and
/// before interrupts are on, so that nothing else records while the log so
/// far is copied in.
pub fn attach(view: &BootView<'_>) {
    let Some((phys, len)) = view.pstore() else {
        println!("  pstore   none (the loader gave no region)");
        return;
    };
    match check(view, phys, len) {
        Ok((virt, sequence)) => {
            adopt(phys, len, virt, sequence);
            // The pages the loader kept out as bad go into this boot's table
            // again, so they stay out across the next warm reset too
            // (`L.mm.110`).
            let mut said = false;
            for region in view
                .regions()
                .iter()
                .filter(|r| r.kind == MemKind::BadFrame)
            {
                for page in 0..region.len / PAGE_SIZE {
                    let frame = (region.base / PAGE_SIZE).saturating_add(page);
                    if record_bad_frame(frame) == Recorded::Full && !said {
                        said = true;
                        println!(
                            "  pstore   the bad-frame table is full: not every kept-out frame is recorded again"
                        );
                    }
                }
            }
        }
        Err(why) => println!("  pstore   not attached: {why} ({phys:#x}, {len} bytes)"),
    }
}

/// Every check [`attach`] makes before it writes a byte: the region's
/// direct-map address and the boot the loader numbered, or why not.
fn check(view: &BootView<'_>, phys: u64, len: u64) -> Result<(u64, u64), &'static str> {
    let virt = placement(view, phys, len)?;
    // SAFETY: (KMEM) `virt` is the direct-map alias of `phys`, which `placement`
    // just found is RAM, whole pages, wholly inside one Pstore entry the
    // loader allocated for this and the kernel never hands out; a
    // page-aligned address is aligned for the array, and a read of plain
    // bytes has no invalid value. RAM, not a device, so a plain read.
    let header = unsafe {
        core::ptr::read(core::ptr::with_exposed_provenance::<[u8; HEADER_BYTES]>(
            virt as usize,
        ))
    };
    let found = pstore::fresh(&header, len).map_err(|why| match why {
        pstore::Refusal::Recorded => "its header says something was recorded already",
        pstore::Refusal::Stopped => "its header says a stop was recorded already",
        _ => "its header is not the loader's",
    })?;
    Ok((virt, found.sequence))
}

/// Where a region of `len` bytes at `phys` may be used, from the memory map
/// alone, before anything there is read: its direct-map address, or why not.
/// Whole pages; a header page and a ring of a length the format allows;
/// wholly inside one [`MemKind::Pstore`] entry of the map; RAM the direct map
/// covers at both ends. `log_check` holds it to cases it makes up.
pub(crate) fn placement(view: &BootView<'_>, phys: u64, len: u64) -> Result<u64, &'static str> {
    let end = phys.checked_add(len).ok_or("it wraps")?;
    if !phys.is_multiple_of(PAGE_SIZE) || !len.is_multiple_of(PAGE_SIZE) {
        return Err("it is not whole pages");
    }
    let capacity = len
        .checked_sub(pstore::HEADER)
        .ok_or("it is shorter than its header")?;
    if !pstore::capacity_allowed(capacity) {
        return Err("its ring is not a length the format allows");
    }
    let inside = view
        .regions()
        .iter()
        .any(|region| region.kind == MemKind::Pstore && region.base <= phys && end <= region.end());
    if !inside {
        return Err("no Pstore entry of the memory map holds it");
    }
    let virt = mm::direct_map_ram(phys).ok_or("the direct map does not cover it as RAM")?;
    let last = end.checked_sub(PAGE_SIZE).ok_or("it is empty")?;
    if mm::direct_map_ram(last) != Some(virt.wrapping_add(last - phys)) {
        return Err("the direct map does not cover its end as RAM");
    }
    Ok(virt)
}

/// Take the checked region at `virt` for the mirror: copy the log so far
/// into it, then publish it to [`record`].
fn adopt(phys: u64, len: u64, virt: u64, sequence: u64) {
    let capacity = (len - pstore::HEADER) as usize;
    let count_at = virt + WRITTEN_OFFSET as u64;
    let ring_at = virt + pstore::HEADER;
    // SAFETY: (KMEM) `count_at` is eight-byte aligned (a page plus 64) inside the
    // region `check` proved is RAM of the kernel's own, never handed out, so
    // it is valid for the life of the kernel; `AtomicU64` has `u64`'s layout,
    // every bit pattern is one, and from here on it is reached only through
    // this reference.
    let written = unsafe { &*core::ptr::with_exposed_provenance::<AtomicU64>(count_at as usize) };
    let stop_at = virt + STOP_OFFSET as u64;
    // SAFETY: (KMEM) as for the count: `stop_at` is eight-byte aligned (a page
    // plus 72) inside the same checked region, and from here on it is
    // reached only through this reference.
    let stop = unsafe { &*core::ptr::with_exposed_provenance::<AtomicU64>(stop_at as usize) };
    let table_count_at = virt + TABLE_COUNT_OFFSET as u64;
    // SAFETY: (KMEM) as for the count: eight-byte aligned (a page plus 128)
    // inside the same checked region, the loader's fresh table zero
    // (`pstore::fresh`), and from here on reached only through this
    // reference.
    let table_count =
        unsafe { &*core::ptr::with_exposed_provenance::<AtomicU64>(table_count_at as usize) };
    let table_at = virt + TABLE_OFFSET as u64;
    // SAFETY: (KMEM) as above, for the table's `TABLE_ENTRIES` eight-byte
    // aligned words after the count, inside the header page.
    let table = unsafe {
        core::slice::from_raw_parts(
            core::ptr::with_exposed_provenance::<AtomicU64>(table_at as usize),
            TABLE_ENTRIES,
        )
    };
    // SAFETY: (KMEM) as above, for the `capacity` ring bytes after the header page,
    // inside the same region; `AtomicU8` has `u8`'s layout.
    let slots = unsafe {
        core::slice::from_raw_parts(
            core::ptr::with_exposed_provenance::<AtomicU8>(ring_at as usize),
            capacity,
        )
    };
    // What the log holds so far, from its first byte, so that the region
    // holds the boot from its start. Nothing else records meanwhile: one
    // processor, interrupts off.
    let mut cursor = 0;
    let mut chunk = [0u8; 256];
    loop {
        let read = log::read(&mut cursor, &mut chunk);
        if read.copied == 0 {
            break;
        }
        for &byte in chunk.get(..read.copied).unwrap_or_default() {
            log::record_into(slots, written, byte);
        }
    }
    let _ = REGION.call_once(|| Region {
        slots,
        written,
        stop,
        table_count,
        table,
        phys,
        len,
        virt,
    });
    println!(
        "  pstore   mirroring the log to {phys:#x}, {} KiB, boot {sequence}; {} bytes so far",
        capacity / 1024,
        written.load(Ordering::Relaxed)
    );
}
