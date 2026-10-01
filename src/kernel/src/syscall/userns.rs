//! User namespaces: who a process is allowed to be, and what it may do
//! there (`docs/NAMESPACES.md` §2.2).
//!
//! # Credentials hold kernel ids
//!
//! A [`UserNamespace`] changes nothing about an id a process holds. Every id in
//! [`Credentials`](crate::syscall::credentials::Credentials) is a *kernel* id,
//! and a namespace changes how ids are shown to a program and accepted from it
//! at the system-call boundary ([`from_kuid`], [`make_kuid`]), and which
//! capabilities a process has over which objects ([`capable_over`]). A kernel
//! id of 0 is reachable only through a map a writer whose own kernel id is 0
//! wrote (rule U1): an unprivileged writer maps only its own effective id.
//!
//! # Capabilities
//!
//! In the first namespace nothing changes: an effective uid of 0 stands for
//! every capability (`credentials.rs` says why), and the sets are kept and
//! reported but not enforced. In any other namespace they are real. A process
//! that makes a namespace has every capability in it and none outside it.
//!
//! # Locks
//!
//! A namespace's two maps and its `setgroups` switch are behind one spin lock,
//! taken for a lookup or a write and never held across anything else. Nothing
//! is allocated under it.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_kmem::Charge;
use ferrix_linux_abi::errno::Errno;
use ferrix_sync::Once;

use crate::sync::SpinLock;
use crate::syscall::credentials::{CAP_LAST_CAP, Credentials};

/// What a namespace shows for an id it does not map: Linux's `overflowuid`
/// and `overflowgid`, which `/proc/sys/kernel` says too.
pub(crate) const OVERFLOW_ID: u32 = 65_534;

/// Deepest nesting of user namespaces: Linux's limit, `EUSERS` past it.
const MAX_LEVEL: u32 = 32;

/// Extents in one map: Linux's small-map limit.
pub(crate) const MAX_EXTENTS: usize = 5;

/// The most a map file takes in one write: a page.
const MAP_WRITE_MAX: usize = 4096;

/// `CAP_SETGID`.
pub(crate) const CAP_SETGID: u32 = 6;
/// `CAP_SETUID`.
pub(crate) const CAP_SETUID: u32 = 7;
/// `CAP_SETPCAP`.
pub(crate) const CAP_SETPCAP: u32 = 8;
/// `CAP_SYS_CHROOT`.
pub(crate) const CAP_SYS_CHROOT: u32 = 18;
/// `CAP_SYS_ADMIN`.
pub(crate) const CAP_SYS_ADMIN: u32 = 21;

/// Every capability Linux defines, as a set.
pub(crate) const FULL: u64 = (1_u64 << (CAP_LAST_CAP + 1)) - 1;

/// What a capability in a child namespace allows. Nothing else is honoured
/// there (`docs/NAMESPACES.md` §2.2): not `CAP_DAC_OVERRIDE`, `CAP_FOWNER`,
/// `CAP_CHOWN`, `CAP_KILL` or `CAP_MKNOD`.
pub(crate) const HONOURED: u64 = (1 << CAP_SETGID)
    | (1 << CAP_SETUID)
    | (1 << CAP_SETPCAP)
    | (1 << CAP_SYS_CHROOT)
    | (1 << CAP_SYS_ADMIN);

/// A process's four capability sets, as Linux's 64-bit masks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CapSets {
    /// What the process may do now.
    pub(crate) effective: u64,
    /// What it may raise into the effective set.
    pub(crate) permitted: u64,
    /// What `execve` may carry over.
    pub(crate) inheritable: u64,
    /// The most `execve` can ever give it.
    pub(crate) bounding: u64,
}

impl CapSets {
    /// Root's, in the first namespace: everything effective and permitted,
    /// nothing inheritable, nothing held back.
    pub(crate) const ROOT: CapSets = CapSets {
        effective: FULL,
        permitted: FULL,
        inheritable: 0,
        bounding: FULL,
    };

    /// What a process that has just made a namespace has in it: Linux's
    /// `set_cred_user_ns`.
    pub(crate) const FRESH: CapSets = CapSets {
        effective: FULL,
        permitted: FULL,
        inheritable: 0,
        bounding: FULL,
    };

    /// What `execve` leaves in a child namespace, without file capabilities
    /// (Linux's `cap_bprm_creds_from_file`): a process whose effective id is
    /// its namespace's root gets the bounding set; any other, nothing.
    pub(crate) fn after_exec(self, is_root: bool) -> CapSets {
        let given = if is_root { self.bounding } else { 0 };
        CapSets {
            effective: given,
            permitted: given,
            inheritable: self.inheritable,
            bounding: self.bounding,
        }
    }
}

/// One run of ids: `count` ids from `inside` are `count` ids from `outside`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Extent {
    /// First id as the namespace names it.
    pub(crate) inside: u32,
    /// First id as the kernel names it.
    pub(crate) outside: u32,
    /// How many.
    pub(crate) count: u32,
}

/// Up to [`MAX_EXTENTS`] runs, written once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IdMap {
    /// The runs, the first `len` of them.
    extents: [Extent; MAX_EXTENTS],
    /// How many are set; zero is "not written".
    len: usize,
}

impl IdMap {
    /// Nothing mapped.
    const EMPTY: IdMap = IdMap {
        extents: [Extent {
            inside: 0,
            outside: 0,
            count: 0,
        }; MAX_EXTENTS],
        len: 0,
    };

    /// Every id but `-1`, to itself: the first namespace's.
    const IDENTITY: IdMap = {
        let mut map = IdMap::EMPTY;
        map.extents[0] = Extent {
            inside: 0,
            outside: 0,
            count: u32::MAX,
        };
        map.len = 1;
        map
    };

    /// The extents set.
    pub(crate) fn extents(&self) -> &[Extent] {
        self.extents.get(..self.len).unwrap_or_default()
    }

    /// The kernel id of `id` as the namespace names it.
    fn to_kernel(self, id: u32) -> Option<u32> {
        self.extents().iter().find_map(|extent| {
            let offset = id.checked_sub(extent.inside)?;
            (offset < extent.count).then(|| extent.outside.wrapping_add(offset))
        })
    }

    /// The kernel id of the whole run of `count` ids from `first`, which must
    /// lie inside one extent: Linux's `map_id_range_down`. A run that spans
    /// two extents is refused even when both ends map with the same offset,
    /// for the ids between were never given to the namespace.
    fn range_to_kernel(&self, first: u32, count: u32) -> Option<u32> {
        self.extents().iter().find_map(|extent| {
            let offset = first.checked_sub(extent.inside)?;
            (u64::from(offset) + u64::from(count) <= u64::from(extent.count))
                .then(|| extent.outside.wrapping_add(offset))
        })
    }

    /// The namespace's name for kernel id `kernel`.
    fn inside_id(&self, kernel: u32) -> Option<u32> {
        self.extents().iter().find_map(|extent| {
            let offset = kernel.checked_sub(extent.outside)?;
            (offset < extent.count).then(|| extent.inside.wrapping_add(offset))
        })
    }
}

/// What the two maps and the `setgroups` switch hold, under one lock.
#[derive(Debug, Clone, Copy)]
struct Maps {
    /// User ids.
    uid: IdMap,
    /// Group ids.
    gid: IdMap,
    /// Whether `setgroups` is allowed: until `deny` is written, and never in a
    /// child of a namespace that denied it.
    setgroups: bool,
}

/// Which map a call is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `uid_map`.
    User,
    /// `gid_map`.
    Group,
}

/// A user namespace.
#[derive(Debug)]
pub(crate) struct UserNamespace {
    /// The namespace it was made in; `None` for the first.
    parent: Option<Arc<UserNamespace>>,
    /// 0 for the first.
    level: u32,
    /// Its creator's effective ids, as kernel ids: the creator has every
    /// capability in it, from outside.
    owner_uid: u32,
    /// What `/proc/<pid>/ns/user` names.
    id: u64,
    /// The maps.
    maps: SpinLock<Maps>,
    /// The kernel heap this is, charged to the job that made it (F-37).
    _charge: Option<Charge>,
}

/// `/proc/<pid>/ns/user`'s number for the first namespace: Linux's own.
const FIRST_ID: u64 = 0xEFFF_FFFD;
/// And for the ones made after it: a range of its own, so that no user
/// namespace shares a number with a mount namespace (from 0xF0000000) or a
/// UTS, IPC or cgroup one (from 0xF9000000).
static NEXT_ID: AtomicU64 = AtomicU64::new(0xF800_0000);

/// The process a boot check is acting as, when no task is running: the
/// kernel's own self-checks drive system calls on behalf of a process of their
/// own, and a procfs file they open or write has to know whose ids to judge.
/// Always `None` outside [`acting_as`].
///
/// With the id of the task that set it, so that it answers only for that
/// task: another thread asking meanwhile, an interrupt's work or a second
/// check, is not made to act as the check's process.
static ACTING: SpinLock<Option<(Arc<crate::syscall::process::Process>, Option<u64>)>> =
    SpinLock::new(None);

/// The process making the call: the running task's, or the one a boot check is
/// acting as.
pub(crate) fn acting() -> Option<Arc<crate::syscall::process::Process>> {
    crate::syscall::process::current().or_else(|| {
        let slot = ACTING.lock();
        let (process, task) = slot.as_ref()?;
        (*task == crate::sched::current_id()).then(|| Arc::clone(process))
    })
}

/// Run `body` with `process` as [`acting`]'s answer when no task is running.
/// For the boot checks alone.
///
/// [`acting`] consults the setting only when no task is current, which is a
/// kernel thread; a program's own call never reaches it. It is set once, for
/// one body: a setting already there is a nested or leaked one, which would
/// have a check quietly impersonate another process, so it is refused.
///
/// # Errors
///
/// A setting was already there.
pub(crate) fn acting_as<R>(
    process: &Arc<crate::syscall::process::Process>,
    body: impl FnOnce() -> R,
) -> Result<R, &'static str> {
    {
        let mut slot = ACTING.lock();
        if slot.is_some() {
            return Err("acting_as was nested, or a check left it set");
        }
        *slot = Some((Arc::clone(process), crate::sched::current_id()));
    }
    let answer = body();
    *ACTING.lock() = None;
    Ok(answer)
}

/// The first namespace, made on first use.
pub(crate) fn first() -> &'static Arc<UserNamespace> {
    static FIRST: Once<Arc<UserNamespace>> = Once::new();
    FIRST.call_once(|| {
        Arc::new(UserNamespace {
            parent: None,
            level: 0,
            owner_uid: 0,
            id: FIRST_ID,
            maps: SpinLock::new(Maps {
                uid: IdMap::IDENTITY,
                gid: IdMap::IDENTITY,
                setgroups: true,
            }),
            _charge: None,
        })
    })
}

impl PartialEq for UserNamespace {
    fn eq(&self, other: &Self) -> bool {
        self.same(other)
    }
}

impl Eq for UserNamespace {}

impl UserNamespace {
    /// Whether this is the first namespace.
    pub(crate) fn is_first(&self) -> bool {
        self.parent.is_none()
    }

    /// Whether `self` and `other` are one namespace.
    pub(crate) fn same(&self, other: &UserNamespace) -> bool {
        core::ptr::eq(self, other)
    }

    /// What `/proc/<pid>/ns/user` names.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// The namespace it was made in; `None` for the first.
    pub(crate) fn parent(&self) -> Option<&Arc<UserNamespace>> {
        self.parent.as_ref()
    }

    /// Its creator's effective uid, as a kernel id: `NS_GET_OWNER_UID`.
    pub(crate) fn owner_uid(&self) -> u32 {
        self.owner_uid
    }

    /// Whether `setgroups` is allowed.
    pub(crate) fn setgroups_allowed(&self) -> bool {
        self.maps.lock().setgroups
    }

    /// The map of `kind`.
    pub(crate) fn map(&self, kind: Kind) -> IdMap {
        let maps = self.maps.lock();
        match kind {
            Kind::User => maps.uid,
            Kind::Group => maps.gid,
        }
    }

    /// Whether the map of `kind` is written.
    pub(crate) fn mapped_any(&self, kind: Kind) -> bool {
        self.map(kind).len > 0
    }

    /// Linux's `make_kuid` and `make_kgid`: the kernel id of `id` as this
    /// namespace names it, or `None` when it names no id.
    pub(crate) fn make_kid(&self, kind: Kind, id: u32) -> Option<u32> {
        self.map(kind).to_kernel(id)
    }

    /// Linux's `from_kuid` and `from_kgid`: this namespace's name for a
    /// kernel id, or `None` when it has none.
    pub(crate) fn name_of(&self, kind: Kind, kernel: u32) -> Option<u32> {
        self.map(kind).inside_id(kernel)
    }
}

/// The name `ns` gives a kernel user or group id, [`OVERFLOW_ID`] when it
/// gives none: what a program is told (Linux's `from_kuid_munged`).
pub(crate) fn from_kid_munged(ns: &UserNamespace, kind: Kind, kernel: u32) -> u32 {
    ns.name_of(kind, kernel).unwrap_or(OVERFLOW_ID)
}

/// Linux's `cap_capable`: whether a process with `credentials` holds `cap`
/// over `target`, a namespace.
///
/// It holds it if `target` is the namespace it is in and `cap` is effective
/// there (in the first namespace: its effective uid is 0); or if `target` is a
/// descendant of the one it is in, reached through a namespace whose owner is
/// the process's effective kernel uid -- a namespace's creator has every
/// capability in it from the outside.
pub(crate) fn capable_over(credentials: &Credentials, target: &UserNamespace, cap: u32) -> bool {
    let own = &credentials.user_ns;
    let mut at = target;
    while at.level > own.level {
        if at.level == own.level + 1
            && at.parent.as_deref().is_some_and(|parent| parent.same(own))
            && at.owner_uid == credentials.user.effective
        {
            return true;
        }
        match &at.parent {
            Some(parent) => at = parent,
            None => return false,
        }
    }
    at.same(own) && credentials.holds(cap)
}

/// Make the namespace a process with `creator`'s credentials asks for with
/// `CLONE_NEWUSER` (the refusals of `docs/NAMESPACES.md` §2.2 that concern the
/// creator's credentials; the ones that concern its context are the caller's).
///
/// # Errors
///
/// `EUSERS` past 32 levels; `EPERM` when the creator's effective uid or gid is
/// not mapped in its own namespace; `ENOMEM` past the job's memory.
pub(crate) fn create(creator: &Credentials) -> Result<Arc<UserNamespace>, Errno> {
    let parent = &creator.user_ns;
    if parent.level >= MAX_LEVEL {
        return Err(Errno::EUSERS);
    }
    if parent.name_of(Kind::User, creator.user.effective).is_none()
        || parent
            .name_of(Kind::Group, creator.group.effective)
            .is_none()
    {
        return Err(Errno::EPERM);
    }
    let charge = Charge::arc::<UserNamespace>().map_err(|_| Errno::ENOMEM)?;
    let namespace = UserNamespace {
        parent: Some(Arc::clone(parent)),
        level: parent.level + 1,
        owner_uid: creator.user.effective,
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        maps: SpinLock::new(Maps {
            uid: IdMap::EMPTY,
            gid: IdMap::EMPTY,
            setgroups: parent.setgroups_allowed(),
        }),
        _charge: Some(charge),
    };
    crate::fallible::try_arc(namespace).map_err(|_| Errno::ENOMEM)
}

/// What `uid_map` or `gid_map` reads as to a reader in `reader`: each extent
/// with the outside id in the reader's terms (or the namespace's parent's,
/// when the reader is in the namespace itself), as Linux's `uid_m_show`
/// prints them.
pub(crate) fn render_map(ns: &UserNamespace, kind: Kind, reader: &UserNamespace) -> Vec<u8> {
    let lower = if reader.same(ns) {
        ns.parent.as_deref().unwrap_or(ns)
    } else {
        reader
    };
    let mut out = Vec::new();
    for extent in ns.map(kind).extents() {
        let Some(outside) = lower.name_of(kind, extent.outside) else {
            continue;
        };
        out.extend_from_slice(
            alloc::format!(
                "{:>10} {:>10} {:>10}\n",
                extent.inside,
                outside,
                extent.count
            )
            .as_bytes(),
        );
    }
    out
}

/// What `setgroups` reads as: `allow` or `deny`.
pub(crate) fn render_setgroups(ns: &UserNamespace) -> Vec<u8> {
    if ns.setgroups_allowed() {
        b"allow\n".to_vec()
    } else {
        b"deny\n".to_vec()
    }
}

/// Parse a map written: lines of `inside outside count`, at most
/// [`MAX_EXTENTS`], counts non-zero, no overflow, no overlap on either side.
/// `EINVAL` for anything else.
fn parse(data: &[u8]) -> Result<Vec<Extent>, Errno> {
    let text = core::str::from_utf8(data).map_err(|_| Errno::EINVAL)?;
    let mut extents: Vec<Extent> = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let mut next = || -> Result<u32, Errno> {
            fields
                .next()
                .ok_or(Errno::EINVAL)?
                .parse::<u32>()
                .map_err(|_| Errno::EINVAL)
        };
        let (inside, outside, count) = (next()?, next()?, next()?);
        if fields.next().is_some() || count == 0 {
            return Err(Errno::EINVAL);
        }
        // `-1` is no id, so the last id a run may reach is `u32::MAX - 1`.
        if u64::from(inside) + u64::from(count) > u64::from(u32::MAX)
            || u64::from(outside) + u64::from(count) > u64::from(u32::MAX)
        {
            return Err(Errno::EINVAL);
        }
        if extents.len() == MAX_EXTENTS {
            return Err(Errno::EINVAL);
        }
        extents.push(Extent {
            inside,
            outside,
            count,
        });
    }
    if extents.is_empty() {
        return Err(Errno::EINVAL);
    }
    for (index, a) in extents.iter().enumerate() {
        for b in extents.iter().skip(index + 1) {
            let disjoint = |first: fn(&Extent) -> u32| {
                u64::from(first(a)) + u64::from(a.count) <= u64::from(first(b))
                    || u64::from(first(b)) + u64::from(b.count) <= u64::from(first(a))
            };
            if !disjoint(|e| e.inside) || !disjoint(|e| e.outside) {
                return Err(Errno::EINVAL);
            }
        }
    }
    Ok(extents)
}

/// Write `uid_map` or `gid_map` of `ns`: Linux's `map_write` and
/// `new_idmap_permitted`. `opener` is who opened the file and `writer` who is
/// writing it; both are judged, as Linux judges `f_cred` and `current`.
///
/// # Errors
///
/// `EINVAL` for a malformed write; `EPERM` for a map already written, for a
/// writer or opener that may not write it, and for an outside id the parent
/// does not map.
pub(crate) fn write_map(
    ns: &UserNamespace,
    kind: Kind,
    opener: &Credentials,
    writer: &Credentials,
    data: &[u8],
) -> Result<usize, Errno> {
    let Some(parent) = ns.parent.as_deref() else {
        // The first namespace's identity map is not written.
        return Err(Errno::EPERM);
    };
    if data.len() >= MAP_WRITE_MAX {
        return Err(Errno::EINVAL);
    }
    // The opener and the writer are each in the namespace or its parent, and
    // hold CAP_SYS_ADMIN over it.
    for who in [opener, writer] {
        let placed = who.user_ns.same(ns) || who.user_ns.same(parent);
        if !placed || !capable_over(who, ns, CAP_SYS_ADMIN) {
            return Err(Errno::EPERM);
        }
    }
    let mut extents = parse(data)?;
    // Each outside range is ids the parent maps, all in one of its extents;
    // `outside` becomes the kernel id.
    let parent_map = parent.map(kind);
    for extent in &mut extents {
        let Some(kernel) = parent_map.range_to_kernel(extent.outside, extent.count) else {
            return Err(Errno::EPERM);
        };
        extent.outside = kernel;
    }
    let (effective, cap) = match kind {
        Kind::User => (opener.user.effective, CAP_SETUID),
        Kind::Group => (opener.group.effective, CAP_SETGID),
    };
    let mut maps = ns.maps.lock();
    let slot = match kind {
        Kind::User => &maps.uid,
        Kind::Group => &maps.gid,
    };
    if slot.len > 0 {
        return Err(Errno::EPERM);
    }
    // Unprivileged: one id, the opener's own effective id, and for groups
    // only once `setgroups` is denied (CVE-2014-8989); and the opener made the
    // namespace. Otherwise both need the capability over the parent.
    let alone = extents.len() == 1
        && extents.first().is_some_and(|extent| {
            extent.count == 1
                && extent.outside == effective
                && (kind == Kind::User || !maps.setgroups)
        });
    let unprivileged = alone && opener.user.effective == ns.owner_uid;
    if !unprivileged && !(capable_over(opener, parent, cap) && capable_over(writer, parent, cap)) {
        return Err(Errno::EPERM);
    }
    let mut written = IdMap::EMPTY;
    for (slot, extent) in written.extents.iter_mut().zip(&extents) {
        *slot = *extent;
    }
    written.len = extents.len();
    match kind {
        Kind::User => maps.uid = written,
        Kind::Group => maps.gid = written,
    }
    Ok(data.len())
}

/// Write `setgroups` of `ns`: `allow` or `deny`, each allowed only while it
/// changes nothing already relied on (CVE-2014-8989): never after `gid_map`
/// is written, and never `allow` once `deny` was.
///
/// # Errors
///
/// `EINVAL` for any other text; `EPERM` as above.
pub(crate) fn write_setgroups(
    ns: &UserNamespace,
    opener: &Credentials,
    writer: &Credentials,
    data: &[u8],
) -> Result<usize, Errno> {
    let Some(parent) = ns.parent.as_deref() else {
        return Err(Errno::EPERM);
    };
    if data.len() >= MAP_WRITE_MAX {
        return Err(Errno::EINVAL);
    }
    let allow = match data.trim_ascii_end() {
        b"allow" => true,
        b"deny" => false,
        _ => return Err(Errno::EINVAL),
    };
    for who in [opener, writer] {
        let placed = who.user_ns.same(ns) || who.user_ns.same(parent);
        if !placed || !capable_over(who, ns, CAP_SYS_ADMIN) {
            return Err(Errno::EPERM);
        }
    }
    let mut maps = ns.maps.lock();
    // Linux's `setgroups_write`: `allow` is refused once `deny` was written
    // and is otherwise a no-op; `deny` is refused once `gid_map` is.
    if allow {
        if !maps.setgroups {
            return Err(Errno::EPERM);
        }
    } else if maps.gid.len > 0 {
        return Err(Errno::EPERM);
    } else {
        maps.setgroups = false;
    }
    Ok(data.len())
}
