//! `unshare` and `setns`: mount, user, UTS, IPC and cgroup namespaces.
//!
//! A user namespace (`docs/NAMESPACES.md` §2.2) is made first when asked for
//! with a mount namespace, and owns it. The caller ends up in it holding every
//! capability there and none outside; see [`userns`].
//!
//! A process is in one mount namespace, named in its fs context beside its
//! root and working directory, and `unshare(CLONE_NEWNS)` or
//! `clone(CLONE_NEWNS)` gives it a copy of the one it was in
//! (`docs/NAMESPACES.md` §2.1): the same mounts, new ones, so that what it
//! mounts or unmounts after is its own. The UTS, IPC and cgroup namespaces
//! are made with [`nsproxy::make`] and held in the process's proxy
//! (`docs/NAMESPACES.md` §12). Pid and network namespaces do not exist: a
//! program that asks for one is told no, and `EINVAL` is the no it already
//! handles -- `CONFIG_*_NS` off is an ordinary configuration, and
//! `unshare(1)` and container runtimes check for it.
//!
//! The two `unshare` flags that are not namespaces are different. Giving up a
//! descriptor table or a working directory shared through `clone(CLONE_FILES)`
//! or `clone(CLONE_FS)` needs no namespace at all, and a process that shares
//! neither has already done it: see [`sys_unshare`].

use alloc::sync::Arc;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{CLONE_FILES, CLONE_FS};
use ferrix_vfs::Context;

use crate::fallible;
use crate::fs;
use crate::sync::SpinLock;
use crate::syscall::fd;
use crate::syscall::nsproxy::{self, CLONE_NEWCGROUP, CLONE_NEWIPC, CLONE_NEWUTS};
use crate::syscall::process::Process;
use crate::syscall::userns::{self, CAP_SYS_ADMIN};

/// Give the process a mount namespace of its own.
pub(crate) const CLONE_NEWNS: u64 = 0x0002_0000;
/// Give the process a user namespace of its own.
pub(crate) const CLONE_NEWUSER: u64 = 0x1000_0000;

/// `unshare`.
///
/// Zero flags is nothing to do, and succeeds. `CLONE_FILES` and `CLONE_FS`
/// succeed when the process's table, or its root and working directory, are
/// its own already -- which is the case unless `clone` was asked to share
/// them -- because the call asks for a result that is then already true, and
/// Linux does nothing in that case either.
///
/// When one of them *is* shared the answer is `EINVAL`, and that is a
/// departure from Linux, which would make the private copy. A [`Process`]
/// holds its table and its context for its whole life and cannot swap in a
/// copy; until it can, refusing is honest and succeeding would not be, since
/// the caller would go on to change descriptors it believes are its own.
///
/// `CLONE_NEWNS` implies `CLONE_FS`, as on Linux, so it is refused the same
/// way in a process whose context is shared -- one of several threads, which
/// share theirs -- and otherwise needs privilege (`EPERM`), and then copies
/// the namespace into the context ([`copy_namespace`]).
///
/// Every other flag names a namespace, or asks to leave a thread group or an
/// address space, and is `EINVAL`.
pub(crate) fn sys_unshare(process: &Process, flags: u64) -> Result<usize, Errno> {
    const SMALL: u64 = CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWCGROUP;
    if flags & !(CLONE_FILES | CLONE_FS | CLONE_NEWNS | CLONE_NEWUSER | SMALL) != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & CLONE_FILES != 0 && Arc::strong_count(process.files()) > 1 {
        return Err(Errno::EINVAL);
    }
    if flags & (CLONE_FS | CLONE_NEWNS | CLONE_NEWUSER) != 0
        && Arc::strong_count(process.fs_context()) > 1
    {
        return Err(Errno::EINVAL);
    }
    // A user namespace cannot be given to one thread of several (U5's kin:
    // Linux implies `CLONE_THREAD`'s opposite), and the small ones are a
    // process's, not a thread's, here (`docs/NAMESPACES.md` §12).
    if flags & (CLONE_NEWUSER | SMALL) != 0 && process.tasks().len() > 1 {
        return Err(Errno::EINVAL);
    }
    let fresh = if flags & CLONE_NEWUSER != 0 {
        Some(make_user_namespace(process)?)
    } else {
        None
    };
    // The owner of everything made here: the new user namespace, if one was
    // asked for, else the one the caller is in.
    let owner = match &fresh {
        Some(fresh) => Arc::clone(fresh),
        None => process.with_credentials(|held| Arc::clone(&held.user_ns)),
    };
    // `CLONE_NEWNS` needs `CAP_SYS_ADMIN` where the caller is: in the
    // namespace it is about to have, if it asked for one.
    if flags & CLONE_NEWNS != 0
        && fresh.is_none()
        && !process.with_credentials(|held| held.holds(CAP_SYS_ADMIN))
    {
        return Err(Errno::EPERM);
    }
    // Made before anything is changed, so a refusal leaves nothing behind.
    let proxy = nsproxy::make(process, flags, &owner, process.job())?;
    if flags & CLONE_NEWNS != 0 {
        copy_namespace(process.fs_context(), &owner)?;
    }
    if flags & SMALL != 0 {
        process.set_nsproxy(proxy);
    }
    if let Some(fresh) = fresh {
        enter_user_namespace(process, fresh);
    }
    Ok(0)
}

/// Make the user namespace `process` asks for: Linux's `create_user_ns`,
/// less the ids, which come when `process` is put in it
/// ([`enter_user_namespace`]). Nothing changes if it is refused.
///
/// # Errors
///
/// `EPERM` for a process whose root is not its mount namespace's (rule U6,
/// CVE-2013-1956: a chrooted process holding `CAP_SYS_CHROOT` inside a
/// namespace it made could walk out), and for ids the creator's namespace
/// does not map; `EUSERS` past 32 levels; `ENOMEM`.
pub(crate) fn make_user_namespace(process: &Process) -> Result<Arc<userns::UserNamespace>, Errno> {
    let chrooted = {
        let context = process.fs_context().lock();
        let namespace = fs::namespace_of(&context);
        !context.root.same(&namespace.root())
    };
    if chrooted {
        return Err(Errno::EPERM);
    }
    process.with_credentials(|held| userns::create(held))
}

/// Put `process` in `namespace`, holding every capability there and none
/// outside it, as Linux's `set_cred_user_ns`. Its ids stay as they are.
pub(crate) fn enter_user_namespace(process: &Process, namespace: Arc<userns::UserNamespace>) {
    process.with_credentials(|held| {
        held.user_ns = namespace;
        held.caps = userns::CapSets::FRESH;
    });
}

/// Put a copy of `context`'s mount namespace in it, with its root and
/// working directory moved to their mounts' copies: what `unshare` and
/// `clone` with `CLONE_NEWNS` do. The copy is charged to the running task's
/// job, the one asking.
///
/// The context is read, the copy made with no lock held, and the context
/// written: nothing else changes it between, since it is either a new
/// child's, not yet running, or one [`sys_unshare`] found unshared. What it
/// held is dropped after its lock is released, since the namespace it named
/// may end there.
///
/// # Errors
///
/// `ENOMEM` for memory or past the job's memory limit; the context is
/// unchanged then.
pub(crate) fn copy_namespace(
    context: &SpinLock<Context>,
    owner: &Arc<userns::UserNamespace>,
) -> Result<(), Errno> {
    let (mut root, mut cwd, from) = {
        let context = context.lock();
        (
            context.root.clone(),
            context.cwd.clone(),
            fs::namespace_of(&context),
        )
    };
    let copy = from.copy(&mut [&mut root, &mut cwd])?;
    let copy = fallible::try_arc(copy).map_err(|_| Errno::ENOMEM)?;
    let owner: Arc<dyn core::any::Any + Send + Sync> = owner.clone();
    copy.set_owner(owner);
    let displaced = {
        let mut context = context.lock();
        (
            core::mem::replace(&mut context.root, root),
            core::mem::replace(&mut context.cwd, cwd),
            context.ns.replace(copy),
        )
    };
    drop((displaced, from));
    Ok(())
}

/// `setns`.
///
/// No descriptor names a namespace yet -- `/proc/<pid>/ns/mnt` is a link to
/// read, not one to open -- so every open descriptor is the wrong kind:
/// `EINVAL`, as Linux answers for a descriptor that is not a namespace. A
/// closed one is `EBADF` first, which is the order Linux checks them in.
pub(crate) fn sys_setns(process: &Process, fd: i32, nstype: u32) -> Result<usize, Errno> {
    let _ = nstype;
    let _open = fd::file(process, fd)?;
    Err(Errno::EINVAL)
}
