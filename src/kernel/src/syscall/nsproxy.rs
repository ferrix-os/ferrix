//! The namespaces a process is in besides its mount and user namespaces: the
//! UTS, IPC and cgroup ones (`docs/NAMESPACES.md` §12, "The small
//! namespaces").
//!
//! A mount namespace is named in the fs context and a user namespace in the
//! credentials. These three are named together, in an [`NsProxy`] that a
//! process holds beside its credentials and a fork child copies. Linux keeps
//! one per task; here it is per process, so a thread cannot leave its group's
//! namespaces, and `clone` refuses `CLONE_THREAD` with any of the flags.
//!
//! # Numbers
//!
//! Each namespace has the number `/proc/<pid>/ns/<kind>` shows and an nsfs
//! inode takes. The first of each kind has Linux's own
//! (`linux/nsfs.h`'s `*_NS_INIT_INO`); the ones made after it count up from
//! one counter that user namespaces do not use, so no two kinds share a number.
//!
//! # Locks
//!
//! The proxy's lock is a leaf, cloned out before anything is done with what it
//! names. Everything a new namespace needs is allocated and charged before it
//! is taken.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_kmem::Charge;
use ferrix_linux_abi::errno::Errno;
use ferrix_sync::Once;

use crate::object::job::{self, Job};
use crate::syscall::process::Process;
use crate::syscall::sem::IpcNamespace;
use crate::syscall::system::UtsNamespace;
use crate::syscall::userns::{self, UserNamespace};

/// `CLONE_NEWCGROUP`.
pub(crate) const CLONE_NEWCGROUP: u64 = 0x0200_0000;
/// `CLONE_NEWUTS`.
pub(crate) const CLONE_NEWUTS: u64 = 0x0400_0000;
/// `CLONE_NEWIPC`.
pub(crate) const CLONE_NEWIPC: u64 = 0x0800_0000;

/// `UTS_NS_INIT_INO`.
pub(crate) const UTS_INIT_ID: u64 = 0xEFFF_FFFE;
/// `IPC_NS_INIT_INO`.
pub(crate) const IPC_INIT_ID: u64 = 0xEFFF_FFFF;
/// `CGROUP_NS_INIT_INO`.
pub(crate) const CGROUP_INIT_ID: u64 = 0xEFFF_FFFB;

/// The number of the next UTS, IPC or cgroup namespace.
static NEXT_ID: AtomicU64 = AtomicU64::new(0xF900_0000);

/// A number nobody has.
pub(crate) fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// A cgroup namespace: a cgroup made the root of what its members see.
#[derive(Debug)]
pub(crate) struct CgroupNamespace {
    /// What `/proc/<pid>/ns/cgroup` names.
    id: u64,
    /// The user namespace that was current when it was made.
    owner: Arc<UserNamespace>,
    /// Its root: the cgroup its creator was in.
    root: Arc<Job>,
    /// The kernel heap this is, charged to the job that made it (F-37).
    _charge: Option<Charge>,
}

/// The first cgroup namespace, rooted at the tree's root.
pub(crate) fn initial_cgroup() -> &'static Arc<CgroupNamespace> {
    static FIRST: Once<Arc<CgroupNamespace>> = Once::new();
    FIRST.call_once(|| {
        Arc::new(CgroupNamespace {
            id: CGROUP_INIT_ID,
            owner: Arc::clone(userns::first()),
            root: Arc::clone(job::root()),
            _charge: None,
        })
    })
}

impl CgroupNamespace {
    /// What `/proc/<pid>/ns/cgroup` names.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// The user namespace that owns it.
    pub(crate) fn owner(&self) -> &Arc<UserNamespace> {
        &self.owner
    }

    /// The cgroup that is its root.
    pub(crate) fn root(&self) -> &Arc<Job> {
        &self.root
    }

    /// A namespace rooted at `root`, owned by `owner`.
    ///
    /// # Errors
    ///
    /// `ENOMEM` past the job's memory.
    pub(crate) fn rooted_at(
        root: Arc<Job>,
        owner: Arc<UserNamespace>,
    ) -> Result<Arc<CgroupNamespace>, Errno> {
        let charge = Charge::arc::<CgroupNamespace>().map_err(|_| Errno::ENOMEM)?;
        crate::fallible::try_arc(CgroupNamespace {
            id: next_id(),
            owner,
            root,
            _charge: Some(charge),
        })
        .map_err(|_| Errno::ENOMEM)
    }
}

/// The three namespaces a process is in.
#[derive(Debug, Clone)]
pub(crate) struct NsProxy {
    /// Its host and domain name.
    pub(crate) uts: Arc<UtsNamespace>,
    /// Its System V semaphores.
    pub(crate) ipc: Arc<IpcNamespace>,
    /// Its view of the cgroup tree.
    pub(crate) cgroup: Arc<CgroupNamespace>,
}

impl NsProxy {
    /// The first of each: what the kernel's own processes are in.
    pub(crate) fn initial() -> NsProxy {
        NsProxy {
            uts: Arc::clone(crate::syscall::system::initial_uts()),
            ipc: Arc::clone(crate::syscall::sem::initial_ipc()),
            cgroup: Arc::clone(initial_cgroup()),
        }
    }
}

/// The proxy of the process making the call: the running task's, or the one a
/// boot check is acting as, or the first's for the kernel's own reads.
pub(crate) fn acting() -> NsProxy {
    match userns::acting() {
        Some(process) => process.nsproxy(),
        None => NsProxy::initial(),
    }
}

/// The namespaces `flags` asks `process` for, made but not yet entered, and
/// the refusals that concern the creator: `EPERM` without `CAP_SYS_ADMIN` in
/// the user namespace the creator will be in -- `owner`, the new one if
/// `CLONE_NEWUSER` is asked beside them. Nothing changes if it is refused.
///
/// # Errors
///
/// `EPERM`; `ENOMEM` past the job's memory.
pub(crate) fn make(
    process: &Process,
    flags: u64,
    owner: &Arc<UserNamespace>,
    cgroup_root: Arc<Job>,
) -> Result<NsProxy, Errno> {
    let mut proxy = process.nsproxy();
    if flags & (CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWCGROUP) == 0 {
        return Ok(proxy);
    }
    // In the namespace it will be in: a creator that asks for a user
    // namespace with them holds every capability there.
    let allowed = process.with_credentials(|held| {
        if held.user_ns.same(owner) {
            held.holds(userns::CAP_SYS_ADMIN)
        } else {
            true
        }
    });
    if !allowed {
        return Err(Errno::EPERM);
    }
    if flags & CLONE_NEWUTS != 0 {
        proxy.uts = proxy.uts.copy(Arc::clone(owner))?;
    }
    if flags & CLONE_NEWIPC != 0 {
        proxy.ipc = IpcNamespace::empty(Arc::clone(owner))?;
    }
    if flags & CLONE_NEWCGROUP != 0 {
        proxy.cgroup = CgroupNamespace::rooted_at(cgroup_root, Arc::clone(owner))?;
    }
    Ok(proxy)
}
