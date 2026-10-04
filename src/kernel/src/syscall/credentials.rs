//! Who a process is: its user and group ids, its supplementary groups, and
//! the capabilities those stand for.
//!
//! # What the ids are for, today
//!
//! Every process carries Linux's four user ids and four group ids and a list
//! of supplementary groups, on its `Process`. `fork` copies them. `execve`
//! keeps the real and effective ids and the groups, and makes the saved and
//! filesystem ids the effective ones, as Linux does. The calls here change them
//! by Linux's rules and report them back, and the kernel acts on them: the
//! VFS checks file permissions against the filesystem ids, and the calls only
//! root may make, and those that reach another user's processes, ask
//! [`require_privilege`], [`same_owner`] and [`may_signal_ids`] here.
//!
//! # Privilege is an effective uid of 0
//!
//! Linux decides who may take ids they do not have with `CAP_SETUID` and
//! `CAP_SETGID`. Root holds both, and loses them as its ids change
//! (`cap_emulate_setxuid`): the effective set empties when the effective uid
//! leaves 0, and every set once no uid is 0. There are no capability sets to
//! track here, so an effective uid of 0 stands for both, and `capget` reports
//! every set empty for any other.
//!
//! The one case that answers differently is a process whose real or saved uid
//! is 0 and whose effective uid is not. Linux leaves it a permitted set it can
//! raise again with `capset`; here `capset` refuses it. It can still return
//! with `setuid(0)` or `seteuid(0)`, because 0 is one of its own ids, and that
//! is how every program that drops privilege for a while takes it back.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;

use crate::syscall::attributes::{self, int, subject};
use crate::syscall::process::Process;
use crate::syscall::uaccess;
use crate::syscall::userns::{self, CAP_SETGID, CAP_SETPCAP, CAP_SETUID, CapSets, UserNamespace};

/// `(uid_t)-1` and `(gid_t)-1`: "unchanged" to the calls that take several,
/// and not a valid id to the ones that take one.
const UNCHANGED: u32 = u32::MAX;

/// The most supplementary groups a process may have: Linux's `NGROUPS_MAX`.
const NGROUPS_MAX: u32 = 65_536;

/// `EPERM` unless `process` may make a call only root may: an effective uid
/// of 0, standing in for the capability Linux checks for that call
/// (`CAP_SYS_ADMIN`, `CAP_SYS_BOOT`, `CAP_SYS_TIME`, `CAP_MKNOD`,
/// `CAP_SYS_CHROOT`, `CAP_SYS_RESOURCE`, `CAP_SYS_NICE`, `CAP_SYSLOG`).
///
/// # Errors
///
/// `EPERM`.
pub(crate) fn require_privilege(process: &Process) -> Result<(), Errno> {
    if process.with_credentials(|ids| ids.privileged()) {
        Ok(())
    } else {
        Err(Errno::EPERM)
    }
}

/// Linux's `ptrace_may_access`, for a process reading what another process
/// keeps private (`docs/NAMESPACES.md` M8): its `/proc/<pid>/root`, `cwd`,
/// `exe`, `fd`, `fdinfo`, `maps` and `ns/*`, and its robust-list head.
///
/// Allowed for the process itself; and for a caller either whose filesystem
/// user and group ids (its real ones when `real` asks, as `get_robust_list`
/// does) are every one of the target's real, effective and saved ids, or with
/// `CAP_SYS_PTRACE` over the target's user namespace -- in the first
/// namespace root, and from a child namespace nothing into a process its
/// namespace does not own -- but, of a target that is not dumpable (it
/// `execve`d a set-id program, changed its ids, or cleared `PR_SET_DUMPABLE`),
/// only when it is privileged in the first namespace.
pub(crate) fn may_access(caller: &Process, target: &Process, real: bool) -> bool {
    if core::ptr::eq(caller, target) {
        return true;
    }
    let (uid, gid, caps_holder) = caller.with_credentials(|held| {
        let (uid, gid) = if real {
            (held.user.real, held.group.real)
        } else {
            (held.user.filesystem, held.group.filesystem)
        };
        (uid, gid, held.clone())
    });
    let (same, namespace) = target.with_credentials(|held| {
        let same = [held.user.real, held.user.effective, held.user.saved]
            .iter()
            .all(|&id| id == uid)
            && [held.group.real, held.group.effective, held.group.saved]
                .iter()
                .all(|&id| id == gid);
        (same, Arc::clone(&held.user_ns))
    });
    let capable = userns::capable_over(&caps_holder, &namespace, userns::CAP_SYS_PTRACE);
    // Linux's `__ptrace_may_access`: the ids or the capability, and then, for
    // a target that is not dumpable -- or not yet given its attributes -- the
    // capability in the namespace its program was loaded in, which is the
    // first one here: `privileged()`, whatever the target's own namespace
    // makes of its owner. Otherwise a process that cleared `PR_SET_DUMPABLE` and
    // made a namespace is readable by every process of its user.
    let dumpable = !target.attributes_pending() && attributes::get(target).dumpable;
    (same || capable) && (dumpable || caps_holder.privileged())
}

/// Linux's `check_same_owner`: whether `caller` may change `target`'s
/// scheduling -- its effective uid is the target's real or effective uid, or
/// it is privileged.
pub(crate) fn same_owner(caller: &Process, target: &Process) -> bool {
    let (euid, privileged) = caller.with_credentials(|ids| (ids.user.effective, ids.privileged()));
    privileged || target.with_credentials(|ids| euid == ids.user.real || euid == ids.user.effective)
}

/// Linux's `kill_ok_by_cred`: whether `caller` may signal `target` by their
/// ids -- the caller's real or effective uid is the target's real or saved
/// uid, or the caller is privileged.
pub(crate) fn may_signal_ids(caller: &Process, target: &Process) -> bool {
    let (real, effective, privileged) =
        caller.with_credentials(|ids| (ids.user.real, ids.user.effective, ids.privileged()));
    privileged
        || target.with_credentials(|ids| {
            [real, effective]
                .into_iter()
                .any(|id| id == ids.user.real || id == ids.user.saved)
        })
}

/// Linux's `check_prlimit_permission`: whether `caller` may read or change
/// another process's limits -- every one of the target's real, effective and
/// saved user and group ids is the caller's real one, or the caller is
/// privileged.
pub(crate) fn same_ids(caller: &Process, target: &Process) -> bool {
    let (uid, gid, privileged) =
        caller.with_credentials(|ids| (ids.user.real, ids.group.real, ids.privileged()));
    privileged
        || target.with_credentials(|ids| {
            [ids.user.real, ids.user.effective, ids.user.saved] == [uid; 3]
                && [ids.group.real, ids.group.effective, ids.group.saved] == [gid; 3]
        })
}

/// The highest capability number Linux 6.x defines, `CAP_CHECKPOINT_RESTORE`.
/// `CAP_LAST_CAP` in `linux/capability.h`, 40, for both builds.
pub(crate) const CAP_LAST_CAP: u32 = 40;

/// `_LINUX_CAPABILITY_VERSION_1`: one 32-bit set per field.
const CAPABILITY_VERSION_1: u32 = 0x1998_0330;
/// `_LINUX_CAPABILITY_VERSION_2`, deprecated for version 3 and the same shape.
const CAPABILITY_VERSION_2: u32 = 0x2007_1026;
/// `_LINUX_CAPABILITY_VERSION_3`: two 32-bit words per set.
const CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// Bytes in one `struct __user_cap_data_struct`: three `__u32`s. Twelve on
/// both builds, and the header eight; checked with `sizeof` against
/// `linux/capability.h` compiled for x86-64 and `arm-linux-gnueabihf`.
const CAP_DATA: usize = 12;

/// One kind of id, user or group, in the four roles Linux gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ids {
    /// Who started the process: what `getuid` reports.
    pub(crate) real: u32,
    /// Who the process acts as: what `geteuid` reports, and for the user ids
    /// what decides privilege.
    pub(crate) effective: u32,
    /// An id the process may return to without privilege.
    pub(crate) saved: u32,
    /// The id a permission check will use. Follows the effective id, unless
    /// `setfsuid` or `setfsgid` moved it.
    pub(crate) filesystem: u32,
}

impl Ids {
    /// Every role 0: root's.
    const ROOT: Ids = Ids {
        real: 0,
        effective: 0,
        saved: 0,
        filesystem: 0,
    };

    /// Whether `id` is one of the three a process may move between without
    /// privilege.
    fn is_own(self, id: u32) -> bool {
        id == self.real || id == self.effective || id == self.saved
    }
}

/// A process's ids and supplementary groups. `Clone` rather than `Copy`
/// because of the group list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Credentials {
    /// The user ids.
    pub(crate) user: Ids,
    /// The group ids.
    pub(crate) group: Ids,
    /// The supplementary groups, sorted, as `setgroups` leaves them on Linux.
    pub(crate) groups: Vec<u32>,
    /// The user namespace the process is in. Every id above is a kernel id,
    /// whatever namespace this is (`docs/NAMESPACES.md` §2.2).
    pub(crate) user_ns: Arc<UserNamespace>,
    /// The capability sets. Real in a child namespace; in the first, an
    /// effective uid of 0 stands for every capability and the sets are kept
    /// and reported, not enforced.
    pub(crate) caps: CapSets,
}

/// Which ids a call is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The `*uid` calls.
    User,
    /// The `*gid` calls.
    Group,
}

impl Credentials {
    /// Root's, which a process the kernel starts has: every id 0, and group 0
    /// as the one supplementary group, as on Linux.
    pub(crate) fn root() -> Credentials {
        Credentials {
            user: Ids::ROOT,
            group: Ids::ROOT,
            groups: Vec::from([0]),
            user_ns: Arc::clone(userns::first()),
            caps: CapSets::ROOT,
        }
    }

    /// Whether it may take ids and groups it does not have, and make the
    /// calls only root may. See the module documentation.
    ///
    /// An effective uid of 0 *in the first user namespace*: a process in a
    /// child namespace, whatever its ids say there, is never privileged in
    /// the whole system's sense (rule U1 of `docs/NAMESPACES.md` §4).
    pub(crate) fn privileged(&self) -> bool {
        self.user.effective == 0 && self.user_ns.is_first()
    }

    /// Whether `cap` is effective in the namespace the process is in: in the
    /// first, an effective uid of 0; in a child, the bit, for the capabilities
    /// a child namespace honours at all ([`userns::HONOURED`]).
    pub(crate) fn holds(&self, cap: u32) -> bool {
        if self.user_ns.is_first() {
            self.user.effective == 0
        } else {
            self.caps.effective & userns::HONOURED & (1_u64 << cap) != 0
        }
    }

    /// Whether the kernel id `euid` is the root of the process's namespace:
    /// what makes `execve` give it the bounding set.
    fn is_namespace_root(&self) -> bool {
        self.user_ns
            .name_of(userns::Kind::User, self.user.effective)
            == Some(0)
    }

    /// What `execve` does to them, from `cap_bprm_creds_from_file`: a
    /// set-user-id or set-group-id file's owner becomes the effective id, the
    /// real ids and the groups stay, and the saved and filesystem ids become
    /// the effective ones. Answers `AT_SECURE`, which Linux sets when the
    /// effective id a program starts with is not its real one: a set-id file,
    /// or a process that moved its own effective id before `execve`.
    pub(crate) fn exec(&mut self, set_uid: Option<u32>, set_gid: Option<u32>) -> bool {
        // U7: in a child namespace a set-id bit gives nothing. Stricter than
        // Linux, which honours an owner mapped in.
        let in_child = !self.user_ns.is_first();
        let (set_uid, set_gid) = if in_child {
            (None, None)
        } else {
            (set_uid, set_gid)
        };
        if let Some(uid) = set_uid {
            self.user.effective = uid;
        }
        if let Some(gid) = set_gid {
            self.group.effective = gid;
        }
        for ids in [&mut self.user, &mut self.group] {
            ids.saved = ids.effective;
            ids.filesystem = ids.effective;
        }
        if in_child {
            self.caps = self.caps.after_exec(self.is_namespace_root());
        }
        self.user.effective != self.user.real || self.group.effective != self.group.real
    }

    /// The ids whose change Linux's `commit_creds` answers by making the
    /// process not dumpable: the effective and filesystem user and group ids.
    pub(crate) fn acting(&self) -> [u32; 4] {
        [
            self.user.effective,
            self.user.filesystem,
            self.group.effective,
            self.group.filesystem,
        ]
    }

    /// Whether every effective id is the real one: what `begin_new_exec`
    /// asks of the caller's ids before it decides a new program's
    /// dumpability.
    pub(crate) fn own(&self) -> bool {
        self.user.effective == self.user.real && self.group.effective == self.group.real
    }

    /// The name this process's own namespace gives kernel id `kernel`, or
    /// the overflow id: what a program is told.
    fn shown(&self, kind: Kind, kernel: u32) -> u32 {
        userns::from_kid_munged(&self.user_ns, kind.mapped(), kernel)
    }

    /// Linux's `cap_emulate_setxuid`, in a child namespace, where the sets
    /// are real: a process that loses its namespace's root loses the
    /// capabilities that came with it. `before` is the user ids as they were.
    fn emulate_setxuid(&mut self, before: Ids) {
        if self.user_ns.is_first() {
            return;
        }
        let Some(root) = self.user_ns.make_kid(userns::Kind::User, 0) else {
            return;
        };
        let had_root = [before.real, before.effective, before.saved].contains(&root);
        let has_root = [self.user.real, self.user.effective, self.user.saved].contains(&root);
        if had_root && !has_root {
            self.caps.permitted = 0;
            self.caps.effective = 0;
        }
        if before.effective == root && self.user.effective != root {
            self.caps.effective = 0;
        }
        if before.effective != root && self.user.effective == root {
            self.caps.effective = self.caps.permitted;
        }
    }

    /// The ids of `kind`.
    fn ids(&self, kind: Kind) -> Ids {
        match kind {
            Kind::User => self.user,
            Kind::Group => self.group,
        }
    }

    /// The ids of `kind`, to change.
    fn ids_mut(&mut self, kind: Kind) -> &mut Ids {
        match kind {
            Kind::User => &mut self.user,
            Kind::Group => &mut self.group,
        }
    }
}

impl Kind {
    /// The map this kind of id is translated by.
    fn mapped(self) -> userns::Kind {
        match self {
            Kind::User => userns::Kind::User,
            Kind::Group => userns::Kind::Group,
        }
    }

    /// The capability that lets a process take ids of this kind it does not
    /// have.
    fn capability(self) -> u32 {
        match self {
            Kind::User => CAP_SETUID,
            Kind::Group => CAP_SETGID,
        }
    }
}

/// The kernel id a program's `id` names in `process`'s namespace: `EINVAL`
/// for one the namespace does not map (rule U9), and `-1` ("unchanged")
/// passed through.
fn kernel_id(process: &Process, kind: Kind, id: u32) -> Result<u32, Errno> {
    if id == UNCHANGED {
        return Ok(id);
    }
    process.with_credentials(|credentials| {
        if credentials.user_ns.is_first() {
            Ok(id)
        } else {
            credentials
                .user_ns
                .make_kid(kind.mapped(), id)
                .ok_or(Errno::EINVAL)
        }
    })
}

/// What the running process's namespace calls kernel user id `kernel`, or
/// the overflow id: the one helper every place that *tells* a program an id
/// goes through (`docs/NAMESPACES.md` §2.2). The kernel's own reads, with no
/// running process, see the first namespace's names.
pub(crate) fn show_uid(kernel: u32) -> u32 {
    show(userns::Kind::User, kernel)
}

/// [`show_uid`] for a group id.
pub(crate) fn show_gid(kernel: u32) -> u32 {
    show(userns::Kind::Group, kernel)
}

fn show(kind: userns::Kind, kernel: u32) -> u32 {
    match userns::acting() {
        Some(reader) => {
            reader.with_credentials(|held| userns::from_kid_munged(&held.user_ns, kind, kernel))
        }
        None => kernel,
    }
}

/// The kernel user id a program's `id` names in `process`'s namespace, or
/// `EINVAL` for one it does not map (rule U9). `-1` is passed through.
pub(crate) fn kernel_uid(process: &Process, id: u32) -> Result<u32, Errno> {
    kernel_id(process, Kind::User, id)
}

/// [`kernel_uid`] for a group id.
pub(crate) fn kernel_gid(process: &Process, id: u32) -> Result<u32, Errno> {
    kernel_id(process, Kind::Group, id)
}

/// Answer `call` if it is one of this module's.
pub(crate) fn dispatch(
    call: Syscall,
    a: &[u64; 6],
    process: &Process,
) -> Option<Result<usize, Errno>> {
    let kind = match call {
        Syscall::Setgid
        | Syscall::Setregid
        | Syscall::Setresgid
        | Syscall::Setfsgid
        | Syscall::Getresgid => Kind::Group,
        _ => Kind::User,
    };
    let raw = |slot: usize| a.get(slot).map_or(0, |&value| value as u32);
    // The ids a `set*id` call names, as kernel ids; unmapped is `EINVAL`.
    let id = |slot: usize| kernel_id(process, kind, raw(slot));
    let answer = match call {
        Syscall::Setuid | Syscall::Setgid => id(0).and_then(|new| {
            change(process, kind, |ids, privileged| {
                set_id(ids, new, privileged)
            })
        }),
        Syscall::Setreuid | Syscall::Setregid => id(0).and_then(|real| {
            id(1).and_then(|effective| {
                change(process, kind, |ids, privileged| {
                    set_real_effective(ids, real, effective, privileged)
                })
            })
        }),
        Syscall::Setresuid | Syscall::Setresgid => id(0).and_then(|real| {
            id(1).and_then(|effective| {
                id(2).and_then(|saved| {
                    change(process, kind, |ids, privileged| {
                        set_real_effective_saved(ids, [real, effective, saved], privileged)
                    })
                })
            })
        }),
        // No error return: the previous id, whether or not it changed. A
        // refused change is found by calling again.
        Syscall::Setfsuid | Syscall::Setfsgid => {
            // An id the namespace does not map changes nothing: the old one
            // is the answer either way.
            let asked = id(0).unwrap_or(UNCHANGED);
            Ok(set_filesystem_id(process, kind, asked) as usize)
        }
        Syscall::Getresuid | Syscall::Getresgid => sys_getresid(process, kind, [a[0], a[1], a[2]]),
        Syscall::Getgroups => sys_getgroups(process, int(a[0]), a[1]),
        Syscall::Setgroups => sys_setgroups(process, raw(0), a[1]),
        Syscall::Capget => sys_capget(process, a[0], a[1]),
        Syscall::Capset => sys_capset(process, a[0], a[1]),
        _ => return None,
    };
    Some(answer)
}

/// `getuid`, `geteuid`, `getgid` and `getegid` for `process`, or `None` for
/// any other call. The lock is taken only for one of the four.
pub(crate) fn identity(call: Syscall, process: &Process) -> Option<u32> {
    let pick: fn(&Credentials) -> u32 = match call {
        Syscall::Getuid => |credentials| credentials.shown(Kind::User, credentials.user.real),
        Syscall::Geteuid => |credentials| credentials.shown(Kind::User, credentials.user.effective),
        Syscall::Getgid => |credentials| credentials.shown(Kind::Group, credentials.group.real),
        Syscall::Getegid => {
            |credentials| credentials.shown(Kind::Group, credentials.group.effective)
        }
        _ => return None,
    };
    Some(process.with_credentials(|credentials| pick(credentials)))
}

/// Apply `rule` to `process`'s ids of `kind`, under one lock, privileged as
/// its effective uid was when the call began -- which is when Linux asks
/// `ns_capable_setid`. A rule changes all the ids it names or none of them.
///
/// A change of the ids the process acts as ends its dumpability, as
/// `commit_creds` ends it: see [`attributes::credentials_changed`].
fn change(
    process: &Process,
    kind: Kind,
    rule: impl FnOnce(&mut Ids, bool) -> Result<(), Errno>,
) -> Result<usize, Errno> {
    let (answer, changed) = process.with_credentials(|credentials| {
        let before = credentials.acting();
        let user_before = credentials.user;
        let privileged = credentials.holds(kind.capability());
        let answer = rule(credentials.ids_mut(kind), privileged);
        if answer.is_ok() && kind == Kind::User {
            credentials.emulate_setxuid(user_before);
        }
        (answer, credentials.acting() != before)
    });
    if changed {
        attributes::credentials_changed(process);
    }
    answer.map(|()| 0)
}

/// `setuid` and `setgid`, as `__sys_setuid` has them.
///
/// Privileged, all four ids become `id`. Unprivileged, `id` must be the real
/// or the saved id, and only the effective and filesystem ids change. `-1` is not a valid id -- `uid_valid` fails for
/// it -- so it is `EINVAL`, not "unchanged".
fn set_id(ids: &mut Ids, id: u32, privileged: bool) -> Result<(), Errno> {
    if id == UNCHANGED {
        return Err(Errno::EINVAL);
    }
    if privileged {
        ids.real = id;
        ids.saved = id;
    } else if id != ids.real && id != ids.saved {
        return Err(Errno::EPERM);
    }
    ids.effective = id;
    ids.filesystem = id;
    Ok(())
}

/// `setreuid` and `setregid`, as `__sys_setreuid` has them.
///
/// Unprivileged, the real id may become the real or effective id, and the
/// effective id any of the three. The saved id follows the new effective id
/// whenever the real id is set, or the effective id is set to anything but the
/// old real id -- which is what makes `setreuid(1000, 1000)` permanent and
/// `setreuid(-1, 1000)` from real uid 1000 not.
fn set_real_effective(
    ids: &mut Ids,
    real: u32,
    effective: u32,
    privileged: bool,
) -> Result<(), Errno> {
    let old = *ids;
    let mut new = old;
    if real != UNCHANGED {
        if !privileged && real != old.real && real != old.effective {
            return Err(Errno::EPERM);
        }
        new.real = real;
    }
    if effective != UNCHANGED {
        if !privileged && !old.is_own(effective) {
            return Err(Errno::EPERM);
        }
        new.effective = effective;
    }
    if real != UNCHANGED || (effective != UNCHANGED && effective != old.real) {
        new.saved = new.effective;
    }
    new.filesystem = new.effective;
    *ids = new;
    Ok(())
}

/// `setresuid` and `setresgid`, as `__sys_setresuid` has them: unprivileged,
/// each id asked for must already be one of the three.
fn set_real_effective_saved(
    ids: &mut Ids,
    [real, effective, saved]: [u32; 3],
    privileged: bool,
) -> Result<(), Errno> {
    let old = *ids;
    let asked = [real, effective, saved];
    if !privileged && asked.iter().any(|&id| id != UNCHANGED && !old.is_own(id)) {
        return Err(Errno::EPERM);
    }
    let keep = |id: u32, current: u32| if id == UNCHANGED { current } else { id };
    ids.real = keep(real, old.real);
    ids.effective = keep(effective, old.effective);
    ids.saved = keep(saved, old.saved);
    ids.filesystem = ids.effective;
    Ok(())
}

/// `setfsuid` and `setfsgid`, as `__sys_setfsuid` has them: the filesystem id
/// becomes `id` if it is one of the process's four, or the process is
/// privileged, and the old one is the answer either way. A new one ends the
/// process's dumpability, as in [`change`].
fn set_filesystem_id(process: &Process, kind: Kind, id: u32) -> u32 {
    let (old, changed) = process.with_credentials(|credentials| {
        let privileged = credentials.holds(kind.capability());
        let ids = credentials.ids_mut(kind);
        let old = ids.filesystem;
        if id != UNCHANGED && (privileged || ids.is_own(id) || id == old) {
            ids.filesystem = id;
        }
        let changed = ids.filesystem != old;
        (credentials.shown(kind, old), changed)
    });
    if changed {
        attributes::credentials_changed(process);
    }
    old
}

/// `getresuid` and `getresgid`: three `uid_t`s, 32 bits on every architecture
/// this kernel has (the 16-bit calls are not in its tables), written one after
/// another as `kernel/sys.c` writes them.
fn sys_getresid(process: &Process, kind: Kind, at: [u64; 3]) -> Result<usize, Errno> {
    let ids = process.with_credentials(|credentials| {
        let ids = credentials.ids(kind);
        [ids.real, ids.effective, ids.saved].map(|kernel| credentials.shown(kind, kernel))
    });
    for (address, id) in at.into_iter().zip(ids) {
        uaccess::put_u32(process.space(), address, id)?;
    }
    Ok(0)
}

/// `getgroups`: the supplementary groups.
///
/// A size of zero asks only for the count, which is how `id` sizes its
/// buffer; a size smaller than the count is `EINVAL`, as `kernel/groups.c`
/// answers. Each `gid_t` is 32 bits on all three architectures.
pub(crate) fn sys_getgroups(process: &Process, size: i32, list: u64) -> Result<usize, Errno> {
    let size = usize::try_from(size).map_err(|_| Errno::EINVAL)?;
    let groups: Vec<u32> = process.with_credentials(|credentials| {
        credentials
            .groups
            .iter()
            .map(|&group| credentials.shown(Kind::Group, group))
            .collect()
    });
    if size == 0 {
        return Ok(groups.len());
    }
    if size < groups.len() {
        return Err(Errno::EINVAL);
    }
    let bytes: Vec<u8> = groups
        .iter()
        .flat_map(|group| group.to_le_bytes())
        .collect();
    if !bytes.is_empty() {
        uaccess::copy_to_user(process.space(), list, &bytes).map_err(|_| Errno::EFAULT)?;
    }
    Ok(groups.len())
}

/// `setgroups`, in `kernel/groups.c`'s order: `EPERM` unless privileged, then
/// `EINVAL` for more than [`NGROUPS_MAX`], then the list is read -- `EFAULT`
/// for a bad pointer, `EINVAL` for a group of `-1` -- and stored sorted.
fn sys_setgroups(process: &Process, size: u32, list: u64) -> Result<usize, Errno> {
    // Linux's `may_setgroups`: `CAP_SETGID` in the namespace, and a
    // namespace that has a `gid_map` and has not denied it (U4).
    let allowed = process.with_credentials(|credentials| {
        credentials.holds(CAP_SETGID)
            && (credentials.user_ns.is_first()
                || (credentials.user_ns.setgroups_allowed()
                    && credentials.user_ns.mapped_any(userns::Kind::Group)))
    });
    if !allowed {
        return Err(Errno::EPERM);
    }
    if size > NGROUPS_MAX {
        return Err(Errno::EINVAL);
    }
    let mut groups = read_groups(process, size, list)?;
    for group in &mut groups {
        *group = kernel_id(process, Kind::Group, *group)?;
    }
    groups.sort_unstable();
    process.with_credentials(|credentials| credentials.groups = groups);
    Ok(0)
}

/// Read a `setgroups` list of `size` groups.
fn read_groups(process: &Process, size: u32, list: u64) -> Result<Vec<u32>, Errno> {
    let count = usize::try_from(size).map_err(|_| Errno::EINVAL)?;
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut bytes = alloc::vec![0_u8; count * 4];
    uaccess::copy_from_user(process.space(), list, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let groups: Vec<u32> = bytes
        .chunks_exact(4)
        .filter_map(|word| match *word {
            [a, b, c, d] => Some(u32::from_le_bytes([a, b, c, d])),
            _ => None,
        })
        .collect();
    if groups.contains(&UNCHANGED) {
        return Err(Errno::EINVAL);
    }
    Ok(groups)
}

/// How many data structures a capability version reads or writes, or `None`
/// for a version the kernel does not speak.
fn data_count(version: u32) -> Option<usize> {
    match version {
        CAPABILITY_VERSION_1 => Some(1),
        CAPABILITY_VERSION_2 | CAPABILITY_VERSION_3 => Some(2),
        _ => None,
    }
}

/// Validate the header's version as `cap_validate_magic` does.
///
/// An unknown version is how a program asks which one the kernel prefers:
/// the kernel writes version 3 into the header and answers `EINVAL`. libcap
/// probes exactly this way, with a zeroed header.
fn validate_version(process: &Process, header: u64) -> Result<usize, Errno> {
    let version = uaccess::get_u32(process.space(), header)?;
    match data_count(version) {
        Some(count) => Ok(count),
        None => {
            uaccess::put_u32(process.space(), header, CAPABILITY_VERSION_3)?;
            Err(Errno::EINVAL)
        }
    }
}

/// `capget`: for a process with effective uid 0, every capability effective
/// and permitted and none inheritable, which is what root reports on Linux;
/// for any other, every set empty. See the module documentation.
///
/// The order of refusals is `kernel/capability.c`'s -- in particular a version
/// probe with a null data pointer succeeds, having written the preferred
/// version, because that is the form libcap's probe takes.
pub(crate) fn sys_capget(process: &Process, header: u64, data: u64) -> Result<usize, Errno> {
    let validated = validate_version(process, header);
    if data == 0 {
        return match validated {
            Ok(_) | Err(Errno::EINVAL) => Ok(0),
            Err(errno) => Err(errno),
        };
    }
    let count = validated?;
    let pid = int(u64::from(uaccess::get_u32(
        process.space(),
        header.wrapping_add(4),
    )?));
    if pid < 0 {
        return Err(Errno::EINVAL);
    }
    // In the first namespace the sets are reported, not enforced: root's
    // are full and everyone else's empty. In a child they are the real ones.
    let [effective, permitted, inheritable] = subject(process, pid)?.with_credentials(|held| {
        if !held.user_ns.is_first() {
            [
                held.caps.effective,
                held.caps.permitted,
                held.caps.inheritable,
            ]
        } else if held.privileged() {
            [userns::FULL, userns::FULL, 0]
        } else {
            [0, 0, 0]
        }
    });
    let mut bytes = [0_u8; CAP_DATA * 2];
    for (index, slot) in bytes.chunks_mut(CAP_DATA).enumerate() {
        let shift = 32 * index;
        for (field, set) in slot.chunks_mut(4).zip([effective, permitted, inheritable]) {
            field.copy_from_slice(&((set >> shift) as u32).to_le_bytes());
        }
    }
    let used = bytes.get(..CAP_DATA * count).ok_or(Errno::EINVAL)?;
    uaccess::copy_to_user(process.space(), data, used).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `capset`: accepted for the caller, within the capabilities it has.
///
/// Another process is `EPERM`, as it has been on Linux since 2.6.24. Bits
/// above [`CAP_LAST_CAP`] are `EPERM` too, because Linux requires the new
/// permitted set to be within the old one and those bits are in no set; and so
/// is any bit at all from a process whose effective uid is not 0, whose sets
/// `capget` reports empty. A narrower set from root is accepted and not
/// enforced: nothing checks a capability, so there is nothing to narrow.
pub(crate) fn sys_capset(process: &Process, header: u64, data: u64) -> Result<usize, Errno> {
    let count = validate_version(process, header)?;
    let pid = int(u64::from(uaccess::get_u32(
        process.space(),
        header.wrapping_add(4),
    )?));
    if !matches!(subject(process, pid), Ok(attributes::Subject::Caller(_))) {
        return Err(Errno::EPERM);
    }
    let mut bytes = [0_u8; CAP_DATA * 2];
    let used = bytes.get_mut(..CAP_DATA * count).ok_or(Errno::EINVAL)?;
    uaccess::copy_from_user(process.space(), data, used).map_err(|_| Errno::EFAULT)?;
    if !process.with_credentials(|credentials| credentials.user_ns.is_first()) {
        return set_child_namespace_sets(process, &bytes, count);
    }
    if !process.with_credentials(|credentials| credentials.privileged())
        && bytes.iter().any(|&byte| byte != 0)
    {
        return Err(Errno::EPERM);
    }
    let high_mask = !((1_u32 << (CAP_LAST_CAP - 31)) - 1);
    if let Some(high) = bytes.get(CAP_DATA..)
        && count == 2
        && high
            .chunks(4)
            .any(|word| matches!(*word, [a, b, c, d] if u32::from_le_bytes([a, b, c, d]) & high_mask != 0))
    {
        return Err(Errno::EPERM);
    }
    Ok(0)
}

/// `capset` in a child namespace, by Linux's `cap_capset`: the new permitted
/// set within the old, the new effective set within the new permitted, and
/// the new inheritable set within the old inheritable and the bounding set
/// -- and, without `CAP_SETPCAP`, within the old inheritable and permitted
/// ones. A version that carries only the low words leaves the high words
/// empty, as Linux does.
fn set_child_namespace_sets(process: &Process, bytes: &[u8], count: usize) -> Result<usize, Errno> {
    let mut new = [0_u64; 3];
    for (index, chunk) in bytes.chunks(CAP_DATA).take(count).enumerate() {
        for (set, field) in new.iter_mut().zip(chunk.chunks(4)) {
            if let [a, b, c, d] = *field {
                *set |= u64::from(u32::from_le_bytes([a, b, c, d])) << (32 * index);
            }
        }
    }
    let [effective, permitted, inheritable] = new;
    process.with_credentials(|held| {
        let old = held.caps;
        let inheritable_limit = if held.holds(CAP_SETPCAP) {
            old.inheritable | old.bounding
        } else {
            (old.inheritable | old.permitted) & (old.inheritable | old.bounding)
        };
        if permitted & !old.permitted != 0
            || effective & !permitted != 0
            || inheritable & !inheritable_limit != 0
        {
            return Err(Errno::EPERM);
        }
        held.caps.effective = effective;
        held.caps.permitted = permitted;
        held.caps.inheritable = inheritable;
        Ok(0)
    })
}
