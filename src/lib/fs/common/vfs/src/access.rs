//! Who is asking, and whether they may: discretionary access control.
//!
//! Linux's `generic_permission`, `may_create`, `may_delete`,
//! `setattr_prepare` and `inode_init_owner`, as pure functions of an
//! [`Access`] and the [`Metadata`] of the objects involved. The namespace
//! calls them at every place Linux does; they are here, apart from it, so the
//! rules can be tested one at a time on the host.
//!
//! Capabilities are not modelled yet. A filesystem user id of 0 stands in for
//! the ones these checks consult -- `CAP_DAC_OVERRIDE`,
//! `CAP_DAC_READ_SEARCH`, `CAP_FOWNER`, `CAP_CHOWN` and `CAP_FSETID` -- which is
//! what Linux's own rules give root when capabilities follow the ids, as they
//! do by default.

use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;

use crate::Result;
use crate::node::{FileType, Metadata, SetAttributes};

/// Read permission, in the bit position `access(2)`'s `R_OK` has.
pub const MAY_READ: u32 = 4;
/// Write permission, `W_OK`'s bit.
pub const MAY_WRITE: u32 = 2;
/// Execute permission, or search on a directory: `X_OK`'s bit.
pub const MAY_EXEC: u32 = 1;

/// The set-user-id bit.
const S_ISUID: u32 = 0o4000;
/// The set-group-id bit.
const S_ISGID: u32 = 0o2000;
/// The sticky bit.
const S_ISVTX: u32 = 0o1000;
/// Group execute.
const S_IXGRP: u32 = 0o010;

/// The identity permission checks are made against: a process's filesystem
/// user and group ids and its supplementary groups.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Access {
    /// The filesystem user id.
    pub uid: u32,
    /// The filesystem group id.
    pub gid: u32,
    /// The supplementary groups.
    pub groups: Vec<u32>,
}

impl Access {
    /// Root's: every check passes that root passes on Linux.
    #[must_use]
    pub fn root() -> Access {
        Access {
            uid: 0,
            gid: 0,
            groups: Vec::new(),
        }
    }

    /// An identity with no supplementary groups.
    #[must_use]
    pub fn user(uid: u32, gid: u32) -> Access {
        Access {
            uid,
            gid,
            groups: Vec::new(),
        }
    }

    /// Whether it stands in for the capabilities the checks consult.
    #[must_use]
    pub fn privileged(&self) -> bool {
        self.uid == 0
    }

    /// Linux's `in_group_p`: the filesystem group, or a supplementary one.
    #[must_use]
    pub fn in_group(&self, gid: u32) -> bool {
        self.gid == gid || self.groups.contains(&gid)
    }

    /// Linux's `inode_owner_or_capable`.
    #[must_use]
    pub fn owns(&self, meta: &Metadata) -> bool {
        self.uid == meta.uid || self.privileged()
    }

    /// Whether every access in `want` ([`MAY_READ`], [`MAY_WRITE`],
    /// [`MAY_EXEC`]) is allowed on an object with `meta`.
    ///
    /// The owner's bits apply to the owner even when the group's or others'
    /// allow more, as `acl_permission_check` does. Root passes everything on
    /// a directory, and on anything else everything but execute, which needs
    /// an execute bit somewhere in the mode: a file nobody may execute is not
    /// a program even to root.
    #[must_use]
    pub fn permitted(&self, meta: &Metadata, want: u32) -> bool {
        let mode = meta.permissions;
        let bits = if self.uid == meta.uid {
            (mode >> 6) & 7
        } else if self.in_group(meta.gid) {
            (mode >> 3) & 7
        } else {
            mode & 7
        };
        if want & !bits & 7 == 0 {
            return true;
        }
        if !self.privileged() {
            return false;
        }
        meta.kind == FileType::Directory || want & MAY_EXEC == 0 || mode & 0o111 != 0
    }

    /// [`Access::permitted`] as a result: `EACCES` when refused.
    ///
    /// # Errors
    ///
    /// `EACCES`.
    pub fn require(&self, meta: &Metadata, want: u32) -> Result<()> {
        if self.permitted(meta, want) {
            Ok(())
        } else {
            Err(Errno::EACCES)
        }
    }

    /// Linux's `may_create`: write and search on the directory.
    ///
    /// # Errors
    ///
    /// `EACCES`.
    pub fn may_create(&self, dir: &Metadata) -> Result<()> {
        self.require(dir, MAY_WRITE | MAY_EXEC)
    }

    /// Linux's `may_delete`: write and search on the directory, and in a
    /// sticky directory, ownership of the victim or the directory.
    ///
    /// # Errors
    ///
    /// `EACCES` without write and search, `EPERM` for the sticky bit.
    pub fn may_delete(&self, dir: &Metadata, victim: &Metadata) -> Result<()> {
        self.require(dir, MAY_WRITE | MAY_EXEC)?;
        let sticky = dir.permissions & S_ISVTX != 0;
        if sticky && self.uid != victim.uid && self.uid != dir.uid && !self.privileged() {
            return Err(Errno::EPERM);
        }
        Ok(())
    }

    /// What `chmod` and `chown` may do to an object with `meta`: the change
    /// as it will be applied, or the refusal. Linux's `setattr_prepare`,
    /// with what `chmod_common` and `chown_common` add.
    ///
    /// * A new owner needs privilege, unless it is the owner already.
    /// * A new group needs privilege, or the caller's owning the object and
    ///   belonging to the group.
    /// * New permissions need ownership; the set-group-id bit is dropped when
    ///   the caller is not privileged and not in the object's group.
    /// * A change of owner or group on anything but a directory clears the
    ///   set-user-id bit, and the set-group-id bit when the group may
    ///   execute, whoever makes it.
    ///
    /// # Errors
    ///
    /// `EPERM`.
    pub fn check_change(&self, meta: &Metadata, change: &SetAttributes) -> Result<SetAttributes> {
        let mut applied = *change;
        if let Some(uid) = change.uid
            && !self.privileged()
            && !(self.uid == meta.uid && uid == meta.uid)
        {
            return Err(Errno::EPERM);
        }
        if let Some(gid) = change.gid
            && !self.privileged()
            && !(self.uid == meta.uid && (gid == meta.gid || self.in_group(gid)))
        {
            return Err(Errno::EPERM);
        }
        if let Some(mut permissions) = change.permissions {
            if !self.owns(meta) {
                return Err(Errno::EPERM);
            }
            let group = change.gid.unwrap_or(meta.gid);
            if permissions & S_ISGID != 0 && !self.privileged() && !self.in_group(group) {
                permissions &= !S_ISGID;
            }
            applied.permissions = Some(permissions);
        }
        let reowned = change.uid.is_some() || change.gid.is_some();
        if reowned && meta.kind != FileType::Directory {
            let mut permissions = applied.permissions.unwrap_or(meta.permissions);
            permissions &= !S_ISUID;
            if permissions & (S_ISGID | S_IXGRP) == S_ISGID | S_IXGRP {
                permissions &= !S_ISGID;
            }
            if permissions != meta.permissions || applied.permissions.is_some() {
                applied.permissions = Some(permissions);
            }
        }
        Ok(applied)
    }

    /// Whether `utimensat` may set the times: to given values needs
    /// ownership, and to now needs ownership or write permission.
    ///
    /// # Errors
    ///
    /// `EPERM` for given values, `EACCES` for now.
    pub fn may_set_times(&self, meta: &Metadata, given: bool) -> Result<()> {
        if self.owns(meta) {
            return Ok(());
        }
        if given {
            return Err(Errno::EPERM);
        }
        self.require(meta, MAY_WRITE)
    }

    /// Linux's `inode_init_owner`: the owner, group and permissions a new
    /// object made with `permissions` in `dir` gets.
    ///
    /// The group is the directory's when the directory is set-group-id,
    /// which a new directory inherits; otherwise the caller's. A new file
    /// that asks for set-group-id with group execute, in a group the caller
    /// is not in, loses the bit.
    #[must_use]
    pub fn new_owner(&self, dir: &Metadata, kind: FileType, permissions: u32) -> (u32, u32, u32) {
        let inherit = dir.permissions & S_ISGID != 0;
        let gid = if inherit { dir.gid } else { self.gid };
        let mut permissions = permissions;
        if inherit && kind == FileType::Directory {
            permissions |= S_ISGID;
        } else if permissions & (S_ISGID | S_IXGRP) == S_ISGID | S_IXGRP
            && !self.in_group(gid)
            && !self.privileged()
        {
            permissions &= !S_ISGID;
        }
        (self.uid, gid, permissions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Timespec;

    fn meta(kind: FileType, permissions: u32, uid: u32, gid: u32) -> Metadata {
        let zero = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        Metadata {
            ino: 1,
            kind,
            permissions,
            nlink: 1,
            uid,
            gid,
            size: 0,
            rdev: 0,
            blocks: 0,
            block_size: 4096,
            atime: zero,
            mtime: zero,
            ctime: zero,
        }
    }

    const FILE: FileType = FileType::Regular;
    const DIR: FileType = FileType::Directory;

    #[test]
    fn the_owner_bits_bind_the_owner_even_when_others_may_do_more() {
        let user = Access::user(1000, 1000);
        let file = meta(FILE, 0o077, 1000, 1000);
        assert!(!user.permitted(&file, MAY_READ));
        assert!(Access::user(1001, 1000).permitted(&file, MAY_READ | MAY_WRITE));
        assert!(Access::user(1001, 7).permitted(&file, MAY_READ | MAY_WRITE | MAY_EXEC));
    }

    #[test]
    fn supplementary_groups_count_as_the_group() {
        let mut user = Access::user(1000, 1000);
        let file = meta(FILE, 0o640, 0, 42);
        assert!(!user.permitted(&file, MAY_READ));
        user.groups.push(42);
        assert!(user.permitted(&file, MAY_READ));
        assert!(!user.permitted(&file, MAY_WRITE));
    }

    #[test]
    fn root_passes_but_needs_an_execute_bit_to_run_a_file() {
        let root = Access::root();
        assert!(root.permitted(&meta(FILE, 0o000, 5, 5), MAY_READ | MAY_WRITE));
        assert!(!root.permitted(&meta(FILE, 0o644, 5, 5), MAY_EXEC));
        assert!(root.permitted(&meta(FILE, 0o001, 5, 5), MAY_EXEC));
        assert!(root.permitted(&meta(DIR, 0o000, 5, 5), MAY_EXEC | MAY_WRITE));
    }

    #[test]
    fn deleting_in_a_sticky_directory_needs_ownership() {
        let tmp = meta(DIR, 0o1777, 0, 0);
        let theirs = meta(FILE, 0o666, 1001, 1001);
        let mine = meta(FILE, 0o600, 1000, 1000);
        let user = Access::user(1000, 1000);
        assert_eq!(user.may_delete(&tmp, &theirs), Err(Errno::EPERM));
        assert_eq!(user.may_delete(&tmp, &mine), Ok(()));
        assert_eq!(Access::root().may_delete(&tmp, &theirs), Ok(()));
        assert_eq!(
            user.may_delete(&meta(DIR, 0o755, 0, 0), &mine),
            Err(Errno::EACCES)
        );
        let own_dir = meta(DIR, 0o1777, 1000, 1000);
        assert_eq!(user.may_delete(&own_dir, &theirs), Ok(()));
    }

    #[test]
    fn chown_and_chmod_follow_setattr_prepare() {
        let user = Access::user(1000, 1000);
        let mine = meta(FILE, 0o6755, 1000, 1000);
        let change = |uid, gid, permissions| SetAttributes {
            uid,
            gid,
            permissions,
            ..SetAttributes::default()
        };
        assert_eq!(
            user.check_change(&mine, &change(Some(0), None, None)),
            Err(Errno::EPERM)
        );
        assert!(
            user.check_change(&mine, &change(Some(1000), None, None))
                .is_ok()
        );
        assert_eq!(
            user.check_change(&mine, &change(None, Some(42), None)),
            Err(Errno::EPERM)
        );
        let mut member = user.clone();
        member.groups.push(42);
        let regrouped = member
            .check_change(&mine, &change(None, Some(42), None))
            .unwrap();
        assert_eq!(regrouped.permissions, Some(0o0755), "set-id bits cleared");
        let theirs = meta(FILE, 0o644, 0, 0);
        assert_eq!(
            user.check_change(&theirs, &change(None, None, Some(0o777))),
            Err(Errno::EPERM)
        );
        let not_member = meta(FILE, 0o644, 1000, 42);
        assert_eq!(
            user.check_change(&not_member, &change(None, None, Some(0o2755)))
                .unwrap()
                .permissions,
            Some(0o755)
        );
        assert_eq!(
            Access::root()
                .check_change(&theirs, &change(Some(7), Some(7), None))
                .unwrap()
                .uid,
            Some(7)
        );
    }

    #[test]
    fn times_need_ownership_or_for_now_write_permission() {
        let user = Access::user(1000, 1000);
        let shared = meta(FILE, 0o666, 0, 0);
        assert_eq!(user.may_set_times(&shared, true), Err(Errno::EPERM));
        assert_eq!(user.may_set_times(&shared, false), Ok(()));
        assert_eq!(
            user.may_set_times(&meta(FILE, 0o644, 0, 0), false),
            Err(Errno::EACCES)
        );
    }

    #[test]
    fn new_objects_belong_to_their_creator_and_set_gid_directories_pass_on_their_group() {
        let user = Access::user(1000, 1000);
        let plain = meta(DIR, 0o1777, 0, 0);
        assert_eq!(user.new_owner(&plain, FILE, 0o644), (1000, 1000, 0o644));
        let shared = meta(DIR, 0o2775, 0, 50);
        assert_eq!(user.new_owner(&shared, DIR, 0o755), (1000, 50, 0o2755));
        assert_eq!(user.new_owner(&shared, FILE, 0o2750), (1000, 50, 0o750));
    }
}
