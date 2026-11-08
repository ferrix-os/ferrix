//! **fault-race**: page faults from eight threads of one address space at once,
//! racing the calls that change what the faults find (`L.user.222`, `L.user.224`).
//!
//! A fault takes a zeroed frame before its space's lock and resolves in one hold of
//! it; another thread's `munmap`, `mprotect` or `fork` may change the space between.
//! Whatever the order, a program sees memory as Linux shows it: a page never touched
//! reads as zeros, a page a thread wrote reads what it wrote, a private file page
//! reads the file until the program writes it and the file never sees that write, a
//! shared file page's write is the file's, and a fork child's copy is the parent's
//! memory at the fork, unchanged by the parent's later writes. The rounds:
//!
//! * **fresh**: eight threads each write a tag into every page of their slice of one
//!   fresh anonymous mapping, half of them reading each page first (zeros), while a
//!   ninth maps and unmaps a mapping beside it, and a tenth flips the protection of
//!   a range the eight do not write between read-only and read-write;
//! * **fork**: the same writers rewrite their pages with a new generation while the
//!   main thread forks; each child reads every page twice, a pause apart, and needs
//!   both reads equal and every word a tag a thread could have written;
//! * **file**: eight threads read then write their pages of a private mapping of a
//!   file, and write their pages of a shared one; the private one shows the file and
//!   then the thread's own write, the file keeps its words, and the shared writes are
//!   in the file;
//! * **shared-fork**: eight threads write private anonymous, shared anonymous and
//!   shared `memfd` pages, each generation read back, while the main thread forks;
//!   each child checks its private copy, overwrites it, and answers in a page of
//!   each shared mapping, which the parent must see, while the writers must never
//!   see the child's private writes in their own pages.

#![expect(
    unsafe_code,
    reason = "mappings made, changed and written by hand, and a fork child that reads them"
)]

use core::ptr;
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

const PAGE: usize = 4096;
/// Threads that fault.
const WRITERS: usize = 8;
/// Pages each writer owns in a mapping.
const SLICE: usize = 256;
/// Pages of the range the protector flips, which nobody writes.
const FLIPPED: usize = 64;
/// Rounds of the fresh step, each on a new mapping.
const ROUNDS: u32 = 6;
/// Forks during the fork step.
const FORKS: usize = 6;

/// A writer's tag for `page` in `generation`: never zero, and never another
/// writer's or another page's.
fn tag(writer: usize, page: usize, generation: u32) -> u32 {
    (generation << 24) | ((writer as u32) << 16) | (page as u32 + 1)
}

/// Whether `word`, read at page `page` of writer `writer`'s slice, is one its
/// writer could have written by `generation`, or zero.
fn plausible(word: u32, writer: usize, page: usize, generation: u32) -> bool {
    word == 0 || (word & 0x00ff_ffff == tag(writer, page, 0) && word >> 24 <= generation)
}

/// An anonymous private mapping of `pages` pages, read-write.
fn anonymous(pages: usize, fail: fn(&str) -> !) -> *mut u8 {
    // SAFETY: a fresh address chosen by the kernel.
    let at = unsafe {
        libc::mmap(
            ptr::null_mut(),
            pages * PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if at == libc::MAP_FAILED {
        fail("fault-race: mmap");
    }
    at.cast()
}

/// The word at the start of page `page` of `base`.
fn word(base: usize, page: usize) -> u32 {
    // SAFETY: the callers' mappings, read while they are mapped.
    unsafe { ptr::read_volatile(ptr::with_exposed_provenance::<u32>(base + page * PAGE)) }
}

/// Write `value` at the start of page `page` of `base`.
fn put(base: usize, page: usize, value: u32) {
    // SAFETY: the callers' writable mappings, written while they are mapped.
    unsafe {
        ptr::write_volatile(
            ptr::with_exposed_provenance_mut::<u32>(base + page * PAGE),
            value,
        )
    };
}

/// A wrong word a writer found, as `(writer, page, found, wanted)`: kept in
/// atomics, so a thread reports and the main thread fails.
static WRONG: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];
static ANY_WRONG: AtomicBool = AtomicBool::new(false);

fn report(writer: usize, page: usize, found: u32, wanted: u32) {
    if !ANY_WRONG.swap(true, Ordering::AcqRel) {
        for (slot, value) in WRONG
            .iter()
            .zip([writer as u32, page as u32, found, wanted])
        {
            slot.store(value, Ordering::Relaxed);
        }
    }
}

fn check_reports(what: &str, fail: fn(&str) -> !) {
    if ANY_WRONG.load(Ordering::Acquire) {
        let [writer, page, found, wanted] =
            WRONG.each_ref().map(|slot| slot.load(Ordering::Relaxed));
        println!(
            "threads: fault-race {what}: writer {writer} page {page} read {found:#x}, wanted {wanted:#x}"
        );
        fail("fault-race: a page read wrong");
    }
}

/// A writer's pass over its slice: each page read first if `read_first`, where a
/// page no generation has written must read zeros, then written with this
/// generation's tag; then every page read back.
fn write_slice(slice: usize, writer: usize, generation: u32, read_first: bool) {
    for page in 0..SLICE {
        if read_first && !plausible(word(slice, page), writer, page, generation - 1) {
            report(writer, page, word(slice, page), 0);
        }
        put(slice, page, tag(writer, page, generation));
    }
    for page in 0..SLICE {
        let wanted = tag(writer, page, generation);
        if word(slice, page) != wanted {
            report(writer, page, word(slice, page), wanted);
        }
    }
}

/// A writer's pass over its pages of the file step's two mappings.
fn write_file_slice(private: usize, shared: usize, writer: usize) {
    let first = writer * SLICE;
    for page in first..first + SLICE {
        let file = tag(0xff, page, 0);
        if word(private, page) != file {
            report(writer, page, word(private, page), file);
        }
        put(private, page, tag(writer, page, 1));
        put(shared, page, word(shared, page) | 0x8000_0000);
    }
    for page in first..first + SLICE {
        if word(private, page) != tag(writer, page, 1) {
            report(writer, page, word(private, page), tag(writer, page, 1));
        }
    }
}

/// One fresh round: the writers on `base`, the unmapper and the protector beside it.
fn fresh_round(generation: u32, fail: fn(&str) -> !) {
    let base = anonymous(WRITERS * SLICE + FLIPPED, fail).addr();
    let flipped = base + WRITERS * SLICE * PAGE;
    // The flipped range holds words written before the race, read during it.
    for page in 0..FLIPPED {
        put(flipped, page, tag(WRITERS, page, generation));
    }
    let done = AtomicBool::new(false);
    let finished = AtomicUsize::new(0);
    let start = Barrier::new(WRITERS + 2);
    std::thread::scope(|scope| {
        for writer in 0..WRITERS {
            let (start, finished) = (&start, &finished);
            let _ = scope.spawn(move || {
                let slice = base + writer * SLICE * PAGE;
                let _ = start.wait();
                write_slice(slice, writer, generation, writer % 2 == 1);
                let _ = finished.fetch_add(1, Ordering::AcqRel);
            });
        }
        let (start_ref, done_ref) = (&start, &done);
        // The unmapper: a mapping beside the writers', made, touched and gone.
        let _ = scope.spawn(move || {
            let _ = start_ref.wait();
            while !done_ref.load(Ordering::Acquire) {
                let scratch = anonymous(16, fail);
                for page in 0..16 {
                    put(scratch.addr(), page, 1);
                }
                // SAFETY: the mapping just made, touched by this thread alone.
                if unsafe { libc::munmap(scratch.cast(), 16 * PAGE) } != 0 {
                    fail("fault-race: munmap");
                }
            }
        });
        // The protector, on the main thread: the flipped range read-only and
        // back, splitting the writers' region from it each time, and read,
        // until every writer is done.
        let _ = start.wait();
        let mut flips = 0_u32;
        while finished.load(Ordering::Acquire) < WRITERS {
            let protection = if flips % 2 == 0 {
                libc::PROT_READ
            } else {
                libc::PROT_READ | libc::PROT_WRITE
            };
            // SAFETY: the flipped range, which only reads touch during the race.
            if unsafe {
                libc::mprotect(
                    ptr::with_exposed_provenance_mut(flipped),
                    FLIPPED * PAGE,
                    protection,
                )
            } != 0
            {
                fail("fault-race: mprotect");
            }
            let page = flips as usize % FLIPPED;
            if word(flipped, page) != tag(WRITERS, page, generation) {
                report(
                    WRITERS,
                    page,
                    word(flipped, page),
                    tag(WRITERS, page, generation),
                );
            }
            flips = flips.wrapping_add(1);
        }
        done.store(true, Ordering::Release);
    });
    // SAFETY: the round's mapping, which every thread is done with.
    if unsafe {
        libc::munmap(
            ptr::with_exposed_provenance_mut(base),
            (WRITERS * SLICE + FLIPPED) * PAGE,
        )
    } != 0
    {
        fail("fault-race: munmap of the round");
    }
}

/// A fork child's look at the writers' pages: every word plausible, and the same
/// twice, a pause apart. Only reads, a sleep and `_exit`, as a child of a
/// threaded program may.
fn child_reads(base: usize, generation: u32) -> ! {
    let mut sum = 0_u64;
    for writer in 0..WRITERS {
        for page in 0..SLICE {
            let value = word(base + writer * SLICE * PAGE, page);
            if !plausible(value, writer, page, generation) {
                // SAFETY: ending the child.
                unsafe { libc::_exit(3) };
            }
            sum = sum.wrapping_mul(31).wrapping_add(u64::from(value));
        }
    }
    let pause = libc::timespec {
        tv_sec: 0,
        tv_nsec: 20_000_000,
    };
    // SAFETY: a valid timespec, and no remainder wanted.
    let _ = unsafe { libc::nanosleep(&raw const pause, ptr::null_mut()) };
    let mut again = 0_u64;
    for writer in 0..WRITERS {
        for page in 0..SLICE {
            again = again
                .wrapping_mul(31)
                .wrapping_add(u64::from(word(base + writer * SLICE * PAGE, page)));
        }
    }
    // SAFETY: ending the child.
    unsafe { libc::_exit(if again == sum { 0 } else { 4 }) };
}

/// The fork step: writers rewrite their pages while the main thread forks.
fn fork_round(fail: fn(&str) -> !) {
    let base = anonymous(WRITERS * SLICE, fail).addr();
    const GENERATIONS: u32 = 8;
    let start = Barrier::new(WRITERS + 1);
    std::thread::scope(|scope| {
        for writer in 0..WRITERS {
            let start = &start;
            let _ = scope.spawn(move || {
                let slice = base + writer * SLICE * PAGE;
                let _ = start.wait();
                for generation in 1..=GENERATIONS {
                    write_slice(slice, writer, generation, false);
                }
            });
        }
        let _ = start.wait();
        for _ in 0..FORKS {
            // SAFETY: the child only reads, sleeps and exits (`child_reads`).
            let pid = unsafe { libc::fork() };
            if pid == 0 {
                child_reads(base, GENERATIONS);
            }
            if pid < 0 {
                fail("fault-race: fork");
            }
            let mut status = 0_i32;
            // SAFETY: `status` is this frame's.
            if unsafe { libc::waitpid(pid, &raw mut status, 0) } != pid {
                fail("fault-race: waitpid");
            }
            match (status & 0x7f, status >> 8 & 0xff) {
                (0, 0) => {}
                (0, 3) => fail("fault-race: a fork child read a word nobody wrote"),
                (0, 4) => fail("fault-race: a fork child's copy changed under it"),
                _ => fail("fault-race: a fork child ended some other way"),
            }
        }
    });
    // SAFETY: the step's mapping, which every thread is done with.
    let _ = unsafe {
        libc::munmap(
            ptr::with_exposed_provenance_mut(base),
            WRITERS * SLICE * PAGE,
        )
    };
}

/// The file step: a private and a shared mapping of one file, faulted by eight.
fn file_round(fail: fn(&str) -> !) {
    let pages = WRITERS * SLICE;
    // SAFETY: a name the kernel copies.
    let fd = unsafe { libc::memfd_create(c"fault-race".as_ptr(), 0) };
    // SAFETY: a descriptor just made, or -1, which fails.
    if fd < 0 || unsafe { libc::ftruncate(fd, (pages * PAGE) as libc::off_t) } != 0 {
        fail("fault-race: memfd");
    }
    for page in 0..pages {
        let bytes = tag(0xff, page, 0).to_le_bytes();
        // SAFETY: four bytes on this stack, inside the file.
        if unsafe { libc::pwrite(fd, bytes.as_ptr().cast(), 4, (page * PAGE) as libc::off_t) } != 4
        {
            fail("fault-race: pwrite");
        }
    }
    let map = |flags| {
        // SAFETY: a fresh address and this program's descriptor.
        let at = unsafe {
            libc::mmap(
                ptr::null_mut(),
                pages * PAGE,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                fd,
                0,
            )
        };
        if at == libc::MAP_FAILED {
            fail("fault-race: mmap of the file");
        }
        at.addr()
    };
    let (private, shared) = (map(libc::MAP_PRIVATE), map(libc::MAP_SHARED));
    let start = Barrier::new(WRITERS);
    std::thread::scope(|scope| {
        for writer in 0..WRITERS {
            let start = &start;
            let _ = scope.spawn(move || {
                let _ = start.wait();
                write_file_slice(private, shared, writer);
            });
        }
    });
    for page in 0..pages {
        let mut bytes = [0_u8; 4];
        // SAFETY: four bytes on this stack, inside the file.
        if unsafe {
            libc::pread(
                fd,
                bytes.as_mut_ptr().cast(),
                4,
                (page * PAGE) as libc::off_t,
            )
        } != 4
        {
            fail("fault-race: pread");
        }
        let wanted = tag(0xff, page, 0) | 0x8000_0000;
        if u32::from_le_bytes(bytes) != wanted {
            report(0xff, page, u32::from_le_bytes(bytes), wanted);
        }
    }
    // SAFETY: the step's two mappings and descriptor.
    unsafe {
        let _ = libc::munmap(ptr::with_exposed_provenance_mut(private), pages * PAGE);
        let _ = libc::munmap(ptr::with_exposed_provenance_mut(shared), pages * PAGE);
        let _ = libc::close(fd);
    }
}

/// A shared mapping of `pages` pages: anonymous when `fd` is -1, else of `fd`.
fn shared_map(pages: usize, fd: i32, fail: fn(&str) -> !) -> usize {
    let flags = if fd < 0 {
        libc::MAP_SHARED | libc::MAP_ANONYMOUS
    } else {
        libc::MAP_SHARED
    };
    // SAFETY: a fresh address and this program's descriptor, or none.
    let at = unsafe {
        libc::mmap(
            ptr::null_mut(),
            pages * PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            flags,
            fd,
            0,
        )
    };
    if at == libc::MAP_FAILED {
        fail("fault-race: shared mmap");
    }
    at.addr()
}

/// A child's word, never a writer's: its high byte is 0xc0 plus the fork's number.
fn child_tag(fork: usize, page: usize) -> u32 {
    0xc000_0000 | ((fork as u32) << 20) | page as u32
}

/// The shared-fork step's child: its private copy is plausible, then it is its
/// own to overwrite; then it answers in the mailboxes. Only memory, `_exit`.
fn child_writes(private: usize, mailboxes: [usize; 2], fork: usize, generation: u32) -> ! {
    for writer in 0..WRITERS {
        for page in 0..SLICE {
            let at = private + writer * SLICE * PAGE;
            if !plausible(word(at, page), writer, page, generation) {
                // SAFETY: ending the child.
                unsafe { libc::_exit(3) };
            }
            put(at, page, child_tag(fork, page));
        }
    }
    for writer in 0..WRITERS {
        for page in 0..SLICE {
            if word(private + writer * SLICE * PAGE, page) != child_tag(fork, page) {
                // SAFETY: ending the child.
                unsafe { libc::_exit(4) };
            }
        }
    }
    for mailbox in mailboxes {
        // Word `fork` of the mailbox page.
        put(mailbox + fork * 4, 0, child_tag(fork, 0));
    }
    // SAFETY: ending the child.
    unsafe { libc::_exit(0) };
}

/// A writer's pass in the shared-fork step: its private, shared and `memfd`
/// pages written with `generation`'s tag and read back; a child's word in any of
/// them is reported, as is anything but the tag.
fn write_three(slices: [usize; 3], writer: usize, generation: u32) {
    for slice in slices {
        for page in 0..SLICE {
            put(slice, page, tag(writer, page, generation));
        }
    }
    for slice in slices {
        for page in 0..SLICE {
            let wanted = tag(writer, page, generation);
            if word(slice, page) != wanted {
                report(writer, page, word(slice, page), wanted);
            }
        }
    }
}

/// The shared-fork step.
fn shared_fork_round(fail: fn(&str) -> !) {
    const GENERATIONS: u32 = 24;
    const SHARED_FORKS: usize = 12;
    let pages = WRITERS * SLICE;
    let private = anonymous(pages, fail).addr();
    let shared = shared_map(pages + 1, -1, fail);
    // SAFETY: a name the kernel copies.
    let fd = unsafe { libc::memfd_create(c"fault-race-shared".as_ptr(), 0) };
    // SAFETY: a descriptor just made, or -1, which fails.
    if fd < 0 || unsafe { libc::ftruncate(fd, ((pages + 1) * PAGE) as libc::off_t) } != 0 {
        fail("fault-race: memfd");
    }
    let memfd = shared_map(pages + 1, fd, fail);
    let mailboxes = [shared + pages * PAGE, memfd + pages * PAGE];
    let done = AtomicUsize::new(0);
    let start = Barrier::new(WRITERS + 1);
    std::thread::scope(|scope| {
        for writer in 0..WRITERS {
            let (start, done) = (&start, &done);
            let _ = scope.spawn(move || {
                let at = writer * SLICE * PAGE;
                let _ = start.wait();
                for generation in 1..=GENERATIONS {
                    write_three([private + at, shared + at, memfd + at], writer, generation);
                }
                let _ = done.fetch_add(1, Ordering::AcqRel);
            });
        }
        let _ = start.wait();
        for fork in 0..SHARED_FORKS {
            // SAFETY: the child only reads and writes memory and exits.
            let pid = unsafe { libc::fork() };
            if pid == 0 {
                child_writes(private, mailboxes, fork, GENERATIONS);
            }
            if pid < 0 {
                fail("fault-race: fork");
            }
            let mut status = 0_i32;
            // SAFETY: `status` is this frame's.
            if unsafe { libc::waitpid(pid, &raw mut status, 0) } != pid {
                fail("fault-race: waitpid");
            }
            match (status & 0x7f, status >> 8 & 0xff) {
                (0, 0) => {}
                (0, 3) => fail("fault-race: a shared-fork child read a word nobody wrote"),
                (0, 4) => fail("fault-race: a shared-fork child lost its own write"),
                _ => fail("fault-race: a shared-fork child ended some other way"),
            }
            for mailbox in mailboxes {
                if word(mailbox + fork * 4, 0) != child_tag(fork, 0) {
                    report(0xfe, fork, word(mailbox + fork * 4, 0), child_tag(fork, 0));
                }
            }
        }
    });
    if done.load(Ordering::Acquire) != WRITERS {
        fail("fault-race: a shared-fork writer did not finish");
    }
    for writer in 0..WRITERS {
        for page in 0..SLICE {
            let wanted = tag(writer, page, GENERATIONS);
            let at = writer * SLICE * PAGE;
            for slice in [private, shared, memfd] {
                if word(slice + at, page) != wanted {
                    report(writer, page, word(slice + at, page), wanted);
                }
            }
        }
    }
    // SAFETY: the step's mappings and descriptor, which every thread is done with.
    unsafe {
        let _ = libc::munmap(ptr::with_exposed_provenance_mut(private), pages * PAGE);
        let _ = libc::munmap(ptr::with_exposed_provenance_mut(shared), (pages + 1) * PAGE);
        let _ = libc::munmap(ptr::with_exposed_provenance_mut(memfd), (pages + 1) * PAGE);
        let _ = libc::close(fd);
    }
}

/// The step.
pub fn check_fault_race(fail: fn(&str) -> !) {
    for generation in 1..=ROUNDS {
        fresh_round(generation, fail);
        check_reports("fresh", fail);
    }
    fork_round(fail);
    check_reports("fork", fail);
    file_round(fail);
    check_reports("file", fail);
    shared_fork_round(fail);
    check_reports("shared-fork", fail);
}
