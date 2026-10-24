//! Stage 8's self-check of shared file mappings.
//!
//! A shared mapping of a tmpfs file maps the file's own pages, the VMO `read`
//! copies out of, so a write through the mapping is what the file's next read
//! returns and a write to the file is what the mapping shows, with no copy in
//! between to go stale. The check drives that as a program would, from a
//! process built for it: `openat` and `mmap` go in by the handlers, the
//! mapping is read and written through user memory so that every touch takes
//! the fault path, and the file is read and written through its open
//! description.
//!
//! Around that, `mmap` must refuse a file as Linux does, `msync` must answer
//! as Linux does, `/proc/<pid>/maps` must name the mapping by the file's path,
//! and a truncation must take the pages past the new end away from the
//! mapping, which a copy from the kernel then finds refused.
//!
//! A private mapping of the same file must show the file until it writes a
//! page, and then keep that page to itself: a write through it, a `read` into
//! it, and a write by a `fork` child never reach the file or a shared mapping,
//! while a write to the file still shows through the pages it has not copied.
//! A truncation takes its copies past the cut too, so the file grown back
//! shows zeros there, as on Linux. And a program writes a page of one from user
//! mode that it has only read so far, which must copy rather than fault on the
//! file's read-only page forever. Like the other stage 8 checks it runs twice
//! and counts frames across the second run, in a frame window.

use alloc::format;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_elf::Class;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    AT_FDCWD, MAP_ANONYMOUS, MAP_FIXED, MAP_PRIVATE, MAP_SHARED, MS_ASYNC, MS_SYNC, O_CREAT,
    O_RDONLY, O_RDWR, O_TRUNC, PROT_EXEC, PROT_READ, PROT_WRITE, SEEK_SET,
};
use ferrix_vfs::{Errno, OpenFile, OpenFlags};
use ferrix_vma::VmaFlags;

use crate::arch;
use crate::fs;
use crate::mm;
use crate::syscall::check as syscall_check;
use crate::syscall::memory::{self, MmapRequest, OffsetUnit};
use crate::syscall::process::{self, Process};
use crate::syscall::{fd, image, uaccess};
use crate::user::space::{FileMapping, FilePlace};
use crate::user::vmo::Vmo;

/// The working directory, as `openat` takes it.
const CWD: u64 = AT_FDCWD as i64 as u64;

/// The file the check maps, with the terminator a program's copy carries.
const PATH: &[u8] = b"/tmp/ferrix-mmap-check\0";
/// Where [`PATH`] is staged in the check's page.
const AT_PATH: u64 = 0;
/// A directory, which has nothing to map.
const TMP: &[u8] = b"/tmp\0";
/// Where [`TMP`] is staged.
const AT_TMP: u64 = 64;

/// How many pages the shared mapping covers: the file's three, and one past
/// its end.
const MAPPED_PAGES: u64 = 4;

/// Bytes written through the mapping and looked for in the file.
const THROUGH_MAP: &[u8] = b"written through a shared mapping";
/// Bytes written to the file and looked for in the mapping.
const THROUGH_FILE: &[u8] = b"written to the file";

/// Bytes a kernel write puts into a private mapping, and looks for nowhere
/// else.
const PRIVATELY: &[u8] = b"written into a private mapping";
/// Where the private mapping is written: page 1, clear of the shared checks'
/// bytes.
const PRIVATE_AT: u64 = PAGE_SIZE + 2000;
/// Where a `read` into the private mapping lands: page 0.
const READ_INTO: u64 = 300;
/// Bytes written to the file behind the private mapping.
const BEHIND: &[u8] = b"written to the file behind a private mapping";
/// Where [`BEHIND`] goes in a page the private mapping never copies: page 2.
const UNCOPIED_AT: u64 = 2 * PAGE_SIZE + 10;
/// Where [`BEHIND`] goes in a page the private mapping has copied: page 0.
const COPIED_AT: u64 = 600;
/// What a `fork` child writes into its copy of the private mapping.
const CHILD_WRITES: &[u8] = b"a fork child's write";
/// Where it writes it: page 1, which the parent copied before the fork.
const CHILD_AT: u64 = PAGE_SIZE + 3000;

/// Where the loader-shaped mapping goes. It is well clear of the scratch
/// mappings this check asks the kernel to place, and is a page boundary on all
/// architectures.
const LOADER_BASE: u64 = 0x7100_0000;

/// Where the reverse map check's program touches memory: page 0 under test,
/// page 1 its control page. Written into each architecture's program.
const PROGRAM_BASE: u64 = 0x7000_0000;
/// The program's command: read page 0 over and over.
const WARM: u32 = 0;
/// Read page 0, write the marker, read it back, report both, answer 2.
const PROBE: u32 = 2;
/// Exit with status 0.
const EXIT: u32 = 4;
/// What the program writes, for role 0.
const MARK: u32 = 0x5EED_0000;
/// How long the program reads the file's page before it is told to write.
const WARM_NANOS: u64 = 10_000_000;
/// How long the program has to answer, and to exit.
const ANSWER_NANOS: u64 = 10_000_000_000;

/// What the check measured, for the boot log.
#[derive(Debug, Clone, Copy)]
pub struct Report {
    /// Bytes compared between the mapping and the file, both ways.
    pub bytes: u64,
    /// Pages of the mapping a truncation took away.
    pub cut: u64,
    /// Pages a private mapping copied and kept from the file.
    pub copied: u64,
    /// Frames the second run cost. Zero, or a mapping kept a file's pages.
    pub leaked: i64,
}

/// Run it twice, measured on the second: the first grows the heap to where it
/// stays.
pub fn run() -> Result<Report, &'static str> {
    let process =
        process::new_for_check().map_err(|_| "could not make a process for the mmap check")?;
    let _warm = check_once(&process)?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let window = mm::FrameWindow::open();
    let (bytes, cut, copied) = check_once(&process)?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let leaked = window.kept();
    if leaked != 0 {
        window.report("mmap");
        crate::console::println!("  mmap     {leaked} frames across the second run");
    }
    if leaked < 0 {
        return Err(
            "the free frame count rose across the mmap check: something outside it freed frames in the window",
        );
    }
    if leaked > 0 {
        return Err("the mmap check kept frames it did not give back");
    }
    Ok(Report {
        bytes,
        cut,
        copied,
        leaked,
    })
}

/// A byte of the file's contents, by its offset.
fn pattern(at: u64) -> u8 {
    (at.wrapping_mul(29) ^ (at >> 12)) as u8
}

/// An `mmap` of `len` bytes of `descriptor` from its start.
fn request(descriptor: i32, len: u64, prot: u32, flags: u32) -> MmapRequest {
    MmapRequest {
        addr: 0,
        len,
        prot,
        flags,
        fd: i64::from(descriptor),
        offset: 0,
        unit: OffsetUnit::Bytes,
    }
}

/// A page of a file at the address a loader chose for it.
fn fixed_request(descriptor: i32, addr: u64, offset: u64, prot: u32) -> MmapRequest {
    MmapRequest {
        addr,
        len: PAGE_SIZE,
        prot,
        flags: MAP_PRIVATE | MAP_FIXED,
        fd: i64::from(descriptor),
        offset,
        unit: OffsetUnit::Bytes,
    }
}

/// Make `call` by its number, as a program on this architecture would.
fn by_number(process: &Process, call: Syscall, args: [u64; 6]) -> Result<usize, Errno> {
    syscall_check::call_by_number(process, call, args)
}

/// A descriptor a handler returned.
fn descriptor(got: Result<usize, Errno>, what: &'static str) -> Result<i32, &'static str> {
    got.ok().and_then(|fd| i32::try_from(fd).ok()).ok_or(what)
}

/// An address a handler returned.
fn address(got: Result<usize, Errno>, what: &'static str) -> Result<u64, &'static str> {
    got.ok().and_then(|at| u64::try_from(at).ok()).ok_or(what)
}

/// Open [`PATH`] with `flags` by number, from the staged page.
fn open(process: &Process, page: u64, flags: u32) -> Result<i32, &'static str> {
    descriptor(
        by_number(
            process,
            Syscall::Openat,
            [CWD, page + AT_PATH, u64::from(flags), 0o600, 0, 0],
        ),
        "the mmap check's file would not open",
    )
}

/// One run: stage the paths, check, and clean up whatever happened.
fn check_once(process: &Process) -> Result<(u64, u64, u64), &'static str> {
    let page = address(
        memory::sys_mmap(
            process,
            &request(
                -1,
                PAGE_SIZE,
                PROT_READ | PROT_WRITE,
                MAP_ANONYMOUS | MAP_PRIVATE,
            ),
        ),
        "a page for the mmap check was refused",
    )?;
    let outcome = [(AT_PATH, PATH), (AT_TMP, TMP)]
        .into_iter()
        .try_for_each(|(at, bytes)| uaccess::copy_to_user(process.space(), page + at, bytes))
        .map_err(|_| "could not stage the mmap check")
        .and_then(|()| check_a_shared_file_mapping(process, page));

    for descriptor in 3..32 {
        let _ = fd::sys_close(process, descriptor);
    }
    let _ = memory::sys_munmap(process, page, PAGE_SIZE);
    let ns = fs::namespace();
    let _ = ns.unlink(
        &ns.context(),
        None,
        PATH.strip_suffix(b"\0").unwrap_or(PATH),
    );
    outcome
}

/// Steps 1 to 6, on a file of two pages and a hundred bytes.
fn check_a_shared_file_mapping(
    process: &Process,
    page: u64,
) -> Result<(u64, u64, u64), &'static str> {
    let rw = open(process, page, O_RDWR | O_CREAT | O_TRUNC)?;
    let file = fd::file(process, rw).map_err(|_| "the mmap check's descriptor names nothing")?;
    let len = 2 * PAGE_SIZE + 100;
    let data: Vec<u8> = (0..len).map(pattern).collect();
    if file.write_at(0, &data) != Ok(data.len()) {
        return Err("the mmap check's file was not written whole");
    }

    check_mmap_refuses_as_linux_does(process, page)?;
    let at = address(
        memory::sys_mmap(
            process,
            &request(
                rw,
                MAPPED_PAGES * PAGE_SIZE,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
            ),
        ),
        "a shared mapping of a tmpfs file was refused",
    )?;
    let private = match address(
        memory::sys_mmap(
            process,
            &request(
                rw,
                MAPPED_PAGES * PAGE_SIZE,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE,
            ),
        ),
        "a private mapping of a tmpfs file was refused",
    ) {
        Ok(private) => private,
        Err(why) => {
            let _ = memory::sys_munmap(process, at, MAPPED_PAGES * PAGE_SIZE);
            return Err(why);
        }
    };
    let object = file
        .inode()
        .mapping()
        .and_then(|object| object.downcast::<Vmo>().ok())
        .ok_or("a mapped tmpfs file has no object")?;
    let outcome = check_the_mapping_is_the_file(process, &file, at, &data)
        .and_then(|bytes| check_msync_and_maps(process, at).map(|()| bytes))
        .and_then(|bytes| check_the_loader_mapping_pattern(process, rw, &file).map(|()| bytes))
        .and_then(|bytes| {
            check_a_private_mapping_copies(process, &file, rw, at, private)
                .map(|copied| (bytes, copied))
        })
        .and_then(|(bytes, copied)| {
            check_a_truncation_reaches_the_mapping(process, &file, at, private)
                .map(|cut| (bytes, cut, copied))
        })
        .and_then(|counts| check_a_program_writes_privately(&file).map(|()| counts));
    let committed = object.committed();
    let unmapped = memory::sys_munmap(process, private, MAPPED_PAGES * PAGE_SIZE);
    if memory::sys_munmap(process, at, MAPPED_PAGES * PAGE_SIZE) != Ok(0) || unmapped != Ok(0) {
        return Err("a file mapping would not unmap");
    }
    if outcome.is_ok() && object.committed() != committed {
        return Err("unmapping a private file mapping took pages out of the file");
    }
    outcome
}

/// The two file mappings a dynamic loader needs before it enters a program.
///
/// Its text is fixed, file-backed and executable. Its writable segment holds
/// the GOT and the `PT_GNU_RELRO` span while relocations run; then `mprotect`
/// takes write access away. This makes all three flags matter together: a
/// test of an anonymous fixed mapping, or of `mprotect` after no file mapping,
/// would leave the loader's actual path unexercised.
///
/// Verifies: L.user.97
fn check_the_loader_mapping_pattern(
    process: &Process,
    descriptor: i32,
    file: &OpenFile,
) -> Result<(), &'static str> {
    let text = address(
        memory::sys_mmap(
            process,
            &fixed_request(descriptor, LOADER_BASE, 0, PROT_READ | PROT_EXEC),
        ),
        "a loader's fixed executable file mapping was refused",
    )?;
    if text != LOADER_BASE {
        return Err("a loader's executable mapping did not land at its fixed address");
    }
    let relro = match address(
        memory::sys_mmap(
            process,
            &fixed_request(
                descriptor,
                LOADER_BASE + PAGE_SIZE,
                PAGE_SIZE,
                PROT_READ | PROT_WRITE,
            ),
        ),
        "a loader's fixed writable file mapping was refused",
    ) {
        Ok(relro) => relro,
        Err(problem) => {
            let _ = memory::sys_munmap(process, text, PAGE_SIZE);
            return Err(problem);
        }
    };

    let outcome = (|| {
        if relro != LOADER_BASE + PAGE_SIZE {
            return Err("a loader's writable mapping did not land at its fixed address");
        }
        let mut expected = vec![0_u8; PAGE_SIZE as usize];
        if file.read_at(0, &mut expected) != Ok(expected.len()) {
            return Err("the loader mapping fixture's first page could not be read");
        }
        let mut read = vec![0_u8; expected.len()];
        uaccess::copy_from_user(process.space(), text, &mut read)
            .map_err(|_| "a loader's executable mapping could not be read")?;
        if read != expected {
            return Err("a loader's executable mapping does not show its file's bytes");
        }
        if uaccess::copy_to_user(process.space(), text, b"x").is_ok() {
            return Err("a loader's executable mapping was writable");
        }

        const RELOCATED: &[u8] = b"relocated relro";
        uaccess::copy_to_user(process.space(), relro + 32, RELOCATED)
            .map_err(|_| "a loader could not write its RELRO span before mprotect")?;
        if memory::sys_mprotect(process, relro, PAGE_SIZE, PROT_READ) != Ok(0) {
            return Err("mprotect of a loader's RELRO span was refused");
        }
        let mut kept = [0_u8; RELOCATED.len()];
        uaccess::copy_from_user(process.space(), relro + 32, &mut kept)
            .map_err(|_| "a loader's RELRO span could not be read after mprotect")?;
        if kept != *RELOCATED {
            return Err("mprotect of a loader's RELRO span changed its relocations");
        }
        if uaccess::copy_to_user(process.space(), relro + 32, b"x").is_ok() {
            return Err("a loader's RELRO span stayed writable after mprotect");
        }
        Ok(())
    })();
    let relro_unmapped = memory::sys_munmap(process, relro, PAGE_SIZE);
    let text_unmapped = memory::sys_munmap(process, text, PAGE_SIZE);
    if outcome.is_ok() && (relro_unmapped != Ok(0) || text_unmapped != Ok(0)) {
        return Err("a loader-shaped file mapping would not unmap");
    }
    outcome
}

/// Step 1: `mmap` of a file refuses in Linux's order and for Linux's reasons.
fn check_mmap_refuses_as_linux_does(process: &Process, page: u64) -> Result<(), &'static str> {
    let refused = |got: Result<usize, Errno>, errno: Errno, what| {
        if got == Err(errno) { Ok(()) } else { Err(what) }
    };
    refused(
        memory::sys_mmap(process, &request(29, PAGE_SIZE, PROT_READ, MAP_SHARED)),
        Errno::EBADF,
        "mmap of a descriptor that names nothing was not EBADF",
    )?;
    let directory = descriptor(
        by_number(
            process,
            Syscall::Openat,
            [CWD, page + AT_TMP, u64::from(O_RDONLY), 0, 0, 0],
        ),
        "/tmp would not open",
    )?;
    refused(
        memory::sys_mmap(
            process,
            &request(directory, PAGE_SIZE, PROT_READ, MAP_SHARED),
        ),
        Errno::ENODEV,
        "mmap of a directory was not ENODEV",
    )?;
    let read_only = open(process, page, O_RDONLY)?;
    refused(
        memory::sys_mmap(
            process,
            &request(read_only, PAGE_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED),
        ),
        Errno::EACCES,
        "a writable shared mapping of a file opened read-only was not EACCES",
    )?;
    let copy = address(
        memory::sys_mmap(
            process,
            &request(read_only, PAGE_SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE),
        ),
        "a writable private mapping of a file opened read-only was refused, which Linux allows",
    )?;
    if memory::sys_munmap(process, copy, PAGE_SIZE) != Ok(0) {
        return Err("a private file mapping would not unmap");
    }
    let readable = address(
        memory::sys_mmap(
            process,
            &request(read_only, PAGE_SIZE, PROT_READ, MAP_SHARED),
        ),
        "a read-only shared mapping of a file opened read-only was refused",
    )?;
    if memory::sys_munmap(process, readable, PAGE_SIZE) != Ok(0) {
        return Err("a read-only file mapping would not unmap");
    }
    Ok(())
}

/// Step 2: the mapping shows the file, a write through it reaches the file,
/// and a write to the file reaches it. Returns the bytes compared.
///
/// Verifies: L.user.93
fn check_the_mapping_is_the_file(
    process: &Process,
    file: &OpenFile,
    at: u64,
    data: &[u8],
) -> Result<u64, &'static str> {
    let space = process.space();
    let mut shown = vec![0_u8; data.len()];
    uaccess::copy_from_user(space, at, &mut shown)
        .map_err(|_| "a shared file mapping could not be read")?;
    if shown != data {
        return Err("a shared file mapping does not show the file's bytes");
    }
    let tail_len = usize::try_from(3 * PAGE_SIZE).unwrap_or(0) - data.len();
    let mut tail = vec![0xff_u8; tail_len];
    uaccess::copy_from_user(space, at + data.len() as u64, &mut tail)
        .map_err(|_| "the rest of a file's last page could not be read through its mapping")?;
    if tail.iter().any(|&byte| byte != 0) {
        return Err("the rest of a file's last page is not zeros in its mapping");
    }

    uaccess::copy_to_user(space, at + PAGE_SIZE + 7, THROUGH_MAP)
        .map_err(|_| "a write through a shared file mapping was refused")?;
    let mut back = vec![0_u8; THROUGH_MAP.len()];
    if file.read_at(PAGE_SIZE + 7, &mut back) != Ok(back.len()) || back != THROUGH_MAP {
        return Err("a write through a shared mapping was not what the file read back");
    }

    if file.write_at(50, THROUGH_FILE) != Ok(THROUGH_FILE.len()) {
        return Err("a write to a mapped file failed");
    }
    let mut seen = vec![0_u8; THROUGH_FILE.len()];
    uaccess::copy_from_user(space, at + 50, &mut seen)
        .map_err(|_| "a mapped file's page could not be read again")?;
    if seen != THROUGH_FILE {
        return Err("a write to a mapped file was not what its mapping showed");
    }
    Ok((data.len() + tail.len() + THROUGH_MAP.len() + THROUGH_FILE.len()) as u64)
}

/// Step 3: `msync` answers as Linux does, and `/proc/<pid>/maps` names the
/// mapping by its file.
///
/// Verifies: L.user.98
fn check_msync_and_maps(process: &Process, at: u64) -> Result<(), &'static str> {
    let len = MAPPED_PAGES * PAGE_SIZE;
    if memory::sys_msync(process, at, len, MS_SYNC) != Ok(0)
        || memory::sys_msync(process, at, len, MS_SYNC | MS_ASYNC) != Err(Errno::EINVAL)
        || memory::sys_msync(process, at + 1, PAGE_SIZE, MS_SYNC) != Err(Errno::EINVAL)
    {
        return Err("msync did not answer a whole mapping, or refuse bad flags, as Linux does");
    }
    let gone = address(
        memory::sys_mmap(
            process,
            &request(-1, PAGE_SIZE, PROT_READ, MAP_ANONYMOUS | MAP_PRIVATE),
        ),
        "a page for msync's refusal was refused",
    )?;
    let _ = memory::sys_munmap(process, gone, PAGE_SIZE);
    if memory::sys_msync(process, gone, PAGE_SIZE, MS_SYNC) != Err(Errno::ENOMEM) {
        return Err("msync of an unmapped range was not ENOMEM");
    }

    let ns = fs::namespace();
    let maps_path = format!("/proc/{}/maps", process.pid());
    let read = OpenFlags {
        read: true,
        ..OpenFlags::default()
    };
    let maps = ns
        .open(&ns.context(), None, maps_path.as_bytes(), &read, 0)
        .map_err(|_| "the check process's maps would not open")?;
    let mut text = Vec::new();
    let mut chunk = [0_u8; 256];
    loop {
        match maps.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => text.extend_from_slice(chunk.get(..count).unwrap_or_default()),
            Err(_) => return Err("the check process's maps would not read"),
        }
    }
    let name = PATH.strip_suffix(b"\0").unwrap_or(PATH);
    let named = text
        .split(|&byte| byte == b'\n')
        .any(|line| line.ends_with(name) && line.windows(5).any(|w| w == b"rw-s "));
    if !named {
        return Err("/proc/<pid>/maps does not name a shared file mapping by its file");
    }
    Ok(())
}

/// Step 5: a truncation takes the pages past the new end away from both
/// mappings, the private mapping's copies included, and a grow shows zeros
/// where they were. Returns the pages cut from the shared mapping.
///
/// Verifies: L.user.39, L.user.40, L.user.92
fn check_a_truncation_reaches_the_mapping(
    process: &Process,
    file: &OpenFile,
    at: u64,
    private: u64,
) -> Result<u64, &'static str> {
    let space = process.space();
    let mut byte = [0_u8; 1];
    file.set_len(PAGE_SIZE)
        .map_err(|_| "a mapped file would not truncate")?;
    if uaccess::copy_from_user(space, at, &mut byte).is_err() {
        return Err("a mapped file's first page was refused after a cut behind it");
    }
    let cut = (1..MAPPED_PAGES)
        .filter(|&index| uaccess::copy_from_user(space, at + index * PAGE_SIZE, &mut byte).is_err())
        .count() as u64;
    if cut != MAPPED_PAGES - 1 {
        return Err("a page past a mapped file's new end could still be read through the mapping");
    }
    let private_cut = (1..MAPPED_PAGES)
        .filter(|&index| {
            uaccess::copy_from_user(space, private + index * PAGE_SIZE, &mut byte).is_err()
        })
        .count() as u64;
    if private_cut != MAPPED_PAGES - 1 {
        return Err(
            "a page past a privately mapped file's new end, copied or not, could still be read",
        );
    }

    file.set_len(2 * PAGE_SIZE)
        .map_err(|_| "a mapped file would not grow again")?;
    let mut regrown = vec![0xff_u8; usize::try_from(PAGE_SIZE).unwrap_or(0)];
    uaccess::copy_from_user(space, at + PAGE_SIZE, &mut regrown)
        .map_err(|_| "a regrown page could not be read through the mapping")?;
    if regrown.iter().any(|&b| b != 0) {
        return Err("a page cut and grown again showed its old bytes through the mapping");
    }
    uaccess::copy_from_user(space, private + PAGE_SIZE, &mut regrown)
        .map_err(|_| "a regrown page could not be read through a private mapping")?;
    if regrown.iter().any(|&b| b != 0) {
        return Err(
            "a private mapping still showed its copy of a page cut and grown again, which Linux drops",
        );
    }
    Ok(cut)
}

/// Step 4: a private mapping shows the file until it writes a page, and keeps
/// what it writes: a write through it and a `read` into it reach neither the
/// file nor the shared mapping `shared`, while a write to the file still shows
/// through a page it has not copied and not through one it has. Returns the
/// pages it copied.
///
/// Verifies: L.user.94
fn check_a_private_mapping_copies(
    process: &Process,
    file: &OpenFile,
    descriptor: i32,
    shared: u64,
    private: u64,
) -> Result<u64, &'static str> {
    let space = process.space();
    let in_file = |at: u64, len: usize| -> Result<Vec<u8>, &'static str> {
        let mut bytes = vec![0_u8; len];
        if file.read_at(at, &mut bytes) != Ok(len) {
            return Err("a privately mapped file could not be read");
        }
        Ok(bytes)
    };
    let shown = |base: u64, at: u64, len: usize| -> Result<Vec<u8>, &'static str> {
        let mut bytes = vec![0_u8; len];
        uaccess::copy_from_user(space, base + at, &mut bytes)
            .map_err(|_| "a file mapping could not be read")?;
        Ok(bytes)
    };
    let page = usize::try_from(PAGE_SIZE).map_err(|_| "the page size does not fit")?;
    if shown(private, 0, page)? != in_file(0, page)? {
        return Err("a private file mapping does not show the file's bytes");
    }

    let before = in_file(PRIVATE_AT, PRIVATELY.len())?;
    uaccess::copy_to_user(space, private + PRIVATE_AT, PRIVATELY)
        .map_err(|_| "a write into a private file mapping was refused")?;
    if shown(private, PRIVATE_AT, PRIVATELY.len())? != PRIVATELY {
        return Err("a write into a private file mapping is not what the mapping shows");
    }
    if in_file(PRIVATE_AT, PRIVATELY.len())? != before
        || shown(shared, PRIVATE_AT, PRIVATELY.len())? != before
    {
        return Err("a write into a private mapping reached the file or a shared mapping of it");
    }

    // `read` into page 0, by number: the copy into user memory is what must
    // copy the page, not write the file's.
    let len = THROUGH_MAP.len();
    let before = in_file(READ_INTO, len)?;
    let fd = u64::from(descriptor.cast_unsigned());
    let offset = PAGE_SIZE + 7;
    if by_number(
        process,
        Syscall::Lseek,
        [fd, offset, u64::from(SEEK_SET), 0, 0, 0],
    ) != Ok(offset as usize)
        || by_number(
            process,
            Syscall::Read,
            [fd, private + READ_INTO, len as u64, 0, 0, 0],
        ) != Ok(len)
    {
        return Err("read into a private file mapping failed");
    }
    if shown(private, READ_INTO, len)? != THROUGH_MAP {
        return Err("read into a private file mapping did not land in it");
    }
    if in_file(READ_INTO, len)? != before || shown(shared, READ_INTO, len)? != before {
        return Err("read into a private mapping wrote the file or a shared mapping of it");
    }

    if file.write_at(UNCOPIED_AT, BEHIND) != Ok(BEHIND.len())
        || file.write_at(COPIED_AT, BEHIND) != Ok(BEHIND.len())
    {
        return Err("a write to a privately mapped file failed");
    }
    if shown(private, UNCOPIED_AT, BEHIND.len())? != BEHIND {
        return Err(
            "a write to the file did not show through a page its private mapping never copied",
        );
    }
    if shown(private, COPIED_AT, BEHIND.len())? == BEHIND {
        return Err("a write to the file showed through a page its private mapping had copied");
    }
    check_a_fork_keeps_private_copies_apart(process, file, private)?;
    Ok(2)
}

/// Step 4, across a `fork`: the child inherits the parent's copies, and its
/// own write into one reaches neither the parent nor the file.
///
/// Verifies: L.user.95
fn check_a_fork_keeps_private_copies_apart(
    process: &Process,
    file: &OpenFile,
    private: u64,
) -> Result<(), &'static str> {
    let space = process.space();
    let child = space
        .fork()
        .map_err(|_| "the check process's space would not fork")?;
    let mut inherited = vec![0_u8; PRIVATELY.len()];
    uaccess::copy_from_user(&child, private + PRIVATE_AT, &mut inherited)
        .map_err(|_| "a fork child could not read its private file mapping")?;
    if inherited != PRIVATELY {
        return Err("a fork child did not inherit its parent's private copy");
    }
    uaccess::copy_to_user(&child, private + CHILD_AT, CHILD_WRITES)
        .map_err(|_| "a fork child's write into its private file mapping was refused")?;
    let mut parent = vec![0_u8; CHILD_WRITES.len()];
    uaccess::copy_from_user(space, private + CHILD_AT, &mut parent)
        .map_err(|_| "the parent's private file mapping could not be read after a fork")?;
    let mut in_file = vec![0_u8; CHILD_WRITES.len()];
    if file.read_at(CHILD_AT, &mut in_file) != Ok(in_file.len()) {
        return Err("a privately mapped file could not be read after a fork");
    }
    if parent == CHILD_WRITES || in_file == CHILD_WRITES {
        return Err(
            "a fork child's write into a private file mapping reached its parent or the file",
        );
    }
    Ok(())
}

/// Step 6, from user mode: a program reads a page of a private file mapping,
/// so the file's page is present and read-only, and then writes it. The write
/// must copy the page and read back, not fault on the file's page forever, and
/// the file must not change. The reverse map check's program does both, role
/// 0, alone: page 0 at [`PROGRAM_BASE`] is the private mapping, page 1 its
/// control page.
///
/// Verifies: L.user.96
fn check_a_program_writes_privately(file: &Arc<OpenFile>) -> Result<(), &'static str> {
    if arch::USER_RMAP_PROGRAM.is_empty() {
        return Ok(());
    }
    let image = image::build_with(
        class_of_this_build(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_RMAP_PROGRAM,
    );
    let program = process::load(
        &image,
        &[b"/rmap"],
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "the private write program could not be loaded")?;
    let object = file
        .inode()
        .mapping()
        .and_then(|object| object.downcast::<Vmo>().ok())
        .ok_or("a mapped tmpfs file has no object")?;
    let space = program.space();
    let _ = space
        .map_file(
            FilePlace::Fixed(PROGRAM_BASE),
            PAGE_SIZE,
            VmaFlags::READ_WRITE,
            object,
            0,
            FileMapping {
                file: Arc::clone(file) as Arc<dyn Any + Send + Sync>,
                may_write: false,
            },
        )
        .map_err(|_| "a private file mapping could not be placed where the program writes")?;
    let shared = VmaFlags {
        shared: true,
        ..VmaFlags::READ_WRITE
    };
    let control_id = space
        .map_anonymous(PROGRAM_BASE + PAGE_SIZE, PAGE_SIZE, shared)
        .map_err(|_| "the program's control page could not be mapped")?;
    let control = space
        .object(control_id)
        .ok_or("the program's control page has no object")?;
    control
        .write_page(0, 0, &[WARM.to_le_bytes(), [0; 4], [0; 4], [0; 4]].concat())
        .map_err(|_| "the program's control page could not be written")?;
    let mut before = [0_u8; 4];
    if file.read_at(0, &mut before) != Ok(before.len()) {
        return Err("the program's file could not be read");
    }

    let task = process::start_on(&program, None)
        .map_err(|_| "the private write program could not be started")?;
    let outcome = probe(
        &control,
        &program,
        u32::from_le_bytes(before),
        "a write from user mode into a read page of a private file mapping never finished",
    )
    .and_then(|()| {
        // The page the program just copied is now in a region a fork marks
        // copy-on-write, and the child's going leaves this space its only
        // holder: the next write must remap it writable, not retry into the
        // read-only entry forever.
        let child = program
            .space()
            .fork()
            .map_err(|_| "the private write program's space would not fork")?;
        drop(child);
        probe(
            &control,
            &program,
            MARK,
            "a write from user mode into a private copy a fork shared, once the other side had gone, never finished",
        )
    });
    let _ = control.write_page(0, 0, &EXIT.to_le_bytes());
    let deadline = crate::timer::now_nanos().saturating_add(ANSWER_NANOS);
    if program.wait_for_exit(deadline).is_none() {
        process::kill(&program, 137);
    }
    while !task.is_dead() {
        if crate::timer::now_nanos() >= deadline.saturating_add(ANSWER_NANOS) {
            return Err("the private write program's task never died");
        }
        crate::sched::sleep_for(1_000_000);
    }
    // Its space gone before the check counts, so what it held comes back
    // inside this check's window: the last reference goes when the reaper
    // drops the task.
    let gone = Arc::downgrade(program.space());
    drop((task, program, control));
    while gone.strong_count() > 0 {
        if crate::timer::now_nanos() >= deadline.saturating_add(2 * ANSWER_NANOS) {
            return Err("the private write program kept its address space after it was reaped");
        }
        crate::sched::sleep_for(1_000_000);
    }
    outcome?;
    let mut after = [0_u8; 4];
    if file.read_at(0, &mut after) != Ok(after.len()) || after != before {
        return Err("a program's write into a private file mapping reached the file");
    }
    Ok(())
}

/// Tell the program to write page 0, and require it to answer within
/// [`ANSWER_NANOS`], or fail with `hang`, with `before` read before its write
/// and its marker read after.
fn probe(
    control: &Vmo,
    program: &Process,
    before: u32,
    hang: &'static str,
) -> Result<(), &'static str> {
    crate::sched::sleep_for(WARM_NANOS);
    let put = |at: usize, word: u32| {
        control
            .write_page(0, at, &word.to_le_bytes())
            .map_err(|_| "the program's control page could not be written")
    };
    let get = |at: usize| -> Result<u32, &'static str> {
        let mut word = [0_u8; 4];
        control
            .read_page(0, at, &mut word)
            .map_err(|_| "the program's control page could not be read")?;
        Ok(u32::from_le_bytes(word))
    };
    put(4, 0)?;
    put(0, PROBE)?;
    let deadline = crate::timer::now_nanos().saturating_add(ANSWER_NANOS);
    while get(4)? != PROBE {
        if program.is_terminated() {
            return Err("the program ended instead of writing its private file mapping");
        }
        if crate::timer::now_nanos() >= deadline {
            return Err(hang);
        }
        crate::sched::sleep_for(1_000_000);
    }
    if get(8)? != before {
        return Err("the program did not read what its private file mapping held before its write");
    }
    if get(12)? != MARK {
        return Err("the program's write into a private file mapping did not read back");
    }
    Ok(())
}

/// The ELF class a program for this build is.
fn class_of_this_build() -> Class {
    if size_of::<usize>() == 8 {
        Class::Elf64
    } else {
        Class::Elf32
    }
}
