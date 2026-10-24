//! Stage 8's self-check of `memfd_create` and its seals.
//!
//! A memfd is a tmpfs file nothing names, so it reads, writes, truncates and
//! maps as one does. What is new is the seals, and what they refuse is checked
//! in the order a Wayland compositor meets them: a file made to allow sealing
//! takes a shrink seal and refuses a truncation downwards; a grow seal refuses
//! extending it by truncation or by a write. A write seal is refused with
//! `EBUSY` while any shared mapping may write the file -- a `fork` child's
//! copy of one included -- and once it stands, writes, shared writable
//! mappings and `mprotect` to writable are refused, while a private mapping
//! still maps and keeps its writes to itself. `F_SEAL_SEAL` then refuses every
//! further seal.
//!
//! Beside the seals: an unknown flag and an overlong name are `EINVAL`, a file
//! made without `MFD_ALLOW_SEALING` carries `F_SEAL_SEAL`, and a read-only
//! shared mapping of a file opened read-only may not be made writable, the gap
//! the may-write accounting closed. Like the other stage 8 checks it runs
//! twice and counts frames across the second run, in a frame window.
//!
//! And the exec seal (Linux 6.3): `MFD_NOEXEC_SEAL` makes a file of mode 0666
//! carrying `F_SEAL_EXEC` and no `F_SEAL_SEAL`, which `execveat` refuses and
//! no `fchmod` gives an execute bit; `F_SEAL_EXEC` added to an executable one
//! brings the write seals with it; `vm.memfd_noexec` decides for a memfd that
//! names neither exec flag, refuses `MFD_EXEC` at 2, and cannot be set below a
//! pid namespace's parent.

use alloc::vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    AT_EMPTY_PATH, AT_FDCWD, F_ADD_SEALS, F_GET_SEALS, F_SEAL_EXEC, F_SEAL_FUTURE_WRITE,
    F_SEAL_GROW, F_SEAL_SEAL, F_SEAL_SHRINK, F_SEAL_WRITE, MAP_ANONYMOUS, MAP_PRIVATE, MAP_SHARED,
    MFD_ALLOW_SEALING, MFD_CLOEXEC, MFD_EXEC, MFD_NOEXEC_SEAL, O_CREAT, O_RDONLY, O_RDWR, O_TRUNC,
    O_WRONLY, PROT_READ, PROT_WRITE,
};
use ferrix_vfs::Errno;

use crate::fs;
use crate::mm;
use crate::syscall::check as syscall_check;
use crate::syscall::memory::{self, MmapRequest, OffsetUnit};
use crate::syscall::process::{self, Process};
use crate::syscall::{fd, uaccess};

/// A memfd's name, staged in the check's page.
const NAME: &[u8] = b"ferrix-seals\0";
/// Where [`NAME`] is staged.
const AT_NAME: u64 = 0;
/// A name one byte past the longest `memfd_create` takes, staged after it.
const AT_LONG: u64 = 64;
/// The longest name `memfd_create` takes.
const NAME_MAX_LEN: usize = 249;
/// A regular file for the read-only mapping check, staged in the page.
const PATH: &[u8] = b"/tmp/ferrix-memfd-check\0";
/// Where [`PATH`] is staged.
const AT_PATH: u64 = 512;
/// An empty path, for `execveat`'s `AT_EMPTY_PATH`: the page is zeroed.
const AT_EMPTY: u64 = 1024;
/// `vm.memfd_noexec`'s file, staged in the page.
const NOEXEC_SYSCTL: &[u8] = b"/proc/sys/vm/memfd_noexec\0";
/// Where [`NOEXEC_SYSCTL`] is staged.
const AT_SYSCTL: u64 = 1536;
/// A value written to it, staged in the page.
const AT_VALUE: u64 = 1600;
/// A seal bit no kernel defines.
const UNKNOWN_SEAL: u32 = 0x100;
/// `MFD_HUGETLB`, which has no hugetlbfs behind it here.
const MFD_HUGETLB: u32 = 0x0004;

/// What the check measured, for the boot log.
#[derive(Debug, Clone, Copy)]
pub struct Report {
    /// Seals added and then enforced.
    pub seals: u32,
    /// Calls refused as Linux refuses them.
    pub refusals: u32,
    /// Frames the second run cost.
    pub leaked: i64,
}

/// Run it twice, measured on the second.
pub fn run() -> Result<Report, &'static str> {
    let process =
        process::new_for_check().map_err(|_| "could not make a process for the memfd check")?;
    let _warm = check_once(&process)?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let window = mm::FrameWindow::open();
    let (seals, refusals) = check_once(&process)?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let leaked = window.kept();
    if leaked != 0 {
        window.report("memfd");
        crate::console::println!("  memfd    {leaked} frames across the second run");
        return Err("the memfd check did not give back every frame it took");
    }
    Ok(Report {
        seals,
        refusals,
        leaked,
    })
}

/// Make `call` by its number, as a program would.
fn by_number(process: &Process, call: Syscall, args: [u64; 6]) -> Result<usize, Errno> {
    syscall_check::call_by_number(process, call, args)
}

/// A descriptor a call returned, as a register.
fn descriptor(got: Result<usize, Errno>, what: &'static str) -> Result<u64, &'static str> {
    got.ok().map(|fd| fd as u64).ok_or(what)
}

/// Require `got` to be `Err(wanted)`, counting the refusal.
fn refused(
    got: Result<usize, Errno>,
    wanted: Errno,
    what: &'static str,
    refusals: &mut u32,
) -> Result<(), &'static str> {
    if got != Err(wanted) {
        return Err(what);
    }
    *refusals += 1;
    Ok(())
}

/// `fcntl(fd, F_ADD_SEALS, seals)`.
fn add_seals(process: &Process, fd: u64, seals: u32) -> Result<usize, Errno> {
    by_number(
        process,
        Syscall::Fcntl,
        [fd, u64::from(F_ADD_SEALS), u64::from(seals), 0, 0, 0],
    )
}

/// `fcntl(fd, F_GET_SEALS)`.
fn get_seals(process: &Process, fd: u64) -> Result<usize, Errno> {
    by_number(
        process,
        Syscall::Fcntl,
        [fd, u64::from(F_GET_SEALS), 0, 0, 0, 0],
    )
}

/// An `mmap` of `len` bytes of `fd` from its start.
fn map(process: &Process, fd: u64, len: u64, prot: u32, flags: u32) -> Result<usize, Errno> {
    memory::sys_mmap(
        process,
        &MmapRequest {
            addr: 0,
            len,
            prot,
            flags,
            fd: fd as i64,
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    )
}

/// One run: stage the names, check, and close and unlink whatever it made.
fn check_once(process: &Process) -> Result<(u32, u32), &'static str> {
    let page = memory::sys_mmap(
        process,
        &MmapRequest {
            addr: 0,
            len: PAGE_SIZE,
            prot: PROT_READ | PROT_WRITE,
            flags: MAP_ANONYMOUS | MAP_PRIVATE,
            fd: -1,
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    )
    .map(|at| at as u64)
    .map_err(|_| "a page for the memfd check was refused")?;
    let long = vec![b'n'; NAME_MAX_LEN + 1];
    let outcome = uaccess::copy_to_user(process.space(), page + AT_NAME, NAME)
        .and_then(|()| uaccess::copy_to_user(process.space(), page + AT_LONG, &long))
        .and_then(|()| {
            uaccess::copy_to_user(process.space(), page + AT_LONG + long.len() as u64, &[0])
        })
        .and_then(|()| uaccess::copy_to_user(process.space(), page + AT_PATH, PATH))
        .and_then(|()| uaccess::copy_to_user(process.space(), page + AT_SYSCTL, NOEXEC_SYSCTL))
        .map_err(|_| "could not stage the memfd check")
        .and_then(|()| check_memfd(process, page));

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

/// The whole check on one staged page. Returns the seals enforced and the
/// refusals counted.
fn check_memfd(process: &Process, page: u64) -> Result<(u32, u32), &'static str> {
    let mut refusals = 0;
    // First, so that each of their controls fails on its own message rather
    // than on a later check that leans on the same code.
    check_protect_keeps_a_mapping_shared(process, page)?;
    check_a_protected_read_only_mapping_stays_read_only(process, page, &mut refusals)?;
    let create = |name: u64, flags: u32| {
        by_number(
            process,
            Syscall::MemfdCreate,
            [page + name, u64::from(flags), 0, 0, 0, 0],
        )
    };
    refused(
        create(AT_NAME, MFD_HUGETLB),
        Errno::EINVAL,
        "memfd_create with MFD_HUGETLB was not EINVAL",
        &mut refusals,
    )?;
    refused(
        create(AT_LONG, 0),
        Errno::EINVAL,
        "memfd_create with a 250-byte name was not EINVAL",
        &mut refusals,
    )?;

    let plain = descriptor(
        create(AT_NAME, MFD_CLOEXEC),
        "memfd_create refused a plain memfd",
    )?;
    if get_seals(process, plain) != Ok(F_SEAL_SEAL as usize) {
        return Err("a memfd made without MFD_ALLOW_SEALING does not carry F_SEAL_SEAL");
    }
    refused(
        add_seals(process, plain, F_SEAL_SHRINK),
        Errno::EPERM,
        "a seal on a memfd that does not allow sealing was not EPERM",
        &mut refusals,
    )?;

    let sealed = descriptor(
        create(AT_NAME, MFD_ALLOW_SEALING),
        "memfd_create refused a sealable memfd",
    )?;
    if get_seals(process, sealed) != Ok(0) {
        return Err("a memfd made with MFD_ALLOW_SEALING starts with seals");
    }
    let seals = check_size_seals(process, sealed, &mut refusals)?
        + check_the_write_seal(process, sealed, &mut refusals)?;
    refused(
        add_seals(process, sealed, UNKNOWN_SEAL),
        Errno::EINVAL,
        "an unknown seal was not EINVAL",
        &mut refusals,
    )?;
    if add_seals(process, sealed, F_SEAL_SEAL) != Ok(0) {
        return Err("F_SEAL_SEAL was refused");
    }
    refused(
        add_seals(process, sealed, F_SEAL_FUTURE_WRITE),
        Errno::EPERM,
        "a seal after F_SEAL_SEAL was not EPERM",
        &mut refusals,
    )?;
    check_a_read_only_mapping_stays_read_only(process, page, &mut refusals)?;
    let exec_seals = check_the_exec_seal(process, page, &mut refusals)?;
    Ok((seals + 1 + exec_seals, refusals))
}

/// The shrink and grow seals, on a memfd of three pages.
fn check_size_seals(
    process: &Process,
    sealed: u64,
    refusals: &mut u32,
) -> Result<u32, &'static str> {
    let truncate = |len: u64| by_number(process, Syscall::Ftruncate, [sealed, len, 0, 0, 0, 0]);
    if truncate(3 * PAGE_SIZE) != Ok(0) {
        return Err("a sealable memfd would not grow");
    }
    let file =
        fd::file(process, fd::arg(sealed)).map_err(|_| "a memfd's descriptor names nothing")?;
    if file.write_at(0, b"sealed contents") != Ok(15) {
        return Err("a sealable memfd could not be written");
    }

    if add_seals(process, sealed, F_SEAL_SHRINK) != Ok(0) {
        return Err("F_SEAL_SHRINK was refused");
    }
    refused(
        truncate(PAGE_SIZE),
        Errno::EPERM,
        "a truncation downwards past F_SEAL_SHRINK was not EPERM",
        refusals,
    )?;
    if truncate(4 * PAGE_SIZE) != Ok(0) {
        return Err("a truncation upwards was refused by a shrink seal");
    }

    if add_seals(process, sealed, F_SEAL_GROW) != Ok(0) {
        return Err("F_SEAL_GROW was refused");
    }
    refused(
        truncate(5 * PAGE_SIZE),
        Errno::EPERM,
        "a truncation upwards past F_SEAL_GROW was not EPERM",
        refusals,
    )?;
    if file.write_at(4 * PAGE_SIZE - 2, b"past") != Err(Errno::EPERM) {
        return Err("a write extending a file past F_SEAL_GROW was not EPERM");
    }
    *refusals += 1;
    if file.write_at(PAGE_SIZE, b"inside") != Ok(6) {
        return Err("a write inside a grow-sealed memfd was refused");
    }
    Ok(2)
}

/// The write seal: refused while a shared mapping may write the file, a fork
/// child's copy included, and enforced once it stands.
///
/// Verifies: L.user.42
fn check_the_write_seal(
    process: &Process,
    sealed: u64,
    refusals: &mut u32,
) -> Result<u32, &'static str> {
    let len = PAGE_SIZE;
    let at = map(process, sealed, len, PROT_READ | PROT_WRITE, MAP_SHARED)
        .map_err(|_| "a shared writable mapping of a memfd was refused")? as u64;
    refused(
        add_seals(process, sealed, F_SEAL_WRITE),
        Errno::EBUSY,
        "F_SEAL_WRITE with a writable shared mapping alive was not EBUSY",
        refusals,
    )?;
    if get_seals(process, sealed).map(|seals| seals as u32 & F_SEAL_WRITE) != Ok(0) {
        return Err("a refused F_SEAL_WRITE was left standing");
    }

    // A fork child's copy of the mapping counts on its own.
    let child = process
        .space()
        .fork()
        .map_err(|_| "the memfd check's space would not fork")?;
    if memory::sys_munmap(process, at, len) != Ok(0) {
        return Err("a memfd mapping would not unmap");
    }
    refused(
        add_seals(process, sealed, F_SEAL_WRITE),
        Errno::EBUSY,
        "F_SEAL_WRITE with only a fork child's writable mapping alive was not EBUSY",
        refusals,
    )?;
    drop(child);
    if add_seals(process, sealed, F_SEAL_WRITE) != Ok(0) {
        return Err("F_SEAL_WRITE was refused once no shared mapping could write the file");
    }

    let file =
        fd::file(process, fd::arg(sealed)).map_err(|_| "a memfd's descriptor names nothing")?;
    if file.write_at(0, b"x") != Err(Errno::EPERM) {
        return Err("a write past F_SEAL_WRITE was not EPERM");
    }
    *refusals += 1;
    refused(
        map(process, sealed, len, PROT_READ | PROT_WRITE, MAP_SHARED),
        Errno::EPERM,
        "a shared writable mapping of a write-sealed memfd was not EPERM",
        refusals,
    )?;
    let shown = map(process, sealed, len, PROT_READ, MAP_SHARED)
        .map_err(|_| "a shared read-only mapping of a write-sealed memfd was refused")?
        as u64;
    let outcome = refused(
        memory::sys_mprotect(process, shown, len, PROT_READ | PROT_WRITE),
        Errno::EACCES,
        "mprotect to writable of a write-sealed memfd's shared mapping was not EACCES",
        refusals,
    );
    let _ = memory::sys_munmap(process, shown, len);
    outcome?;

    let private = map(process, sealed, len, PROT_READ | PROT_WRITE, MAP_PRIVATE)
        .map_err(|_| "a private mapping of a write-sealed memfd was refused")?
        as u64;
    let written = uaccess::copy_to_user(process.space(), private, b"private");
    let mut kept = [0_u8; 7];
    let read = file.read_at(0, &mut kept);
    let _ = memory::sys_munmap(process, private, len);
    written.map_err(|_| "a write into a private mapping of a write-sealed memfd was refused")?;
    if read != Ok(kept.len()) || &kept == b"private" {
        return Err("a private mapping's write reached a write-sealed memfd");
    }
    Ok(1)
}

/// A shared read-only mapping of a file opened read-only, `mprotect`ed to
/// `PROT_READ` first, still may not be made writable, and its file is left as
/// it was. Before `protect` kept `MAP_SHARED`, the first `mprotect` made the
/// region private, so the second one turning it writable was safe only by
/// accident; now the may-write test alone stands on that path. With that test
/// taken out of `AddressSpace::protect` (scratch), this fails on its own
/// message, ahead of the seal checks that lean on the same test.
///
/// Verifies: L.user.89
fn check_a_protected_read_only_mapping_stays_read_only(
    process: &Process,
    page: u64,
    refusals: &mut u32,
) -> Result<(), &'static str> {
    let cwd = AT_FDCWD as i64 as u64;
    let open = |flags: u32| {
        by_number(
            process,
            Syscall::Openat,
            [cwd, page + AT_PATH, u64::from(flags), 0o600, 0, 0],
        )
    };
    let writer = descriptor(
        open(O_RDWR | O_CREAT | O_TRUNC),
        "the memfd check's file would not open",
    )?;
    let file = fd::file(process, fd::arg(writer)).map_err(|_| "the check's file names nothing")?;
    if file.write_at(0, b"read only") != Ok(9) {
        return Err("the memfd check's file could not be written");
    }
    let reader = descriptor(
        open(O_RDONLY),
        "the memfd check's file would not open read-only",
    )?;
    let at = map(process, reader, PAGE_SIZE, PROT_READ, MAP_SHARED)
        .map_err(|_| "a shared read-only mapping of a file opened read-only was refused")?
        as u64;
    let outcome = (|| {
        if memory::sys_mprotect(process, at, PAGE_SIZE, PROT_READ) != Ok(0) {
            return Err("mprotect of a shared read-only mapping to PROT_READ was refused");
        }
        refused(
            memory::sys_mprotect(process, at, PAGE_SIZE, PROT_READ | PROT_WRITE),
            Errno::EACCES,
            "mprotect made a shared mapping of a file opened read-only writable after a PROT_READ",
            refusals,
        )?;
        let mut held = [0u8; 9];
        if file.read_at(0, &mut held) != Ok(9) || &held != b"read only" {
            return Err("a refused mprotect left the read-only file changed");
        }
        Ok(())
    })();
    let _ = memory::sys_munmap(process, at, PAGE_SIZE);
    let _ = fd::sys_close(process, fd::arg(reader));
    let _ = fd::sys_close(process, fd::arg(writer));
    outcome
}

/// The gap the may-write accounting closed: a shared read-only mapping of a
/// file opened read-only may not be made writable.
///
/// Verifies: L.user.89
fn check_a_read_only_mapping_stays_read_only(
    process: &Process,
    page: u64,
    refusals: &mut u32,
) -> Result<(), &'static str> {
    let cwd = AT_FDCWD as i64 as u64;
    let open = |flags: u32| {
        by_number(
            process,
            Syscall::Openat,
            [cwd, page + AT_PATH, u64::from(flags), 0o600, 0, 0],
        )
    };
    let writer = descriptor(
        open(O_RDWR | O_CREAT | O_TRUNC),
        "the memfd check's file would not open",
    )?;
    let file = fd::file(process, fd::arg(writer)).map_err(|_| "the check's file names nothing")?;
    if file.write_at(0, b"read only") != Ok(9) {
        return Err("the memfd check's file could not be written");
    }
    let reader = descriptor(
        open(O_RDONLY),
        "the memfd check's file would not open read-only",
    )?;
    let at = map(process, reader, PAGE_SIZE, PROT_READ, MAP_SHARED)
        .map_err(|_| "a shared read-only mapping of a file opened read-only was refused")?
        as u64;
    let outcome = refused(
        memory::sys_mprotect(process, at, PAGE_SIZE, PROT_READ | PROT_WRITE),
        Errno::EACCES,
        "mprotect made a shared mapping of a file opened read-only writable",
        refusals,
    );
    let _ = memory::sys_munmap(process, at, PAGE_SIZE);
    outcome
}

/// What Proton's Wine does with its user shared data: a memfd mapped shared
/// and read-write, `mprotect`ed read-write again, then written through the
/// mapping, must have the byte in the file -- `protect` changes the
/// permissions and keeps `MAP_SHARED` -- and a mapping of it with
/// `PROT_WRITE` alone, as wineserver makes, must read what the file holds.
///
/// `protect` took the new flags whole, so the region came out private, and
/// the write after it went to a private copy (Proton's `wineboot` faulted
/// on it); and a write-only region refused a read. With `region.flags =
/// flags` put back in `ferrix_vma`'s `protect` (scratch), the first test
/// here fails; with the write-implies-read taken out of `permits`, the
/// second.
///
/// Whether `protect` keeps a region's growth and `mlock` is ferrix-vma's
/// host test alone: no kernel path sets either today.
///
/// Verifies: L.user.128
/// Verifies: L.user.129
fn check_protect_keeps_a_mapping_shared(process: &Process, page: u64) -> Result<(), &'static str> {
    let shared = descriptor(
        by_number(
            process,
            Syscall::MemfdCreate,
            [page + AT_NAME, u64::from(MFD_EXEC), 0, 0, 0, 0],
        ),
        "memfd_create refused an MFD_EXEC memfd",
    )?;
    let file = fd::file(process, fd::arg(shared)).map_err(|_| "the shared memfd names nothing")?;
    if by_number(process, Syscall::Ftruncate, [shared, PAGE_SIZE, 0, 0, 0, 0]) != Ok(0) {
        return Err("the shared memfd could not be sized");
    }
    let at = map(
        process,
        shared,
        PAGE_SIZE,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
    )
    .map_err(|_| "a shared read-write mapping of a memfd was refused")? as u64;
    let outcome = (|| {
        if memory::sys_mprotect(process, at, PAGE_SIZE, PROT_READ | PROT_WRITE) != Ok(0) {
            return Err("mprotect of a shared mapping to read-write was refused");
        }
        uaccess::copy_to_user(process.space(), at + 0x268, &[0x5a])
            .map_err(|_| "a write through a shared mapping mprotect made read-write faulted")?;
        let mut byte = [0u8];
        if file.read_at(0x268, &mut byte) != Ok(1) || byte[0] != 0x5a {
            return Err("a write through a shared mapping after mprotect did not reach its file");
        }
        let writer = map(process, shared, PAGE_SIZE, PROT_WRITE, MAP_SHARED)
            .map_err(|_| "a write-only shared mapping of a memfd was refused")?
            as u64;
        let mut read = [0u8];
        let got = uaccess::copy_from_user(process.space(), writer + 0x268, &mut read);
        let _ = memory::sys_munmap(process, writer, PAGE_SIZE);
        if got.is_err() || read[0] != 0x5a {
            return Err("a write-only mapping did not read what its file holds");
        }
        Ok(())
    })();
    let _ = memory::sys_munmap(process, at, PAGE_SIZE);
    let _ = fd::sys_close(process, shared as i32);
    outcome
}

/// The exec seal and `vm.memfd_noexec`, Linux 6.18's `mm/memfd.c` and
/// `shmem_setattr`. Leaves the first namespace's scope at 0, as it found it,
/// whatever the outcome.
fn check_the_exec_seal(
    process: &Process,
    page: u64,
    refusals: &mut u32,
) -> Result<u32, &'static str> {
    if crate::syscall::memfd::noexec_scope(Some(process)) != 0 {
        return Err("vm.memfd_noexec was not 0 before the exec seal check");
    }
    let outcome = check_exec_flags(process, page, refusals)
        .and_then(|seals| check_noexec_scopes(process, page, refusals).map(|()| seals));
    crate::syscall::pidns::set_memfd_noexec(None, 0);
    outcome
}

/// `memfd_create`, by number, with the staged name.
fn create_named(process: &Process, page: u64, flags: u32) -> Result<usize, Errno> {
    by_number(
        process,
        Syscall::MemfdCreate,
        [page + AT_NAME, u64::from(flags), 0, 0, 0, 0],
    )
}

/// The permission bits of the file `fd` names.
fn mode_of(process: &Process, fd: u64) -> Result<u32, &'static str> {
    fd::file(process, fd::arg(fd))
        .map(|file| file.inode().metadata().permissions & 0o7777)
        .map_err(|_| "a memfd's descriptor names nothing")
}

/// `fchmod(fd, mode)`.
fn chmod(process: &Process, fd: u64, mode: u32) -> Result<usize, Errno> {
    by_number(process, Syscall::Fchmod, [fd, u64::from(mode), 0, 0, 0, 0])
}

/// `MFD_NOEXEC_SEAL` and `MFD_EXEC` asked for by name. Returns the seals
/// enforced.
fn check_exec_flags(process: &Process, page: u64, refusals: &mut u32) -> Result<u32, &'static str> {
    refused(
        create_named(process, page, MFD_EXEC | MFD_NOEXEC_SEAL),
        Errno::EINVAL,
        "memfd_create with MFD_EXEC and MFD_NOEXEC_SEAL was not EINVAL",
        refusals,
    )?;

    let noexec = descriptor(
        create_named(process, page, MFD_NOEXEC_SEAL),
        "memfd_create refused MFD_NOEXEC_SEAL",
    )?;
    if mode_of(process, noexec)? != 0o666 {
        return Err("a memfd made with MFD_NOEXEC_SEAL is not mode 0666");
    }
    if get_seals(process, noexec) != Ok(F_SEAL_EXEC as usize) {
        return Err("a memfd made with MFD_NOEXEC_SEAL does not carry F_SEAL_EXEC alone");
    }
    refused(
        chmod(process, noexec, 0o766),
        Errno::EPERM,
        "fchmod gave an execute bit to a memfd under F_SEAL_EXEC",
        refusals,
    )?;
    if mode_of(process, noexec)? != 0o666 {
        return Err("a refused fchmod past F_SEAL_EXEC changed the mode");
    }
    if chmod(process, noexec, 0o600) != Ok(0) || mode_of(process, noexec)? != 0o600 {
        return Err("fchmod that changes no execute bit was refused past F_SEAL_EXEC");
    }
    if run_descriptor(process, page, noexec) != Some(Errno::EACCES) {
        return Err("execveat of a memfd made with MFD_NOEXEC_SEAL was not EACCES");
    }
    *refusals += 1;
    if add_seals(process, noexec, F_SEAL_SHRINK) != Ok(0) {
        return Err("a memfd made with MFD_NOEXEC_SEAL could not be sealed further");
    }
    if get_seals(process, noexec) != Ok((F_SEAL_EXEC | F_SEAL_SHRINK) as usize) {
        return Err("F_SEAL_EXEC on a file with no execute bit brought other seals");
    }

    let exec = descriptor(
        create_named(process, page, MFD_EXEC | MFD_ALLOW_SEALING),
        "memfd_create refused MFD_EXEC",
    )?;
    if mode_of(process, exec)? != 0o777 || get_seals(process, exec) != Ok(0) {
        return Err("a memfd made with MFD_EXEC is not mode 0777 with no seals");
    }
    // The control for the refusal above: the same call on a file with execute
    // bits gets past the permission and fails only on the empty file.
    if matches!(
        run_descriptor(process, page, exec),
        None | Some(Errno::EACCES)
    ) {
        return Err("execveat of an empty MFD_EXEC memfd was not refused as no program");
    }
    if add_seals(process, exec, F_SEAL_EXEC) != Ok(0) {
        return Err("F_SEAL_EXEC was refused on an executable memfd");
    }
    let implied = F_SEAL_EXEC | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE;
    if get_seals(process, exec) != Ok(implied as usize) {
        return Err("F_SEAL_EXEC on an executable memfd did not bring the write seals");
    }
    let file =
        fd::file(process, fd::arg(exec)).map_err(|_| "a memfd's descriptor names nothing")?;
    if file.write_at(0, b"x") != Err(Errno::EPERM) {
        return Err("a write to an executable memfd past F_SEAL_EXEC was not EPERM");
    }
    *refusals += 1;
    refused(
        chmod(process, exec, 0o666),
        Errno::EPERM,
        "fchmod took the execute bits off a memfd under F_SEAL_EXEC",
        refusals,
    )?;
    Ok(2)
}

/// `execveat(fd, "", NULL, NULL, AT_EMPTY_PATH)`: the error it was refused
/// with, or `None` if it ran -- which the check's empty files never can.
fn run_descriptor(process: &Process, page: u64, fd: u64) -> Option<Errno> {
    use crate::syscall::exec::{self, ExecveError};
    let word = crate::syscall::signal::word_of(crate::trap::Abi::Native);
    match exec::sys_execveat(
        process,
        fd::arg(fd),
        page + AT_EMPTY,
        0,
        0,
        AT_EMPTY_PATH,
        word,
    ) {
        Err(ExecveError::Refused(error)) => Some(error),
        Ok(_) | Err(ExecveError::Lost) => None,
    }
}

/// `vm.memfd_noexec`: what a memfd naming neither exec flag becomes at each
/// scope, `MFD_EXEC` refused at 2, the file's own refusals, and a pid
/// namespace that cannot be set below its parent.
fn check_noexec_scopes(
    process: &Process,
    page: u64,
    refusals: &mut u32,
) -> Result<(), &'static str> {
    use crate::syscall::{memfd, pidns};

    let plain = descriptor(
        create_named(process, page, 0),
        "memfd_create refused a plain memfd",
    )?;
    if mode_of(process, plain)? != 0o777 || get_seals(process, plain) != Ok(F_SEAL_SEAL as usize) {
        return Err("at vm.memfd_noexec 0 a plain memfd is not mode 0777 and unsealable");
    }

    // Set through the file, as a program sets it, then read back.
    let written = set_through_proc(process, page, b"1\n")?;
    if written != Ok(2) || memfd::noexec_scope(Some(process)) != 1 {
        return Err("a write of 1 to /proc/sys/vm/memfd_noexec did not set the scope");
    }
    let sealed = descriptor(
        create_named(process, page, MFD_ALLOW_SEALING),
        "memfd_create refused a memfd at vm.memfd_noexec 1",
    )?;
    if mode_of(process, sealed)? != 0o666 || get_seals(process, sealed) != Ok(F_SEAL_EXEC as usize)
    {
        return Err(
            "at vm.memfd_noexec 1 a memfd naming no exec flag was not made MFD_NOEXEC_SEAL",
        );
    }
    let exec = descriptor(
        create_named(process, page, MFD_EXEC),
        "memfd_create refused MFD_EXEC at vm.memfd_noexec 1",
    )?;
    if mode_of(process, exec)? != 0o777 {
        return Err("at vm.memfd_noexec 1 MFD_EXEC did not make an executable memfd");
    }

    for refusal in [&b"3"[..], b"-1", b"one"] {
        if set_through_proc(process, page, refusal)? != Err(Errno::EINVAL) {
            return Err("vm.memfd_noexec took a value outside 0 to 2");
        }
        *refusals += 1;
    }
    if memfd::set_noexec(process, b"2") != Ok(1) {
        return Err("vm.memfd_noexec would not be set to 2");
    }
    refused(
        create_named(process, page, MFD_EXEC),
        Errno::EACCES,
        "memfd_create with MFD_EXEC at vm.memfd_noexec 2 was not EACCES",
        refusals,
    )?;
    let enforced = descriptor(
        create_named(process, page, 0),
        "memfd_create refused a plain memfd at vm.memfd_noexec 2",
    )?;
    if mode_of(process, enforced)? != 0o666 {
        return Err("at vm.memfd_noexec 2 a plain memfd was made executable");
    }

    // A namespace made at 2 keeps 2 when the first goes back to 0, and may
    // not be set below the 2 its parent then had; one made at 0 may not be
    // set below 0 either.
    let strict =
        pidns::create(None).map_err(|_| "no memory for the exec seal check's namespace")?;
    pidns::set_memfd_noexec(None, 0);
    if pidns::memfd_noexec(Some(&strict)) != 2 || pidns::memfd_noexec_floor(Some(&strict)) != 0 {
        return Err("a pid namespace did not start at the vm.memfd_noexec it was made under");
    }
    let below = pidns::create(Some(&strict)).map_err(|_| "no memory for a nested namespace")?;
    if pidns::memfd_noexec_floor(Some(&below)) != 2 || pidns::memfd_noexec(Some(&below)) != 2 {
        return Err("a nested pid namespace's vm.memfd_noexec floor is not its parent's scope");
    }
    Ok(())
}

/// Write `value` to `/proc/sys/vm/memfd_noexec` through the file, acting as
/// `process`. The inner result is the write's.
fn set_through_proc(
    process: &Process,
    page: u64,
    value: &[u8],
) -> Result<Result<usize, Errno>, &'static str> {
    let process = crate::syscall::registry::find(process.pid())
        .ok_or("the memfd check's process is not registered")?;
    uaccess::copy_to_user(process.space(), page + AT_VALUE, value)
        .map_err(|_| "could not stage a vm.memfd_noexec value")?;
    let cwd = AT_FDCWD as i64 as u64;
    crate::syscall::userns::acting_as(&process, || {
        let fd = descriptor(
            by_number(
                &process,
                Syscall::Openat,
                [cwd, page + AT_SYSCTL, u64::from(O_WRONLY), 0, 0, 0],
            ),
            "/proc/sys/vm/memfd_noexec would not open for writing",
        )?;
        let written = by_number(
            &process,
            Syscall::Write,
            [fd, page + AT_VALUE, value.len() as u64, 0, 0, 0],
        );
        let _ = fd::sys_close(&process, fd as i32);
        Ok(written)
    })?
}
