//! `memfd_create`: an anonymous file, on a tmpfs of its own that nothing
//! mounts.
//!
//! The file is a tmpfs regular file that no directory names, opened for
//! reading and writing. Its pages are VMO pages like any tmpfs file's, so it
//! reads, writes, truncates and maps as one does -- which is the point: a
//! Wayland client hands `wl_shm` a memfd, both sides map it shared, and the
//! compositor seals it so the client cannot shrink it under the mapping.
//!
//! # Seals
//!
//! With `MFD_ALLOW_SEALING` the file starts with no seals and takes them
//! through `fcntl(F_ADD_SEALS)`; without it, it carries `F_SEAL_SEAL` from the
//! start, as every other tmpfs file does. What each seal refuses is tmpfs's to
//! enforce, and a write seal's refusal while a shared mapping may write the
//! file is mm's, through the file object's may-write count.
//!
//! # Exec
//!
//! Linux 6.3's exec seal, as Linux 6.18's `mm/memfd.c` has it. `MFD_EXEC`
//! makes the file mode 0777, as every memfd was before. `MFD_NOEXEC_SEAL`
//! makes it 0666, so no `execve` or `fexecve` of it runs, and gives it
//! `F_SEAL_EXEC`, which refuses any `chmod` that changes an execute bit; such
//! a file may be sealed further, `MFD_ALLOW_SEALING` or not. `F_SEAL_EXEC`
//! added later to a file that still has an execute bit seals its contents as
//! well (tmpfs's), so it is never writable and executable at once.
//!
//! Neither flag given, the caller's pid namespace's `vm.memfd_noexec` decides
//! ([`pidns::memfd_noexec`]): 0 means `MFD_EXEC`, 1 `MFD_NOEXEC_SEAL`, and 2
//! the same as 1 with `MFD_EXEC` refused `EACCES`. The sysctl is
//! `/proc/sys/vm/memfd_noexec` ([`noexec_scope`], [`set_noexec`]).
//!
//! # What is refused
//!
//! Flags other than `MFD_CLOEXEC`, `MFD_ALLOW_SEALING`, `MFD_NOEXEC_SEAL` and
//! `MFD_EXEC` are `EINVAL`, and so are the last two together. That includes
//! `MFD_HUGETLB`, since there is no hugetlbfs. `MFD_EXEC` under
//! `vm.memfd_noexec` 2 is `EACCES`. A name longer than 249 bytes is `EINVAL`,
//! and one that cannot be read is `EFAULT`. The flags are judged before the
//! name, as Linux's `sanitize_flags` is.

use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{MFD_ALLOW_SEALING, MFD_CLOEXEC, MFD_EXEC, MFD_NOEXEC_SEAL};
use ferrix_sync::Once;
use ferrix_vfs::tmpfs::{SEAL_EXEC, SEAL_SEAL, Tmpfs};
use ferrix_vfs::{FileSystem, Location, OpenFile, OpenFlags, SetAttributes};

use crate::fs;
use crate::syscall::pidns;
use crate::syscall::process::Process;
use crate::syscall::uaccess::{self, UserError};
use crate::syscall::userns::CAP_SYS_ADMIN;

/// The longest name: `NAME_MAX` less the `memfd:` every name is shown with.
const NAME_MAX_LEN: usize = 249;

/// The mode a memfd's inode has: `S_IFREG | 0777`, as on Linux.
const PERMISSIONS: u32 = 0o777;

/// The execute bits `MFD_NOEXEC_SEAL` takes off [`PERMISSIONS`].
const EXECUTE_BITS: u32 = 0o111;

/// Every flag `memfd_create` takes.
const FLAGS: u32 = MFD_CLOEXEC | MFD_ALLOW_SEALING | MFD_NOEXEC_SEAL | MFD_EXEC;

/// `vm.memfd_noexec`'s scopes, from Linux's `include/linux/pid_namespace.h`:
/// `MEMFD_NOEXEC_SCOPE_NOEXEC_SEAL`, a memfd asked for neither way is made
/// `MFD_NOEXEC_SEAL`.
const SCOPE_NOEXEC_SEAL: u32 = 1;
/// `MEMFD_NOEXEC_SCOPE_NOEXEC_ENFORCED`: as [`SCOPE_NOEXEC_SEAL`], and
/// `MFD_EXEC` is refused.
const SCOPE_NOEXEC_ENFORCED: u32 = 2;

/// The tmpfs every memfd lives on, made on first use.
static MEMFD_FS: Once<Arc<Tmpfs>> = Once::new();

/// The one memfd tmpfs.
fn memfd_fs() -> &'static Arc<Tmpfs> {
    MEMFD_FS.call_once(fs::kernel_tmpfs)
}

/// `memfd_create(name, flags)`.
///
/// # Errors
///
/// `EINVAL` for an unknown flag, for `MFD_EXEC` with `MFD_NOEXEC_SEAL`, or a
/// name longer than 249 bytes, `EACCES` for an executable memfd under
/// `vm.memfd_noexec` 2, `EFAULT` for a name that cannot be read, `EMFILE`
/// for a full descriptor table, and whatever tmpfs's storage refuses.
pub(crate) fn sys_memfd_create(process: &Process, name: u64, flags: u32) -> Result<usize, Errno> {
    let flags = sanitize_flags(process, flags)?;
    let mut bytes = Vec::new();
    match uaccess::copy_cstr_from_user(process.space(), name, NAME_MAX_LEN + 1, &mut bytes) {
        Ok(()) => {}
        // Unterminated within the limit: the name is too long, not unreadable.
        Err(UserError::Fault) if bytes.len() > NAME_MAX_LEN => return Err(Errno::EINVAL),
        Err(_) => return Err(Errno::EFAULT),
    }
    if bytes.len() > NAME_MAX_LEN {
        return Err(Errno::EINVAL);
    }

    let tmpfs = memfd_fs();
    let (permissions, seals) = if flags & MFD_NOEXEC_SEAL != 0 {
        (PERMISSIONS & !EXECUTE_BITS, SEAL_EXEC)
    } else if flags & MFD_ALLOW_SEALING != 0 {
        (PERMISSIONS, 0)
    } else {
        (PERMISSIONS, SEAL_SEAL)
    };
    let inode = tmpfs.new_unlinked_file(permissions, seals)?;
    // Its creator's, as Linux's tmpfs gives a memfd through `inode_init_owner`.
    let (uid, gid) = crate::syscall::path::creator_ids(process);
    inode.set_attributes(&SetAttributes {
        uid: Some(uid),
        gid: Some(gid),
        ..SetAttributes::default()
    })?;
    let shown = format!("memfd:{}", alloc::string::String::from_utf8_lossy(&bytes));
    let location = Location::detached(
        Arc::clone(tmpfs) as Arc<dyn FileSystem>,
        inode,
        shown.as_bytes(),
        Arc::new(crate::sync::SchedParker),
    )?;
    let open = OpenFlags {
        read: true,
        write: true,
        ..OpenFlags::default()
    };
    let file: Arc<OpenFile> = OpenFile::new(location, &open)?;
    let descriptor = process
        .files()
        .lock()
        .insert(file, flags & MFD_CLOEXEC != 0)?;
    usize::try_from(descriptor).map_err(|_| Errno::EMFILE)
}

/// The flags `memfd_create` goes on with, as Linux's `sanitize_flags` and
/// `check_sysctl_memfd_noexec` judge them: `MFD_EXEC` or `MFD_NOEXEC_SEAL`
/// added where neither was asked for, as `vm.memfd_noexec` says.
///
/// # Errors
///
/// `EINVAL` for an unknown flag or for both exec flags; `EACCES` for a memfd
/// that would be executable under [`SCOPE_NOEXEC_ENFORCED`].
fn sanitize_flags(process: &Process, flags: u32) -> Result<u32, Errno> {
    let both = MFD_EXEC | MFD_NOEXEC_SEAL;
    if flags & !FLAGS != 0 || flags & both == both {
        return Err(Errno::EINVAL);
    }
    let scope = noexec_scope(Some(process));
    let mut flags = flags;
    if flags & both == 0 {
        flags |= if scope >= SCOPE_NOEXEC_SEAL {
            MFD_NOEXEC_SEAL
        } else {
            MFD_EXEC
        };
    }
    if flags & MFD_NOEXEC_SEAL == 0 && scope >= SCOPE_NOEXEC_ENFORCED {
        return Err(Errno::EACCES);
    }
    Ok(flags)
}

/// The `vm.memfd_noexec` scope `process` obeys -- the first namespace's for
/// none, a kernel thread -- which `/proc/sys/vm/memfd_noexec` shows it: the
/// highest of its pid namespace's own and its ancestors'.
pub(crate) fn noexec_scope(process: Option<&Process>) -> u32 {
    pidns::memfd_noexec(
        process
            .and_then(Process::numbers)
            .map(|numbers| numbers.namespace()),
    )
}

/// A write of `data` to `/proc/sys/vm/memfd_noexec` by `process`: Linux's
/// `pid_mfd_noexec_dointvec_minmax`. One decimal integer, white space around
/// it allowed, from the scope of the namespace's parent -- a namespace may
/// not be set laxer than the one it was made in -- to
/// [`SCOPE_NOEXEC_ENFORCED`]; it sets the writer's own pid namespace.
///
/// Two differences from Linux, both in `docs/BACKLOG.md`: the number is
/// decimal, with an optional `+`, where `proc_dointvec` takes any base and
/// no `+`; and `CAP_SYS_ADMIN` is asked in the writer's own user namespace,
/// where Linux asks it over the pid namespace's owner, which a
/// `PidNamespace` does not record yet.
///
/// # Errors
///
/// `EPERM` without `CAP_SYS_ADMIN`, asked first; `EINVAL` for anything but
/// such a number.
pub(crate) fn set_noexec(process: &Process, data: &[u8]) -> Result<usize, Errno> {
    if !process.with_credentials(|held| held.holds(CAP_SYS_ADMIN)) {
        return Err(Errno::EPERM);
    }
    let namespace = process.numbers().map(|numbers| numbers.namespace());
    let floor = pidns::memfd_noexec_floor(namespace);
    let scope = core::str::from_utf8(data)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .filter(|scope| (floor..=SCOPE_NOEXEC_ENFORCED).contains(scope))
        .ok_or(Errno::EINVAL)?;
    pidns::set_memfd_noexec(namespace, scope);
    Ok(data.len())
}
