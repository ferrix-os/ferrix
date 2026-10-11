//! A/B system slots (`docs/AB-BOOT.md` §4): which slot to boot, and its
//! kernel, initramfs and command line, read through Block I/O and verified.
//!
//! What is decided here is decided by `ferrix-slots`, as pure functions the
//! host tests reach: the GPT scan, the current record, the plan, the
//! record write with its read-back, the slot header and its hashes, and the
//! command line. This file only moves blocks between firmware and them, in
//! the order the design gives:
//!
//! 1. no partition table on the boot disk, or no control partition: today's
//!    path, from the ESP;
//! 2. each attempt of the plan: read the slot and verify it; a trial only
//!    after the record with one try fewer is written and read back (P1);
//! 3. nothing verified: the ESP's files, as the rescue path.

use ferrix_slots::gpt::{self, GptError, Layout, Part};
use ferrix_slots::plan::{self, Kind, LoaderWords};
use ferrix_slots::record::{self, Current, Record};
use ferrix_slots::slot::{Blob, HEADER_LEN, SlotHeader};

use crate::console::println;
use crate::services::{Allocation, Disk, Result, Services};
use crate::uefi::tables::MemoryType;

/// The most bytes read from the disk in one call.
const CHUNK: u64 = 8 << 20;

/// The largest block size a slot is read in: blobs start on 4096-byte
/// boundaries, which must be whole blocks.
const MAX_BLOCK: u32 = HEADER_LEN as u32;

/// What the loader boots.
#[derive(Debug)]
pub(crate) enum Choice {
    /// The boot disk has no slots: today's path, from the ESP.
    NoSlots,
    /// It has slots and none verified: the ESP's files, as the rescue path.
    Rescue,
    /// A slot, read and verified, its command line already in place.
    Slot(SlotBoot),
}

/// A slot's blobs in memory, and how many command-line bytes were written.
#[derive(Debug)]
pub(crate) struct SlotBoot {
    /// The kernel file, and its length.
    pub(crate) kernel: (Allocation, u64),
    /// The initramfs, when the slot carries one.
    pub(crate) initrd: Option<(Allocation, u64)>,
    /// Bytes of command line written.
    pub(crate) cmdline_len: u64,
}

/// Decide what to boot, and when it is a slot, read it and write its
/// command line into `cmdline`.
pub(crate) fn choose(services: &Services, cmdline: &mut [u8]) -> Result<Choice> {
    let Some(mut disk) = services.boot_disk()? else {
        return Ok(Choice::NoSlots);
    };
    let layout = match find_layout(services, &disk) {
        Ok(layout) => layout,
        // A disk with no control partition is not set up for slots, which
        // is every disk but an A/B install: nothing to say.
        Err(Why::Gpt(GptError::NoControl)) => return Ok(Choice::NoSlots),
        Err(why) => {
            println!("  slots    not used: {why}");
            return Ok(Choice::NoSlots);
        }
    };
    let current = match record::read_current(&mut disk, &layout.control) {
        Ok(current) => current,
        Err(why) => {
            println!("  slots    not used: the control record cannot be read ({why:?})");
            return Ok(Choice::NoSlots);
        }
    };
    report(&current);

    for attempt in plan::plan(&current).attempts() {
        let part = layout.slot(attempt.slot);
        let (sequence, next) = match (attempt.kind, current) {
            (Kind::Trial, Current::Valid { record, .. }) => match record.after_try() {
                Some(next) => (next.sequence, Some(next)),
                None => continue,
            },
            (_, Current::Valid { record, .. }) => (record.sequence, None),
            (_, Current::Uninitialised) => (0, None),
        };
        let words = LoaderWords {
            slot: attempt.slot,
            kind: attempt.kind,
            sequence,
        };
        let loaded = match load_slot(services, &disk, &part, &words, cmdline) {
            Ok(loaded) => loaded,
            Err(why) => {
                println!(
                    "  slots    slot {} ({}) does not verify: {why}",
                    attempt.slot.letter(),
                    attempt.kind.word()
                );
                continue;
            }
        };
        if let Some(next) = next {
            // P1: the try is recorded, and read back, before the trial starts.
            if let Err(why) = take_try(&mut disk, &layout, &current, &next) {
                println!(
                    "  slots    slot {}'s try could not be recorded ({why:?}); not starting it",
                    attempt.slot.letter()
                );
                release(services, &loaded);
                continue;
            }
            println!(
                "  slots    took a try off slot {}: {} left",
                attempt.slot.letter(),
                next.tries
            );
        }
        println!(
            "  boot     slot {} {} seq {sequence}",
            attempt.slot.letter(),
            attempt.kind.word()
        );
        return Ok(Choice::Slot(loaded));
    }
    println!("  slots    no slot verifies; the ESP's files are the rescue path");
    Ok(Choice::Rescue)
}

/// Write the record with one try fewer.
fn take_try(
    disk: &mut Disk,
    layout: &Layout,
    current: &Current,
    next: &Record,
) -> core::result::Result<(), record::WriteError> {
    record::write_record(disk, &layout.control, current, next)
}

/// Say what the record holds.
fn report(current: &Current) {
    match current {
        Current::Uninitialised => {
            println!("  slots    control record uninitialised: A, then B, nothing written");
        }
        Current::Valid { copy, record } => println!(
            "  slots    control record seq {} (copy {copy}): good {}, trial {}, {} tries left",
            record.sequence,
            record.good.letter(),
            record.trial.map_or("none", ferrix_slots::Slot::letter),
            record.tries
        ),
    }
}

/// Why slots are not used, or a slot did not verify.
#[derive(Clone, Copy, Debug)]
enum Why {
    /// The partition table.
    Gpt(GptError),
    /// The slot's header.
    Header(ferrix_slots::slot::SlotError),
    /// A blob's bytes do not hash to its header's digest.
    Hash(&'static str),
    /// The command line is not UTF-8, or does not fit with the loader's words.
    CommandLine,
    /// A block size the formats cannot be read in.
    BlockSize,
    /// Firmware failed a read or an allocation.
    Firmware(&'static str),
}

impl core::fmt::Display for Why {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Why::Gpt(error) => write!(f, "the partition table: {error:?}"),
            Why::Header(error) => write!(f, "its header: {error:?}"),
            Why::Hash(what) => write!(f, "its {what} does not match its digest"),
            Why::CommandLine => {
                f.write_str("its command line is not UTF-8 or does not fit with the loader's words")
            }
            Why::BlockSize => f.write_str("the disk's block size"),
            Why::Firmware(what) => write!(f, "firmware failed {what}"),
        }
    }
}

/// Read the GPT and find the slots.
fn find_layout(services: &Services, disk: &Disk) -> core::result::Result<Layout, Why> {
    if !(512..=MAX_BLOCK).contains(&disk.block_size) || !disk.block_size.is_power_of_two() {
        return Err(Why::BlockSize);
    }
    let size = u64::from(disk.block_size);
    let block = read_into(services, disk, 1, size, MemoryType::LOADER_DATA)?;
    let header = gpt::parse_header(bytes(&block, size), disk.blocks(), disk.block_size);
    free(services, block);
    let header = header.map_err(Why::Gpt)?;
    let array_bytes = header.array_bytes();
    let array = read_into(
        services,
        disk,
        header.entries_lba,
        array_bytes.div_ceil(size) * size,
        MemoryType::LOADER_DATA,
    )?;
    let layout = gpt::find(
        &header,
        bytes(&array, array_bytes),
        disk.blocks(),
        disk.block_size,
    );
    free(services, array);
    layout.map_err(Why::Gpt)
}

/// Read and verify one slot, and compose its command line into `cmdline`.
fn load_slot(
    services: &Services,
    disk: &Disk,
    part: &Part,
    words: &LoaderWords,
    cmdline: &mut [u8],
) -> core::result::Result<SlotBoot, Why> {
    let header_page = read_into(
        services,
        disk,
        part.first_lba,
        HEADER_LEN as u64,
        MemoryType::LOADER_DATA,
    )?;
    let header = SlotHeader::decode(
        bytes(&header_page, HEADER_LEN as u64),
        part.bytes(disk.block_size),
    );
    free(services, header_page);
    let header = header.map_err(Why::Header)?;

    let line = read_blob(
        services,
        disk,
        part,
        &header.cmdline,
        MemoryType::LOADER_DATA,
        "command line",
    )?;
    let written = plan::compose(words, bytes(&line, header.cmdline.length), cmdline);
    free(services, line);
    let cmdline_len = written.ok_or(Why::CommandLine)? as u64;

    let kernel = read_blob(
        services,
        disk,
        part,
        &header.kernel,
        MemoryType::LOADER_DATA,
        "kernel",
    )?;
    let initrd = if header.initrd.length == 0 {
        None
    } else {
        match read_blob(
            services,
            disk,
            part,
            &header.initrd,
            MemoryType::FERRIX_INITRD,
            "initramfs",
        ) {
            Ok(initrd) => Some((initrd, header.initrd.length)),
            Err(why) => {
                free(services, kernel);
                return Err(why);
            }
        }
    };
    Ok(SlotBoot {
        kernel: (kernel, header.kernel.length),
        initrd,
        cmdline_len,
    })
}

/// Read one blob of a slot into memory of `kind`, and check its digest (P3).
fn read_blob(
    services: &Services,
    disk: &Disk,
    part: &Part,
    blob: &Blob,
    kind: MemoryType,
    what: &'static str,
) -> core::result::Result<Allocation, Why> {
    let size = u64::from(disk.block_size);
    let lba = part.first_lba + blob.offset / size;
    let memory = read_into(services, disk, lba, blob.length.div_ceil(size) * size, kind)?;
    if blob.matches(bytes(&memory, blob.length)) {
        Ok(memory)
    } else {
        free(services, memory);
        Err(Why::Hash(what))
    }
}

/// Allocate `len` bytes (at least a page) of `kind`, and read `len` bytes,
/// a whole number of blocks, from `lba` into it.
fn read_into(
    services: &Services,
    disk: &Disk,
    lba: u64,
    len: u64,
    kind: MemoryType,
) -> core::result::Result<Allocation, Why> {
    let memory = services
        .allocate("allocating for a slot read", len.max(1), kind)
        .map_err(|_| Why::Firmware("an allocation"))?;
    let size = u64::from(disk.block_size);
    let mut done = 0u64;
    while done < len {
        let chunk = CHUNK.min(len - done);
        // SAFETY: `memory` is the loader's own allocation of at least `len`
        // bytes, page aligned, and nothing else refers to it yet.
        let into = unsafe {
            core::slice::from_raw_parts_mut((memory.address + done) as *mut u8, chunk as usize)
        };
        if disk.read(lba + done / size, into).is_err() {
            free(services, memory);
            return Err(Why::Firmware("a read"));
        }
        done += chunk;
    }
    Ok(memory)
}

/// The first `len` bytes of an allocation.
fn bytes(memory: &Allocation, len: u64) -> &[u8] {
    let len = len.min(memory.len) as usize;
    // SAFETY: the loader's own allocation, at least `len` bytes, filled by
    // `read_into`, and not written while this borrow lives.
    unsafe { core::slice::from_raw_parts(memory.address as *const u8, len) }
}

/// Give an allocation back; a failure only leaks it until the kernel
/// reclaims loader memory.
fn free(services: &Services, memory: Allocation) {
    let _ = services.free("freeing a slot buffer", memory);
}

/// Give back a loaded slot that is not booted.
fn release(services: &Services, loaded: &SlotBoot) {
    free(services, loaded.kernel.0);
    if let Some((initrd, _)) = loaded.initrd {
        free(services, initrd);
    }
}
