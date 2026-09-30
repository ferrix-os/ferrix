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
use ferrix_vfs::{Context, Namespace};

use crate::fallible;
use crate::fs;
use crate::net::namespace::CLONE_NEWNET;
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
    if flags & !(CLONE_FILES | CLONE_FS | CLONE_NEWNS | CLONE_NEWUSER | CLONE_NEWNET | SMALL) != 0 {
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
    if flags & (CLONE_NEWUSER | CLONE_NEWNET | SMALL) != 0 && process.tasks().len() > 1 {
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
    // A network namespace needs `CAP_SYS_ADMIN` where the caller will be, and
    // is owned by the user namespace it will be in.
    let net = if flags & CLONE_NEWNET != 0 {
        Some(make_net_namespace(process, fresh.as_ref())?)
    } else {
        None
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
    if let Some(net) = net {
        process.set_net_ns(net);
    }
    Ok(0)
}

/// Make the network namespace `process` asks for, owned by `fresh` if it
/// asked for a user namespace with it and by the one it is in otherwise.
///
/// # Errors
///
/// `EPERM` without `CAP_SYS_ADMIN` in the user namespace it will be in;
/// `ENOMEM` past the job's memory.
pub(crate) fn make_net_namespace(
    process: &Process,
    fresh: Option<&Arc<userns::UserNamespace>>,
) -> Result<Arc<crate::net::NetNamespace>, Errno> {
    let owner = match fresh {
        Some(fresh) => Arc::clone(fresh),
        None => process
            .with_credentials(|held| held.holds(CAP_SYS_ADMIN).then(|| Arc::clone(&held.user_ns)))
            .ok_or(Errno::EPERM)?,
    };
    crate::net::namespace::create(owner)
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

/// `setns`: join the namespace a descriptor names, as Linux's `setns` does
/// for a namespace file (`docs/NAMESPACES.md` §12).
///
/// In Linux's order: `EBADF` for a closed descriptor; a pidfd is [`setns_pidfd`]'s;
/// `EINVAL` for any other file that is not a namespace, and for a type that
/// is neither 0 nor the file's own. Then the kind's rules, each `EPERM`
/// unless every capability it
/// needs is held:
///
/// * UTS, IPC and cgroup: `CAP_SYS_ADMIN` over the target's owner and in the
///   caller's own user namespace;
/// * mount: the same, and `CAP_SYS_CHROOT` in the caller's own; `EINVAL` for
///   a fs context shared with another process, which cannot be swapped. The
///   caller's root and working directory become the target's root;
/// * user: `EINVAL` into the namespace the caller is in already, from a
///   process of several threads and with a shared fs context, then
///   `CAP_SYS_ADMIN` in the target, which refuses every ancestor. The caller
///   then holds every capability there (`set_cred_user_ns`).
///
/// A process of several threads may not join a UTS, IPC or cgroup namespace
/// either (`EINVAL`): the proxy is the process's, not the thread's.
pub(crate) fn sys_setns(process: &Process, fd: i32, nstype: u32) -> Result<usize, Errno> {
    let file = fd::file(process, fd)?;
    if let Some(pidfd) = fs::pidfd::of(&file) {
        let target = Arc::clone(pidfd.process());
        drop(file);
        return setns_pidfd(process, &target, u64::from(nstype));
    }
    let target = fs::nsfs::of(&file).ok_or(Errno::EINVAL)?;
    drop(file);
    if nstype != 0 && u64::from(nstype) != target.nstype() {
        return Err(Errno::EINVAL);
    }
    match target {
        fs::nsfs::Handle::User(namespace) => join_user(process, namespace),
        fs::nsfs::Handle::Mount(namespace) => join_mount(process, &namespace),
        small => join_small(process, small),
    }
}

/// Whether the caller holds `CAP_SYS_ADMIN` over `owner` and, in the user
/// namespace it is in, each of `own`.
fn may_join(process: &Process, owner: &userns::UserNamespace, own: &[u32]) -> bool {
    may_join_as(process, owner, own, false)
}

/// [`may_join`], judging the capabilities in the caller's own namespace as a
/// caller who has just joined a user namespace holds them -- every one -- when
/// `joining_user` is set: `setns` on a pidfd checks the rest after the user
/// namespace, as Linux's `validate_nsset` does, so that an owner can enter a
/// user namespace and its mount namespace in one call.
fn may_join_as(
    process: &Process,
    owner: &userns::UserNamespace,
    own: &[u32],
    joining_user: bool,
) -> bool {
    process.with_credentials(|held| {
        userns::capable_over(held, owner, CAP_SYS_ADMIN)
            && (joining_user || own.iter().all(|&cap| held.holds(cap)))
    })
}

/// `setns` into a UTS, IPC or cgroup namespace.
fn join_small(process: &Process, target: fs::nsfs::Handle) -> Result<usize, Errno> {
    let owner = target.owner().ok_or(Errno::EPERM)?;
    if !may_join(process, &owner, &[CAP_SYS_ADMIN]) {
        return Err(Errno::EPERM);
    }
    if process.tasks().len() > 1 {
        return Err(Errno::EINVAL);
    }
    let mut proxy = process.nsproxy();
    put_in_proxy(&mut proxy, target)?;
    process.set_nsproxy(proxy);
    Ok(0)
}

/// Replace the member of `proxy` that `target` is one of.
fn put_in_proxy(proxy: &mut nsproxy::NsProxy, target: fs::nsfs::Handle) -> Result<(), Errno> {
    match target {
        fs::nsfs::Handle::Uts(namespace) => proxy.uts = namespace,
        fs::nsfs::Handle::Ipc(namespace) => proxy.ipc = namespace,
        fs::nsfs::Handle::Cgroup(namespace) => proxy.cgroup = namespace,
        fs::nsfs::Handle::Mount(_) | fs::nsfs::Handle::User(_) => return Err(Errno::EINVAL),
    }
    Ok(())
}

/// `setns` into a mount namespace: Linux's `mntns_install`.
fn join_mount(process: &Process, target: &Arc<Namespace>) -> Result<usize, Errno> {
    let owner = fs::owner_of(target);
    if !may_join(process, &owner, &[userns::CAP_SYS_CHROOT, CAP_SYS_ADMIN]) {
        return Err(Errno::EPERM);
    }
    if Arc::strong_count(process.fs_context()) > 1 {
        return Err(Errno::EINVAL);
    }
    install_mount(process, target);
    Ok(0)
}

/// Put `process` in mount namespace `target`, its root and working directory
/// at the namespace's root.
fn install_mount(process: &Process, target: &Arc<Namespace>) {
    let top = target.root();
    let displaced = {
        let mut context = process.fs_context().lock();
        (
            core::mem::replace(&mut context.root, top.clone()),
            core::mem::replace(&mut context.cwd, top),
            context.ns.replace(Arc::clone(target)),
        )
    };
    // Dropped after the lock: the namespace it named may end here.
    drop(displaced);
}

/// `setns` into a user namespace: Linux's `userns_install`.
fn join_user(process: &Process, target: Arc<userns::UserNamespace>) -> Result<usize, Errno> {
    check_user(process, &target)?;
    enter_user_namespace(process, target);
    Ok(0)
}

/// What refuses `process` a user namespace: its own, a thread group or a
/// shared fs context, or no `CAP_SYS_ADMIN` in it.
fn check_user(process: &Process, target: &Arc<userns::UserNamespace>) -> Result<(), Errno> {
    if process.with_credentials(|held| held.user_ns.same(target)) {
        return Err(Errno::EINVAL);
    }
    if process.tasks().len() > 1 || Arc::strong_count(process.fs_context()) > 1 {
        return Err(Errno::EINVAL);
    }
    let allowed =
        process.with_credentials(|held| userns::capable_over(held, target, CAP_SYS_ADMIN));
    if !allowed {
        return Err(Errno::EPERM);
    }
    Ok(())
}

/// Every namespace `setns` on a pidfd can join.
const PIDFD_FLAGS: u64 =
    CLONE_NEWNS | CLONE_NEWUSER | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWCGROUP;

/// `setns` on a pidfd: the namespaces `flags` names, each the process
/// `target`'s, joined together or not at all. `EINVAL` for no flag or a flag
/// that is no namespace here. Each is judged as a descriptor for it would be;
/// when `CLONE_NEWUSER` is among them the rest are judged after it, as Linux
/// does, so an owner can enter a user namespace and its mount namespace in one
/// call.
fn setns_pidfd(process: &Process, target: &Process, flags: u64) -> Result<usize, Errno> {
    use fs::nsfs::{Handle, Kind};
    if flags == 0 || flags & !PIDFD_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }
    let joining_user = flags & CLONE_NEWUSER != 0;
    let user = match Handle::of(target, Kind::User) {
        Handle::User(namespace) if joining_user => {
            check_user(process, &namespace)?;
            Some(namespace)
        }
        _ => None,
    };
    let mut proxy = process.nsproxy();
    let mut small = false;
    for (flag, kind) in [
        (CLONE_NEWUTS, Kind::Uts),
        (CLONE_NEWIPC, Kind::Ipc),
        (CLONE_NEWCGROUP, Kind::Cgroup),
    ] {
        if flags & flag == 0 {
            continue;
        }
        let namespace = Handle::of(target, kind);
        let owner = namespace.owner().ok_or(Errno::EPERM)?;
        if !may_join_as(process, &owner, &[CAP_SYS_ADMIN], joining_user) {
            return Err(Errno::EPERM);
        }
        put_in_proxy(&mut proxy, namespace)?;
        small = true;
    }
    if small && process.tasks().len() > 1 {
        return Err(Errno::EINVAL);
    }
    let mount = match Handle::of(target, Kind::Mount) {
        Handle::Mount(namespace) if flags & CLONE_NEWNS != 0 => {
            let owner = fs::owner_of(&namespace);
            let needs = [userns::CAP_SYS_CHROOT, CAP_SYS_ADMIN];
            if !may_join_as(process, &owner, &needs, joining_user) {
                return Err(Errno::EPERM);
            }
            if Arc::strong_count(process.fs_context()) > 1 {
                return Err(Errno::EINVAL);
            }
            Some(namespace)
        }
        _ => None,
    };
    // Everything is judged; nothing below can be refused.
    if let Some(namespace) = mount {
        install_mount(process, &namespace);
    }
    if small {
        process.set_nsproxy(proxy);
    }
    if let Some(namespace) = user {
        enter_user_namespace(process, namespace);
    }
    Ok(0)
}
