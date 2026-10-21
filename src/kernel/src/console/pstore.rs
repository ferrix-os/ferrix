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

use ferrix_bootinfo::pstore::{self, HEADER_BYTES, STOP_OFFSET, Stop, WRITTEN_OFFSET};
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
/// A panic's word is never replaced: a stop racing a failure report on
/// another processor is not how the boot ended. Takes no lock and allocates
/// nothing.
pub(crate) fn stop(stop: Stop) -> bool {
    REGION.get().is_some_and(|region| {
        let word = stop.word();
        let panic = Stop::Panic.word();
        let _ = region
            .stop
            .fetch_update(Ordering::Release, Ordering::Relaxed, |now| {
                (now != panic).then_some(word)
            });
        arch::write_back_for_reset(region.virt, region.len);
        true
    })
}

/// Record a panic as how this boot stopped: the first failure report, as it
/// begins, so that a report that faults on its way still says so. The
/// report's own write-back ([`write_back`]) comes later. One store, no lock,
/// no allocation.
pub(crate) fn panicked() {
    if let Some(region) = REGION.get() {
        region.stop.store(Stop::Panic.word(), Ordering::Release);
    }
}

/// The attached region's physical address and length: for the boot check
/// that no frame of it is free.
pub(crate) fn attached() -> Option<(u64, u64)> {
    REGION.get().map(|region| (region.phys, region.len))
}

/// Check the region the loader gave and, if every check passes, mirror the
/// log into it from here on, starting with what the log holds so far.
///
/// Once, on the boot processor, after the direct map's RAM is recorded and
/// before interrupts are on, so that nothing else records while the log so
/// far is copied in.
pub(crate) fn attach(view: &BootView<'_>) {
    let Some((phys, len)) = view.pstore() else {
        println!("  pstore   none (the loader gave no region)");
        return;
    };
    match check(view, phys, len) {
        Ok((virt, sequence)) => adopt(phys, len, virt, sequence),
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
