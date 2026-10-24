//! The self-check of `madvise`.
//!
//! What `PartitionAlloc` and V8 rely on is that a range dropped with
//! `MADV_DONTNEED` or `MADV_FREE` stays mapped and its frames go back to the
//! allocator at once. So the check counts them: eight pages of private
//! anonymous memory are written, dropped, and must come back as exactly eight
//! frames in a [`mm::FrameWindow`] -- a page on either side of the range stays
//! written, so no page table empties and the count is the pages' alone --
//! and then read as zeros, while the pages beside them keep what they held.
//! `MADV_FREE` must do the same.
//!
//! Around that: a page a `fork` child still shares copy-on-write keeps the
//! child's contents when its parent drops it. A private mapping of a memfd
//! gives back the two pages it copied and shows the file there again, and the
//! file keeps every page. A shared mapping of it only loses its translations,
//! and still shows what it wrote. `MADV_REMOVE` punches a hole in shared
//! anonymous memory that a `fork` child sees too, and gives the hole's two
//! frames back. The hints are accepted and drop nothing. And the refusals are
//! Linux's: an address off a page boundary, advice there is none of, the
//! advice this kernel cannot honour, a length that wraps, `MADV_FREE` and
//! `MADV_REMOVE` where they do not apply, and a range with a hole in it, which
//! is `ENOMEM` once the mapped part has been advised. The run is done twice
//! and must keep no frame.

use alloc::vec;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    MADV_COLD, MADV_DODUMP, MADV_DOFORK, MADV_DONTDUMP, MADV_DONTFORK, MADV_DONTNEED, MADV_FREE,
    MADV_HUGEPAGE, MADV_MERGEABLE, MADV_NOHUGEPAGE, MADV_NORMAL, MADV_PAGEOUT, MADV_RANDOM,
    MADV_REMOVE, MADV_SEQUENTIAL, MADV_WILLNEED, MADV_WIPEONFORK, MAP_ANONYMOUS, MAP_PRIVATE,
    MAP_SHARED, PROT_READ, PROT_WRITE,
};
use ferrix_vfs::Errno;

use crate::mm;
use crate::syscall::check as syscall_check;
use crate::syscall::memory::{self, MmapRequest, OffsetUnit};
use crate::syscall::process::{self, Process};
use crate::syscall::{fd, uaccess};
use crate::user::space::AddressSpace;
use crate::user::vmo::Vmo;

/// Pages of the anonymous mapping the frames are counted on.
const PAGES: u64 = 10;
/// Pages dropped from it: 1 to 8, so pages 0 and 9 keep their page tables.
const DROPPED: u64 = 8;
/// Pages of the file, and of each mapping of it.
const FILE_PAGES: u64 = 4;
/// The memfd's name, with its terminator, staged at the start of the
/// scratch page.
const NAME: &[u8] = b"madvise-check\0";
/// What a shared mapping of the file writes, and must still show after
/// `MADV_DONTNEED`.
const SHARED_WRITES: &[u8] = b"written through a shared mapping, then advised away";
/// Where in the shared mapping it writes that.
const SHARED_AT: u64 = PAGE_SIZE + 100;
/// What a private mapping writes over the file's pages 1 and 2.
const PRIVATE_BYTE: u8 = 0xA5;

/// The hints, which Linux accepts on memory like this and which must drop
/// nothing.
const HINTS: [i32; 12] = [
    MADV_NORMAL,
    MADV_RANDOM,
    MADV_SEQUENTIAL,
    MADV_WILLNEED,
    MADV_DONTFORK,
    MADV_DOFORK,
    MADV_HUGEPAGE,
    MADV_NOHUGEPAGE,
    MADV_DONTDUMP,
    MADV_DODUMP,
    MADV_COLD,
    MADV_PAGEOUT,
];

/// What the check measured, for the boot log.
#[derive(Debug, Clone, Copy)]
pub struct Report {
    /// Frames the advice gave back to the allocator, counted in windows.
    pub given_back: u64,
    /// Calls refused as Linux refuses them.
    pub refusals: u32,
    /// Frames the second run cost.
    pub leaked: i64,
}

/// What one run counts.
#[derive(Debug, Default)]
struct Counts {
    given_back: u64,
    refusals: u32,
}

/// Run it twice, measured on the second.
pub fn run() -> Result<Report, &'static str> {
    let process =
        process::new_for_check().map_err(|_| "could not make a process for the madvise check")?;
    let _warm = check_once(&process)?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let window = mm::FrameWindow::open();
    let counts = check_once(&process)?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let leaked = window.kept();
    if leaked != 0 {
        window.report("madvise");
        crate::console::println!("  madvise  {leaked} frames across the second run");
        return Err("the madvise check did not give back every frame it took");
    }
    Ok(Report {
        given_back: counts.given_back,
        refusals: counts.refusals,
        leaked,
    })
}

/// One run.
fn check_once(process: &Process) -> Result<Counts, &'static str> {
    let scratch = map(process, PAGE_SIZE, MAP_ANONYMOUS | MAP_PRIVATE, -1)?;
    let mut counts = Counts::default();
    let outcome = check_anonymous_memory(process, &mut counts)
        .and_then(|()| check_a_fork_keeps_its_copy(process))
        .and_then(|()| check_file_mappings(process, scratch, &mut counts))
        .and_then(|()| check_a_hole_in_shared_memory(process, &mut counts))
        .and_then(|()| check_refusals(process, &mut counts));
    for descriptor in 3..32 {
        let _ = fd::sys_close(process, descriptor);
    }
    let _ = memory::sys_munmap(process, scratch, PAGE_SIZE);
    outcome.map(|()| counts)
}

/// Eight pages dropped with `MADV_DONTNEED`, then with `MADV_FREE`: eight
/// frames back each time, zeros after, the neighbours untouched; and every
/// hint accepted and dropping nothing.
///
/// Verifies: L.user.79
fn check_anonymous_memory(process: &Process, counts: &mut Counts) -> Result<(), &'static str> {
    let at = map(process, PAGES * PAGE_SIZE, MAP_ANONYMOUS | MAP_PRIVATE, -1)?;
    let outcome = drop_and_count(process, at, MADV_DONTNEED, counts)
        .and_then(|()| drop_and_count(process, at, MADV_FREE, counts))
        .and_then(|()| {
            for hint in HINTS {
                if advise(process, at, PAGES * PAGE_SIZE, hint) != Ok(0) {
                    return Err("madvise refused a hint Linux accepts on private memory");
                }
            }
            for index in 0..PAGES {
                if !page_is(process.space(), at + index * PAGE_SIZE, pattern(index))? {
                    return Err("a madvise hint dropped a page it should have left alone");
                }
            }
            Ok(())
        });
    unmapped(process, at, PAGES * PAGE_SIZE)?;
    outcome
}

/// Write every page of the mapping at `at`, drop pages 1 to 8 with `advice`,
/// and require eight frames back, eight fewer resident, zeros there and the
/// two neighbours as they were. Leaves every page written again.
fn drop_and_count(
    process: &Process,
    at: u64,
    advice: i32,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    let space = process.space();
    for index in 0..PAGES {
        fill(space, at + index * PAGE_SIZE, pattern(index))?;
    }
    let resident = space.resident_pages().unwrap_or(0);
    let window = mm::FrameWindow::open();
    if advise(process, at + PAGE_SIZE, DROPPED * PAGE_SIZE, advice) != Ok(0) {
        return Err("madvise refused to drop pages of private anonymous memory");
    }
    let kept = window.kept();
    if kept != -i64::try_from(DROPPED).unwrap_or(i64::MAX) {
        window.report("madvise");
        crate::console::println!(
            "  madvise  advice {advice} gave back {} frames for {DROPPED} pages",
            -kept
        );
        return Err(
            "madvise did not give the frames of the pages it dropped back to the allocator",
        );
    }
    if resident.saturating_sub(space.resident_pages().unwrap_or(0)) != DROPPED {
        return Err("madvise dropped pages that were still counted resident");
    }
    counts.given_back += DROPPED;
    for index in 1..=DROPPED {
        if !page_is(space, at + index * PAGE_SIZE, 0)? {
            return Err("a page madvise dropped did not read as zeros");
        }
    }
    for index in [0, PAGES - 1] {
        if !page_is(space, at + index * PAGE_SIZE, pattern(index))? {
            return Err("madvise dropped a page outside the range it was given");
        }
    }
    for index in 1..=DROPPED {
        fill(space, at + index * PAGE_SIZE, pattern(index))?;
    }
    Ok(())
}

/// A parent drops a page its `fork` child still shares copy-on-write: the
/// parent reads zeros, and the child what the page held.
///
/// Verifies: L.user.80
fn check_a_fork_keeps_its_copy(process: &Process) -> Result<(), &'static str> {
    let at = map(process, 3 * PAGE_SIZE, MAP_ANONYMOUS | MAP_PRIVATE, -1)?;
    let space = process.space();
    let outcome = (0..3)
        .try_for_each(|index| fill(space, at + index * PAGE_SIZE, pattern(index)))
        .and_then(|()| {
            let child = space
                .fork()
                .map_err(|_| "the madvise check's space would not fork")?;
            if advise(process, at + PAGE_SIZE, PAGE_SIZE, MADV_DONTNEED) != Ok(0) {
                return Err("madvise refused to drop a page a fork child shares");
            }
            if !page_is(space, at + PAGE_SIZE, 0)? {
                return Err("a page a parent dropped after a fork did not read as zeros");
            }
            if !page_is(&child, at + PAGE_SIZE, pattern(1))? {
                return Err("a parent's MADV_DONTNEED reached the copy its fork child holds");
            }
            Ok(())
        });
    unmapped(process, at, 3 * PAGE_SIZE)?;
    outcome
}

/// A private mapping of a memfd gives back what it copied and shows the file
/// again; a shared one keeps what it wrote; the file keeps every page.
///
/// Verifies: L.user.81
fn check_file_mappings(
    process: &Process,
    scratch: u64,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    uaccess::copy_to_user(process.space(), scratch, NAME)
        .map_err(|_| "could not stage the memfd's name")?;
    let memfd = by_number(process, Syscall::MemfdCreate, [scratch, 0, 0, 0, 0, 0])
        .ok()
        .and_then(|made| i32::try_from(made).ok())
        .ok_or("memfd_create was refused")?;
    let file = fd::file(process, memfd).map_err(|_| "the memfd is gone")?;
    let data: Vec<u8> = (0..FILE_PAGES * PAGE_SIZE).map(file_byte).collect();
    if file.write_at(0, &data) != Ok(data.len()) {
        return Err("the memfd was not written whole");
    }
    let object = file
        .inode()
        .mapping()
        .and_then(|object| object.downcast::<Vmo>().ok())
        .ok_or("a memfd has no object")?;
    let committed = object.committed();
    let len = FILE_PAGES * PAGE_SIZE;
    let private = map(process, len, MAP_PRIVATE, memfd)?;
    let outcome = check_a_private_file_mapping(process, private, &data, counts);
    unmapped(process, private, len)?;
    outcome?;
    let shared = map(process, len, MAP_SHARED, memfd)?;
    let outcome = check_a_shared_file_mapping(process, shared, &file, counts);
    unmapped(process, shared, len)?;
    outcome?;
    if object.committed() != committed {
        return Err("madvise through a mapping took pages out of the file it maps");
    }
    Ok(())
}

/// Pages 1 and 2 of a private file mapping, copied by a write, are dropped:
/// two frames back, and the file's bytes shown there again.
fn check_a_private_file_mapping(
    process: &Process,
    private: u64,
    data: &[u8],
    counts: &mut Counts,
) -> Result<(), &'static str> {
    let space = process.space();
    let page = usize::try_from(PAGE_SIZE).map_err(|_| "the page size does not fit")?;
    // Pages 0 and 3 read, so their page tables stay; 1 and 2 copied.
    let mut shown = vec![0_u8; page];
    for index in [0, FILE_PAGES - 1] {
        uaccess::copy_from_user(space, private + index * PAGE_SIZE, &mut shown)
            .map_err(|_| "a private file mapping could not be read")?;
    }
    for index in 1..=2 {
        fill(space, private + index * PAGE_SIZE, PRIVATE_BYTE)?;
    }
    let window = mm::FrameWindow::open();
    if advise(process, private + PAGE_SIZE, 2 * PAGE_SIZE, MADV_DONTNEED) != Ok(0) {
        return Err("madvise refused to drop pages of a private file mapping");
    }
    if window.kept() != -2 {
        window.report("madvise");
        return Err("MADV_DONTNEED on a private file mapping did not give its two copies back");
    }
    counts.given_back += 2;
    for index in 1..=2_u64 {
        uaccess::copy_from_user(space, private + index * PAGE_SIZE, &mut shown)
            .map_err(|_| "a private file mapping could not be read after MADV_DONTNEED")?;
        let start = usize::try_from(index * PAGE_SIZE).map_err(|_| "an offset does not fit")?;
        if data.get(start..start + page) != Some(shown.as_slice()) {
            return Err("a private file mapping dropped with MADV_DONTNEED did not show the file");
        }
    }
    refused(
        advise(process, private, PAGE_SIZE, MADV_FREE),
        Errno::EINVAL,
        "MADV_FREE on a private file mapping was not EINVAL",
        counts,
    )?;
    refused(
        advise(process, private, PAGE_SIZE, MADV_REMOVE),
        Errno::EACCES,
        "MADV_REMOVE on a private file mapping was not EACCES",
        counts,
    )
}

/// A shared file mapping writes, is dropped, and still shows what it wrote,
/// as the file does.
fn check_a_shared_file_mapping(
    process: &Process,
    shared: u64,
    file: &ferrix_vfs::OpenFile,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    let space = process.space();
    uaccess::copy_to_user(space, shared + SHARED_AT, SHARED_WRITES)
        .map_err(|_| "a write through a shared file mapping was refused")?;
    if advise(process, shared, FILE_PAGES * PAGE_SIZE, MADV_DONTNEED) != Ok(0) {
        return Err("madvise refused to drop a shared file mapping's translations");
    }
    let mut shown = vec![0_u8; SHARED_WRITES.len()];
    uaccess::copy_from_user(space, shared + SHARED_AT, &mut shown)
        .map_err(|_| "a shared file mapping could not be read after MADV_DONTNEED")?;
    let mut in_file = vec![0_u8; SHARED_WRITES.len()];
    if file.read_at(SHARED_AT, &mut in_file) != Ok(in_file.len()) {
        return Err("the memfd could not be read");
    }
    if shown != SHARED_WRITES || in_file != SHARED_WRITES {
        return Err("MADV_DONTNEED on a shared file mapping lost what the mapping wrote");
    }
    refused(
        advise(process, shared, PAGE_SIZE, MADV_FREE),
        Errno::EINVAL,
        "MADV_FREE on a shared file mapping was not EINVAL",
        counts,
    )?;
    refused(
        advise(process, shared, PAGE_SIZE, MADV_REMOVE),
        Errno::EOPNOTSUPP,
        "MADV_REMOVE on a shared file mapping was not EOPNOTSUPP",
        counts,
    )
}

/// `MADV_REMOVE` punches a hole in shared anonymous memory that a `fork`
/// child sees as well, and gives its two frames back; `MADV_DONTNEED` there
/// keeps the contents.
///
/// Verifies: L.user.82
fn check_a_hole_in_shared_memory(
    process: &Process,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    let len = 4 * PAGE_SIZE;
    let at = map(process, len, MAP_ANONYMOUS | MAP_SHARED, -1)?;
    let space: &AddressSpace = process.space();
    let outcome = (0..4)
        .try_for_each(|index| fill(space, at + index * PAGE_SIZE, pattern(index)))
        .and_then(|()| {
            let forked = space
                .fork()
                .map_err(|_| "the madvise check's space would not fork")?;
            let child: &AddressSpace = &forked;
            for index in 0..4 {
                if !page_is(child, at + index * PAGE_SIZE, pattern(index))? {
                    return Err("a fork child did not share its parent's shared memory");
                }
            }
            let window = mm::FrameWindow::open();
            if advise(process, at + PAGE_SIZE, 2 * PAGE_SIZE, MADV_REMOVE) != Ok(0) {
                return Err("madvise refused to punch a hole in shared anonymous memory");
            }
            if window.kept() != -2 {
                window.report("madvise");
                return Err("MADV_REMOVE did not give the hole's two frames back");
            }
            counts.given_back += 2;
            for (mapping, index) in [(space, 1), (space, 2), (child, 1), (child, 2)] {
                if !page_is(mapping, at + index * PAGE_SIZE, 0)? {
                    return Err(
                        "a hole MADV_REMOVE punched did not read as zeros in every mapping",
                    );
                }
            }
            for (mapping, index) in [(space, 0), (space, 3), (child, 0), (child, 3)] {
                if !page_is(mapping, at + index * PAGE_SIZE, pattern(index))? {
                    return Err("MADV_REMOVE punched a hole wider than its range");
                }
            }
            Ok(())
        })
        .and_then(|()| {
            if advise(process, at, PAGE_SIZE, MADV_DONTNEED) != Ok(0)
                || !page_is(space, at, pattern(0))?
            {
                return Err("MADV_DONTNEED on shared anonymous memory lost its contents");
            }
            refused(
                advise(process, at, PAGE_SIZE, MADV_FREE),
                Errno::EINVAL,
                "MADV_FREE on shared anonymous memory was not EINVAL",
                counts,
            )
        });
    unmapped(process, at, len)?;
    outcome
}

/// The refusals that need only private anonymous memory, and a hole.
///
/// Verifies: L.user.83
fn check_refusals(process: &Process, counts: &mut Counts) -> Result<(), &'static str> {
    let at = map(process, 2 * PAGE_SIZE, MAP_ANONYMOUS | MAP_PRIVATE, -1)?;
    let wraps = usize::MAX as u64;
    let refusals = [
        (
            at + 1,
            PAGE_SIZE,
            MADV_DONTNEED,
            Errno::EINVAL,
            "madvise took an address off a page boundary",
        ),
        (
            at,
            PAGE_SIZE,
            5,
            Errno::EINVAL,
            "madvise took advice Linux has no number for",
        ),
        (
            at,
            PAGE_SIZE,
            MADV_WIPEONFORK,
            Errno::EINVAL,
            "madvise took MADV_WIPEONFORK, which it cannot honour",
        ),
        (
            at,
            PAGE_SIZE,
            MADV_MERGEABLE,
            Errno::EINVAL,
            "madvise took MADV_MERGEABLE with no KSM",
        ),
        (
            at,
            wraps,
            MADV_DONTNEED,
            Errno::EINVAL,
            "madvise took a length that wraps",
        ),
        (
            at,
            PAGE_SIZE,
            MADV_REMOVE,
            Errno::EINVAL,
            "MADV_REMOVE on private anonymous memory was not EINVAL",
        ),
    ];
    let outcome = refusals
        .into_iter()
        .try_for_each(|(start, len, advice, wanted, what)| {
            refused(advise(process, start, len, advice), wanted, what, counts)
        })
        .and_then(|()| {
            if advise(process, at, 0, MADV_DONTNEED) != Ok(0) {
                return Err("madvise of no bytes did not succeed");
            }
            fill(process.space(), at, pattern(0))?;
            if memory::sys_munmap(process, at + PAGE_SIZE, PAGE_SIZE) != Ok(0) {
                return Err("the madvise check could not unmap half of its mapping");
            }
            refused(
                advise(process, at, 2 * PAGE_SIZE, MADV_DONTNEED),
                Errno::ENOMEM,
                "madvise over a range with a hole was not ENOMEM",
                counts,
            )?;
            if !page_is(process.space(), at, 0)? {
                return Err(
                    "madvise over a range with a hole did not advise the part that is mapped",
                );
            }
            refused(
                advise(process, at, 2 * PAGE_SIZE, MADV_WILLNEED),
                Errno::ENOMEM,
                "a hint over a range with a hole was not ENOMEM",
                counts,
            )
        });
    unmapped(process, at, 2 * PAGE_SIZE)?;
    outcome
}

/// A byte for page `index` of the anonymous mappings: never zero.
fn pattern(index: u64) -> u8 {
    (index as u8).wrapping_mul(37) | 0x80
}

/// A byte of the file, by its offset: never zero.
fn file_byte(at: u64) -> u8 {
    (at.wrapping_mul(29) ^ (at >> 12)) as u8 | 1
}

/// `mmap` of `len` bytes, read and write, with `flags`, of `descriptor`.
fn map(process: &Process, len: u64, flags: u32, descriptor: i32) -> Result<u64, &'static str> {
    memory::sys_mmap(
        process,
        &MmapRequest {
            addr: 0,
            len,
            prot: PROT_READ | PROT_WRITE,
            flags,
            fd: i64::from(descriptor),
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    )
    .ok()
    .and_then(|at| u64::try_from(at).ok())
    .ok_or("a mapping for the madvise check was refused")
}

/// `munmap`, required to work.
fn unmapped(process: &Process, at: u64, len: u64) -> Result<(), &'static str> {
    memory::sys_munmap(process, at, len)
        .map(|_| ())
        .map_err(|_| "a mapping the madvise check made would not unmap")
}

/// `madvise` by number, as a program would call it.
fn advise(process: &Process, at: u64, len: u64, advice: i32) -> Result<usize, Errno> {
    by_number(
        process,
        Syscall::Madvise,
        [at, len, u64::from(advice.cast_unsigned()), 0, 0, 0],
    )
}

/// Write the page at `at` full of `byte`.
fn fill(space: &AddressSpace, at: u64, byte: u8) -> Result<(), &'static str> {
    let page = usize::try_from(PAGE_SIZE).map_err(|_| "the page size does not fit")?;
    uaccess::copy_to_user(space, at, &vec![byte; page])
        .map_err(|_| "a page of the madvise check could not be written")
}

/// Whether the page at `at` is full of `byte`.
fn page_is(space: &AddressSpace, at: u64, byte: u8) -> Result<bool, &'static str> {
    let page = usize::try_from(PAGE_SIZE).map_err(|_| "the page size does not fit")?;
    let mut shown = vec![0_u8; page];
    uaccess::copy_from_user(space, at, &mut shown)
        .map_err(|_| "a page of the madvise check could not be read")?;
    Ok(shown.iter().all(|&seen| seen == byte))
}

/// Make `call` by its number, as a program on this architecture would.
fn by_number(process: &Process, call: Syscall, args: [u64; 6]) -> Result<usize, Errno> {
    syscall_check::call_by_number(process, call, args)
}

/// Require `got` to be `Err(wanted)`, counting the refusal.
fn refused(
    got: Result<usize, Errno>,
    wanted: Errno,
    what: &'static str,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    if got != Err(wanted) {
        return Err(what);
    }
    counts.refusals += 1;
    Ok(())
}
