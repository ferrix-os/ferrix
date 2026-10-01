//! Stage 13's delegation and `CLONE_INTO_CGROUP` check, landing G4
//! (`docs/CGROUPS.md` §3.1 and §3.2).
//!
//! Delegation: root `chown`s a cgroup and its `cgroup.procs` to uid 1000,
//! which then may make a cgroup inside it and open that `cgroup.procs` for
//! writing but not root's, and may move a process between two cgroups of its
//! subtree but not out of it -- even into a cgroup whose `cgroup.procs` it
//! also owns, because the common ancestor of the two is root's. A cgroup
//! `rmdir` removed takes no process and no child, `ENODEV`, even through a
//! directory looked up before it went.
//!
//! `CLONE_INTO_CGROUP`: a program (`arch::USER_INTO_CGROUP_PROGRAM`) starts a
//! child in `/check-g` and the child, first thing, reads `/proc/self/cgroup`
//! and must find `0::/check-g`; a descriptor that is not open, and one of a
//! directory that is not a cgroup, are both `EBADF`, as on Linux.

use alloc::sync::Arc;

use ferrix_cgroupfs::files::Kind;
use ferrix_elf::Class;
use ferrix_linux_abi::errno::Errno;
use ferrix_vfs::{Access, NewNode, OpenFlags, SetAttributes};

use super::{Checked, Directory, Harness, Writer, write_to};
use crate::arch;
use crate::object::job::KILLED_STATUS;
use crate::syscall::process::{self, Process};
use crate::syscall::{exec, image};

/// The user a subtree is delegated to.
const DELEGATE: u32 = 1000;

/// What [`arch::USER_INTO_CGROUP_PROGRAM`] exits with when its child started
/// in the cgroup and both refusals were `EBADF`.
const INTO_STATUS: i32 = 44;

/// What the check counted, for the boot line.
#[derive(Debug, Default)]
pub(super) struct Counted {
    /// Moves judged by the delegation rules, allowed and refused.
    pub(super) moves: u32,
    /// Whether the `CLONE_INTO_CGROUP` program ran; not where there is none.
    pub(super) cloned: bool,
}

/// The check.
///
/// # Errors
///
/// The first thing that was not as Linux has it, by name.
pub(super) fn run(harness: &mut Harness) -> Checked<Counted> {
    let mut counted = Counted {
        cloned: check_clone_into_cgroup(harness)?,
        ..Counted::default()
    };
    counted.moves = check_delegation(harness)?;
    check_a_removed_cgroup(harness)?;
    Ok(counted)
}

/// The cgroupfs directory at `tail`, as an inode.
fn directory_at(harness: &Harness, tail: &[u8]) -> Checked<Arc<Directory>> {
    let at = harness
        .ns
        .resolve(&harness.ctx, None, &Harness::path(tail), true)
        .map_err(|_| "a cgroup the delegation check made did not resolve")?;
    let inode = at
        .inode()
        .map_err(|_| "a cgroup the delegation check made has no inode")?;
    Arc::clone(&inode)
        .into_any()
        .downcast::<Directory>()
        .map_err(|_| "a cgroup directory is not cgroupfs's")
}

/// `chown` of the node at `tail` to the delegate, as root.
pub(super) fn delegate(harness: &Harness, tail: &[u8]) -> Checked<()> {
    let at = harness
        .ns
        .resolve(&harness.ctx, None, &Harness::path(tail), true)
        .map_err(|_| "a node to delegate did not resolve")?;
    let change = SetAttributes {
        uid: Some(DELEGATE),
        gid: Some(DELEGATE),
        ..SetAttributes::default()
    };
    harness
        .ns
        .set_attributes(&at, &change)
        .map_err(|_| "chown of a cgroupfs node was refused")?;
    // Looked up again: a directory here is made afresh at every lookup, so
    // the owner must have been kept on the job, not on the inode.
    let owner = harness
        .ns
        .resolve(&harness.ctx, None, &Harness::path(tail), true)
        .and_then(|again| again.inode().map(|inode| inode.metadata()))
        .map_err(|_| "a delegated node did not resolve again")?;
    if (owner.uid, owner.gid) != (DELEGATE, DELEGATE) {
        return Err("chown of a cgroupfs node did not last to the next lookup");
    }
    Ok(())
}

/// A move of `process` into `to` as `who` would have made it by writing its
/// pid to `to`'s `cgroup.procs`, past the open.
fn move_as(who: &Access, process: &Process, to: &Directory) -> Result<usize, Errno> {
    let writer = Writer {
        who: who.clone(),
        shared: Arc::clone(&to.shared),
        ns: Arc::clone(crate::syscall::nsproxy::initial_cgroup()),
    };
    let pid = alloc::format!("{}\n", process.pid());
    write_to(&to.job, Kind::Procs, pid.as_bytes(), &writer)
}

/// `CLONE_INTO_CGROUP`, from a program: its child reads its own cgroup
/// first thing and must be in the one it was started in. Answers whether it
/// ran.
fn check_clone_into_cgroup(harness: &mut Harness) -> Checked<bool> {
    if arch::USER_INTO_CGROUP_PROGRAM.is_empty() {
        return Ok(false);
    }
    harness
        .mkdir(b"/check-g")
        .map_err(|_| "mkdir of the CLONE_INTO_CGROUP check's cgroup failed")?;
    harness.report.made += 1;
    let class = if size_of::<usize>() == 8 {
        Class::Elf64
    } else {
        Class::Elf32
    };
    let file = image::build_with(
        class,
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_INTO_CGROUP_PROGRAM,
    );
    let status = exec::run(
        &file,
        &[b"/into-cgroup"],
        &[],
        [0x5c; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "the CLONE_INTO_CGROUP program could not be started")?;
    match status {
        INTO_STATUS => {}
        2 => {
            return Err(
                "a child started by CLONE_INTO_CGROUP was not in that cgroup from its start",
            );
        }
        3 => return Err("CLONE_INTO_CGROUP with a descriptor that is not open was not EBADF"),
        4 => return Err("CLONE_INTO_CGROUP with a directory that is not a cgroup was not EBADF"),
        5 => return Err("the CLONE_INTO_CGROUP program could not open its cgroup"),
        6 => return Err("clone3 refused CLONE_INTO_CGROUP into a cgroup root may write"),
        _ => return Err("the CLONE_INTO_CGROUP program did not exit as it should"),
    }
    harness
        .rmdir(b"/check-g")
        .map_err(|_| "the CLONE_INTO_CGROUP check's cgroup did not empty")?;
    Ok(true)
}

/// A subtree `chown`ed to a user: it may make cgroups in it, open their
/// `cgroup.procs`, and move a process within it, and not out of it.
fn check_delegation(harness: &mut Harness) -> Checked<u32> {
    for tail in [&b"/check-d"[..], b"/check-o"] {
        harness
            .mkdir(tail)
            .map_err(|_| "mkdir of a cgroup to delegate failed")?;
    }
    for tail in [
        &b"/check-d"[..],
        b"/check-d/cgroup.procs",
        b"/check-o/cgroup.procs",
    ] {
        delegate(harness, tail)?;
    }
    let mut user = harness.ns.context();
    user.who = Access::user(DELEGATE, DELEGATE);
    harness
        .ns
        .mkdir(&user, None, &Harness::path(b"/check-d/a"), 0o755)
        .map_err(|_| "the delegate could not make a cgroup in its subtree")?;
    let made_outside = harness
        .ns
        .mkdir(&user, None, &Harness::path(b"/check-x"), 0o755);
    if made_outside != Err(Errno::EACCES) {
        return Err("the delegate made a cgroup outside its subtree, or was not refused EACCES");
    }
    delegate(harness, b"/check-d/a/cgroup.procs")?;
    harness.report.made += 3;
    let write = OpenFlags {
        write: true,
        ..OpenFlags::default()
    };
    if harness
        .ns
        .open(
            &user,
            None,
            &Harness::path(b"/check-d/a/cgroup.procs"),
            &write,
            0,
        )
        .is_err()
    {
        return Err("the delegate could not open its own cgroup.procs for writing");
    }
    let opened = harness
        .ns
        .open(&user, None, &Harness::path(b"/cgroup.procs"), &write, 0);
    if opened.err() != Some(Errno::EACCES) {
        return Err("the delegate opened root's cgroup.procs for writing");
    }

    let process = process::new_for_check()
        .map_err(|_| "could not make a process for the delegation check")?;
    let moves = judge_moves(harness, &user.who, &process);
    process::kill(&process, KILLED_STATUS);
    let moves = moves?;
    for tail in [&b"/check-d/a"[..], b"/check-d", b"/check-o"] {
        harness
            .rmdir(tail)
            .map_err(|_| "rmdir of a delegated cgroup failed")?;
    }
    Ok(moves)
}

/// The moves: root puts `process` in the delegated subtree, and the
/// delegate moves it within the subtree, is refused moving it out, and moves
/// it back. Answers how many moves were judged.
fn judge_moves(harness: &Harness, who: &Access, process: &Process) -> Checked<u32> {
    let delegated = directory_at(harness, b"/check-d")?;
    let inner = directory_at(harness, b"/check-d/a")?;
    let outside = directory_at(harness, b"/check-o")?;
    let _ = move_as(&Access::root(), process, &delegated)
        .map_err(|_| "root could not move a process into a delegated cgroup")?;
    let _ = move_as(who, process, &inner)
        .map_err(|_| "the delegate could not move a process within its subtree")?;
    if !Arc::ptr_eq(&process.job(), &inner.job) {
        return Err("a move within a delegated subtree did not move the process");
    }
    let out = move_as(who, process, &outside);
    if out != Err(Errno::EACCES) {
        return Err(
            "the delegate moved a process out of its subtree, into a cgroup whose cgroup.procs \
             it owns, or was not refused EACCES",
        );
    }
    if !Arc::ptr_eq(&process.job(), &inner.job) {
        return Err("a refused move out of a delegated subtree moved the process anyway");
    }
    let _ = move_as(who, process, &delegated)
        .map_err(|_| "the delegate could not move a process back up its subtree")?;
    if !Arc::ptr_eq(&process.job(), &delegated.job) {
        return Err("a move up a delegated subtree did not move the process");
    }
    Ok(4)
}

/// A cgroup `rmdir` removed takes neither a process nor a child, through a
/// directory looked up while it was there: `ENODEV`, as Linux answers once
/// the cgroup is dead.
///
/// Verifies: L.object.69
fn check_a_removed_cgroup(harness: &mut Harness) -> Checked<()> {
    harness
        .mkdir(b"/check-r")
        .map_err(|_| "mkdir of a cgroup to remove failed")?;
    harness.report.made += 1;
    let directory = directory_at(harness, b"/check-r")?;
    harness
        .rmdir(b"/check-r")
        .map_err(|_| "rmdir of an empty cgroup failed")?;
    let process = process::new_for_check()
        .map_err(|_| "could not make a process for the removed-cgroup check")?;
    let moved = move_as(&Access::root(), &process, &directory);
    let made = ferrix_vfs::Inode::create(&*directory, b"x", NewNode::Directory, 0o755).err();
    let stayed = Arc::ptr_eq(&process.job(), crate::object::job::root());
    process::kill(&process, KILLED_STATUS);
    harness.refused(
        moved.err(),
        Errno::ENODEV,
        "a move into a removed cgroup was not ENODEV",
    )?;
    harness.refused(
        made,
        Errno::ENODEV,
        "mkdir in a removed cgroup was not ENODEV",
    )?;
    if !stayed {
        return Err("a refused move into a removed cgroup moved the process anyway");
    }
    Ok(())
}
