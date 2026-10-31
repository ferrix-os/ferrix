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
use crate::uefi::Status;
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
pub(crate) fn take(
    services: &Services,
    cmdline: &str,
    direct: DirectMap,
    kept: &mut crate::badframes::Kept,
) -> Option<Allocation> {
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
    // A page of the region a machine-check bank names is never read: poison
    // there would stop this loader inside firmware at every restart
    // (`docs/MACHINE-CHECK-RESTART.md` §2.6). The header's: no region this
    // boot. A ring page's: the header and its table are read, the text not.
    if kept.names_bank_page(region.address) {
        let _ = services.free("freeing the pstore region", region);
        println!(
            "  pstore   a machine-check bank names the region's header page {:#x}: {NONE}",
            region.address
        );
        return None;
    }
    let ring_named = (1..pstore::LEN / pstore::HEADER)
        .any(|page| kept.names_bank_page(region.address + page * pstore::HEADER));
    // SAFETY: the region is the loader's own allocation of `pstore::LEN`
    // bytes, identity mapped under boot services, and nothing else refers to
    // it; it is read before anything writes it.
    let header = unsafe { ptr::read_volatile(region.address as *const [u8; HEADER_BYTES]) };
    let mut opened = events::open(services);
    let sequence = match pstore::validate(&header, region.len) {
        Ok(found) => {
            // The frames the boot before retired, kept out of this one
            // (`docs/MACHINE-CHECK-RESTART.md` §2.5), before anything here
            // reads the ring.
            for frame in pstore::bad_frames(&header, &found) {
                crate::badframes::keep(services, kept, frame, "table");
            }
            let logged = if ring_named {
                println!(
                    "  pstore   boot {}'s text not read: a machine-check bank names a page of it",
                    found.sequence
                );
                events::Kept::NotAPanic
            } else {
                let lines = report(region, &found);
                keep_as_files(services, region, &found, &lines);
                // Kept in the event log before the fresh header lets it go
                // (POSTMORTEM §4, E2); a failed append keeps the region as it
                // is, still allocated so the kernel never uses its pages, and
                // this boot goes without one.
                match &mut opened {
                    events::Opened::Absent => events::Kept::NotAPanic,
                    events::Opened::Unusable => events::Kept::Failed,
                    events::Opened::Present(log) => events::keep(log, services, region),
                }
            };
            if matches!(logged, events::Kept::Failed) {
                if let events::Opened::Present(log) = &mut opened {
                    events::summary(log, services);
                }
                println!(
                    "  pstore   boot {}'s record kept in RAM, not handed over: {NONE}",
                    found.sequence
                );
                return None;
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
    if let events::Opened::Present(log) = &mut opened {
        events::summary(log, services);
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

/// The ring of `region`: its bytes after the header page.
///
/// # Safety
///
/// `region` is the loader's own allocation of `pstore::LEN` bytes, identity
/// mapped under boot services, and nothing writes its ring while the borrow
/// lives: the fresh header is written after the report and the files.
unsafe fn ring(region: Allocation) -> &'static [u8] {
    // SAFETY: as the caller promises.
    unsafe {
        core::slice::from_raw_parts(
            (region.address + pstore::HEADER) as *const u8,
            pstore::CAPACITY as usize,
        )
    }
}

/// The report's lines, kept to head the file with: a fixed buffer, since
/// the loader prints before it may allocate for a file.
struct Lines {
    /// The bytes.
    bytes: [u8; REPORT_MOST],
    /// How many are used.
    used: usize,
}

/// The most the report's lines take.
const REPORT_MOST: usize = 1024;

impl core::fmt::Write for Lines {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.used.saturating_add(text.len()).min(REPORT_MOST);
        let room = end - self.used;
        if let (Some(to), Some(from)) = (
            self.bytes.get_mut(self.used..end),
            text.as_bytes().get(..room),
        ) {
            to.copy_from_slice(from);
        }
        self.used = end;
        Ok(())
    }
}

impl Lines {
    /// One report line: printed, and kept.
    fn say(&mut self, line: core::fmt::Arguments<'_>) {
        use core::fmt::Write as _;
        let start = self.used;
        let _ = self.write_fmt(line);
        let _ = self.write_str("\n");
        let said = self
            .bytes
            .get(start..self.used.saturating_sub(1))
            .unwrap_or_default();
        println!("{}", as_text(said));
    }
}

/// Say what the previous kernel left: how much, and its last report; the
/// lines said, to head its file.
fn report(region: Allocation, found: &Found) -> Lines {
    let mut lines = Lines {
        bytes: [0; REPORT_MOST],
        used: 0,
    };
    lines.say(format_args!(
        "  pstore   at {:#x}, 1 MiB: boot {}'s kernel log, {} bytes, {} kept (unchecked)",
        region.address,
        found.sequence,
        found.written,
        found.kept()
    ));
    // SAFETY: the region is the loader's own, and nothing writes its ring
    // until the fresh header after the report and the files.
    let text = Text::of(unsafe { ring(region) }, found);
    let mut shown = [0u8; SHOWN];
    if let Some(line) = text.last_line_after(PANIC_MARKER) {
        let count = text.shown_into(line, SHOWN, &mut shown);
        lines.say(format_args!(
            "  pstore   boot {}'s last report: panicked: {}",
            found.sequence,
            as_text(shown.get(..count).unwrap_or_default())
        ));
    } else if let Some(line) = text.last_line() {
        let count = text.shown_into(line, SHOWN, &mut shown);
        lines.say(format_args!(
            "  pstore   boot {}'s last report: did not panic; its last line: {}",
            found.sequence,
            as_text(shown.get(..count).unwrap_or_default())
        ));
    } else {
        lines.say(format_args!(
            "  pstore   boot {}'s log is empty",
            found.sequence
        ));
    }
    lines.say(format_args!(
        "  pstore   boot {} ended: {}",
        found.sequence,
        found.ended()
    ));
    lines
}

/// Where the previous kernel's log is kept on the loader's own volume, and
/// the one before it (`docs/POSTMORTEM.md` §3.5), for `lode` to import as the
/// end of that boot (`docs/LOGGING.md` §5).
const LAST: &str = "FERRIX/LASTKERNEL.TXT";
const PREVIOUS: &str = "FERRIX/LASTKERNEL.PRV";

/// Move the file read as `old`, `len` bytes, to [`PREVIOUS`], and free it.
fn move_aside(services: &Services, old: Allocation, len: u64) {
    // SAFETY: `read_file_within` read `len` bytes into the allocation it
    // made, which is the loader's alone until it is freed below.
    let bytes = unsafe { core::slice::from_raw_parts(old.address as *const u8, len as usize) };
    if let Err(error) = services.replace_file(PREVIOUS, &[bytes]) {
        println!("  pstore   {PREVIOUS} not written: {error}");
    }
    let _ = services.free("freeing the previous log file", old);
}

/// Keep the previous kernel's log as [`LAST`], headed by the report's
/// `lines`, the file there before moved to [`PREVIOUS`]. A failure is one
/// line and the boot goes on: a netboot's volume is read only.
fn keep_as_files(services: &Services, region: Allocation, found: &Found, lines: &Lines) {
    // At most what this loader writes: a larger file is not one of its own,
    // and is not read whole to move it.
    let most = pstore::CAPACITY + REPORT_MOST as u64;
    match services.read_file_within(LAST, MemoryType::LOADER_DATA, most) {
        Ok((old, len)) => move_aside(services, old, len),
        Err(error) if error.status == Some(Status::NOT_FOUND) => {}
        Err(error) => println!("  pstore   {LAST} not moved to {PREVIOUS}: {error}"),
    }
    // SAFETY: the region is the loader's own, and nothing writes its ring
    // until the fresh header after this.
    let text = Text::of(unsafe { ring(region) }, found);
    let (first, second) = text.parts();
    let head = lines.bytes.get(..lines.used).unwrap_or_default();
    match services.replace_file(LAST, &[head, first, second]) {
        Ok(()) => println!(
            "  pstore   boot {}'s kernel log kept in {LAST}, {} bytes",
            found.sequence,
            head.len() + first.len() + second.len()
        ),
        Err(error) => println!(
            "  pstore   boot {}'s kernel log not kept in {LAST}: {error}",
            found.sequence
        ),
    }
}

/// Bytes `pstore::shown` produced, which are ASCII, as text.
fn as_text(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).unwrap_or("?")
}
