//! The region the kernel keeps its log in across a reset (`docs/POSTMORTEM.md`
//! §3.3): find it where the memory map says it goes, report what the previous
//! kernel left in it, and hand it to this boot's kernel with a fresh header.
//!
//! The format is `ferrix_bootinfo::pstore`'s, which the kernel compiles too.
//! Every failure here is one line and a boot without a region: what goes
//! wrong with a post-mortem aid never stops a boot.

use core::ptr;

use ferrix_bootinfo::pstore::{self, Found, HEADER_BYTES, Switch, Text};

use crate::console::println;
use crate::events;
use crate::load::{DirectMap, describe};
use crate::services::{Allocation, Services};
use crate::uefi::tables::MemoryType;

/// The longest piece of the previous kernel's text a line of the report
/// shows.
const SHOWN: usize = 160;

/// The marker a kernel's failure report starts with: left off the line that
/// shows it, so that a reader of this boot's log does not take the previous
/// boot's panic for this one's (and `pstore::defuse` makes any other
/// `FERRIX-` in a quoted line harmless too).
const PANIC_MARKER: &[u8] = b"FERRIX-PANIC ";

/// The end of every line that says there is no region this boot: what
/// `xtask` waits for after the loader's banner, beside `goes to`.
const NONE: &str = "no region this boot";

/// Take the region for this boot: find it, report what the previous kernel
/// left there, and write a fresh header. `None` when there is none, said.
pub(crate) fn take(services: &Services, cmdline: &str, direct: DirectMap) -> Option<Allocation> {
    match pstore::switch(cmdline) {
        Switch::On => {}
        Switch::Off => {
            println!("  pstore   off ({}=off): {NONE}", pstore::OPTION);
            return None;
        }
        Switch::NotUnderstood(value) => {
            println!(
                "  pstore   {}={value} is not understood, so off: {NONE}",
                pstore::OPTION
            );
            return None;
        }
    }
    let Some(address) = choose(services, direct) else {
        println!("  pstore   no place for the region below the direct map's end: {NONE}");
        return None;
    };
    let Some(region) = services.allocate_at(address, pstore::LEN, MemoryType::FERRIX_PSTORE) else {
        println!("  pstore   firmware would not give {address:#x}: {NONE}");
        return None;
    };
    if !direct.covers(region) {
        let _ = services.free("freeing the pstore region", region);
        println!("  pstore   {address:#x} is outside the direct map: {NONE}");
        return None;
    }
    // SAFETY: the region is the loader's own allocation of `pstore::LEN`
    // bytes, identity mapped under boot services, and nothing else refers to
    // it; it is read before anything writes it.
    let header = unsafe { ptr::read_volatile(region.address as *const [u8; HEADER_BYTES]) };
    let mut store = events::open(services);
    let sequence = match pstore::validate(&header, region.len) {
        Ok(found) => {
            report(region, &found);
            // Kept in the event log before the fresh header lets it go
            // (POSTMORTEM §4, E2); a failed append keeps the region as it
            // is, still allocated so the kernel never uses its pages, and
            // this boot goes without one.
            if let Some(store) = store.as_mut() {
                let kept = events::keep(store, services, region);
                if matches!(kept, events::Kept::Failed) {
                    events::summary(store, services);
                    println!(
                        "  pstore   boot {}'s record kept in RAM, not handed over: {NONE}",
                        found.sequence
                    );
                    return None;
                }
            }
            found.sequence.wrapping_add(1).max(1)
        }
        Err(why) => {
            println!(
                "  pstore   at {:#x}, 1 MiB: no previous kernel log ({why})",
                region.address
            );
            1
        }
    };
    if let Some(store) = store.as_mut() {
        events::summary(store, services);
    }
    renew(region, sequence);
    // The Arm architectures turn the caches off in the switch to the kernel,
    // which checks this header before it writes a byte there; nothing here
    // writes the region again. No-op on x86-64.
    crate::arch::clean_dcache(region.address, pstore::HEADER);
    println!(
        "  pstore   boot {sequence}'s kernel log goes to {:#x}",
        region.address
    );
    Some(region)
}

/// Write a fresh header for boot `sequence` over the region's.
fn renew(region: Allocation, sequence: u64) {
    let fresh = pstore::encode(pstore::CAPACITY, sequence);
    // SAFETY: the region is the loader's own allocation of `pstore::LEN`
    // bytes, identity mapped under boot services; the header is its first
    // bytes, and the ring after it is left as it is: only the count decides
    // what is text.
    unsafe { ptr::write_volatile(region.address as *mut [u8; HEADER_BYTES], fresh) };
}

/// Where the region goes on this machine: `pstore::choose` over the
/// conventional memory of a fresh map.
fn choose(services: &Services, direct: DirectMap) -> Option<u64> {
    let probe = services
        .allocate(
            "allocating a buffer to place the pstore region",
            services.memory_map_size().ok()?,
            MemoryType::LOADER_DATA,
        )
        .ok()?;
    let chosen = services.memory_map(probe).ok().and_then(|map| {
        let conventional = map
            .entries()
            .filter(|descriptor| descriptor.memory_type == 7)
            .map(|descriptor| {
                let region = describe(&descriptor);
                (region.base, region.len)
            });
        pstore::choose(conventional, direct.end(), pstore::LEN)
    });
    let _ = services.free("freeing the pstore placing buffer", probe);
    chosen
}

/// Say what the previous kernel left: how much, and its last report.
fn report(region: Allocation, found: &Found) {
    println!(
        "  pstore   at {:#x}, 1 MiB: boot {}'s kernel log, {} bytes, {} kept (unchecked)",
        region.address,
        found.sequence,
        found.written,
        found.kept()
    );
    // SAFETY: the ring is the region's bytes after the header page, the
    // loader's own allocation, and nothing writes them while this borrow
    // lives: the fresh header is written after `report` returns.
    let ring = unsafe {
        core::slice::from_raw_parts(
            (region.address + pstore::HEADER) as *const u8,
            pstore::CAPACITY as usize,
        )
    };
    let text = Text::of(ring, found);
    let mut shown = [0u8; SHOWN];
    if let Some(line) = text.last_line_after(PANIC_MARKER) {
        let count = text.shown_into(line, SHOWN, &mut shown);
        println!(
            "  pstore   boot {}'s last report: panicked: {}",
            found.sequence,
            as_text(shown.get(..count).unwrap_or_default())
        );
    } else if let Some(line) = text.last_line() {
        let count = text.shown_into(line, SHOWN, &mut shown);
        println!(
            "  pstore   boot {}'s last report: did not panic; its last line: {}",
            found.sequence,
            as_text(shown.get(..count).unwrap_or_default())
        );
    } else {
        println!("  pstore   boot {}'s log is empty", found.sequence);
    }
}

/// Bytes `pstore::shown` produced, which are ASCII, as text.
fn as_text(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).unwrap_or("?")
}
