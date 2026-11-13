//! The kept resident counts against a recount (`L.user.236`), and a read of
//! them against the number of regions (`L.user.237`): `docs/RSS-COUNTS.md`
//! §7.
//!
//! A process of the check's own makes, by system call as a program would,
//! each kind of change that moves a space's resident pages, and after each
//! one holds `AddressSpace::resident` -- what `/proc/<pid>/status` and `stat`
//! read -- equal to `user::rss_check::recount`, which counts every region's
//! object afresh as the kernel did before the counts were kept. Nothing else
//! runs in the space while it looks. Then 4,000 regions in Wine's pattern,
//! and the read timed over them against a space of one region.

use alloc::vec;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_errno::Errno;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    MADV_DONTNEED, MAP_ANONYMOUS, MAP_FIXED, MAP_PRIVATE, MAP_SHARED, MREMAP_FIXED, MREMAP_MAYMOVE,
    PROT_NONE, PROT_READ, PROT_WRITE,
};

use crate::syscall::check as syscall_check;
use crate::syscall::memory::{self, MmapRequest, OffsetUnit};
use crate::syscall::process::{self, Process};
use crate::syscall::{fd, uaccess};
use crate::user::rss_check;
use crate::user::space::{self, AddressSpace};
use crate::user::vmo::Vmo;

/// Regions of the Wine-pattern reserve.
pub const REGIONS: u64 = 4_000;
/// Pages in each of its pieces.
const PIECE: u64 = 2;
/// Reads of the counts timed over each space.
pub const READS_TIMED: u32 = 200;
/// The memfd's name, with its terminator, staged at the start of the scratch
/// page.
const NAME: &[u8] = b"rss-kept-check\0";

/// What the check measured, for the boot log.
#[derive(Debug, Clone, Copy)]
pub struct Report {
    /// Comparisons of the kept counts with a recount, every one equal.
    pub compared: u32,
    /// Regions of the largest space compared.
    pub regions: usize,
    /// Nanoseconds for [`READS_TIMED`] reads of its counts.
    pub large_ns: u64,
    /// Nanoseconds for as many reads of a space of one region.
    pub small_ns: u64,
}

/// Run it.
///
/// Verifies: L.user.236
/// Verifies: L.user.237
pub fn run() -> Result<Report, &'static str> {
    let process =
        process::new_for_check().map_err(|_| "could not make a process for the rss check")?;
    let mut compared = 0;
    let scratch = map(&process, PAGE_SIZE, MAP_ANONYMOUS | MAP_PRIVATE, -1)?;
    let outcome = check_anonymous(&process, &mut compared)
        .and_then(|()| check_fork_and_exit(&process, &mut compared))
        .and_then(|()| check_files(&process, scratch, &mut compared))
        .and_then(|()| check_a_move(&process, &mut compared))
        .and_then(|()| check_a_shared_remap(&process, &mut compared))
        .and_then(|()| check_a_mask_stays_charged(&process, scratch, &mut compared))
        .and_then(|()| measure_holds(&process, scratch, &mut compared))
        .and_then(|()| check_many_regions(&process, &mut compared));
    for descriptor in 3..32 {
        let _ = fd::sys_close(&process, descriptor);
    }
    let _ = memory::sys_munmap(&process, scratch, PAGE_SIZE);
    let (regions, large_ns, small_ns) = outcome?;
    same(
        process.space(),
        "after the check's last unmap",
        &mut compared,
    )?;
    Ok(Report {
        compared,
        regions,
        large_ns,
        small_ns,
    })
}

/// The kept counts of `space` equal its recount, or the check fails saying
/// after what, with both on the console.
fn same(space: &AddressSpace, after: &'static str, compared: &mut u32) -> Result<(), &'static str> {
    *compared += 1;
    match rss_check::agrees(space) {
        Ok(()) => Ok(()),
        Err((kept, counted)) => {
            crate::console::println!(
                "  rss      {after}: kept anon {} file {} shmem {}, recounted anon {} file {} shmem {}",
                kept.anon,
                kept.file,
                kept.shmem,
                counted.anon,
                counted.file,
                counted.shmem,
            );
            Err("a space's kept resident counts differ from a recount of its regions")
        }
    }
}

/// Private anonymous memory: first touch, an `mprotect` split and its merge,
/// an `munmap` of a middle and `madvise(DONTNEED)`.
fn check_anonymous(process: &Process, compared: &mut u32) -> Result<(), &'static str> {
    let space = process.space();
    let at = map(process, 8 * PAGE_SIZE, MAP_ANONYMOUS | MAP_PRIVATE, -1)?;
    same(space, "a new mapping", compared)?;
    (0..8).try_for_each(|page| fill(space, at + page * PAGE_SIZE, 1))?;
    same(space, "a first touch of eight pages", compared)?;
    let _ = by_number(
        process,
        Syscall::Mprotect,
        [
            at + 2 * PAGE_SIZE,
            2 * PAGE_SIZE,
            u64::from(PROT_READ),
            0,
            0,
            0,
        ],
    )
    .map_err(|_| "mprotect refused to split a region")?;
    same(space, "an mprotect split", compared)?;
    let _ = by_number(
        process,
        Syscall::Mprotect,
        [
            at + 2 * PAGE_SIZE,
            2 * PAGE_SIZE,
            u64::from(PROT_READ | PROT_WRITE),
            0,
            0,
            0,
        ],
    )
    .map_err(|_| "mprotect refused to merge a region back")?;
    same(space, "an mprotect merge", compared)?;
    let _ = memory::sys_munmap(process, at + 3 * PAGE_SIZE, 2 * PAGE_SIZE)
        .map_err(|_| "munmap refused a region's middle")?;
    same(space, "an munmap of a region's middle", compared)?;
    let _ = by_number(
        process,
        Syscall::Madvise,
        [
            at,
            2 * PAGE_SIZE,
            u64::from(MADV_DONTNEED.cast_unsigned()),
            0,
            0,
            0,
        ],
    )
    .map_err(|_| "madvise refused DONTNEED")?;
    same(space, "madvise(DONTNEED)", compared)?;
    let _ = memory::sys_munmap(process, at, 8 * PAGE_SIZE);
    same(space, "an munmap of whole regions", compared)
}

/// A `fork`: copy-on-write in child and parent, shared anonymous memory both
/// see, and the child's exit, after which no object it mapped views it.
fn check_fork_and_exit(process: &Process, compared: &mut u32) -> Result<(), &'static str> {
    let space = process.space();
    let private = map(process, 4 * PAGE_SIZE, MAP_ANONYMOUS | MAP_PRIVATE, -1)?;
    let shared = map(process, 4 * PAGE_SIZE, MAP_ANONYMOUS | MAP_SHARED, -1)?;
    (0..4).try_for_each(|page| fill(space, private + page * PAGE_SIZE, 2))?;
    (0..2).try_for_each(|page| fill(space, shared + page * PAGE_SIZE, 3))?;
    let outcome = (|| {
        let child = space
            .fork()
            .map_err(|_| "the rss check's space would not fork")?;
        same(space, "a fork, in the parent", compared)?;
        same(&child, "a fork, in the child", compared)?;
        fill(&child, private + PAGE_SIZE, 4)?;
        same(&child, "a copy-on-write write, in the child", compared)?;
        same(
            space,
            "a copy-on-write write in the child, in the parent",
            compared,
        )?;
        fill(space, private + 2 * PAGE_SIZE, 5)?;
        same(space, "a copy-on-write write, in the parent", compared)?;
        fill(&child, shared + 3 * PAGE_SIZE, 6)?;
        same(
            space,
            "a child's write to shared memory, in the parent",
            compared,
        )?;
        same(&child, "a write to shared memory, in the child", compared)?;
        // An object the child alone maps: no view of it is left once the
        // child's space goes.
        let flags = ferrix_vma::VmaFlags {
            shared: true,
            ..ferrix_vma::VmaFlags::READ_WRITE
        };
        let own = space::map_anywhere_for_check(&child, None, 2 * PAGE_SIZE, flags)
            .map_err(|_| "the child refused a shared mapping of its own")?;
        let _ = child.unmap(own, 2 * PAGE_SIZE);
        let id = child
            .map_anonymous(own, 2 * PAGE_SIZE, flags)
            .map_err(|_| "the child refused a shared mapping of its own")?;
        fill(&child, own, 7)?;
        same(
            &child,
            "a shared mapping of the child's own, written",
            compared,
        )?;
        let object = child
            .object(id)
            .ok_or("the child's shared mapping has no object")?;
        drop(child);
        if rss_check::viewed(&object) {
            return Err("an object a space mapped still counts for it after the space went");
        }
        same(space, "the child's exit, in the parent", compared)
    })();
    let _ = memory::sys_munmap(process, private, 4 * PAGE_SIZE);
    let _ = memory::sys_munmap(process, shared, 4 * PAGE_SIZE);
    outcome
}

/// A memfd (a memory filesystem's file, shared memory): mapped shared twice,
/// mapped private and written, a fork that writes its own copy, truncated
/// under the private mapping's copies.
fn check_files(process: &Process, scratch: u64, compared: &mut u32) -> Result<(), &'static str> {
    let space = process.space();
    uaccess::copy_to_user(space, scratch, NAME).map_err(|_| "could not stage the memfd's name")?;
    let memfd = by_number(process, Syscall::MemfdCreate, [scratch, 0, 0, 0, 0, 0])
        .ok()
        .and_then(|made| i32::try_from(made).ok())
        .ok_or("memfd_create was refused")?;
    let file = fd::file(process, memfd).map_err(|_| "the memfd is gone")?;
    let data = vec![0x5a_u8; (4 * PAGE_SIZE) as usize];
    if file.write_at(0, &data) != Ok(data.len()) {
        return Err("the memfd was not written whole");
    }
    let _object = file
        .inode()
        .mapping()
        .and_then(|object| object.downcast::<Vmo>().ok())
        .ok_or("a memfd has no object")?;
    let len = 4 * PAGE_SIZE;
    let one = map(process, len, MAP_SHARED, memfd)?;
    let two = map(process, len, MAP_SHARED, memfd)?;
    let private = map(process, len, MAP_PRIVATE, memfd)?;
    let outcome = (|| {
        same(
            space,
            "a file mapped shared twice and private once",
            compared,
        )?;
        fill(space, private + PAGE_SIZE, 8)?;
        same(space, "a private file mapping's write", compared)?;
        let child = space
            .fork()
            .map_err(|_| "the rss check's space would not fork")?;
        same(
            &child,
            "a fork of a private file mapping's copies, in the child",
            compared,
        )?;
        fill(&child, private + PAGE_SIZE, 9)?;
        fill(&child, private + 2 * PAGE_SIZE, 9)?;
        same(
            &child,
            "a write to a private file mapping, in the child",
            compared,
        )?;
        same(
            space,
            "a private file mapping's write in the child, in the parent",
            compared,
        )?;
        drop(child);
        let _ = by_number(
            process,
            Syscall::Ftruncate,
            [u64::from(memfd.cast_unsigned()), PAGE_SIZE, 0, 0, 0, 0],
        )
        .map_err(|_| "ftruncate refused the memfd")?;
        same(
            space,
            "a truncation under a private mapping's copies",
            compared,
        )?;
        let _ = file
            .write_at(0, &data)
            .map_err(|_| "the memfd would not grow back")?;
        same(space, "the file grown back", compared)
    })();
    for at in [one, two, private] {
        let _ = memory::sys_munmap(process, at, len);
    }
    let _ = fd::sys_close(process, memfd);
    outcome?;
    same(space, "the file's mappings gone", compared)
}

/// Pages of the file [`measure_holds`] maps.
const MEASURED_PAGES: u64 = 4096;
/// Spaces' views of one object [`measure_holds`] gives its page events.
const VIEWS: u64 = 64;

/// What `docs/certification/MEMORY-AND-TIMING.md` §2.2y's census owes for the
/// kept counts' new holds, said on the console and asserted nothing of: an
/// `mmap` of a file whose [`MEASURED_PAGES`] pages are all cached, which
/// counts them under the space's lock, a chunk of the pages lock at a time;
/// and a write of as many new pages into a file [`VIEWS`] mappings view,
/// each page's event moving every view's count under the pages lock.
fn measure_holds(process: &Process, scratch: u64, compared: &mut u32) -> Result<(), &'static str> {
    let space = process.space();
    uaccess::copy_to_user(space, scratch, NAME).map_err(|_| "could not stage the memfd's name")?;
    let memfd = by_number(process, Syscall::MemfdCreate, [scratch, 0, 0, 0, 0, 0])
        .ok()
        .and_then(|made| i32::try_from(made).ok())
        .ok_or("memfd_create was refused")?;
    let file = fd::file(process, memfd).map_err(|_| "the memfd is gone")?;
    let page = vec![0x3c_u8; PAGE_SIZE as usize];
    let write_all = || {
        (0..MEASURED_PAGES).all(|index| file.write_at(index * PAGE_SIZE, &page) == Ok(page.len()))
    };
    let outcome = (|| {
        let start = crate::timer::now_nanos();
        let first = write_all();
        let alone = crate::timer::now_nanos().saturating_sub(start);
        if !first {
            return Err("the measured memfd was not written whole");
        }
        let len = MEASURED_PAGES * PAGE_SIZE;
        let start = crate::timer::now_nanos();
        let whole = map(process, len, MAP_SHARED, memfd)?;
        let mapped = crate::timer::now_nanos().saturating_sub(start);
        same(space, "a mapping of a file all cached", compared)?;
        let _ = memory::sys_munmap(process, whole, len);
        let _ = by_number(
            process,
            Syscall::Ftruncate,
            [u64::from(memfd.cast_unsigned()), 0, 0, 0, 0, 0],
        );
        let mut views: Vec<u64> = Vec::new();
        for _ in 0..VIEWS {
            views.push(map(process, PAGE_SIZE, MAP_SHARED, memfd)?);
        }
        let start = crate::timer::now_nanos();
        let wrote = write_all();
        let written = crate::timer::now_nanos().saturating_sub(start);
        same(space, "a file written beside 64 mappings of it", compared)?;
        for at in views {
            let _ = memory::sys_munmap(process, at, PAGE_SIZE);
        }
        if !wrote {
            return Err("the measured memfd was not written again whole");
        }
        crate::console::println!(
            "  rss      an mmap of {MEASURED_PAGES} cached pages took {mapped} ns; {MEASURED_PAGES} new pages written into a file took {alone} ns unmapped, {written} ns with {VIEWS} mappings viewing it"
        );
        Ok(())
    })();
    let _ = fd::sys_close(process, memfd);
    outcome
}

/// `mremap` moving a private region's pages to a new object at a fixed
/// address, over a mapping it replaces.
fn check_a_move(process: &Process, compared: &mut u32) -> Result<(), &'static str> {
    let space = process.space();
    let from = map(process, 4 * PAGE_SIZE, MAP_ANONYMOUS | MAP_PRIVATE, -1)?;
    let to = map(process, 8 * PAGE_SIZE, MAP_ANONYMOUS | MAP_PRIVATE, -1)?;
    (0..4).try_for_each(|page| fill(space, from + page * PAGE_SIZE, 10))?;
    fill(space, to, 11)?;
    let moved = by_number(
        process,
        Syscall::Mremap,
        [
            from,
            4 * PAGE_SIZE,
            6 * PAGE_SIZE,
            u64::from(MREMAP_MAYMOVE | MREMAP_FIXED),
            to,
            0,
        ],
    )
    .map_err(|_| "mremap refused to move a private region");
    let outcome =
        moved.and_then(|_| same(space, "an mremap move over a fixed destination", compared));
    let _ = memory::sys_munmap(process, from, 4 * PAGE_SIZE);
    let _ = memory::sys_munmap(process, to, 8 * PAGE_SIZE);
    outcome
}

/// Pages of the file [`check_a_mask_stays_charged`] maps: four times
/// `COUNT_CHUNK`.
const MASKED_PAGES: u64 = 1024;

/// A private file mapping that writes each page of its file once and drops
/// the copy with `madvise(DONTNEED)` leaves an entry per page in its file
/// view's mask, which stays until the mapping goes (an unmask frees no
/// node): every one of them must be charged (ledger 8894 T1).
fn check_a_mask_stays_charged(
    process: &Process,
    scratch: u64,
    compared: &mut u32,
) -> Result<(), &'static str> {
    let space = process.space();
    uaccess::copy_to_user(space, scratch, NAME).map_err(|_| "could not stage the memfd's name")?;
    let memfd = by_number(process, Syscall::MemfdCreate, [scratch, 0, 0, 0, 0, 0])
        .ok()
        .and_then(|made| i32::try_from(made).ok())
        .ok_or("memfd_create was refused")?;
    let len = MASKED_PAGES * PAGE_SIZE;
    let outcome = (|| {
        let file = fd::file(process, memfd).map_err(|_| "the memfd is gone")?;
        let page = vec![0x6b_u8; PAGE_SIZE as usize];
        if !(0..MASKED_PAGES).all(|index| file.write_at(index * PAGE_SIZE, &page) == Ok(page.len()))
        {
            return Err("the masked memfd was not written whole");
        }
        let object = file
            .inode()
            .mapping()
            .and_then(|object| object.downcast::<Vmo>().ok())
            .ok_or("a memfd has no object")?;
        let private = map(process, len, MAP_PRIVATE, memfd)?;
        let walked = (0..MASKED_PAGES).try_for_each(|index| {
            let at = private + index * PAGE_SIZE;
            fill(space, at, 0x6c)?;
            by_number(
                process,
                Syscall::Madvise,
                [
                    at,
                    PAGE_SIZE,
                    u64::from(MADV_DONTNEED.cast_unsigned()),
                    0,
                    0,
                    0,
                ],
            )
            .map(|_| ())
            .map_err(|_| "madvise(DONTNEED) of a private file mapping was refused")
        });
        let held = rss_check::mask_held(&object);
        let _ = memory::sys_munmap(process, private, len);
        walked?;
        same(
            space,
            "a private file mapping's copies all dropped",
            compared,
        )?;
        if held.0 < MASKED_PAGES as usize || !held.1 {
            crate::console::println!(
                "  rss      a mask of {} entries, charged: {}",
                held.0,
                held.1
            );
            return Err("a private file mapping's mask holds heap its charge does not cover");
        }
        Ok(())
    })();
    let _ = fd::sys_close(process, memfd);
    outcome
}

/// `mremap` of shared anonymous memory, which keeps its object: grown in
/// place or moved, its new placement counted in while the old range still
/// covers the same pages, so the id's cover counts some twice for a moment;
/// then shrunk.
fn check_a_shared_remap(process: &Process, compared: &mut u32) -> Result<(), &'static str> {
    let space = process.space();
    let at = map(process, 4 * PAGE_SIZE, MAP_ANONYMOUS | MAP_SHARED, -1)?;
    let mut now = (at, 4 * PAGE_SIZE);
    let outcome = (|| {
        (0..4).try_for_each(|page| fill(space, at + page * PAGE_SIZE, 15))?;
        same(space, "shared anonymous memory touched", compared)?;
        let grown = by_number(
            process,
            Syscall::Mremap,
            [
                at,
                4 * PAGE_SIZE,
                8 * PAGE_SIZE,
                u64::from(MREMAP_MAYMOVE),
                0,
                0,
            ],
        )
        .map_err(|_| "mremap refused to grow shared memory")?;
        now = (grown as u64, 8 * PAGE_SIZE);
        fill(space, now.0 + 6 * PAGE_SIZE, 16)?;
        same(space, "shared anonymous memory grown and touched", compared)?;
        let to = map(process, 8 * PAGE_SIZE, MAP_ANONYMOUS | MAP_PRIVATE, -1)?;
        let moved = by_number(
            process,
            Syscall::Mremap,
            [
                now.0,
                8 * PAGE_SIZE,
                8 * PAGE_SIZE,
                u64::from(MREMAP_MAYMOVE | MREMAP_FIXED),
                to,
                0,
            ],
        )
        .map_err(|_| "mremap refused to move shared memory");
        if moved.is_err() {
            let _ = memory::sys_munmap(process, to, 8 * PAGE_SIZE);
        }
        now = (moved? as u64, 8 * PAGE_SIZE);
        same(
            space,
            "shared anonymous memory moved over a fixed destination",
            compared,
        )?;
        let _ = by_number(
            process,
            Syscall::Mremap,
            [now.0, 8 * PAGE_SIZE, 2 * PAGE_SIZE, 0, 0, 0],
        )
        .map_err(|_| "mremap refused to shrink shared memory")?;
        now = (now.0, 2 * PAGE_SIZE);
        same(space, "shared anonymous memory shrunk", compared)
    })();
    let _ = memory::sys_munmap(process, now.0, now.1);
    outcome
}

/// 4,000 regions in Wine's pattern: one reserve with no access, every second
/// two-page piece of it made writable and touched. Then the read timed there
/// and over a space of one region. How many regions, and the two times.
fn check_many_regions(
    process: &Process,
    compared: &mut u32,
) -> Result<(usize, u64, u64), &'static str> {
    let space = process.space();
    let len = REGIONS * PIECE * PAGE_SIZE;
    let reserve = memory::sys_mmap(
        process,
        &MmapRequest {
            addr: 0,
            len,
            prot: PROT_NONE,
            flags: MAP_ANONYMOUS | MAP_PRIVATE,
            fd: -1,
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    )
    .ok()
    .and_then(|at| u64::try_from(at).ok())
    .ok_or("the reserve was refused")?;
    let outcome = (|| {
        for piece in (1..REGIONS).step_by(2) {
            let at = reserve + piece * PIECE * PAGE_SIZE;
            let _ = by_number(
                process,
                Syscall::Mprotect,
                [
                    at,
                    PIECE * PAGE_SIZE,
                    u64::from(PROT_READ | PROT_WRITE),
                    0,
                    0,
                    0,
                ],
            )
            .map_err(|_| "mprotect refused a piece of the reserve")?;
            fill(space, at, 12)?;
        }
        // And one piece replaced by a fixed mapping, as Wine allocates.
        let fixed = reserve + 3 * PIECE * PAGE_SIZE;
        let _ = memory::sys_mmap(
            process,
            &MmapRequest {
                addr: fixed,
                len: PIECE * PAGE_SIZE,
                prot: PROT_READ | PROT_WRITE,
                flags: MAP_ANONYMOUS | MAP_PRIVATE | MAP_FIXED,
                fd: -1,
                offset: 0,
                unit: OffsetUnit::Bytes,
            },
        )
        .map_err(|_| "a fixed mapping into the reserve was refused")?;
        fill(space, fixed, 13)?;
        same(space, "4,000 regions over one reserve", compared)?;
        let regions = space.region_count();
        let large_ns = time_reads(space);
        let small = process::new_for_check().map_err(|_| "could not make a second process")?;
        let single = map(&small, PAGE_SIZE, MAP_ANONYMOUS | MAP_PRIVATE, -1)?;
        fill(small.space(), single, 14)?;
        let small_ns = time_reads(small.space());
        let _ = memory::sys_munmap(&small, single, PAGE_SIZE);
        crate::console::println!(
            "  rss      {READS_TIMED} reads of the resident counts: {large_ns} ns over {regions} regions, {small_ns} ns over one"
        );
        // Work independent of the regions: the large space's reads within
        // four times the small's, and a millisecond for a slow processor or
        // an interrupt between them. A recount takes milliseconds a read.
        if large_ns > small_ns.saturating_mul(4).saturating_add(1_000_000) {
            return Err("a read of the resident counts grew with the regions");
        }
        Ok((regions, large_ns, small_ns))
    })();
    let _ = memory::sys_munmap(process, reserve, len);
    outcome
}

/// Nanoseconds for [`READS_TIMED`] reads of `space`'s counts, as procfs reads them.
fn time_reads(space: &AddressSpace) -> u64 {
    let start = crate::timer::now_nanos();
    let mut seen = 0u64;
    for _ in 0..READS_TIMED {
        seen = seen.wrapping_add(core::hint::black_box(space.resident()).total());
    }
    let _ = core::hint::black_box(seen);
    crate::timer::now_nanos().saturating_sub(start)
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
    .ok_or("a mapping for the rss check was refused")
}

/// Write the page at `at` full of `byte`.
fn fill(space: &AddressSpace, at: u64, byte: u8) -> Result<(), &'static str> {
    let page: Vec<u8> = vec![byte; PAGE_SIZE as usize];
    uaccess::copy_to_user(space, at, &page)
        .map_err(|_| "a page of the rss check could not be written")
}

/// Make `call` by its number, as a program on this architecture would.
fn by_number(process: &Process, call: Syscall, args: [u64; 6]) -> Result<usize, Errno> {
    syscall_check::call_by_number(process, call, args)
}
