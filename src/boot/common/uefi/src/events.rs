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
//! (POSTMORTEM §4.2, E1), and only on a boot that found a panic record.

use core::ptr;

use ferrix_bootinfo::pstore::{self, HEADER_BYTES};
use ferrix_eventlog::{self as eventlog, AppendError, Geometry, Outcome, RECORD_LEN};
use ferrix_slots::gpt::{self, Lookup};

use crate::console::println;
use crate::services::{Allocation, Disk, Services};
use crate::uefi::tables::MemoryType;

/// The longest piece of a record's panic line the summary shows.
const SHOWN: usize = 160;

/// The event log on the boot disk, opened.
pub(crate) struct Store {
    disk: Disk,
    geometry: Geometry,
}

/// What became of a previous boot's record.
pub(crate) enum Kept {
    /// It holds no panic report: nothing to keep.
    NotAPanic,
    /// It is in the log, durably.
    Durable,
    /// The append failed: the record must stay where it is.
    Failed,
}

/// Open the boot disk's event log, or say why there is none. `None` with
/// nothing said when the boot disk has no partition table or no partition
/// of the log's type: every machine set up without one.
pub(crate) fn open(services: &Services) -> Option<Store> {
    let disk = services.boot_disk().ok().flatten()?;
    let part = match find(services, &disk) {
        Ok(part) => part,
        Err(Lookup::Missing | Lookup::Table(gpt::GptError::Header)) => return None,
        Err(why) => {
            println!("  events   not used: the partition table ({why:?}); nothing written");
            return None;
        }
    };
    match Geometry::of(&part, disk.block_size) {
        Ok(geometry) => Some(Store { disk, geometry }),
        Err(why) => {
            println!("  events   not used: the partition ({why:?}); nothing written");
            None
        }
    }
}

/// Keep the panic record the pstore region holds in the log.
pub(crate) fn keep(store: &mut Store, services: &Services, region: Allocation) -> Kept {
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
pub(crate) fn summary(store: &mut Store, services: &Services) {
    let Some(mut buffers) = Buffers::new(services) else {
        return;
    };
    let (buffer, _) = buffers.slices();
    match eventlog::read_header(&mut store.disk, &store.geometry) {
        Ok(None) => println!(
            "  events   empty ({} records of room)",
            store.geometry.records()
        ),
        Ok(Some(header)) => newest(store, &header, buffer),
        Err(_) => println!("  events   the header cannot be read"),
    }
    buffers.free(services);
}

/// The summary line of a log with a header.
fn newest(store: &mut Store, header: &eventlog::Header, buffer: &mut [u8]) {
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

/// Read the GPT and find the log's partition.
fn find(services: &Services, disk: &Disk) -> Result<gpt::Part, Lookup> {
    let size = u64::from(disk.block_size);
    if !(512..=u64::from(eventlog::MAX_BLOCK)).contains(&size) {
        return Err(Lookup::Table(gpt::GptError::Header));
    }
    let block = read(services, disk, 1, size).ok_or(Lookup::Table(gpt::GptError::Header))?;
    let header = gpt::parse_header(block.bytes(size), disk.blocks(), disk.block_size);
    block.free(services);
    let header = header.map_err(Lookup::Table)?;
    let array_bytes = header.array_bytes();
    let array = read(
        services,
        disk,
        header.entries_lba,
        array_bytes.div_ceil(size) * size,
    )
    .ok_or(Lookup::Table(gpt::GptError::Entries))?;
    let found = gpt::find_one(
        &header,
        array.bytes(array_bytes),
        disk.blocks(),
        gpt::EVENTS_TYPE,
    );
    array.free(services);
    found
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
