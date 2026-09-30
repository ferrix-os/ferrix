//! cgroupfs: the job tree, seen as cgroup v2 (`docs/CGROUPS.md` §3).
//!
//! Every directory is a job and every job beneath the root job is a
//! directory: one made by `mkdir` under its name, one native `job_create`
//! made as `job-<id>`. The job is the truth and this is a view of it, as
//! procfs is a view of processes, so nothing is stored here: each lookup asks
//! the job, and each file renders what the job says when it is opened.
//!
//! The text of every file, and how every write is read, is `ferrix-cgroupfs`'s,
//! where the host tests and the fuzzer reach it. What this module adds is the
//! kernel's half: which job, which process, which errno.
//!
//! # What is here, and what comes later
//!
//! The tree, `cgroup.procs` read and moves, `cgroup.kill`, `cgroup.events`
//! read and polled for `POLLPRI` (G3), the limits on depth and descendants,
//! and `cgroup.subtree_control` with three controllers to enable: `cpu`
//! (`cpu.weight`), `memory` (`memory.max`, `memory.current`,
//! `memory.events`) and `pids` (`pids.max`, `pids.current`, `pids.events`).
//! Each of their files reads and writes the job's quota (`object::quota`,
//! `FRU_RSA.1`), which a native job handle reaches as well. `cgroup.freeze`
//! reads `0` and refuses writes until F1.
//!
//! # Delegation (G4)
//!
//! Every directory and file has an owner, a group and a mode, which `chown`
//! and `chmod` change and a `mkdir` by someone other than root sets to its
//! maker, as kernfs does. They are kept on the job ([`Job::node`]), since a
//! directory here is made afresh at every lookup. A move by `cgroup.procs`
//! needs what Linux's cgroup v2 asks: write access to the target's
//! `cgroup.procs`, which the open checks, and to the `cgroup.procs` of the
//! common ancestor of where the process is and where it goes, checked here as
//! whoever opened the file. So a user given a subtree by `chown` moves its
//! processes within it and not out of it. `clone3`'s `CLONE_INTO_CGROUP`
//! asks the same, of the caller, through [`clone_target`]. The
//! no-internal-process rule is `ferrix-cgroupfs`'s and the job's.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use ferrix_cgroupfs::controllers::{self, Controller, Set};
use ferrix_cgroupfs::files::{self, Kind};
use ferrix_cgroupfs::write::{self, Target};
use ferrix_cgroupfs::{Refusal, name, render};
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::nr::NativeCall;
use ferrix_native_abi::rights::{Requested, Rights};
use ferrix_native_abi::status;
use ferrix_vfs::access::{Access, MAY_READ, MAY_WRITE};
use ferrix_vfs::{
    DirEntry, FIRST_CURSOR, FileSystem, FileType, Inode, Metadata, NewNode, OpenFile, Readiness,
    SetAttributes, StatFs, Timespec,
};

use crate::audit;
use crate::fs::{self, procfs};
use crate::hooks::Full;
use crate::object::Object;
use crate::object::job::{self, Job, JobError, NodeAttributes};
use crate::object::process::Host;
use crate::object::quota::{self, Resource};
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::syscall::fd;
use crate::syscall::native;
use crate::syscall::process::{self, Process};
use crate::syscall::registry;

mod controllers_check;
mod creator_check;
mod delegation_check;
mod events_check;
mod limits_check;
mod native_check;
mod oom_check;

/// The result every operation here returns.
type Result<T> = core::result::Result<T, Errno>;

/// `CGROUP2_SUPER_MAGIC`, from `include/uapi/linux/magic.h`: how systemd
/// tells a cgroup v2 mount from a v1 one before trusting it.
const CGROUP2_SUPER_MAGIC: u64 = 0x6367_7270;

/// The controllers this kernel has built: `cpu`, `memory` and `pids`, over
/// the job quotas. `io` is landing B1.
const BUILT: Set = Set::EMPTY
    .with(Controller::Cpu)
    .with(Controller::Memory)
    .with(Controller::Pids);

/// Where a directory's children begin in its cursor space, past its files.
const CHILD_CURSORS: u64 = 1 << 32;

/// A cgroupfs instance: a view of a subtree of the job tree -- the whole of it,
/// or, mounted by a process in a cgroup namespace, the namespace's.
#[derive(Debug)]
pub(crate) struct Cgroupfs {
    /// What every node shares.
    shared: Arc<Shared>,
    /// The cgroup its root directory is.
    root: Arc<Job>,
}

/// What every node of one instance shares.
#[derive(Debug)]
struct Shared {
    /// `st_dev`.
    device: u64,
    /// Every timestamp: when it was mounted.
    made: Timespec,
}

impl Cgroupfs {
    /// A cgroupfs over the root job, stamped with the time it was made.
    pub(crate) fn new() -> Cgroupfs {
        Cgroupfs::rooted_at(Arc::clone(job::root()))
    }

    /// A cgroupfs whose root directory is `root`: the root of a cgroup
    /// namespace, so that a process in it sees that cgroup as `/`.
    pub(crate) fn rooted_at(root: Arc<Job>) -> Cgroupfs {
        Cgroupfs {
            shared: Arc::new(Shared {
                device: fs::anonymous_device(),
                made: fs::clock().now(),
            }),
            root,
        }
    }
}

impl FileSystem for Cgroupfs {
    fn root(&self) -> Arc<dyn Inode> {
        Arc::new(Directory {
            job: Arc::clone(&self.root),
            shared: Arc::clone(&self.shared),
        })
    }

    fn name(&self) -> &'static str {
        "cgroup2"
    }

    fn device(&self) -> u64 {
        self.shared.device
    }

    fn statfs(&self) -> StatFs {
        StatFs {
            magic: CGROUP2_SUPER_MAGIC,
            block_size: 4096,
            name_max: 255,
            ..StatFs::default()
        }
    }
}

/// The errno Linux answers a refused write or name with.
fn errno(refusal: Refusal) -> Errno {
    match refusal {
        Refusal::Invalid => Errno::EINVAL,
        Refusal::Range => Errno::ERANGE,
        Refusal::NotSupported => Errno::EOPNOTSUPP,
        Refusal::TooLong => Errno::ENAMETOOLONG,
        Refusal::Busy => Errno::EBUSY,
    }
}

/// The errno Linux answers a move a job refused with.
fn move_errno(refused: JobError) -> Errno {
    match refused {
        // Linux's `cgroup_kn_lock_live` on a cgroup whose directory is gone.
        JobError::Removed => Errno::ENODEV,
        // `cgroup_migrate_vet_dst`.
        JobError::Internal => Errno::EBUSY,
        JobError::NoMemory => Errno::ENOMEM,
        // Sealed by the native `job_kill`, which Linux has no counterpart of.
        JobError::Killed
        | JobError::Exists
        | JobError::Missing
        | JobError::Busy
        | JobError::Limited => Errno::ENOENT,
    }
}

/// The identity the running process's permission checks are made as: its
/// filesystem ids and groups, or root's for the kernel's own checks.
fn caller_access() -> Access {
    process::current().map_or_else(Access::root, |caller| access_of(&caller))
}

/// The identity `process`'s permission checks are made as.
pub(crate) fn access_of(process: &Process) -> Access {
    process.with_credentials(|credentials| Access {
        uid: credentials.user.filesystem,
        gid: credentials.group.filesystem,
        groups: credentials.groups.clone(),
    })
}

/// The slot of `cgroup.procs` among a directory's nodes.
fn procs_slot() -> u64 {
    files::FILES
        .iter()
        .find(|file| file.kind == Kind::Procs)
        .map_or(0, |file| 1 + file_slot(file))
}

/// The inode number of a job's directory; its files follow it. A job's id is
/// never reused, so neither is a number.
fn ino(job: &Job, slot: u64) -> u64 {
    job.id().saturating_mul(32).saturating_add(slot)
}

/// The metadata of the node at `slot` of `job`'s directory -- 0 for the
/// directory itself, one past a file's place in [`files::FILES`] for a file --
/// with the owner, group and mode `chown` and `chmod` gave it, or root's and
/// `permissions` if neither did.
fn node_metadata(
    job: &Job,
    shared: &Shared,
    slot: u64,
    kind: FileType,
    permissions: u32,
) -> Metadata {
    let owned = job.node(slot).unwrap_or(NodeAttributes {
        uid: 0,
        gid: 0,
        permissions,
    });
    Metadata {
        ino: ino(job, slot),
        kind,
        permissions: owned.permissions,
        nlink: if kind == FileType::Directory { 2 } else { 1 },
        uid: owned.uid,
        gid: owned.gid,
        size: 0,
        rdev: 0,
        blocks: 0,
        block_size: 4096,
        atime: shared.made,
        mtime: shared.made,
        ctime: shared.made,
    }
}

/// Record `change`'s owner, group and mode for the node `before` describes,
/// as chown and chmod leave them. Times are accepted and not kept: every
/// time here is the mount's.
fn set_node(job: &Job, slot: u64, before: &Metadata, change: &SetAttributes) -> Result<()> {
    job.set_node(
        slot,
        NodeAttributes {
            uid: change.uid.unwrap_or(before.uid),
            gid: change.gid.unwrap_or(before.gid),
            permissions: change
                .permissions
                .map_or(before.permissions, |permissions| permissions & 0o7777),
        },
    )
    .map_err(|_| Errno::ENOMEM)
}

/// The metadata of `job`'s `cgroup.procs`, whose write permission is what a
/// move is judged by.
fn procs_metadata(job: &Job, shared: &Shared) -> Metadata {
    node_metadata(job, shared, procs_slot(), FileType::Regular, 0o644)
}

/// The files whose write permission decides who may set `job`'s own limits,
/// the ones native `job_set_limit` reaches: whether its controller is
/// enabled or not, each has its owner and mode, which a delegation by
/// `chown` leaves root's.
const LIMIT_FILES: [Kind; 3] = [Kind::MemoryMax, Kind::PidsMax, Kind::CpuWeight];

/// The metadata of each of `job`'s [`LIMIT_FILES`].
fn limit_metadata(job: &Job, shared: &Shared) -> impl Iterator<Item = Metadata> {
    files::FILES
        .iter()
        .filter(|file| LIMIT_FILES.contains(&file.kind))
        .map(|file| {
            node_metadata(
                job,
                shared,
                1 + file_slot(file),
                FileType::Regular,
                u32::from(file.mode()),
            )
        })
}

/// Whether `who` may move a process from `from` to `to`, as Linux's
/// `cgroup_attach_permissions` decides for cgroup v2: write access to the
/// `cgroup.procs` of the nearest job containing both, and a destination the
/// no-internal-process rule allows. Write access to the destination's own
/// `cgroup.procs` is the caller's to have checked: the open of the file, or
/// [`clone_target`].
///
/// Linux's cgroup v1 also asked that the writer's effective uid match the
/// process's; v2 dropped that, the common ancestor being the delegation
/// boundary, and so does this.
fn attach_permissions(who: &Access, from: &Arc<Job>, to: &Arc<Job>, shared: &Shared) -> Result<()> {
    let mut common = Some(Arc::clone(from));
    while let Some(at) = common.take_if(|at| !at.contains(to)) {
        common = at.parent().cloned();
    }
    match common {
        Some(common) => who.require(&procs_metadata(&common, shared), MAY_WRITE)?,
        // Two trees with nothing above both: only a boot check's own jobs.
        None if who.privileged() => {}
        None => return Err(Errno::EACCES),
    }
    to.admits().map_err(move_errno)
}

/// The job `clone3`'s `CLONE_INTO_CGROUP` starts a child of `parent` in: the
/// cgroupfs directory `file` is open on, if `parent`, whose job is `from`,
/// may put a process there. Linux's `cgroup_css_set_fork`, in its order.
///
/// # Errors
///
/// `EBADF` for a file that is not a cgroup directory -- a file inside one
/// included, as `cgroup_get_from_file` refuses it; `ENODEV` for one `rmdir`
/// removed; `EACCES` without write access to its `cgroup.procs` or the
/// common ancestor's; `EBUSY` for the no-internal-process rule.
pub(crate) fn clone_target(file: &OpenFile, parent: &Process, from: &Arc<Job>) -> Result<Arc<Job>> {
    let directory = Arc::clone(file.inode())
        .into_any()
        .downcast::<Directory>()
        .map_err(|_| Errno::EBADF)?;
    let to = &directory.job;
    if to.is_removed() {
        return Err(Errno::ENODEV);
    }
    let who = access_of(parent);
    who.require(&procs_metadata(to, &directory.shared), MAY_WRITE)?;
    attach_permissions(&who, from, to, &directory.shared)?;
    Ok(Arc::clone(to))
}

/// The job behind the cgroupfs directory `file` is open on, `O_PATH` or not,
/// with the metadata of that directory's `cgroup.procs`, by whose permissions
/// native `job_for_cgroup` decides what a handle to it may do
/// (`docs/CGROUPS.md` §5). `None` for a file that is not a cgroup directory,
/// a file inside one included, as [`clone_target`] refuses it.
pub(crate) fn directory_job(file: &OpenFile) -> Option<(Arc<Job>, Metadata)> {
    let directory = directory_of(file)?;
    let procs = procs_metadata(&directory.job, &directory.shared);
    Some((Arc::clone(&directory.job), procs))
}

/// The cgroupfs directory `file` is open on.
fn directory_of(file: &OpenFile) -> Option<Arc<Directory>> {
    Arc::clone(file.inode())
        .into_any()
        .downcast::<Directory>()
        .ok()
}

/// Answer native `job_for_cgroup` from here: the native ABI is the item's,
/// and names no filesystem, so cgroupfs registers into it. Called once from
/// `fs::install`.
///
/// # Errors
///
/// [`Full`] when the item has no room for the registration.
pub(crate) fn install() -> core::result::Result<(), Full> {
    native::serve(NativeCall::JobForCgroup, job_for_cgroup)
}

/// `job_for_cgroup`: a handle to the job behind a cgroupfs directory, with
/// the rights the caller's access to its `cgroup.procs` allows
/// (`docs/CGROUPS.md` §5): `WAIT` to read it, `MANAGE` as well to write it,
/// and `DUPLICATE` and `TRANSFER` with either, so a service manager can hand
/// the handle on. `SET_LIMIT` only with `MANAGE`, and only to whoever may
/// also write every one of its [`LIMIT_FILES`]: a user a cgroup was
/// delegated to by `chown` may fill and kill it, and not raise its own
/// limits. It is judged as the caller, whoever opened the descriptor,
/// since what is made is a new capability and not a use of the open file.
///
/// One way only: nothing names a cgroup's path from a job handle, because a
/// handle is a capability and a path is not.
fn job_for_cgroup(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize> {
    let [dirfd, rights, ..] = *registers;
    let process = process::of_host(caller).ok_or(status::BAD_HANDLE)?;
    let requested = Requested::from_register(rights).ok_or(status::INVALID_ARGS)?;
    let file = fd::file(process, fd::arg(dirfd)).map_err(|_| status::BAD_HANDLE)?;
    let directory = directory_of(&file).ok_or(status::WRONG_TYPE)?;
    drop(file);
    let job = Arc::clone(&directory.job);
    if job.is_removed() {
        return Err(status::BAD_STATE);
    }
    let procs = procs_metadata(&job, &directory.shared);
    let who = access_of(process);
    let passed = Rights::DUPLICATE | Rights::TRANSFER;
    let allowed = if who.permitted(&procs, MAY_WRITE) {
        let limits = limit_metadata(&job, &directory.shared)
            .all(|metadata| who.permitted(&metadata, MAY_WRITE));
        let managed = passed | Rights::WAIT | Rights::MANAGE;
        if limits {
            managed | Rights::SET_LIMIT
        } else {
            managed
        }
    } else if who.permitted(&procs, MAY_READ) {
        passed | Rights::WAIT
    } else {
        return Err(status::ACCESS_DENIED);
    };
    let granted = requested.resolve(allowed).ok_or(status::ACCESS_DENIED)?;
    native::insert_new(caller.core(), Object::Job(job), granted)
}

/// A job's directory.
#[derive(Debug)]
struct Directory {
    /// The job.
    job: Arc<Job>,
    /// The instance's device and timestamps.
    shared: Arc<Shared>,
}

impl Directory {
    /// The child job shown as `name`, if there is one.
    ///
    /// With no memory to list the children, there is none: the lookup is
    /// answered as `ENOENT` rather than stopping the machine.
    fn child(&self, name: &[u8]) -> Option<Arc<Job>> {
        self.job.children().ok()?.into_iter().find(|child| {
            child
                .display_name()
                .is_ok_and(|shown| shown.as_bytes() == name)
        })
    }

    /// Whether this is the root of the tree cgroupfs shows.
    fn is_root(&self) -> bool {
        self.job.is_root()
    }
}

impl Inode for Directory {
    fn metadata(&self) -> Metadata {
        node_metadata(&self.job, &self.shared, 0, FileType::Directory, 0o755)
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    /// `chown` and `chmod` of the directory alone, as kernfs keeps them per
    /// node: delegating a subtree is a `chown` of the directory and of the
    /// files the delegate is to write (`cgroup.procs`, `cgroup.threads`,
    /// `cgroup.subtree_control`). The namespace has already checked the
    /// change is the caller's to make.
    fn set_attributes(&self, change: &SetAttributes) -> Result<()> {
        set_node(&self.job, 0, &self.metadata(), change)
    }

    /// Asked afresh on every walk: a native `job_create` adds a directory,
    /// and a job going away takes one, without the VFS seeing either.
    fn caches_lookups(&self) -> bool {
        false
    }

    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn Inode>> {
        if let Some(file) = files::named(name, self.is_root(), offered(&self.job)) {
            return Ok(Arc::new(Interface {
                job: Arc::clone(&self.job),
                file,
                shared: Arc::clone(&self.shared),
            }));
        }
        let child = self.child(name).ok_or(Errno::ENOENT)?;
        Ok(Arc::new(Directory {
            job: child,
            shared: Arc::clone(&self.shared),
        }))
    }

    /// `mkdir`: a new named job. Anything else is refused, as kernfs refuses
    /// a file made in a cgroup directory.
    fn create(&self, name: &[u8], node: NewNode<'_>, permissions: u32) -> Result<Arc<dyn Inode>> {
        if node != NewNode::Directory {
            return Err(Errno::EACCES);
        }
        name::check(name).map_err(errno)?;
        if files::named(name, self.is_root(), offered(&self.job)).is_some()
            || self.child(name).is_some()
        {
            return Err(Errno::EEXIST);
        }
        let text = core::str::from_utf8(name).map_err(|_| Errno::EINVAL)?;
        // A cgroup made in a directory someone other than root may write is
        // a delegatee's own, and its refusals are charged to the audit
        // budget above it rather than starting one of their own.
        let here = self.metadata();
        let budget = if here.uid != 0 || here.permissions & 0o022 != 0 {
            job::Budget::Parents
        } else {
            job::Budget::Own
        };
        let child = self
            .job
            .new_named_child(text, budget)
            .map_err(|refused| match refused {
                JobError::Exists => Errno::EEXIST,
                JobError::Limited => Errno::EAGAIN,
                JobError::Removed => Errno::ENODEV,
                JobError::NoMemory => Errno::ENOMEM,
                JobError::Killed | JobError::Missing | JobError::Busy | JobError::Internal => {
                    Errno::ENOENT
                }
            })?;
        own_new(&child, permissions)?;
        Ok(Arc::new(Directory {
            job: child,
            shared: Arc::clone(&self.shared),
        }))
    }

    /// `rmdir`: only an empty named job. An anonymous one is held by the
    /// handles of whoever made it, and goes when they do.
    fn rmdir(&self, name: &[u8]) -> Result<()> {
        let text = core::str::from_utf8(name).map_err(|_| Errno::ENOENT)?;
        match self.job.remove_named_child(text) {
            Ok(_removed) => Ok(()),
            Err(JobError::Missing) if self.child(name).is_some() => Err(Errno::EBUSY),
            Err(JobError::Missing) => Err(Errno::ENOENT),
            Err(_) => Err(Errno::EBUSY),
        }
    }

    fn read_dir(&self, cursor: u64, emit: &mut dyn FnMut(DirEntry<'_>) -> bool) -> Result<()> {
        let root = self.is_root();
        let first = usize::try_from(cursor.saturating_sub(FIRST_CURSOR)).unwrap_or(usize::MAX);
        if cursor < CHILD_CURSORS {
            for (index, file) in files::of(root, offered(&self.job)).enumerate().skip(first) {
                let accepted = emit(DirEntry {
                    ino: ino(&self.job, 1 + file_slot(file)),
                    kind: FileType::Regular,
                    name: file.name.as_bytes(),
                    next: FIRST_CURSOR.saturating_add(index as u64 + 1),
                });
                if !accepted {
                    return Ok(());
                }
            }
        }
        // Children in id order, resumed by id, so a child made or removed
        // between two reads neither repeats nor hides another.
        let from = cursor.saturating_sub(CHILD_CURSORS);
        let mut children = self.job.children().map_err(|_| Errno::ENOMEM)?;
        children.sort_unstable_by_key(|child| child.id());
        for child in children.iter().filter(|child| child.id() >= from) {
            let name = child.display_name().map_err(|_| Errno::ENOMEM)?;
            let accepted = emit(DirEntry {
                ino: ino(child, 0),
                kind: FileType::Directory,
                name: name.as_bytes(),
                next: CHILD_CURSORS.saturating_add(child.id()).saturating_add(1),
            });
            if !accepted {
                break;
            }
        }
        Ok(())
    }
}

/// Give a job `mkdir` just made its maker's ownership, as Linux's
/// `cgroup_kn_set_ugid` does for the directory and every file in it: the
/// maker's filesystem ids, unless both are root's, which every node has
/// anyway. The directory takes the mode `mkdir` asked for, as kernfs gives
/// it.
fn own_new(job: &Job, permissions: u32) -> Result<()> {
    let who = caller_access();
    let root = who.uid == 0 && who.gid == 0;
    if permissions & 0o7777 != 0o755 || !root {
        job.set_node(
            0,
            NodeAttributes {
                uid: who.uid,
                gid: who.gid,
                permissions: permissions & 0o7777,
            },
        )
        .map_err(|_| Errno::ENOMEM)?;
    }
    if root {
        return Ok(());
    }
    for file in files::FILES {
        job.set_node(
            1 + file_slot(file),
            NodeAttributes {
                uid: who.uid,
                gid: who.gid,
                permissions: u32::from(file.mode()),
            },
        )
        .map_err(|_| Errno::ENOMEM)?;
    }
    Ok(())
}

/// A file's place in [`files::FILES`], which its inode number, and where its
/// owner is kept, are made from.
///
/// Found by its kind, which is one per file, and not by address: `FILES` is a
/// `const`, so the table `files::named` hands out a reference into need not
/// be the one this crate's copy of it is, and a comparison of addresses found
/// no file at all, giving every file the same inode number.
fn file_slot(file: &files::File) -> u64 {
    files::FILES
        .iter()
        .position(|each| each.kind == file.kind)
        .map_or(0, |at| at as u64)
}

/// One interface file of one job.
#[derive(Debug)]
struct Interface {
    /// The job it is a file of.
    job: Arc<Job>,
    /// Which file.
    file: &'static files::File,
    /// The instance's device and timestamps.
    shared: Arc<Shared>,
}

impl Interface {
    /// Its slot among its directory's nodes.
    fn slot(&self) -> u64 {
        1 + file_slot(self.file)
    }
}

impl Inode for Interface {
    fn metadata(&self) -> Metadata {
        node_metadata(
            &self.job,
            &self.shared,
            self.slot(),
            FileType::Regular,
            u32::from(self.file.mode()),
        )
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    /// `chown` and `chmod` of this one file.
    fn set_attributes(&self, change: &SetAttributes) -> Result<()> {
        set_node(&self.job, self.slot(), &self.metadata(), change)
    }

    /// Accepted and ignored, as kernfs ignores the `O_TRUNC` a shell's `>`
    /// asks for.
    fn set_len(&self, _len: u64) -> Result<()> {
        Ok(())
    }

    /// The contents as they are now, and a writer for a file that takes
    /// writes. `cgroup.events` and `memory.events` are the exceptions: they
    /// are rendered when read, and polled, so an open of one is an
    /// [`EventsFile`].
    fn open(&self) -> Result<Option<Arc<dyn Inode>>> {
        let metadata = self.metadata();
        if matches!(self.file.kind, Kind::Events | Kind::MemoryEvents) {
            return Ok(Some(Arc::new(EventsFile {
                job: Arc::clone(&self.job),
                kind: self.file.kind,
                metadata,
                rendered: SpinLock::new(Rendered::default()),
            })));
        }
        let bytes = contents(&self.job, self.file.kind);
        let job = Arc::clone(&self.job);
        let kind = self.file.kind;
        // A move is judged as whoever opened the file, as Linux judges it
        // with the opener's credentials, so a descriptor handed to someone
        // else carries only what its opener could do.
        let writer: Option<procfs::Writer> = self.file.writable.then(|| {
            let opener = Writer {
                who: caller_access(),
                shared: Arc::clone(&self.shared),
            };
            Box::new(move |data: &[u8]| write_to(&job, kind, data, &opener)) as procfs::Writer
        });
        procfs::snapshot(metadata, bytes, writer, Errno::EACCES).map(Some)
    }
}

/// One open of a `cgroup.events` or a `memory.events` (`docs/CGROUPS.md`
/// §4): what it says, rendered afresh by every read from its start, and
/// `POLLPRI` from a change until it is read that way again.
///
/// That is kernfs's `kernfs_generic_poll`. The job's event queue is woken at
/// every flip of populated -- its memory event queue at every OOM and OOM
/// kill counted in it -- and its wake count is the change counter: a read
/// from the start records the count it rendered at, and the file reports
/// priority (with `POLLERR`, as Linux does) while the count has moved since.
/// An open file not yet read reports it too, as on Linux, where the open
/// node's counter starts one ahead of a new open file's.
#[derive(Debug)]
struct EventsFile {
    /// The job it reports on.
    job: Arc<Job>,
    /// Which of the two files.
    kind: Kind,
    /// What `stat` reports, less the size, which is the last rendering's.
    metadata: Metadata,
    /// The last rendering.
    rendered: SpinLock<Rendered>,
}

/// What an [`EventsFile`] last rendered, and when.
#[derive(Debug, Default)]
struct Rendered {
    /// The text.
    bytes: Vec<u8>,
    /// The job's wake count read just before it was rendered, or `None`
    /// before the first read.
    seen: Option<u64>,
}

impl EventsFile {
    /// The queue woken at every change of what it says.
    fn queue(&self) -> &Arc<WaitQueue> {
        if self.kind == Kind::MemoryEvents {
            self.job.memory_events()
        } else {
            self.job.events()
        }
    }

    /// Whether the job has changed since the last rendering.
    fn changed(&self) -> bool {
        self.rendered.lock().seen != Some(self.queue().wakes())
    }
}

impl Inode for EventsFile {
    fn metadata(&self) -> Metadata {
        Metadata {
            size: self.rendered.lock().bytes.len() as u64,
            ..self.metadata
        }
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    /// A read from the start renders the file again, as a `seq_file` does
    /// after `lseek` to 0, and that is what clears `POLLPRI`; a read further
    /// on continues the last rendering.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset == 0 {
            // The count before the text: a flip in between leaves the count
            // ahead of what was rendered, and the file still reporting.
            let seen = self.queue().wakes();
            let bytes = contents(&self.job, self.kind);
            *self.rendered.lock() = Rendered {
                bytes,
                seen: Some(seen),
            };
        }
        let rendered = self.rendered.lock();
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let rest = rendered.bytes.get(start..).unwrap_or_default();
        let count = rest.len().min(buf.len());
        let (Some(to), Some(from)) = (buf.get_mut(..count), rest.get(..count)) else {
            return Ok(0);
        };
        to.copy_from_slice(from);
        Ok(count)
    }

    fn write_at(&self, _offset: u64, _data: &[u8], _append: bool) -> Result<(usize, u64)> {
        Err(Errno::EACCES)
    }

    /// Always readable, as kernfs's `DEFAULT_POLLMASK` is, and `POLLPRI`
    /// with `POLLERR` while the job has changed since the last rendering.
    fn poll(&self) -> Readiness {
        let changed = self.changed();
        Readiness {
            error: changed,
            priority: changed,
            ..Readiness::ALWAYS
        }
    }

    fn poll_changes(&self) -> Option<u64> {
        Some(self.queue().wakes())
    }

    /// The job's event queue, which every flip wakes, or its memory event
    /// queue, which every OOM and OOM kill counted in it wakes.
    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        visit(fs::wake::shared(self.queue()));
        true
    }
}

/// What a file of `job` says now.
fn contents(job: &Arc<Job>, kind: Kind) -> Vec<u8> {
    let mut out = Vec::new();
    match kind {
        Kind::Type => out.extend_from_slice(b"domain\n"),
        Kind::Procs => render::ids(&mut out, &members(job)),
        Kind::Threads => {
            let mut tids: Vec<u32> = members_processes(job)
                .iter()
                .flat_map(|process| procfs::thread_ids(process))
                .collect();
            tids.sort_unstable();
            render::ids(&mut out, &tids);
        }
        Kind::Controllers => controllers::render(&mut out, offered(job)),
        Kind::SubtreeControl => controllers::render(&mut out, job.subtree_control()),
        Kind::Events => render::events(&mut out, job.is_populated(), false),
        Kind::MaxDescendants => render::limit(&mut out, job.limits().1),
        Kind::MaxDepth => render::limit(&mut out, job.limits().0),
        // With no memory to count them, as many as a count can say.
        Kind::Stat => render::stat(&mut out, job.descendants().unwrap_or(u32::MAX)),
        Kind::Freeze => out.extend_from_slice(b"0\n"),
        Kind::Kill => {}
        Kind::CpuWeight => render::number(&mut out, u64::from(job.cpu_weight())),
        Kind::MemoryCurrent => render::number(&mut out, usage(job, Resource::Memory).used),
        Kind::MemoryMax => {
            let limit = usage(job, Resource::Memory).limit;
            render::max(&mut out, (limit != quota::UNLIMITED).then_some(limit));
        }
        Kind::MemoryEvents => {
            let (oom, oom_kill) = job.oom_counts();
            render::memory_events(
                &mut out,
                usage(job, Resource::Memory).refused,
                oom,
                oom_kill,
            );
        }
        Kind::MemoryStat => render::memory_stat(&mut out, usage(job, Resource::Kernel).used),
        Kind::PidsCurrent => render::number(&mut out, usage(job, Resource::Tasks).used),
        Kind::PidsMax => {
            let limit = usage(job, Resource::Tasks).limit;
            render::max(&mut out, (limit != quota::UNLIMITED).then_some(limit));
        }
        Kind::PidsEvents => render::pids_events(&mut out, usage(job, Resource::Tasks).refused),
    }
    out
}

/// What `job` holds of `resource`: nothing, and no limit, for the root,
/// which has no controller files anyway.
fn usage(job: &Job, resource: Resource) -> quota::Usage {
    job.usage(resource).unwrap_or(quota::Usage {
        used: 0,
        limit: quota::UNLIMITED,
        refused: 0,
    })
}

/// The live processes directly in `job`, not beneath it, in pid order.
///
/// None, with no memory to list them.
fn members_processes(job: &Arc<Job>) -> Vec<Arc<Process>> {
    registry::live()
        .unwrap_or_default()
        .into_iter()
        .filter(|process| Arc::ptr_eq(&process.job(), job) && !process.is_terminated())
        .collect()
}

/// Their pids.
fn members(job: &Arc<Job>) -> Vec<u32> {
    members_processes(job)
        .iter()
        .map(|process| process.pid())
        .collect()
}

/// What a job's `cgroup.controllers` lists: what its parent enables for
/// its children, or for the root, what is built.
fn offered(job: &Job) -> Set {
    job.parent()
        .map_or(BUILT, |parent| parent.subtree_control())
}

/// Who opened a file that takes writes, which a move is judged as.
struct Writer {
    /// The opener's identity.
    who: Access,
    /// The instance, for the metadata a permission is judged by.
    shared: Arc<Shared>,
}

/// The process writing, as the audit record's subject: the kernel when a
/// write comes from none.
fn writer() -> audit::Subject {
    process::current().map_or(audit::Subject::KERNEL, |process| {
        audit::Subject::of(process.core())
    })
}

/// Record the writer setting `job`'s limit on `resource` to `limit`.
fn limit_set(job: &Job, resource: Resource, limit: u64) {
    audit::limit_set(
        audit::CGROUP_LIMIT,
        writer(),
        job.id(),
        audit::resource_number(resource),
        limit,
    );
}

/// A write of `data` to a file of `job`, opened by `opener`.
fn write_to(job: &Arc<Job>, kind: Kind, data: &[u8], opener: &Writer) -> Result<usize> {
    match kind {
        Kind::Procs => {
            let target = write::parse_procs(data).map_err(errno)?;
            if job.is_removed() {
                return Err(Errno::ENODEV);
            }
            let process = match target {
                Target::Writer => process::current().ok_or(Errno::ESRCH)?,
                Target::Pid(pid) => registry::find(pid).ok_or(Errno::ESRCH)?,
            };
            // Linux's `cgroup_procs_write_permission`: a writer in a cgroup
            // namespace moves only between cgroups inside its own root, and
            // is told `ENOENT`, as if the rest of the tree were not there.
            let visible = crate::syscall::nsproxy::acting().cgroup;
            if !Arc::ptr_eq(&visible, crate::syscall::nsproxy::initial_cgroup())
                && (!visible.root().contains(&process.job()) || !visible.root().contains(job))
            {
                return Err(Errno::ENOENT);
            }
            attach_permissions(&opener.who, &process.job(), job, &opener.shared)?;
            job.adopt(&process).map_err(move_errno)?;
        }
        Kind::Kill => {
            write::parse_kill(data).map_err(errno)?;
            let ended = job.kill_members().map_err(|_| Errno::ENOMEM)?;
            audit::ended(
                audit::CGROUP_KILLED,
                writer(),
                audit::Target {
                    kind: audit::target::JOB,
                    id: job.id(),
                },
                [u32::try_from(ended).unwrap_or(u32::MAX), 0, 0],
            );
        }
        Kind::SubtreeControl => {
            let change = controllers::parse_change(data, BUILT).map_err(errno)?;
            job.change_subtree_control(change, offered(job))
                .map_err(|refused| match refused {
                    JobError::Busy | JobError::Internal => Errno::EBUSY,
                    JobError::NoMemory => Errno::ENOMEM,
                    _ => Errno::ENOENT,
                })?;
        }
        Kind::MaxDepth => job.set_max_depth(write::parse_limit(data).map_err(errno)?),
        Kind::MaxDescendants => job.set_max_descendants(write::parse_limit(data).map_err(errno)?),
        Kind::Type => write::parse_type(data).map_err(errno)?,
        Kind::Threads | Kind::Freeze => return Err(Errno::EOPNOTSUPP),
        Kind::PidsMax => {
            let limit = write::parse_pids_max(data).map_err(errno)?;
            let limit = limit.unwrap_or(quota::UNLIMITED);
            if job.set_limit(Resource::Tasks, limit) {
                limit_set(job, Resource::Tasks, limit);
            }
        }
        Kind::MemoryMax => {
            let limit = write::parse_memory_max(data).map_err(errno)?;
            // Whole pages, as Linux's page counter keeps it.
            let bytes = limit.map_or(quota::UNLIMITED, |bytes| {
                bytes - bytes % ferrix_bootinfo::PAGE_SIZE
            });
            if job.set_limit(Resource::Memory, bytes) {
                limit_set(job, Resource::Memory, bytes);
            }
        }
        Kind::CpuWeight => {
            let weight = write::parse_weight(data).map_err(errno)?;
            if job.set_cpu_weight(weight) {
                audit::limit_set(
                    audit::CGROUP_LIMIT,
                    writer(),
                    job.id(),
                    ferrix_audit::resource::CPU_WEIGHT,
                    u64::from(weight),
                );
            }
        }
        Kind::Controllers
        | Kind::Events
        | Kind::Stat
        | Kind::MemoryCurrent
        | Kind::MemoryEvents
        | Kind::MemoryStat
        | Kind::PidsCurrent
        | Kind::PidsEvents => return Err(Errno::EACCES),
    }
    Ok(data.len())
}

/// `/proc/<pid>/cgroup` for a process in `job`.
pub(crate) fn proc_cgroup(job: &Job, root: &Job) -> Result<Vec<u8>> {
    let names = relative_names(job, root)?;
    let mut out = Vec::new();
    render::proc_cgroup(&mut out, names.iter().map(String::as_bytes));
    Ok(out)
}

/// The names that lead from `root` to `job`, climbing with `..` where `job` is
/// not beneath `root`: what Linux's `cgroup_path_ns` prints for a reader in a
/// cgroup namespace rooted at `root` (`/..` once for each level of `root`
/// below the two's closest common ancestor, then the names down to `job`). A
/// root's own path is empty, which the renderer writes as `/`.
pub(crate) fn relative_names(job: &Job, root: &Job) -> Result<Vec<String>> {
    let to_job = job.path_names().map_err(|_| Errno::ENOMEM)?;
    let to_root = root.path_names().map_err(|_| Errno::ENOMEM)?;
    // Names are unique among siblings, so the names are the path.
    let shared = to_job
        .iter()
        .zip(&to_root)
        .take_while(|(below, above)| below == above)
        .count();
    let mut names = Vec::new();
    for _ in shared..to_root.len() {
        names.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
        names.push(String::from(".."));
    }
    for name in to_job.into_iter().skip(shared) {
        names.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
        names.push(name);
    }
    Ok(names)
}

/// What [`check`] counted, for the boot line.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Cgroups made and removed.
    pub(crate) made: u32,
    /// Writes and names refused as Linux refuses them.
    pub(crate) refusals: u32,
    /// Waits on `cgroup.events` that a release's wake ended.
    pub(crate) woken: u32,
    /// Moves judged by the delegation rules, allowed and refused.
    pub(crate) moves: u32,
    /// Whether a child started by `CLONE_INTO_CGROUP` found itself there.
    pub(crate) cloned: bool,
    /// Native registrations for `EMPTY` fired by the populated flip.
    pub(crate) emptied: u32,
    /// Writes to the controllers' files refused as Linux refuses them.
    pub(crate) controlled: u32,
    /// Processes the scoped OOM kill ended at a `memory.max`.
    pub(crate) oom_killed: u64,
    /// What a native process that uid 1000 made in its delegated cgroup
    /// exited with: the low byte of what `getuid` answered it
    /// (`docs/AUTH.md` §7, P0).
    pub(crate) created_as: u32,
    /// A delegatee's attempts on its own cgroup's limits, natively and
    /// through the files, refused.
    pub(crate) limits_refused: u32,
}

/// Where [`check`] mounts its cgroupfs: under `/tmp`, and gone afterwards.
const CHECK_AT: &[u8] = b"/tmp/cgroup-check";

/// A check's failure, by name.
type Checked<T> = core::result::Result<T, &'static str>;

/// The check's way into its mount: paths beneath [`CHECK_AT`], driven
/// through the namespace as a program's calls would be.
struct Harness {
    /// The namespace.
    ns: &'static ferrix_vfs::Namespace,
    /// Root's view of it.
    ctx: ferrix_vfs::Context,
    /// What was counted.
    report: Report,
}

impl Harness {
    /// `tail` beneath the mount.
    fn path(tail: &[u8]) -> Vec<u8> {
        let mut path = Vec::from(CHECK_AT);
        path.extend_from_slice(tail);
        path
    }

    /// Everything in the file at `whole`, read to the end as a program reads
    /// it. `fs::read_file` sizes its buffer from `stat`, and a generated file
    /// -- here as in procfs -- says 0.
    fn read_path(&self, whole: &[u8]) -> Result<Vec<u8>> {
        let flags = ferrix_vfs::OpenFlags {
            read: true,
            ..ferrix_vfs::OpenFlags::default()
        };
        let file = self.ns.open(&self.ctx, None, whole, &flags, 0)?;
        Harness::read_to_end(&file)
    }

    /// Everything left in an open file, read to the end.
    fn read_to_end(file: &OpenFile) -> Result<Vec<u8>> {
        let mut contents = Vec::new();
        let mut chunk = [0_u8; 256];
        loop {
            let count = file.read(&mut chunk)?;
            let Some(read) = chunk.get(..count).filter(|read| !read.is_empty()) else {
                return Ok(contents);
            };
            contents.extend_from_slice(read);
        }
    }

    /// The file at `tail` beneath the mount, opened for reading.
    fn open_read(&self, tail: &[u8]) -> Result<Arc<OpenFile>> {
        let flags = ferrix_vfs::OpenFlags {
            read: true,
            ..ferrix_vfs::OpenFlags::default()
        };
        self.ns
            .open(&self.ctx, None, &Harness::path(tail), &flags, 0)
    }

    /// The file at `tail` beneath the mount, read whole.
    fn read(&self, tail: &[u8]) -> Result<Vec<u8>> {
        self.read_path(&Harness::path(tail))
    }

    /// Whether the file at `tail` reads exactly `expected`.
    fn reads(&self, tail: &[u8], expected: &[u8]) -> bool {
        self.read(tail).as_deref() == Ok(expected)
    }

    /// Write `data` to the file at `tail`, in one write.
    fn write(&self, tail: &[u8], data: &[u8]) -> Result<usize> {
        let flags = ferrix_vfs::OpenFlags {
            write: true,
            ..ferrix_vfs::OpenFlags::default()
        };
        self.ns
            .open(&self.ctx, None, &Harness::path(tail), &flags, 0)?
            .write(data)
    }

    /// `mkdir` at `tail`.
    fn mkdir(&self, tail: &[u8]) -> Result<()> {
        self.ns.mkdir(&self.ctx, None, &Harness::path(tail), 0o755)
    }

    /// `rmdir` at `tail`.
    fn rmdir(&self, tail: &[u8]) -> Result<()> {
        self.ns.rmdir(&self.ctx, None, &Harness::path(tail))
    }

    /// Whether something is at `tail`.
    fn exists(&self, tail: &[u8]) -> bool {
        self.ns
            .resolve(&self.ctx, None, &Harness::path(tail), true)
            .is_ok()
    }

    /// Require the error an operation answered, `got`, to be `errno`, and
    /// count it.
    fn refused(&mut self, got: Option<Errno>, errno: Errno, what: &'static str) -> Checked<()> {
        match got {
            Some(got) if got == errno => {
                self.report.refusals += 1;
                Ok(())
            }
            _ => Err(what),
        }
    }
}

/// Stage 13's cgroupfs check, landing G2: a mount of `cgroup2` shows the job
/// tree, `mkdir` makes a job and `rmdir` takes an empty one, a pid written to
/// `cgroup.procs` moves that process, `/proc/<pid>/cgroup` and
/// `cgroup.events` say where it is and that its cgroup is populated,
/// `cgroup.kill` ends it and leaves the cgroup to be removed, the limits on
/// descendants hold, and what Linux refuses is refused.
///
/// # Errors
///
/// The first thing that was not as Linux has it, by name.
pub(crate) fn check() -> Checked<Report> {
    let ns = fs::namespace();
    let mut harness = Harness {
        ns,
        ctx: ns.context(),
        report: Report::default(),
    };
    match ns.mkdir(&harness.ctx, None, CHECK_AT, 0o755) {
        Ok(()) | Err(Errno::EEXIST) => {}
        Err(_) => return Err("could not make the cgroupfs check's mount point"),
    }
    let at = ns
        .resolve(&harness.ctx, None, CHECK_AT, true)
        .map_err(|_| "the cgroupfs check's mount point did not resolve")?;
    let _mount = ns
        .mount(Arc::new(Cgroupfs::new()), &at)
        .map_err(|_| "cgroup2 did not mount")?;

    let process =
        process::new_for_check().map_err(|_| "could not make a process for the cgroupfs check")?;
    check_the_root(&harness, process.pid())?;
    check_a_move_and_a_kill(&mut harness, &process)?;
    check_the_limits(&mut harness)?;
    harness.report.woken = events_check::run(&mut harness)?;
    let delegated = delegation_check::run(&mut harness)?;
    harness.report.moves = delegated.moves;
    harness.report.cloned = delegated.cloned;
    harness.report.emptied = native_check::run(&mut harness)?;
    harness.report.controlled = controllers_check::run(&mut harness, &process)?;
    harness.report.oom_killed = oom_check::run(&mut harness)?;
    harness.report.created_as = creator_check::run(&mut harness)?;
    harness.report.limits_refused = limits_check::run(&mut harness)?;

    let root = ns
        .resolve(&harness.ctx, None, CHECK_AT, true)
        .map_err(|_| "the cgroupfs mount did not resolve")?;
    ns.unmount(&root).map_err(|_| "cgroup2 did not unmount")?;
    let _ = ns.rmdir(&harness.ctx, None, CHECK_AT);
    drop(process);
    Ok(harness.report)
}

/// The root lists every process not moved elsewhere, and has none of the
/// files Linux keeps off it.
fn check_the_root(harness: &Harness, pid: u32) -> Checked<()> {
    let listed = alloc::format!("{pid}");
    let procs = harness
        .read(b"/cgroup.procs")
        .map_err(|_| "the root's cgroup.procs did not read")?;
    if !procs
        .split(|&byte| byte == b'\n')
        .any(|line| line == listed.as_bytes())
    {
        return Err("a new process is not listed in the root's cgroup.procs");
    }
    if harness.exists(b"/cgroup.kill") {
        return Err("the root cgroup has a cgroup.kill");
    }
    Ok(())
}

/// A cgroup made, a process moved in by its pid and seen there, the cgroup
/// kept while populated, then emptied by `cgroup.kill` and removed.
fn check_a_move_and_a_kill(harness: &mut Harness, process: &Process) -> Checked<()> {
    const EMPTY: &[u8] = b"populated 0\nfrozen 0\n";
    const FULL: &[u8] = b"populated 1\nfrozen 0\n";
    let pid = process.pid();
    let listed = alloc::format!("{pid}\n");

    harness
        .mkdir(b"/check-a")
        .map_err(|_| "mkdir in cgroupfs failed")?;
    harness.report.made += 1;
    if !harness.reads(b"/check-a/cgroup.events", EMPTY) {
        return Err("a new cgroup's cgroup.events does not say it is empty");
    }
    let again = harness.mkdir(b"/check-a");
    harness.refused(
        again.err(),
        Errno::EEXIST,
        "mkdir over a cgroup was not EEXIST",
    )?;
    let over = harness.mkdir(b"/cgroup.procs");
    harness.refused(
        over.err(),
        Errno::EEXIST,
        "mkdir over an interface file was not EEXIST",
    )?;

    let _ = harness
        .write(b"/check-a/cgroup.procs", listed.as_bytes())
        .map_err(|_| "writing a pid to cgroup.procs failed")?;
    if process.job().name() != Some("check-a") {
        return Err("a pid written to cgroup.procs did not move its process");
    }
    if !harness.reads(b"/check-a/cgroup.procs", listed.as_bytes()) {
        return Err("a cgroup's cgroup.procs does not list the process moved into it");
    }
    let proc_path = alloc::format!("/proc/{pid}/cgroup");
    if harness.read_path(proc_path.as_bytes()).as_deref() != Ok(&b"0::/check-a\n"[..]) {
        return Err("/proc/<pid>/cgroup does not name the cgroup the process was moved to");
    }
    if !harness.reads(b"/check-a/cgroup.events", FULL) {
        return Err("a cgroup with a process in it does not say it is populated");
    }
    let busy = harness.rmdir(b"/check-a");
    harness.refused(
        busy.err(),
        Errno::EBUSY,
        "rmdir of a populated cgroup was not EBUSY",
    )?;

    let zero = harness.write(b"/check-a/cgroup.kill", b"0\n");
    harness.refused(
        zero.err(),
        Errno::ERANGE,
        "cgroup.kill took a number other than 1",
    )?;
    let _ = harness
        .write(b"/check-a/cgroup.kill", b"1\n")
        .map_err(|_| "writing 1 to cgroup.kill failed")?;
    if !process.is_terminated() {
        return Err("cgroup.kill did not end the process in its cgroup");
    }
    if !harness.reads(b"/check-a/cgroup.events", EMPTY) {
        return Err("a killed cgroup does not say it is empty");
    }
    harness
        .rmdir(b"/check-a")
        .map_err(|_| "rmdir of an emptied cgroup failed")?;
    if harness.exists(b"/check-a") {
        return Err("a removed cgroup can still be found");
    }
    Ok(())
}

/// `cgroup.max.descendants` holds and `cgroup.stat` counts, and what the
/// rest of the files refuse is refused.
fn check_the_limits(harness: &mut Harness) -> Checked<()> {
    harness
        .mkdir(b"/check-b")
        .map_err(|_| "mkdir of a second cgroup failed")?;
    let _ = harness
        .write(b"/check-b/cgroup.max.descendants", b"1\n")
        .map_err(|_| "writing cgroup.max.descendants failed")?;
    harness
        .mkdir(b"/check-b/c")
        .map_err(|_| "mkdir within cgroup.max.descendants failed")?;
    harness.report.made += 2;
    let past = harness.mkdir(b"/check-b/d");
    harness.refused(
        past.err(),
        Errno::EAGAIN,
        "mkdir past cgroup.max.descendants was not EAGAIN",
    )?;
    if !harness.reads(
        b"/check-b/cgroup.stat",
        b"nr_descendants 1\nnr_dying_descendants 0\n",
    ) {
        return Err("cgroup.stat does not count a cgroup's descendant");
    }
    for (file, data, errno, what) in [
        (
            &b"/check-b/cgroup.subtree_control"[..],
            &b"+io\n"[..],
            Errno::EINVAL,
            "cgroup.subtree_control enabled a controller that is not built",
        ),
        (
            b"/check-b/cgroup.max.depth",
            b"-1\n",
            Errno::ERANGE,
            "cgroup.max.depth took a negative limit",
        ),
        (
            b"/check-b/cgroup.type",
            b"threaded\n",
            Errno::EOPNOTSUPP,
            "cgroup.type took threaded",
        ),
        (
            b"/check-b/cgroup.procs",
            b"-1\n",
            Errno::EINVAL,
            "cgroup.procs took a negative pid",
        ),
    ] {
        let outcome = harness.write(file, data);
        harness.refused(outcome.err(), errno, what)?;
    }
    harness
        .rmdir(b"/check-b/c")
        .map_err(|_| "rmdir of an empty nested cgroup failed")?;
    harness
        .rmdir(b"/check-b")
        .map_err(|_| "rmdir of an emptied cgroup failed")
}
