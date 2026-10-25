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
//! 2. each attempt of the plan: read the slot, ask `ferrix-boot-policy`
//!    whether it may start -- its header version, its signed manifest, the
//!    trust-policy block (`docs/SIGNING.md` §10.6) -- and match every blob
//!    on the copy it hands over (H.SIGN.7); a trial only after the record
//!    with one try fewer is written and read back (P1);
//! 3. nothing verified: the ESP's files, as the rescue path; with
//!    `slot_signing = required`, the ESP's `RESCUE.SLT` instead, decided as
//!    a slot, and the unsigned files are not read (§9.2).

use ferrix_boot_policy::{Refusal, Rescue, Start};
use ferrix_slots::gpt::{self, GptError, Layout, Part};
use ferrix_slots::plan::{self, Kind, LoaderWords};
use ferrix_slots::record::{self, Current, Record};
use ferrix_slots::slot::{Blob, HEADER_LEN, MANIFEST_LEN, SlotHeader};

use crate::console::println;
use crate::policy;

use crate::services::{Allocation, BootError, Disk, Result, Services};
use crate::uefi::tables::MemoryType;
use core::fmt::Write as _;

/// The most bytes read from the disk in one call.
const CHUNK: u64 = 8 << 20;

/// The largest block size a slot is read in: blobs start on 4096-byte
/// boundaries, which must be whole blocks.
const MAX_BLOCK: u32 = HEADER_LEN as u32;

/// The header page and the manifest block: what is read before deciding.
const HEAD_LEN: u64 = (HEADER_LEN + MANIFEST_LEN) as u64;

/// The ESP's rescue slot image, read only under `slot_signing = required`.
const RESCUE_PATH: &str = "/FERRIX/RESCUE.SLT";

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
    match choose_slot(services, cmdline)? {
        Choice::Slot(slot) => Ok(Choice::Slot(slot)),
        other => match ferrix_boot_policy::rescue(policy::block()) {
            Ok(Rescue::UnsignedFiles) => Ok(other),
            Ok(Rescue::SlotImage) => rescue_slot(services, cmdline).map(Choice::Slot),
            Err(why) => {
                refused("rescue", why, None);
                Err(BootError::plain(
                    "the loader's trust-policy block is malformed",
                ))
            }
        },
    }
}

/// [`choose`], before the rescue path.
fn choose_slot(services: &Services, cmdline: &mut [u8]) -> Result<Choice> {
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
            disk: layout.disk,
            sig: None,
        };
        let loaded = match load_slot(services, &disk, &part, words, cmdline) {
            Ok(loaded) => loaded,
            Err(Why::Policy(why, fields)) => {
                refused(attempt.slot.letter(), why, fields);
                println!(
                    "  slots    slot {} ({}) does not verify: its signature ({})",
                    attempt.slot.letter(),
                    attempt.kind.word(),
                    reason(why)
                );
                continue;
            }
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

/// Say a slot was refused by the policy, in the line the gate reads
/// (`docs/SIGNING.md` §9.3). `fields` are the manifest's anchor id and
/// security version, when it has one.
fn refused(slot: &str, why: Refusal, fields: Option<(u16, u32)>) {
    match fields {
        Some((anchor, version)) => println!(
            "  sign     refused slot {slot}: {} (kind 1 anchor {anchor} version {version} floor 0)",
            reason(why)
        ),
        None => println!(
            "  sign     refused slot {slot}: {} (kind 1 anchor none version none floor 0)",
            reason(why)
        ),
    }
}

/// A refusal in words: the verifier's reasons by their `Refusal` names
/// (§9.3), so every verifier says them alike.
fn reason(why: Refusal) -> ReasonText {
    ReasonText(why)
}

/// [`Refusal`] as the loader prints it.
struct ReasonText(Refusal);

impl core::fmt::Display for ReasonText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Refusal::Block(field) => write!(f, "TrustPolicy({field:?})"),
            Refusal::Header(error) => write!(f, "Header({error:?})"),
            Refusal::Unsigned => f.write_str("Unsigned"),
            Refusal::Manifest(why) => write!(f, "{why:?}"),
        }
    }
}

/// Why slots are not used, or a slot did not verify.
#[derive(Clone, Copy, Debug)]
enum Why {
    /// `ferrix-boot-policy` refused it; the manifest's anchor id and
    /// version, when it has one.
    Policy(Refusal, Option<(u16, u32)>),
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
            Why::Policy(why, _) => write!(f, "its signature: {}", reason(*why)),
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

/// Ask the policy whether the slot whose first [`HEAD_LEN`] bytes are
/// `head` may start (`docs/SIGNING.md` §10.6).
fn decide(head: &[u8], slot_bytes: u64) -> core::result::Result<Start, Why> {
    let page = head.get(..HEADER_LEN).unwrap_or(&[]);
    let manifest = head.get(HEADER_LEN..HEADER_LEN + MANIFEST_LEN);
    let fields = manifest.map(|m| {
        let at = |i: usize| m.get(i).copied().unwrap_or(0);
        (
            u16::from_le_bytes([at(20), at(21)]),
            u32::from_le_bytes([at(16), at(17), at(18), at(19)]),
        )
    });
    let began = crate::arch::counter();
    let start = ferrix_boot_policy::decide(
        policy::block(),
        &policy::BUILD,
        policy::family(),
        0,
        page,
        manifest,
        slot_bytes,
    );
    match start {
        Ok(start) => {
            if let Some(signed) = start.signed {
                // The decision's cost in counter ticks (the TSC on x86-64):
                // MEMORY-AND-TIMING's x86-64 verify figure (ledger 4082 (6)).
                println!(
                    "  sign     anchor {} version {} (decided in {} counter ticks)",
                    signed.anchor_id,
                    signed.security_version,
                    crate::arch::counter().wrapping_sub(began)
                );
            }
            Ok(start)
        }
        // A header that does not decode says so as it always did.
        Err(Refusal::Header(error)) => Err(Why::Header(error)),
        Err(why) => Err(Why::Policy(why, fields.filter(|_| head_is_signed(page)))),
    }
}

/// Whether a header page claims version 2, for what a refusal line names.
fn head_is_signed(page: &[u8]) -> bool {
    page.get(8..10) == Some(&ferrix_slots::slot::VERSION_2.to_le_bytes()[..])
}

/// Read and verify one slot, and compose its command line into `cmdline`.
fn load_slot(
    services: &Services,
    disk: &Disk,
    part: &Part,
    words: LoaderWords,
    cmdline: &mut [u8],
) -> core::result::Result<SlotBoot, Why> {
    let head = read_into(
        services,
        disk,
        part.first_lba,
        HEAD_LEN,
        MemoryType::LOADER_DATA,
    )?;
    let decided = decide(bytes(&head, HEAD_LEN), part.bytes(disk.block_size));
    free(services, head);
    let start = decided?;
    let header = start.header;
    let words = LoaderWords {
        sig: start.signed.map(|signed| signed.anchor_id),
        ..words
    };

    let line = read_blob(
        services,
        disk,
        part,
        &header.cmdline,
        MemoryType::LOADER_DATA,
        "command line",
    )?;
    let written = plan::compose(&words, bytes(&line, header.cmdline.length), cmdline);
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

/// The ESP's `RESCUE.SLT`, decided as a slot and its blobs matched, with
/// its own command line (no slot words: it is neither A nor B) and
/// `ferrix.slot.sig` (§9.2).
fn rescue_slot(services: &Services, cmdline: &mut [u8]) -> Result<SlotBoot> {
    let (file, len) = match services.read_file(RESCUE_PATH, MemoryType::LOADER_DATA) {
        Ok(read) => read,
        Err(error) => {
            println!("  sign     refused slot rescue: no {RESCUE_PATH} ({error})");
            return Err(BootError::plain(
                "no slot verifies, signing is required and the ESP holds no RESCUE.SLT",
            ));
        }
    };
    let image = bytes(&file, len);
    let start = match decide(image.get(..HEAD_LEN as usize).unwrap_or(image), len) {
        Ok(start) => start,
        Err(why) => {
            if let Why::Policy(refusal, fields) = why {
                refused("rescue", refusal, fields);
            } else {
                println!("  sign     refused slot rescue: {why}");
            }
            free(services, file);
            return Err(BootError::plain(
                "no slot verifies and RESCUE.SLT does not either",
            ));
        }
    };
    let loaded = copy_rescue(services, image, &start.header, cmdline, start.signed);
    free(services, file);
    match loaded {
        Ok(loaded) => {
            println!("  boot     rescue");
            Ok(loaded)
        }
        Err(why) => {
            println!("  sign     refused slot rescue: {why}");
            Err(BootError::plain(
                "no slot verifies and RESCUE.SLT does not either",
            ))
        }
    }
}

/// Copy the rescue image's blobs out of the file into memory of their own
/// kinds, matching each on the copy that is handed over (P3).
fn copy_rescue(
    services: &Services,
    image: &[u8],
    header: &SlotHeader,
    cmdline: &mut [u8],
    signed: Option<ferrix_boot_policy::Signed>,
) -> core::result::Result<SlotBoot, Why> {
    let blob_of = |blob: &Blob| {
        let start = usize::try_from(blob.offset).ok()?;
        let end = start.checked_add(usize::try_from(blob.length).ok()?)?;
        image.get(start..end)
    };
    let line = blob_of(&header.cmdline).ok_or(Why::Hash("command line"))?;
    if !header.cmdline.matches(line) {
        return Err(Why::Hash("command line"));
    }
    let line = core::str::from_utf8(line)
        .map_err(|_| Why::CommandLine)?
        .trim();
    let mut cursor = Cursor {
        out: cmdline,
        used: 0,
    };
    // The loader's word first, as `plan::compose` puts a slot's: the
    // kernel takes the first word with a key, so the image's own line
    // cannot forge it (ledger 4082 (1)).
    let separator = if line.is_empty() { "" } else { " " };
    let written = match signed {
        Some(signed) => write!(
            cursor,
            "ferrix.slot.sig={}{separator}{line}",
            signed.anchor_id
        ),
        None => write!(cursor, "ferrix.slot.sig=none{separator}{line}"),
    };
    written.map_err(|_| Why::CommandLine)?;
    let cmdline_len = cursor.used as u64;

    let kernel = copy_blob(
        services,
        header.kernel,
        blob_of(&header.kernel),
        MemoryType::LOADER_DATA,
        "kernel",
    )?;
    let initrd = if header.initrd.length == 0 {
        None
    } else {
        match copy_blob(
            services,
            header.initrd,
            blob_of(&header.initrd),
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

/// A `fmt::Write` over a byte slice that fails instead of truncating (P5).
struct Cursor<'a> {
    out: &'a mut [u8],
    used: usize,
}

impl core::fmt::Write for Cursor<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.used.checked_add(text.len()).ok_or(core::fmt::Error)?;
        self.out
            .get_mut(self.used..end)
            .ok_or(core::fmt::Error)?
            .copy_from_slice(text.as_bytes());
        self.used = end;
        Ok(())
    }
}

/// Copy `bytes`, one blob of the rescue image, into memory of `kind`, and
/// match it there.
fn copy_blob(
    services: &Services,
    blob: Blob,
    bytes: Option<&[u8]>,
    kind: MemoryType,
    what: &'static str,
) -> core::result::Result<Allocation, Why> {
    let bytes = bytes.ok_or(Why::Hash(what))?;
    let memory = services
        .allocate("allocating for a rescue blob", blob.length.max(1), kind)
        .map_err(|_| Why::Firmware("an allocation"))?;
    // SAFETY: `memory` is the loader's own allocation of at least
    // `blob.length` bytes, page aligned, and nothing else refers to it yet.
    let into = unsafe { core::slice::from_raw_parts_mut(memory.address as *mut u8, bytes.len()) };
    into.copy_from_slice(bytes);
    if blob.matches(self::bytes(&memory, blob.length)) {
        Ok(memory)
    } else {
        free(services, memory);
        Err(Why::Hash(what))
    }
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
