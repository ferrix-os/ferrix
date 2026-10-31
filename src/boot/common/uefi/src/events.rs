//! The event log (`docs/POSTMORTEM.md` §4): a previous boot's panic record,
//! found in the pstore region, appended to the boot disk's event-log
//! partition before the region is given a fresh header, so that it outlives
//! the next power cycle.
//!
//! Everything decided is `ferrix-eventlog`'s, as host-tested functions: the
//! partition lookup (exactly one of its type), every block address
//! (`Geometry::lba`), the record's text, the order of writes, idempotence
//! and a reset loop's collapse. This file moves blocks between firmware's
//! Block I/O and them. It writes nothing but blocks of that one partition
//! (POSTMORTEM §4.2, E1), and only on a boot that found a panic record or
//! a bad frame its set did not hold (EL3 amended,
//! `docs/MACHINE-CHECK-RESTART.md` §9.2). The set is read on every boot with
//! a log, before the loader's own allocations (§9.3).

use core::ptr;

use ferrix_bootinfo::pstore::{self, HEADER_BYTES};
use ferrix_eventlog::{
    self as eventlog, AppendError, BAD_FRAMES_MOST, BadFrames, Current, Durability, Geometry,
    Outcome, RECORD_LEN, Scan, Store,
};
use ferrix_slots::gpt::{self, Lookup};

use crate::console::println;
use crate::services::{Allocation, Disk, Services};
use crate::uefi::tables::MemoryType;

/// The longest piece of a record's panic line the summary shows.
const SHOWN: usize = 160;

/// The event log on the boot disk, opened.
pub(crate) struct Log {
    disk: Disk,
    geometry: Geometry,
    /// Its bad-frame set as [`keep_out`] read it.
    set: BadFrames,
    /// Its header copies are damaged or newer: nothing is written to it
    /// (said once, by [`keep_out`]).
    damaged: bool,
}

/// What became of a previous boot's record.
#[derive(Clone, Copy)]
pub(crate) enum Kept {
    /// It holds no panic report: nothing to keep.
    NotAPanic,
    /// It is in the log, durably.
    Durable,
    /// The append failed: the record must stay where it is.
    Failed,
}

/// The boot disk's event log, as [`open`] found it.
#[allow(
    clippy::large_enum_variant,
    reason = "one per boot, on the loader's stack, which has no allocator to box it in; \
              an allow, not an expect, since the sizes differ by target"
)]
pub(crate) enum Opened {
    /// None: no partition table, or no partition of the log's type. PM3's
    /// path: the record is reported and renewed.
    Absent,
    /// One, usable.
    Present(Log),
    /// One may be there and cannot be used: a record found must be kept in
    /// RAM, not renewed (fail closed; `ferrix_eventlog::classify`).
    Unusable,
}

/// Open the boot disk's event log. Absent is said nothing of: every machine
/// set up without one; unusable is said.
pub(crate) fn open(services: &Services) -> Opened {
    let disk = match services.boot_disk() {
        // A volume that is a whole disk, today's bare FAT image: no table.
        Ok(None) => return Opened::Absent,
        Ok(Some(disk)) => disk,
        Err(_) => {
            println!("  events   the boot disk cannot be opened: a record found stays in RAM");
            return Opened::Unusable;
        }
    };
    let scan = scan(services, &disk);
    match eventlog::classify(scan, disk.block_size) {
        Store::Absent => Opened::Absent,
        Store::Present(geometry) => Opened::Present(Log {
            disk,
            geometry,
            set: BadFrames::empty(),
            damaged: false,
        }),
        Store::Unusable(why) => {
            println!(
                "  events   the event log cannot be used ({why:?}): a record found stays in RAM"
            );
            Opened::Unusable
        }
    }
}

/// Keep every frame of the log's bad-frame set out of this boot
/// (`docs/MACHINE-CHECK-RESTART.md` §9.3): right after the bank read, before
/// the boot info area. The header is read into a block on the stack.
pub(crate) fn keep_out(store: &mut Log, services: &Services, kept: &mut crate::badframes::Kept) {
    match eventlog::read_header(&mut store.disk, &store.geometry) {
        Ok(Current::Header(header)) => {
            store.set = header.bad;
            for &frame in header.bad.frames() {
                crate::badframes::keep(services, kept, frame, "log");
            }
        }
        Ok(Current::Fresh) => {}
        Ok(Current::Damaged) => {
            store.damaged = true;
            println!(
                "  events   header damaged or newer: nothing written, its bad frames not read"
            );
        }
        Err(_) => println!("  events   its bad frames not kept out this boot"),
    }
}

/// Add the frames the banks and the table named to the log's set, before the
/// pstore region is renewed (§9.2). `Failed` keeps the region: it is then
/// their only other copy.
pub(crate) fn keep_frames(store: &mut Log, frames: &[u32]) -> Durability {
    if store.damaged
        || frames
            .iter()
            .all(|frame| store.set.frames().contains(frame))
    {
        return Durability::NotNeeded;
    }
    match eventlog::add_bad_frames(&mut store.disk, &store.geometry, frames) {
        Ok(_) => {
            // The set as written: the same merge, said frame by frame.
            for &frame in frames {
                let address = u64::from(frame) * 4096;
                match store.set.insert(frame) {
                    eventlog::Insert::Added => println!(
                        "  events   bad frame {address:#x} kept in the event log ({} of \
                         {BAD_FRAMES_MOST})",
                        store.set.len()
                    ),
                    eventlog::Insert::Already => {}
                    eventlog::Insert::Full => println!(
                        "  events   bad frame {address:#x} not kept in the event log: its \
                         {BAD_FRAMES_MOST} are"
                    ),
                }
            }
            Durability::Durable
        }
        Err(AppendError::Damaged) => {
            store.damaged = true;
            println!(
                "  events   header damaged or newer: nothing written, its bad frames not read"
            );
            Durability::NotNeeded
        }
        Err(why) => {
            println!(
                "  events   the bad frames' write failed ({why:?}): the pstore region stays as it is"
            );
            Durability::Failed
        }
    }
}

/// Keep the panic record the pstore region holds in the log.
pub(crate) fn keep(store: &mut Log, services: &Services, region: Allocation) -> Kept {
    let Some(mut buffers) = Buffers::new(services) else {
        println!("  events   no memory for the append: the record stays in RAM");
        return Kept::Failed;
    };
    // SAFETY: the region is the loader's own allocation of `pstore::LEN`
    // bytes, identity mapped under boot services; nothing writes it while
    // these reads happen.
    let header = unsafe { ptr::read_volatile(region.address as *const [u8; HEADER_BYTES]) };
    // SAFETY: as above: the ring is the region's bytes after its header
    // page, and the borrow ends before the fresh header is written.
    let ring = unsafe {
        core::slice::from_raw_parts(
            (region.address + pstore::HEADER) as *const u8,
            pstore::CAPACITY as usize,
        )
    };
    let time = services
        .firmware_time()
        .map_or(0, |nanos| nanos / 1_000_000_000);
    let (record, scratch) = buffers.slices();
    let kept = match eventlog::compose(
        &header,
        ring,
        time.to_le_bytes(),
        record.get_mut(eventlog::HEAD_LEN..).unwrap_or_default(),
    ) {
        None => Kept::NotAPanic,
        Some(incoming) => {
            match eventlog::append(&mut store.disk, &store.geometry, &incoming, record, scratch) {
                Ok(outcome) => {
                    say(incoming.boot, outcome);
                    Kept::Durable
                }
                Err(why) => {
                    failed(why);
                    Kept::Failed
                }
            }
        }
    };
    buffers.free(services);
    kept
}

/// Say what the log holds: how many records, and the newest.
pub(crate) fn summary(store: &mut Log, services: &Services) {
    let Some(mut buffers) = Buffers::new(services) else {
        return;
    };
    let (buffer, _) = buffers.slices();
    match eventlog::read_header(&mut store.disk, &store.geometry) {
        Ok(Current::Fresh) => println!(
            "  events   empty ({} records of room)",
            store.geometry.records()
        ),
        Ok(Current::Header(header)) => {
            newest(store, &header, buffer);
            if !header.bad.is_empty() {
                println!(
                    "  events   {} bad frame(s) kept out across power cycles",
                    header.bad.len()
                );
            }
        }
        Ok(Current::Damaged) => println!("  events   the header is damaged or newer"),
        Err(_) => println!("  events   the header cannot be read"),
    }
    buffers.free(services);
}

/// The summary line of a log with a header.
fn newest(store: &mut Log, header: &eventlog::Header, buffer: &mut [u8]) {
    let held = header.appended.min(u64::from(store.geometry.records()));
    let Some(slot) = header.newest_slot(store.geometry.records()) else {
        println!(
            "  events   empty ({} records of room)",
            store.geometry.records()
        );
        return;
    };
    match eventlog::read_record(&mut store.disk, &store.geometry, slot, buffer) {
        Ok(Some(meta)) if eventlog::counted(header, &meta) => {
            let text = eventlog::decode_record(buffer)
                .map(|(_, text)| text)
                .unwrap_or_default();
            let line = eventlog::first_line(text, SHOWN);
            println!(
                "  events   {held} records; newest #{}: boot {} {}, {} time(s): {}",
                meta.index,
                meta.last_boot,
                meta.kind.word(),
                meta.count,
                core::str::from_utf8(line).unwrap_or("?")
            );
        }
        _ => println!("  events   {held} records; the newest does not verify (a torn write)"),
    }
}

/// Say what the append did.
fn say(boot: u64, outcome: Outcome) {
    match outcome {
        Outcome::Appended { index, slot } => {
            println!("  events   boot {boot}'s panic kept as record #{index} (slot {slot})");
        }
        Outcome::Collapsed { index, count } => {
            println!("  events   boot {boot}'s panic is record #{index}'s again: {count} times");
        }
        Outcome::Skipped { index } => {
            println!("  events   boot {boot}'s panic is already record #{index}");
        }
    }
}

/// Say why the append failed.
fn failed(why: AppendError) {
    println!("  events   the append failed ({why:?}): the record stays in RAM for the next boot");
}

/// Read the GPT and look for the log's partition: what `classify` decides
/// from. A block size the log cannot be read in, a failed read or a failed
/// allocation is `Unreadable`.
fn scan(services: &Services, disk: &Disk) -> Scan {
    let size = u64::from(disk.block_size);
    if !(512..=u64::from(eventlog::MAX_BLOCK)).contains(&size) {
        return Scan::Unreadable;
    }
    let Some(block) = read(services, disk, 1, size) else {
        return Scan::Unreadable;
    };
    let header = gpt::parse_header(block.bytes(size), disk.blocks(), disk.block_size);
    block.free(services);
    let header = match header {
        Ok(header) => header,
        Err(why) => return Scan::Scanned(Err(Lookup::Table(why))),
    };
    let array_bytes = header.array_bytes();
    let Some(array) = read(
        services,
        disk,
        header.entries_lba,
        array_bytes.div_ceil(size) * size,
    ) else {
        return Scan::Unreadable;
    };
    let found = gpt::find_one(
        &header,
        array.bytes(array_bytes),
        disk.blocks(),
        gpt::EVENTS_TYPE,
    );
    array.free(services);
    Scan::Scanned(found)
}

/// Pages of the loader's own, read from the disk.
struct Pages(Allocation);

impl Pages {
    /// The first `len` bytes.
    fn bytes(&self, len: u64) -> &[u8] {
        let len = len.min(self.0.len) as usize;
        // SAFETY: the loader's own allocation, at least `len` bytes, filled
        // by `read`, and not written while this borrow lives.
        unsafe { core::slice::from_raw_parts(self.0.address as *const u8, len) }
    }

    fn free(self, services: &Services) {
        let _ = services.free("freeing an event-log read", self.0);
    }
}

/// Allocate `len` bytes, a whole number of blocks, and read them from `lba`.
fn read(services: &Services, disk: &Disk, lba: u64, len: u64) -> Option<Pages> {
    let memory = services
        .allocate(
            "allocating for an event-log read",
            len.max(1),
            MemoryType::LOADER_DATA,
        )
        .ok()?;
    // SAFETY: the loader's own allocation of at least `len` bytes, page
    // aligned, and nothing else refers to it yet.
    let into = unsafe { core::slice::from_raw_parts_mut(memory.address as *mut u8, len as usize) };
    if disk.read(lba, into).is_err() {
        let _ = services.free("freeing an event-log read", memory);
        return None;
    }
    Some(Pages(memory))
}

/// Two record buffers: the record being written, and the newest read back.
struct Buffers(Allocation);

impl Buffers {
    fn new(services: &Services) -> Option<Buffers> {
        services
            .allocate(
                "allocating the event-log buffers",
                2 * RECORD_LEN as u64,
                MemoryType::LOADER_DATA,
            )
            .ok()
            .map(Buffers)
    }

    /// The two buffers, each [`RECORD_LEN`] bytes, zeroed.
    fn slices(&mut self) -> (&mut [u8], &mut [u8]) {
        // SAFETY: the loader's own allocation of two records' bytes, page
        // aligned, borrowed only through this one call while it lives.
        let all =
            unsafe { core::slice::from_raw_parts_mut(self.0.address as *mut u8, 2 * RECORD_LEN) };
        all.fill(0);
        all.split_at_mut(RECORD_LEN)
    }

    fn free(self, services: &Services) {
        let _ = services.free("freeing the event-log buffers", self.0);
    }
}
