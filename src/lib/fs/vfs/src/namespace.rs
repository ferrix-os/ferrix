//! Mounts, and every operation that takes a path.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use ferrix_kmem::{Charge, arc_footprint};
use ferrix_linux_abi::errno::Errno;
use ferrix_sync::{Parker, SleepLock};

use crate::Result;
use crate::SpinLock;
use crate::access::{Access, MAY_READ, MAY_WRITE};
use crate::bottom::Bottom;
use crate::dentry::Dentry;
use crate::file::{OpenFile, OpenFlags};
use crate::node::{FileSystem, FileType, Inode, Metadata, NewNode, SetAttributes, StatFs};
use crate::walk::{LastPart, Resolve, Walked, up};

/// How many dentries the namespace keeps alive that nothing else refers to.
///
/// The cache's whole eviction policy: a queue of this many strong references,
/// oldest dropped first. Sized for a shell and a boot, not for a compiler;
/// stage 16 will want it scaled to memory, which is a change to this number's
/// source and not to the structure.
pub const DEFAULT_CACHE: usize = 4096;

/// What one mount allows, apart from what its filesystem does: Linux's
/// per-mount `MNT_*` flags, which `mount(2)` sets from its `MS_*` flags and
/// `MS_REMOUNT` changes.
///
/// Four are enforced where the operation they govern is decided -- see
/// [`Location::require_writable`], [`Mount::no_devices`],
/// [`Mount::no_exec`] and [`Mount::no_set_id`] -- and the three access-time
/// ones are recorded and shown, and change nothing, because no filesystem
/// here keeps an access time apart from the others.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MountFlags(u32);

impl MountFlags {
    /// None: read-write, set-id bits and devices honoured, programs run.
    pub const NONE: MountFlags = MountFlags(0);
    /// `MS_RDONLY`: nothing on it may be changed through this mount.
    pub const READ_ONLY: MountFlags = MountFlags(1);
    /// `MS_NOSUID`: set-user-id and set-group-id bits are ignored.
    pub const NOSUID: MountFlags = MountFlags(1 << 1);
    /// `MS_NODEV`: its device nodes cannot be opened.
    pub const NODEV: MountFlags = MountFlags(1 << 2);
    /// `MS_NOEXEC`: nothing on it runs, or is mapped executable.
    pub const NOEXEC: MountFlags = MountFlags(1 << 3);
    /// `MS_NOATIME`.
    pub const NOATIME: MountFlags = MountFlags(1 << 4);
    /// `MS_NODIRATIME`.
    pub const NODIRATIME: MountFlags = MountFlags(1 << 5);
    /// `MS_RELATIME`.
    pub const RELATIME: MountFlags = MountFlags(1 << 6);
    /// The access-time flags, which a remount that names none keeps.
    pub const ATIME: MountFlags = MountFlags((1 << 4) | (1 << 5) | (1 << 6));
    /// Every flag there is.
    const ALL: u32 = (1 << 7) - 1;

    /// The flags `bits` holds, from [`MountFlags::bits`].
    #[must_use]
    pub const fn from_bits(bits: u32) -> MountFlags {
        MountFlags(bits & Self::ALL)
    }

    /// The flags as a word.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether every flag of `other` is set.
    #[must_use]
    pub const fn contains(self, other: MountFlags) -> bool {
        self.0 & other.0 == other.0
    }

    /// Both sets.
    #[must_use]
    pub const fn union(self, other: MountFlags) -> MountFlags {
        MountFlags(self.0 | other.0)
    }

    /// These, less `other`'s.
    #[must_use]
    pub const fn without(self, other: MountFlags) -> MountFlags {
        MountFlags(self.0 & !other.0)
    }

    /// `statfs`'s `f_flags` for a mount with these: `ST_VALID` and the
    /// `ST_*` bit of each flag, Linux's `flags_by_mnt`.
    #[must_use]
    pub fn statfs_flags(self) -> u64 {
        use ferrix_linux_abi::types::{
            ST_NOATIME, ST_NODEV, ST_NODIRATIME, ST_NOEXEC, ST_NOSUID, ST_RDONLY, ST_RELATIME,
            ST_VALID,
        };
        [
            (Self::READ_ONLY, ST_RDONLY),
            (Self::NOSUID, ST_NOSUID),
            (Self::NODEV, ST_NODEV),
            (Self::NOEXEC, ST_NOEXEC),
            (Self::NOATIME, ST_NOATIME),
            (Self::NODIRATIME, ST_NODIRATIME),
            (Self::RELATIME, ST_RELATIME),
        ]
        .into_iter()
        .filter(|&(flag, _)| self.contains(flag))
        .fold(ST_VALID, |bits, (_, bit)| bits | bit)
    }

    /// The options `/proc/mounts` and `mountinfo` print for them, in
    /// Linux's order (`show_vfsmnt`, `show_mnt_opts`): `rw` or `ro`, then
    /// the others that are set.
    #[must_use]
    pub fn options(self) -> Vec<u8> {
        let mut out = Vec::from(if self.contains(Self::READ_ONLY) {
            &b"ro"[..]
        } else {
            &b"rw"[..]
        });
        for (flag, name) in [
            (Self::NOSUID, &b",nosuid"[..]),
            (Self::NODEV, b",nodev"),
            (Self::NOEXEC, b",noexec"),
            (Self::NOATIME, b",noatime"),
            (Self::NODIRATIME, b",nodiratime"),
            (Self::RELATIME, b",relatime"),
        ] {
            if self.contains(flag) {
                out.extend_from_slice(name);
            }
        }
        out
    }
}

/// Mount flags that cannot be cleared, and a mount that cannot be separated
/// from its parent: Linux's `MNT_LOCK_*` and `MNT_LOCKED`, set on every mount
/// a namespace copy brings into a namespace its creator's user namespace
/// does not own (`docs/NAMESPACES.md` M3, M4). The low bits are
/// [`MountFlags`]' own: a set lock bit keeps that flag set.
const LOCK_FLAGS: u32 =
    MountFlags::READ_ONLY.0 | MountFlags::NOSUID.0 | MountFlags::NODEV.0 | MountFlags::NOEXEC.0;
/// The access-time mode cannot change.
const LOCK_ATIME: u32 = 1 << 8;
/// The mount cannot be unmounted, bound without `MS_REC`, or pivoted to on its own:
/// what it covers stays hidden.
const LOCK_PARENT: u32 = 1 << 9;

/// A filesystem as every mount of it shares it: Linux's superblock, less
/// everything a filesystem here keeps for itself.
///
/// What it adds is the one state a plain `MS_REMOUNT` changes for the whole
/// filesystem rather than for the one mount it names: read-only. A bind of
/// the filesystem, and every copy `MS_REC` makes, share it with the mount
/// they came from, so a plain remount read-only through any of them refuses
/// writes through all of them, while `MS_REMOUNT | MS_BIND` changes only the
/// mount's own [`MountFlags`] (`docs/NAMESPACES.md` §2.1, N2).
struct Superblock {
    fs: Arc<dyn FileSystem>,
    /// Set by a plain remount read-only, cleared by a plain remount without.
    read_only: AtomicBool,
}

impl Superblock {
    /// A new one for `fs`, writable.
    fn new(fs: Arc<dyn FileSystem>) -> Arc<Superblock> {
        Arc::new(Superblock {
            fs,
            read_only: AtomicBool::new(false),
        })
    }
}

/// A filesystem instance attached to the tree.
pub struct Mount {
    id: u64,
    /// Its [`MountFlags`], which a remount changes in place: an atomic, so
    /// that every check reads them without a lock and sees one remount
    /// whole.
    flags: AtomicU32,
    /// Which of those cannot change, and whether it is locked to its parent
    /// (`LOCK_*`): set when a copy into a less privileged namespace makes it.
    locks: AtomicU32,
    sb: Arc<Superblock>,
    /// Where in its filesystem it starts: the filesystem's root for a mount
    /// made by `mount(2)`, any directory or file of it for a bind.
    root: Arc<Dentry>,
    /// The mount it is on and the dentry it covers; `None` for the root and
    /// for a mount `MNT_DETACH` took out of the tree.
    ///
    /// Written only while the namespace's table lock is held, so that the
    /// table and every parent pointer change together; its own lock, a leaf,
    /// is what lets a walk read it without the table's. Every change keeps
    /// the graph of parents acyclic at each step, so a walk going `..` never
    /// meets a cycle, whenever it reads.
    parent: SpinLock<Option<(Arc<Mount>, Arc<Dentry>)>>,
    /// The table of the namespace it was made in, which is where a walk
    /// crossing a mount point on it looks (`docs/NAMESPACES.md` §2.1): a
    /// descriptor opened before an `unshare`, or `fchdir` to one, keeps
    /// working in the tree it was opened in, whoever walks from it. Set for
    /// its whole life, since a copy of a namespace makes new mounts; empty
    /// for a [`Location::detached`] one.
    tree: Weak<Tree>,
    /// Where a sleeping lock made for something on this mount waits: the
    /// namespace's, so that an open file reaches it through its location
    /// and nothing that opens a file has to be told.
    parker: Arc<dyn Parker>,
    /// The kernel heap it holds, charged to the job that mounted it, or that
    /// made the pipe or socket a detached one is for (F-37).
    _charge: Charge,
}

impl Mount {
    /// Identifier, unique within the namespace.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Whether it is locked to its parent: unmounting it alone, binding it
    /// without `MS_REC` and pivoting to it would reveal what it covers
    /// (`docs/NAMESPACES.md` M4).
    #[must_use]
    pub fn is_locked(&self) -> bool {
        self.locks.load(Ordering::Acquire) & LOCK_PARENT != 0
    }

    /// The filesystem mounted here.
    #[must_use]
    pub fn filesystem(&self) -> &Arc<dyn FileSystem> {
        &self.sb.fs
    }

    /// Whether the filesystem, as every mount of it shares it, is read-only:
    /// what a plain `MS_REMOUNT` with `MS_RDONLY` set, or the filesystem
    /// itself refusing every write. `mountinfo`'s last field.
    #[must_use]
    pub fn filesystem_read_only(&self) -> bool {
        self.sb.read_only.load(Ordering::Acquire) || self.sb.fs.read_only()
    }

    /// Whether `other` is a mount of the same filesystem: a bind of it, or
    /// what it was bound from.
    #[must_use]
    pub fn shares_filesystem(&self, other: &Mount) -> bool {
        Arc::ptr_eq(&self.sb, &other.sb)
    }

    /// The dentry of the filesystem's root directory.
    #[must_use]
    pub fn root(&self) -> &Arc<Dentry> {
        &self.root
    }

    /// The mount this one is on, and the dentry it covers.
    #[must_use]
    pub fn parent(&self) -> Option<(Arc<Mount>, Arc<Dentry>)> {
        self.parent.lock().clone()
    }

    /// Make `parent` its parent, answering the old one for the caller to
    /// drop after every lock is released. The table lock is held.
    fn set_parent(
        &self,
        parent: Option<(Arc<Mount>, Arc<Dentry>)>,
    ) -> Option<(Arc<Mount>, Arc<Dentry>)> {
        core::mem::replace(&mut *self.parent.lock(), parent)
    }

    /// Where a sleeping lock for something on this mount waits.
    #[must_use]
    pub fn parker(&self) -> &Arc<dyn Parker> {
        &self.parker
    }

    /// Its flags, as the last `mount` or remount left them.
    #[must_use]
    pub fn flags(&self) -> MountFlags {
        MountFlags::from_bits(self.flags.load(Ordering::Acquire))
    }

    /// Whether nothing may be changed through it: `MS_RDONLY`.
    #[must_use]
    pub fn read_only(&self) -> bool {
        self.flags().contains(MountFlags::READ_ONLY)
    }

    /// Whether its device nodes may not be opened: `MS_NODEV`, Linux's
    /// `path_nodev`.
    #[must_use]
    pub fn no_devices(&self) -> bool {
        self.flags().contains(MountFlags::NODEV)
    }

    /// Whether nothing on it may run or be mapped executable: `MS_NOEXEC`,
    /// Linux's `path_noexec`.
    #[must_use]
    pub fn no_exec(&self) -> bool {
        self.flags().contains(MountFlags::NOEXEC)
    }

    /// Whether a program on it runs without its set-id bits: `MS_NOSUID`,
    /// Linux's `mnt_may_suid`.
    #[must_use]
    pub fn no_set_id(&self) -> bool {
        self.flags().contains(MountFlags::NOSUID)
    }
}

impl fmt::Debug for Mount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mount")
            .field("id", &self.id)
            .field("fs", &self.sb.fs.name())
            .finish_non_exhaustive()
    }
}

/// A place in the tree: a dentry, and the mount it was reached through.
///
/// Both halves, because a dentry alone is ambiguous — the same filesystem can
/// be mounted twice — and `..` from a mount's root depends on which mount.
#[derive(Clone, Debug)]
pub struct Location {
    /// The mount.
    pub mount: Arc<Mount>,
    /// The name within it.
    pub dentry: Arc<Dentry>,
}

impl Location {
    /// What it names.
    ///
    /// # Errors
    ///
    /// `ENOENT` for a negative dentry.
    pub fn inode(&self) -> Result<Arc<dyn Inode>> {
        self.dentry.inode().ok_or(Errno::ENOENT)
    }

    /// Whether two locations are the same place.
    #[must_use]
    pub fn same(&self, other: &Location) -> bool {
        Arc::ptr_eq(&self.mount, &other.mount) && Arc::ptr_eq(&self.dentry, &other.dentry)
    }

    /// `..`, with no root to stop at but the top of the tree: the namespace's
    /// root, or a [`Location::detached`] one, which is its own parent.
    #[must_use]
    pub fn parent(&self) -> Location {
        up(self, None)
    }

    /// A place for an object no directory holds: a pipe.
    ///
    /// An open file needs a location -- `fstat` takes the device from its
    /// mount, `fstatfs` the filesystem, `/proc/self/fd` the name -- and `pipe`
    /// makes two open files for an inode that has none. This is one: a mount
    /// of `fs` on nothing, whose root dentry is `inode`, called `name`. It is
    /// in no namespace's table, so no walk can reach it, `..` from it stays
    /// where it is, and nothing can be mounted on it.
    /// [`Namespace::path_of`] reports it as `name` alone, which is how Linux
    /// reports `pipe:[1234]`. `parker` is where the open file's sleeping
    /// lock waits; a stream never takes it, but every description has one.
    ///
    /// # Errors
    ///
    /// `ENOMEM` past the job's memory limit.
    pub fn detached(
        fs: Arc<dyn FileSystem>,
        inode: Arc<dyn Inode>,
        name: &[u8],
        parker: Arc<dyn Parker>,
    ) -> Result<Location> {
        let charge = crate::charge(mount_footprint())?;
        let dentry = Dentry::named_root(Box::from(name), inode)?;
        let mount = Arc::new(Mount {
            id: DETACHED_MOUNT,
            flags: AtomicU32::new(0),
            locks: AtomicU32::new(0),
            sb: Superblock::new(fs),
            root: Arc::clone(&dentry),
            parent: SpinLock::new(None),
            tree: Weak::new(),
            parker,
            _charge: charge,
        });
        Ok(Location { mount, dentry })
    }

    /// Whether this is a [`Location::detached`] one.
    #[must_use]
    pub fn is_detached(&self) -> bool {
        self.mount.id == DETACHED_MOUNT
    }

    /// `EROFS` if its mount is read-only: Linux's `mnt_want_write`, which
    /// every change to a file through a path or a descriptor asks first.
    ///
    /// # Errors
    ///
    /// `EROFS`.
    pub fn require_writable(&self) -> Result<()> {
        if self.mount.read_only() || self.mount.sb.read_only.load(Ordering::Acquire) {
            Err(Errno::EROFS)
        } else {
            Ok(())
        }
    }

    /// Whether this is the root of its mount, the one place `MS_REMOUNT`
    /// and `umount2` act on.
    #[must_use]
    pub fn is_mount_root(&self) -> bool {
        Arc::ptr_eq(&self.dentry, &self.mount.root)
    }
}

/// The mount identifier every [`Location::detached`] mount carries. Mounts
/// in a tree are numbered from one, so no mount in a tree is ever this.
const DETACHED_MOUNT: u64 = 0;

/// What one mount holds of the kernel heap, as it is charged: the mount, and
/// the superblock a mount made by `mount(2)` brings (a bind shares one, and
/// is charged the same, which over-counts by a superblock).
fn mount_footprint() -> usize {
    arc_footprint::<Mount>() + arc_footprint::<Superblock>()
}

/// The two places a relative and an absolute path start from, and who is
/// walking.
///
/// A process's, in the kernel: `chroot` changes `root`, `chdir` changes
/// `cwd`, and `who` is its filesystem ids. The namespace takes them as an
/// argument rather than knowing about processes, which is what lets the host
/// tests be two processes -- or two users -- at once.
#[derive(Clone, Debug)]
pub struct Context {
    /// Where `/` is.
    pub root: Location,
    /// Where a relative path starts.
    pub cwd: Location,
    /// The identity every permission check along the way is made against.
    pub who: Access,
    /// The mount namespace its mount calls act on, and whose mounts
    /// `/proc/self/mounts` lists; `None` for the kernel's first one, which is
    /// every context's until an `unshare` or `clone` asks for another
    /// (`docs/NAMESPACES.md` §2.1). A walk does not read it: crossing a
    /// mount point looks in the table of the mount it is on.
    pub ns: Option<Arc<Namespace>>,
}

/// What `stat` reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    /// The device of the filesystem holding it.
    pub dev: u64,
    /// Everything else.
    pub metadata: Metadata,
}

/// Whether a rename may replace an existing name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenameMode {
    /// Replace it, as plain `rename` does.
    Replace,
    /// Refuse with `EEXIST`, as `RENAME_NOREPLACE` does.
    NoReplace,
}

/// What every mount namespace of the kernel shares: the mount ids, the
/// rename lock and the dentry cache (`docs/NAMESPACES.md` §2.1).
///
/// A cache per namespace would keep an unlinked file's dentry, and its
/// pages, alive in every namespace but the one that unlinked it; a rename
/// lock per namespace would let two renames on one filesystem, from two
/// namespaces, each invalidate the other's ancestry check. So they are the
/// kernel's, and a namespace holds them through an `Arc`.
struct Shared {
    /// The next mount's id: one counter for the kernel, so that a table's
    /// key, which names a mount by id, never names another namespace's.
    next_mount: AtomicU64,
    /// Held across a rename, so that the ancestry checks it makes are not
    /// invalidated by another rename moving a directory underneath it.
    /// Linux's `s_vfs_rename_mutex`, one for the kernel rather than per
    /// filesystem because renames across filesystems are refused anyway.
    ///
    /// A sleeping lock, because it is held across both of the rename's path
    /// walks, and a walk into a filesystem that does I/O -- btrfs -- may wait
    /// for a disk. The kernel lends the wait through the namespace's
    /// [`Parker`]; on the host it spins.
    rename_lock: SleepLock<()>,
    /// The unused dentries kept, oldest first: never more than
    /// `cache_limit`, in a buffer of that capacity made with the namespace.
    ///
    /// Whole at the start, not grown as the cache fills. A queue grown on
    /// demand doubled its buffer each time the count crossed a power of two
    /// -- a heap block of a page and more, taken by whichever lookup happened
    /// to cross, under this lock, and never given back. That is a cost no
    /// lookup should pay, and it is what a frame window around a boot check
    /// read as a leak: the stage 7 handler checks failed whenever their own
    /// lookups took the cache past 512 entries, which a boot with a larger
    /// initramfs did inside the measured run.
    cache: SpinLock<VecDeque<Arc<Dentry>>>,
    cache_limit: usize,
    /// The next namespace's number, which `/proc/<pid>/ns/mnt` shows. From
    /// `0xF000_0000` up, the numbers Linux's `proc_alloc_inum` hands out, so
    /// that a program comparing two never meets one another inode claims.
    next_namespace: AtomicU64,
    /// Every namespace's table, so that removing a name that another
    /// namespace has a mount on can take that mount off (Linux's
    /// `detach_mounts`, since 3.18): otherwise a user with its own namespace
    /// could pin any directory it can see against its owner.
    trees: SpinLock<Vec<Weak<Tree>>>,
}

/// The number the first namespace is given, and the next ones count up from:
/// Linux's `PROC_DYNAMIC_FIRST`.
const FIRST_NAMESPACE: u64 = 0xF000_0000;

/// The most mounts one namespace may hold, its root included:
/// `/proc/sys/fs/mount-max`, Linux's default. A mount, a bind or a copy past
/// it is `ENOSPC`. It bounds a copy's length, `mountinfo`'s size and how
/// long a change holds the namespace's lock (`docs/NAMESPACES.md` §7).
pub const MOUNT_MAX: usize = 4096;

/// A namespace's table of mounts, apart from the namespace so that each of
/// its mounts can point at it (`Mount::tree`) without keeping it alive.
///
/// Keyed by the mount a mount point is in and the mount point's dentry. Its
/// lock is also the one every mount's parent pointer is written under
/// ([`Mount::set_parent`]).
struct Tree {
    mounts: SpinLock<BTreeMap<(u64, u64), Arc<Mount>>>,
}

/// A mount table with a root: one mount namespace.
///
/// Its root mount is the empty, read-only bottom filesystem
/// (`crate::bottom`), and the filesystem it is made with is mounted on top
/// of that as `/`, so that `/` is a mount with a parent, as every Linux
/// machine's is once booted, and `pivot_root` can move it aside. What
/// [`Namespace::root`] answers is that top.
///
/// The first is the kernel's, made at boot. Every other is a copy of one
/// that already exists, made by [`Namespace::copy`] for `unshare` or `clone`
/// with `CLONE_NEWNS`, and ends when the last context naming it does: then
/// its mounts leave their mount points, and each goes when its last user
/// does ([`Drop`]).
pub struct Namespace {
    /// The bottom mount, which has no parent and is never unmounted.
    root: Arc<Mount>,
    tree: Arc<Tree>,
    /// Held across every change to the tree -- a mount, a bind, an unmount
    /// -- so that one change sees the table the last one left, and what it
    /// allocates it allocates before the table's spin lock is taken
    /// (`docs/NAMESPACES.md` §6). Taken before the rename lock, never two at
    /// once.
    change: SleepLock<()>,
    shared: Arc<Shared>,
    /// Where a lock that sleeps waits: lent by the kernel, and handed on to
    /// every lock this namespace and its open files make.
    parker: Arc<dyn Parker>,
    /// Its number, as `/proc/<pid>/ns/mnt` shows it.
    id: u64,
    /// The kernel heap it holds, charged to the job of the process that
    /// asked for the copy; nothing for the first (F-37).
    _charge: Charge,
    /// What the kernel says owns it: the VFS does not look inside. Set once,
    /// by the kernel, to the user namespace that was current when it was
    /// copied; empty for the first, whose owner is the first user namespace
    /// (`docs/NAMESPACES.md` §12).
    owner: SpinLock<Option<Arc<dyn core::any::Any + Send + Sync>>>,
}

impl fmt::Debug for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Namespace")
            .field("root", &self.root)
            .field("id", &self.id)
            .field("mounts", &self.tree.mounts.lock().len())
            .finish_non_exhaustive()
    }
}

impl Namespace {
    /// A namespace whose root is `fs`, whose sleeping locks wait as `parker`
    /// says: the kernel's wait queues, or [`ferrix_sync::SpinParker`] on the
    /// host.
    #[must_use]
    pub fn new(fs: Arc<dyn FileSystem>, parker: Arc<dyn Parker>) -> Namespace {
        Namespace::with_cache(fs, DEFAULT_CACHE, parker)
    }

    /// As [`Namespace::new`], keeping at most `cache_limit` unused dentries.
    #[must_use]
    pub fn with_cache(
        fs: Arc<dyn FileSystem>,
        cache_limit: usize,
        parker: Arc<dyn Parker>,
    ) -> Namespace {
        // Made before any program runs: nobody to charge, and nothing to
        // refuse. The bottom first, then `fs` on it as `/`.
        let tree = Arc::new(Tree {
            mounts: SpinLock::new(BTreeMap::new()),
        });
        let bottom: Arc<dyn FileSystem> = Arc::new(Bottom);
        let root = Arc::new(Mount {
            id: 1,
            flags: AtomicU32::new(MountFlags::READ_ONLY.bits()),
            locks: AtomicU32::new(0),
            root: Dentry::uncharged_root(bottom.root()),
            sb: Superblock::new(bottom),
            parent: SpinLock::new(None),
            tree: Arc::downgrade(&tree),
            parker: Arc::clone(&parker),
            _charge: Charge::none(),
        });
        let top = Arc::new(Mount {
            id: 2,
            flags: AtomicU32::new(0),
            locks: AtomicU32::new(0),
            root: Dentry::uncharged_root(fs.root()),
            sb: Superblock::new(fs),
            parent: SpinLock::new(Some((Arc::clone(&root), Arc::clone(&root.root)))),
            tree: Arc::downgrade(&tree),
            parker: Arc::clone(&parker),
            _charge: Charge::none(),
        });
        root.root.add_mount();
        let _ = tree.mounts.lock().insert((root.id, root.root.id()), top);
        let shared = Arc::new(Shared {
            next_mount: AtomicU64::new(3),
            rename_lock: SleepLock::new((), parker.as_ref()),
            cache: SpinLock::new(VecDeque::with_capacity(cache_limit)),
            cache_limit,
            next_namespace: AtomicU64::new(FIRST_NAMESPACE + 1),
            trees: SpinLock::new(Vec::from([Arc::downgrade(&tree)])),
        });
        Namespace {
            root,
            tree,
            change: SleepLock::new((), parker.as_ref()),
            shared,
            parker,
            id: FIRST_NAMESPACE,
            _charge: Charge::none(),
            owner: SpinLock::new(None),
        }
    }

    /// Its number: what `/proc/<pid>/ns/mnt` names it by, as `mnt:[N]`, and
    /// how two processes are told to be in the same namespace or not.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Record what owns it. The kernel calls this once, on a copy it has just
    /// made and nothing else names; a second call is ignored.
    pub fn set_owner(&self, owner: Arc<dyn core::any::Any + Send + Sync>) {
        let mut slot = self.owner.lock();
        if slot.is_none() {
            *slot = Some(owner);
        }
    }

    /// What owns it, as [`Namespace::set_owner`] recorded it; `None` for the
    /// first namespace.
    #[must_use]
    pub fn owner(&self) -> Option<Arc<dyn core::any::Any + Send + Sync>> {
        self.owner.lock().clone()
    }

    /// Where this namespace's sleeping locks wait, and every mount's in it.
    #[must_use]
    pub fn parker(&self) -> &Arc<dyn Parker> {
        &self.parker
    }

    /// `/`: the mount on top of the bottom one, which a new process starts
    /// at -- the filesystem the namespace was made with, until a
    /// `pivot_root` puts another there.
    #[must_use]
    pub fn root(&self) -> Location {
        self.descend_mounts(self.bottom())
    }

    /// The bottom mount's one directory, which nothing is ever made in and
    /// no program's root is.
    #[must_use]
    pub(crate) fn bottom(&self) -> Location {
        Location {
            mount: Arc::clone(&self.root),
            dentry: Arc::clone(&self.root.root),
        }
    }

    /// A context with both root and working directory at the root, acting as
    /// root.
    #[must_use]
    pub fn context(&self) -> Context {
        Context {
            root: self.root(),
            cwd: self.root(),
            who: Access::root(),
            ns: None,
        }
    }

    /// Every mount, the bottom one first.
    #[must_use]
    pub fn mounts(&self) -> Vec<Arc<Mount>> {
        let mut all = Vec::new();
        all.push(Arc::clone(&self.root));
        all.extend(self.tree.mounts.lock().values().cloned());
        all
    }

    /// How many unused dentries the cache is holding.
    #[must_use]
    pub fn cached(&self) -> usize {
        self.shared.cache.lock().len()
    }

    /// How many dentries the cache's queue has room for without growing.
    #[cfg(test)]
    pub(crate) fn cache_capacity(&self) -> usize {
        self.shared.cache.lock().capacity()
    }

    pub(crate) fn remember(&self, dentry: &Arc<Dentry>) {
        if self.shared.cache_limit == 0 {
            return;
        }
        // The oldest out before the newest in, so the queue never holds more
        // than its limit even for a moment and the buffer made for it is
        // never grown (`Shared::cache`).
        let evicted = {
            let mut cache = self.shared.cache.lock();
            let evicted = if cache.len() >= self.shared.cache_limit {
                cache.pop_front()
            } else {
                None
            };
            cache.push_back(Arc::clone(dentry));
            evicted
        };
        // Dropped with the lock released: the last reference to a dentry
        // releases its parent, and that chain is unbounded.
        drop(evicted);
    }

    /// Drop the cache's references to a dentry that has left the tree.
    ///
    /// An unlinked file's dentry still names its inode, so a cache entry
    /// keeping that dentry alive would keep the file's contents allocated until
    /// it happened to be evicted: up to the cache's size in deleted files that
    /// nobody can reach. A linear scan of a bounded queue, paid only by the
    /// operations that remove a name. The caller still holds the dentry, so
    /// nothing is freed under the lock.
    fn forget(&self, dentry: &Arc<Dentry>) {
        self.shared
            .cache
            .lock()
            .retain(|cached| !Arc::ptr_eq(cached, dentry));
    }

    /// As [`Namespace::forget`], for a directory that has just been removed,
    /// and every cached child of it too.
    ///
    /// A removed directory was empty, so its only cached children are misses
    /// -- names somebody looked for inside it. Each holds its parent strongly,
    /// which is what makes `..` work, and so each would keep the removed
    /// directory's dentry, and through it the directory's inode, alive until
    /// the cache got round to evicting it. Linux prunes them on `rmdir` for
    /// the same reason.
    ///
    /// Nothing is freed under the lock: the caller holds the directory, and
    /// the children taken out are dropped after the lock is released.
    fn forget_with_children(&self, directory: &Arc<Dentry>) {
        let released: Vec<Arc<Dentry>> = {
            let mut cache = self.shared.cache.lock();
            let mut released = Vec::new();
            cache.retain(|cached| {
                let gone = Arc::ptr_eq(cached, directory)
                    || cached
                        .parent()
                        .is_some_and(|parent| Arc::ptr_eq(&parent, directory));
                if gone {
                    released.push(Arc::clone(cached));
                }
                !gone
            });
            released
        };
        drop(released);
    }

    /// What is mounted on `at`, in the table of the namespace `at`'s mount
    /// is in -- not this one's, which is only the walker's
    /// (`docs/NAMESPACES.md` §2.1, Linux's mount hash keyed by mount). A
    /// mount of a namespace that has ended has nothing on it.
    pub(crate) fn mounted_on(&self, at: &Location) -> Option<Arc<Mount>> {
        let tree = at.mount.tree.upgrade()?;
        let found = tree
            .mounts
            .lock()
            .get(&(at.mount.id, at.dentry.id()))
            .cloned();
        // The table's last reference may go here, and with it every mount
        // in it: after the lock is released.
        drop(tree);
        found
    }

    fn start<'a>(ctx: &'a Context, start: Option<&'a Location>) -> &'a Location {
        start.unwrap_or(&ctx.cwd)
    }

    // -- Reading the tree ---------------------------------------------------

    /// Resolve a path to something that exists.
    ///
    /// # Errors
    ///
    /// As `Namespace::walk`, and `ENOENT` if the last component is missing.
    pub fn resolve(
        &self,
        ctx: &Context,
        start: Option<&Location>,
        path: &[u8],
        follow: bool,
    ) -> Result<Location> {
        let walked = self.walk(ctx, Self::start(ctx, start), path, follow)?;
        let _ = walked.found.inode()?;
        Ok(walked.found)
    }

    /// `stat` on a location.
    ///
    /// # Errors
    ///
    /// `ENOENT` for a negative dentry.
    pub fn stat(&self, at: &Location) -> Result<Stat> {
        Ok(Stat {
            dev: at.mount.sb.fs.device(),
            metadata: at.inode()?.metadata(),
        })
    }

    /// `statfs` on a location: the filesystem it is on.
    #[must_use]
    pub fn statfs(&self, at: &Location) -> StatFs {
        at.mount.sb.fs.statfs()
    }

    /// A symbolic link's target, without following it.
    ///
    /// # Errors
    ///
    /// `EINVAL` if the last component is not a symbolic link.
    pub fn read_link(
        &self,
        ctx: &Context,
        start: Option<&Location>,
        path: &[u8],
    ) -> Result<Vec<u8>> {
        let at = self.resolve(ctx, start, path, false)?;
        let inode = at.inode()?;
        if inode.metadata().kind != FileType::Symlink {
            return Err(Errno::EINVAL);
        }
        inode.read_link()
    }

    /// The path from `root` to `at`, as `getcwd` and `/proc/self/fd` report it.
    ///
    /// A location outside `root` — reached before a `chroot`, say — is
    /// reported from the namespace's root instead, which is what Linux does
    /// short of prefixing it with `(unreachable)`.
    #[must_use]
    pub fn path_of(&self, at: &Location, root: &Location) -> Vec<u8> {
        if at.is_detached() {
            // Not in any tree, so there is no path to build: the name it was
            // given is the whole answer, and a `/` in front would claim a
            // place it does not have.
            return at.dentry.name().into_vec();
        }
        let mut parts: Vec<Box<[u8]>> = Vec::new();
        let mut here = at.clone();
        loop {
            if here.same(root) {
                break;
            }
            if Arc::ptr_eq(&here.dentry, &here.mount.root) {
                match here.mount.parent() {
                    Some((mount, dentry)) => {
                        here = Location { mount, dentry };
                        continue;
                    }
                    None => break,
                }
            }
            parts.push(here.dentry.name());
            match here.dentry.parent() {
                Some(parent) => here.dentry = parent,
                None => break,
            }
        }
        if parts.is_empty() {
            return Vec::from(&b"/"[..]);
        }
        let mut path = Vec::new();
        for part in parts.iter().rev() {
            path.push(b'/');
            path.extend_from_slice(part);
        }
        path
    }

    /// The path from `root` to `at`, or `None` when `at` is not at or below
    /// `root`: what `mountinfo` prints a mount point as, and how it leaves
    /// out a mount the reader's root cannot reach, as Linux's
    /// `seq_path_root` does.
    #[must_use]
    pub fn path_within(&self, at: &Location, root: &Location) -> Option<Vec<u8>> {
        if at.is_detached() {
            return None;
        }
        let mut here = at.clone();
        loop {
            if here.same(root) {
                return Some(self.path_of(at, root));
            }
            let up = up(&here, None);
            if up.same(&here) {
                return None;
            }
            here = up;
        }
    }

    /// The path of `mount`'s root inside its own filesystem: `/` for a
    /// mount of the whole filesystem, which is every mount until a bind
    /// mounts a part of one. `mountinfo`'s fourth field.
    #[must_use]
    pub fn root_path(mount: &Mount) -> Vec<u8> {
        let mut parts: Vec<Box<[u8]>> = Vec::new();
        let mut here = Arc::clone(&mount.root);
        while let Some(parent) = here.parent() {
            parts.push(here.name());
            here = parent;
        }
        if parts.is_empty() {
            return Vec::from(&b"/"[..]);
        }
        let mut path = Vec::new();
        for part in parts.iter().rev() {
            path.push(b'/');
            path.extend_from_slice(part);
        }
        path
    }

    // -- Opening ------------------------------------------------------------

    /// `openat`.
    ///
    /// # Errors
    ///
    /// The ones `open(2)` documents: `ENOENT`, `EEXIST` for `O_CREAT|O_EXCL`
    /// on an existing name, `ELOOP` for `O_NOFOLLOW` on a link, `ENOTDIR` for
    /// `O_DIRECTORY` on something else, `EISDIR` for writing a directory, and
    /// whatever the walk refuses.
    pub fn open(
        &self,
        ctx: &Context,
        start: Option<&Location>,
        path: &[u8],
        flags: &OpenFlags,
        permissions: u32,
    ) -> Result<Arc<OpenFile>> {
        self.open_resolving(ctx, start, path, flags, permissions, Resolve::default())
    }

    /// `openat2`: [`Namespace::open`], resolving as `resolve` says. For
    /// `RESOLVE_IN_ROOT` and `RESOLVE_BENEATH` the caller gives a context
    /// whose root is the directory the walk starts from.
    ///
    /// # Errors
    ///
    /// As [`Namespace::open`], and `ELOOP` or `EXDEV` where `resolve` refuses
    /// a step.
    pub fn open_resolving(
        &self,
        ctx: &Context,
        start: Option<&Location>,
        path: &[u8],
        flags: &OpenFlags,
        permissions: u32,
        resolve: Resolve,
    ) -> Result<Arc<OpenFile>> {
        // How many times a create may lose to another one before `EEXIST` is
        // passed on after all. Linux never loses: it looks the name up and
        // creates it under the directory's lock. Here the walk and the create
        // are separate steps, so two `echo x >> log` on a new file can both
        // find nothing and both create, and the loser must open what the
        // winner made rather than say "File exists" -- which without
        // `O_EXCL` it never may. Each lost race means another create
        // completed, and losing again means the name was removed and made
        // again in the gap. The bound is there so that a name the cache and
        // the filesystem disagree about fails rather than spins.
        const CREATE_RACES: u32 = 8;

        let exclusive_create = flags.create && flags.exclusive;
        let follow = !(flags.nofollow || exclusive_create);
        let mut lost = 0;
        let (walked, inode) = loop {
            let walked =
                self.walk_resolving(ctx, Self::start(ctx, start), path, follow, resolve)?;
            if let Some(inode) = walked.found.dentry.inode() {
                break (walked, inode);
            }
            if !flags.create {
                return Err(Errno::ENOENT);
            }
            if walked.must_be_dir {
                return Err(Errno::EISDIR);
            }
            match self.create_at(ctx, &walked, NewNode::Regular, permissions) {
                Ok(dentry) => {
                    let created = Location {
                        mount: walked.found.mount,
                        dentry,
                    };
                    return OpenFile::new(created, flags);
                }
                Err(Errno::EEXIST) if !exclusive_create && lost < CREATE_RACES => lost += 1,
                Err(other) => return Err(other),
            }
        };

        if exclusive_create {
            return Err(Errno::EEXIST);
        }
        let kind = inode.metadata().kind;
        if kind == FileType::Symlink && !flags.path {
            return Err(Errno::ELOOP);
        }
        if flags.directory && kind != FileType::Directory {
            return Err(Errno::ENOTDIR);
        }
        if kind == FileType::Directory && (flags.write || flags.create) && !flags.path {
            return Err(Errno::EISDIR);
        }
        // Linux's `may_open`: read for reading, write for writing or
        // truncating. A file this call just created is not checked, and
        // neither is an `O_PATH` handle, which allows no I/O.
        if !flags.path {
            let mut want = 0;
            if flags.read {
                want |= MAY_READ;
            }
            if flags.write || flags.truncate {
                want |= MAY_WRITE;
            }
            ctx.who.require(&inode.metadata(), want)?;
            // Linux's `may_open` and `do_dentry_open`, in that order: a
            // device on a `nodev` mount is `EACCES`, and writing anything
            // but a device, a pipe or a socket through a read-only mount is
            // `EROFS` -- those three are not the filesystem's to change.
            let special = matches!(
                kind,
                FileType::CharDevice | FileType::BlockDevice | FileType::Fifo | FileType::Socket
            );
            if matches!(kind, FileType::CharDevice | FileType::BlockDevice)
                && walked.found.mount.no_devices()
            {
                return Err(Errno::EACCES);
            }
            if (flags.write || flags.truncate) && !special {
                walked.found.require_writable()?;
            }
        }
        if flags.truncate && flags.write && kind == FileType::Regular {
            inode.set_len(0)?;
        }
        OpenFile::new(walked.found, flags)
    }

    // -- Changing the tree --------------------------------------------------

    /// Create `node` at the negative name a walk found, returning the dentry
    /// that now names it.
    ///
    /// The directory must allow the context to write and search it, and the
    /// new object belongs to the context: its filesystem user id, and its
    /// filesystem group id or a set-group-id directory's group. A filesystem
    /// makes what it creates root's, so the owner is given afterwards; one
    /// that keeps no owners refuses, and what it made stays as it made it.
    fn create_at(
        &self,
        ctx: &Context,
        walked: &Walked,
        node: NewNode<'_>,
        permissions: u32,
    ) -> Result<Arc<Dentry>> {
        let name = walked.name_or(Errno::EEXIST)?;
        if walked.found.dentry.inode().is_some() {
            return Err(Errno::EEXIST);
        }
        // A name that exists is `EEXIST` whatever the mount; a new one on a
        // read-only mount is `EROFS` before any permission is asked, as
        // Linux's `filename_create` has it.
        walked.parent.require_writable()?;
        let dir = walked.parent.inode()?;
        let dir_meta = dir.metadata();
        ctx.who.may_create(&dir_meta)?;
        let (uid, gid, permissions) = ctx.who.new_owner(&dir_meta, node.kind(), permissions);
        let inode = dir.create(name, node, permissions)?;
        let made = inode.metadata();
        if made.uid != uid || made.gid != gid {
            let owner = SetAttributes {
                uid: Some(uid),
                gid: Some(gid),
                ..SetAttributes::default()
            };
            let _ = inode.set_attributes(&owner);
        }
        Ok(walked.parent.dentry.fill(name, &walked.found.dentry, inode))
    }

    /// `mkdirat`.
    ///
    /// # Errors
    ///
    /// `EEXIST` if the name exists, including `.` and `..`.
    pub fn mkdir(
        &self,
        ctx: &Context,
        start: Option<&Location>,
        path: &[u8],
        permissions: u32,
    ) -> Result<()> {
        let walked = self.walk(ctx, Self::start(ctx, start), path, false)?;
        self.create_at(ctx, &walked, NewNode::Directory, permissions)
            .map(drop)
    }

    /// `mknodat`, for everything but a directory.
    ///
    /// # Errors
    ///
    /// `EEXIST` if the name exists, `ENOENT` for a path ending in a slash.
    pub fn mknod(
        &self,
        ctx: &Context,
        start: Option<&Location>,
        path: &[u8],
        node: NewNode<'_>,
        permissions: u32,
    ) -> Result<()> {
        self.mknod_at(ctx, start, path, node, permissions).map(drop)
    }

    /// `mknodat`, answering where the new node is.
    ///
    /// [`Namespace::mknod`] is this and a `drop`. A caller that has to use
    /// what it made -- binding an `AF_UNIX` socket to a name, which keys its
    /// table by the node the bind created -- needs that node, and looking the
    /// path up again afterwards would find whatever is there by then rather
    /// than what this call made.
    ///
    /// # Errors
    ///
    /// As [`Namespace::mknod`].
    pub fn mknod_at(
        &self,
        ctx: &Context,
        start: Option<&Location>,
        path: &[u8],
        node: NewNode<'_>,
        permissions: u32,
    ) -> Result<Location> {
        let walked = self.walk(ctx, Self::start(ctx, start), path, false)?;
        if walked.must_be_dir && walked.found.dentry.inode().is_none() {
            return Err(Errno::ENOENT);
        }
        let dentry = self.create_at(ctx, &walked, node, permissions)?;
        Ok(Location {
            mount: Arc::clone(&walked.found.mount),
            dentry,
        })
    }

    /// `symlinkat`: make `path` a link to `target`.
    ///
    /// # Errors
    ///
    /// `ENOENT` for an empty target, `EEXIST` if the name exists.
    pub fn symlink(
        &self,
        ctx: &Context,
        start: Option<&Location>,
        path: &[u8],
        target: &[u8],
    ) -> Result<()> {
        if target.is_empty() {
            return Err(Errno::ENOENT);
        }
        self.mknod(ctx, start, path, NewNode::Symlink(target), 0o777)
    }

    /// `linkat`: give what `old` names the further name `new`.
    ///
    /// # Errors
    ///
    /// `EPERM` for a directory, `EXDEV` across mounts, `EEXIST` if `new`
    /// exists.
    pub fn link(
        &self,
        ctx: &Context,
        old: (Option<&Location>, &[u8]),
        follow: bool,
        new: (Option<&Location>, &[u8]),
    ) -> Result<()> {
        let source = self.walk(ctx, Self::start(ctx, old.0), old.1, follow)?;
        let inode = source.found.inode()?;
        if inode.metadata().kind == FileType::Directory {
            return Err(Errno::EPERM);
        }
        let dest = self.walk(ctx, Self::start(ctx, new.0), new.1, false)?;
        let name = dest.name_or(Errno::EEXIST)?;
        if dest.found.dentry.inode().is_some() {
            return Err(Errno::EEXIST);
        }
        dest.parent.require_writable()?;
        if !Arc::ptr_eq(&source.found.mount, &dest.parent.mount) {
            return Err(Errno::EXDEV);
        }
        let dir = dest.parent.inode()?;
        ctx.who.may_create(&dir.metadata())?;
        dir.link(name, &inode)?;
        let _ = dest.parent.dentry.fill(name, &dest.found.dentry, inode);
        Ok(())
    }

    /// `unlinkat` without `AT_REMOVEDIR`.
    ///
    /// # Errors
    ///
    /// `EISDIR` for a directory, `EBUSY` for a mount point.
    pub fn unlink(&self, ctx: &Context, start: Option<&Location>, path: &[u8]) -> Result<()> {
        let walked = self.walk(ctx, Self::start(ctx, start), path, false)?;
        let name = walked.name_or(Errno::EISDIR)?;
        walked.parent.require_writable()?;
        let inode = walked.found.inode()?;
        ctx.who
            .may_delete(&walked.parent.inode()?.metadata(), &inode.metadata())?;
        if inode.metadata().kind == FileType::Directory {
            return Err(Errno::EISDIR);
        }
        if walked.must_be_dir {
            return Err(Errno::ENOTDIR);
        }
        self.busy_or_detach(&walked)?;
        walked.parent.inode()?.unlink(name)?;
        walked.parent.dentry.remove_name(name);
        self.forget(&walked.found.dentry);
        Ok(())
    }

    /// Whether removing or renaming the name `walked` found may go on: it is
    /// `EBUSY` where a mount of this namespace's tree is on it -- crossed, or
    /// reached through another bind of its filesystem -- and a mount that
    /// only another namespace has there is taken off it, as Linux does since
    /// 3.18 (`detach_mounts`). Otherwise a user mounting over a directory in
    /// a namespace of their own could pin it against its owner, and the
    /// `EBUSY` would tell the owner a mount was there.
    fn busy_or_detach(&self, walked: &Walked) -> Result<()> {
        if !Arc::ptr_eq(&walked.found.mount, &walked.parent.mount) {
            return Err(Errno::EBUSY);
        }
        let dentry = &walked.found.dentry;
        if !dentry.is_mountpoint() {
            return Ok(());
        }
        let here = dentry.id();
        if self
            .tree
            .mounts
            .lock()
            .keys()
            .any(|&(_, covered)| covered == here)
        {
            return Err(Errno::EBUSY);
        }
        self.detach_elsewhere(dentry);
        Ok(())
    }

    /// Take off every mount that another namespace has on `dentry`, and every
    /// mount on those, each left without a parent as `MNT_DETACH` leaves one.
    fn detach_elsewhere(&self, dentry: &Arc<Dentry>) {
        let trees: Vec<Arc<Tree>> = {
            let mut all = self.shared.trees.lock();
            all.retain(|tree| tree.strong_count() > 0);
            all.iter().filter_map(Weak::upgrade).collect()
        };
        let mut old_parents = Vec::new();
        for tree in trees {
            if Arc::ptr_eq(&tree, &self.tree) {
                continue;
            }
            let mut table = tree.mounts.lock();
            let mut going: Vec<Arc<Mount>> = table
                .iter()
                .filter(|&(&(_, covered), _)| covered == dentry.id())
                .map(|(_, mount)| Arc::clone(mount))
                .collect();
            let mut next = 0;
            while let Some(top) = going.get(next).cloned() {
                next += 1;
                going.extend(
                    table
                        .values()
                        .filter(|mount| {
                            mount
                                .parent()
                                .is_some_and(|(above, _)| Arc::ptr_eq(&above, &top))
                        })
                        .cloned(),
                );
            }
            for mount in &going {
                let Some((above, covered)) = mount.parent() else {
                    continue;
                };
                if table.remove(&(above.id, covered.id())).is_some() {
                    covered.remove_mount();
                }
                old_parents.push(mount.set_parent(None));
            }
        }
        // Outside every table's lock: a parent's last reference may go here.
        drop(old_parents);
    }

    /// `unlinkat` with `AT_REMOVEDIR`.
    ///
    /// # Errors
    ///
    /// `EINVAL` for `.`, `ENOTEMPTY` for `..` or a directory with entries,
    /// `ENOTDIR` for something else, `EBUSY` for a mount point or the root.
    pub fn rmdir(&self, ctx: &Context, start: Option<&Location>, path: &[u8]) -> Result<()> {
        let walked = self.walk(ctx, Self::start(ctx, start), path, false)?;
        let name = match &walked.last {
            LastPart::Name(name) => name,
            LastPart::Dot => return Err(Errno::EINVAL),
            LastPart::DotDot => return Err(Errno::ENOTEMPTY),
            LastPart::Root => return Err(Errno::EBUSY),
        };
        walked.parent.require_writable()?;
        let inode = walked.found.inode()?;
        ctx.who
            .may_delete(&walked.parent.inode()?.metadata(), &inode.metadata())?;
        if inode.metadata().kind != FileType::Directory {
            return Err(Errno::ENOTDIR);
        }
        self.busy_or_detach(&walked)?;
        walked.parent.inode()?.rmdir(name)?;
        walked.parent.dentry.remove_name(name);
        self.forget_with_children(&walked.found.dentry);
        Ok(())
    }

    /// `renameat2`, without `RENAME_EXCHANGE`.
    ///
    /// # Errors
    ///
    /// `EXDEV` across mounts, `EBUSY` for a mount point or `.`/`..`,
    /// `EINVAL` for moving a directory into itself, `ENOTEMPTY` for replacing
    /// a directory with entries — including an ancestor of the source —
    /// `EEXIST` under [`RenameMode::NoReplace`], and the kind mismatches
    /// `rename(2)` lists.
    pub fn rename(
        &self,
        ctx: &Context,
        old: (Option<&Location>, &[u8]),
        new: (Option<&Location>, &[u8]),
        mode: RenameMode,
    ) -> Result<()> {
        let _serialised = self.shared.rename_lock.lock();
        let source = self.walk(ctx, Self::start(ctx, old.0), old.1, false)?;
        let dest = self.walk(ctx, Self::start(ctx, new.0), new.1, false)?;
        let old_name = source.name_or(Errno::EBUSY)?;
        let new_name = dest.name_or(Errno::EBUSY)?;
        // `do_renameat2`'s order: across mounts is `EXDEV`, and a read-only
        // mount `EROFS`, before either name is looked at.
        if !Arc::ptr_eq(&source.parent.mount, &dest.parent.mount) {
            return Err(Errno::EXDEV);
        }
        source.parent.require_writable()?;
        let moving = source.found.inode()?;
        let moving_meta = moving.metadata();
        let moving_dir = moving_meta.kind == FileType::Directory;

        self.busy_or_detach(&source)?;
        self.busy_or_detach(&dest)?;
        if (source.must_be_dir || dest.must_be_dir) && !moving_dir {
            return Err(Errno::ENOTDIR);
        }
        let mut replacing_dir = false;
        if let Some(target) = dest.found.dentry.inode() {
            if mode == RenameMode::NoReplace {
                return Err(Errno::EEXIST);
            }
            let target_meta = target.metadata();
            if target_meta.ino == moving_meta.ino {
                // Two names for one file: rename(2) says do nothing.
                return Ok(());
            }
            if dest.found.dentry.is_ancestor_of(&source.parent.dentry) {
                return Err(Errno::ENOTEMPTY);
            }
            replacing_dir = target_meta.kind == FileType::Directory;
        }
        if moving_dir && source.found.dentry.is_ancestor_of(&dest.parent.dentry) {
            return Err(Errno::EINVAL);
        }

        // Linux's `vfs_rename`: the source may be deleted from its directory,
        // the destination made or replaced in its own, and a directory moving
        // to a new parent must be writable itself, because its `..` changes.
        let who = &ctx.who;
        who.may_delete(&source.parent.inode()?.metadata(), &moving_meta)?;
        match dest.found.dentry.inode() {
            Some(target) => who.may_delete(&dest.parent.inode()?.metadata(), &target.metadata())?,
            None => who.may_create(&dest.parent.inode()?.metadata())?,
        }
        if moving_dir && !Arc::ptr_eq(&source.parent.dentry, &dest.parent.dentry) {
            who.require(&moving_meta, MAY_WRITE)?;
        }

        let new_dir = dest.parent.inode()?;
        source
            .parent
            .inode()?
            .rename(old_name, &new_dir, new_name, mode == RenameMode::Replace)?;
        source.parent.dentry.move_child(
            old_name,
            &source.found.dentry,
            &dest.parent.dentry,
            new_name,
        );
        // Whatever stood at the destination has left the tree: a replaced
        // file, whose pages it would keep, or -- for a rename to a new name --
        // the miss the walk cached, which holds the destination directory. A
        // replaced directory was empty, and goes as `rmdir` sends one: with
        // the misses cached inside it, each of which holds it.
        if replacing_dir {
            self.forget_with_children(&dest.found.dentry);
        } else {
            self.forget(&dest.found.dentry);
        }
        Ok(())
    }

    /// `chmod`, `chown` and `utimensat`, on a location.
    ///
    /// # Errors
    ///
    /// Whatever the filesystem refuses.
    pub fn set_attributes(&self, at: &Location, change: &SetAttributes) -> Result<()> {
        at.require_writable()?;
        at.inode()?.set_attributes(change)
    }

    /// `truncate` on a location.
    ///
    /// # Errors
    ///
    /// `EISDIR` for a directory, `EINVAL` for anything else not a regular
    /// file.
    pub fn truncate(&self, at: &Location, len: u64) -> Result<()> {
        let inode = at.inode()?;
        match inode.metadata().kind {
            FileType::Regular => {
                at.require_writable()?;
                inode.set_len(len)
            }
            FileType::Directory => Err(Errno::EISDIR),
            _ => Err(Errno::EINVAL),
        }
    }

    // -- Mounting -----------------------------------------------------------

    /// Mount `fs` on the directory `at`.
    ///
    /// # Errors
    ///
    /// `ENOTDIR` if `at` is not a directory, `EINVAL` if it is not in this
    /// namespace's tree, `EBUSY` if something is already mounted exactly
    /// there, `ENOMEM` past the job's memory limit.
    pub fn mount(&self, fs: Arc<dyn FileSystem>, at: &Location) -> Result<Arc<Mount>> {
        self.mount_with(fs, at, MountFlags::NONE)
    }

    /// Mount `fs` on the directory `at` with `flags`.
    ///
    /// # Errors
    ///
    /// As [`Namespace::mount`].
    pub fn mount_with(
        &self,
        fs: Arc<dyn FileSystem>,
        at: &Location,
        flags: MountFlags,
    ) -> Result<Arc<Mount>> {
        if at.inode()?.metadata().kind != FileType::Directory {
            return Err(Errno::ENOTDIR);
        }
        let _changing = self.change.lock();
        if !self.owns(&at.mount) {
            return Err(Errno::EINVAL);
        }
        // Charged and made before the table is locked: a refusal drops `fs`,
        // which may be its last reference, with no lock held.
        let charge = crate::charge(mount_footprint())?;
        let root = Dentry::root(fs.root())?;
        let mount = Arc::new(Mount {
            id: self.next_id(),
            flags: AtomicU32::new(flags.bits()),
            locks: AtomicU32::new(0),
            root,
            sb: Superblock::new(fs),
            parent: SpinLock::new(Some((Arc::clone(&at.mount), Arc::clone(&at.dentry)))),
            tree: Arc::downgrade(&self.tree),
            parker: Arc::clone(&self.parker),
            _charge: charge,
        });
        self.attach(core::slice::from_ref(&mount))?;
        Ok(mount)
    }

    /// `mount(source, target, NULL, MS_BIND)`: a new mount on `target` of
    /// the filesystem `source` is on, starting at `source` -- a directory, a
    /// subdirectory, or a single file -- with the flags of `source`'s mount.
    /// With `recursive` (`MS_REC`), every mount at or below `source` is
    /// copied under the new one, each with its own flags; without it, none
    /// is, and what they cover shows through.
    ///
    /// A directory binds onto a directory and anything else onto anything
    /// but a directory, as Linux's `graft_tree` has it. Flags other than
    /// the source mount's are a remount's to set, as on Linux.
    ///
    /// # Errors
    ///
    /// `ENOTDIR` for a directory onto a non-directory or the reverse;
    /// `EINVAL` if either place is not in this namespace's tree (Linux's
    /// `check_mnt`: a descriptor kept into a mount `MNT_DETACH` took out
    /// binds nothing); `EBUSY` if something is mounted exactly on `target`;
    /// `ENOMEM` past the job's memory limit, with nothing mounted.
    pub fn bind(
        &self,
        source: &Location,
        target: &Location,
        recursive: bool,
    ) -> Result<Arc<Mount>> {
        let source_dir = source.inode()?.metadata().kind == FileType::Directory;
        let target_dir = target.inode()?.metadata().kind == FileType::Directory;
        if source_dir != target_dir {
            return Err(Errno::ENOTDIR);
        }
        let _changing = self.change.lock();
        if !self.owns(&source.mount) || !self.owns(&target.mount) {
            return Err(Errno::EINVAL);
        }
        // A bind that leaves out the mounts on `source` would show what a
        // locked one hides (Linux's `has_locked_children`, which looks at the
        // mounts on `source` alone: one further down covers a place of a
        // mount the bind does not copy). One pass over the table, as a
        // namespace of `mount-max` binds of `/` would otherwise cost the
        // square of its size for each.
        if !recursive
            && self.snapshot()?.iter().any(|mount| {
                mount.is_locked()
                    && mount.parent().is_some_and(|(above, covered)| {
                        Arc::ptr_eq(&above, &source.mount) && source.dentry.is_ancestor_of(&covered)
                    })
            })
        {
            return Err(Errno::EINVAL);
        }
        let mut originals = Vec::new();
        originals.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
        originals.push(Arc::clone(&source.mount));
        if recursive {
            let below = self.descendants(&source.mount, Some(&source.dentry))?;
            originals
                .try_reserve(below.len())
                .map_err(|_| Errno::ENOMEM)?;
            originals.extend(below);
        }
        // Every copy charged and made before the table is locked, so a
        // refusal leaves the tree as it was.
        let mut made: Vec<Arc<Mount>> = Vec::new();
        made.try_reserve(originals.len())
            .map_err(|_| Errno::ENOMEM)?;
        made.push(self.copy_of(
            &source.mount,
            Arc::clone(&source.dentry),
            Some((Arc::clone(&target.mount), Arc::clone(&target.dentry))),
            &self.tree,
        )?);
        for original in originals.iter().skip(1) {
            let (above, covered) = original.parent().ok_or(Errno::EINVAL)?;
            let copied_above = originals
                .iter()
                .position(|candidate| Arc::ptr_eq(candidate, &above))
                .and_then(|index| made.get(index))
                .cloned()
                .ok_or(Errno::EINVAL)?;
            made.push(self.copy_of(
                original,
                Arc::clone(&original.root),
                Some((copied_above, covered)),
                &self.tree,
            )?);
        }
        // The top is a mount point of its own now; what is under it stays
        // locked to the mounts it was locked to.
        if let Some(top) = made.first() {
            let _ = top.locks.fetch_and(!LOCK_PARENT, Ordering::AcqRel);
        }
        self.attach(&made)?;
        made.into_iter().next().ok_or(Errno::EINVAL)
    }

    /// A new mount like `like` -- its filesystem and flags -- starting at
    /// `root`, on `parent`, for `tree`'s namespace, charged; not yet in any
    /// table.
    fn copy_of(
        &self,
        like: &Mount,
        root: Arc<Dentry>,
        parent: Option<(Arc<Mount>, Arc<Dentry>)>,
        tree: &Arc<Tree>,
    ) -> Result<Arc<Mount>> {
        let charge = crate::charge(mount_footprint())?;
        Ok(Arc::new(Mount {
            id: self.next_id(),
            flags: AtomicU32::new(like.flags().bits()),
            // A bind or a copy keeps which flags were locked and which mounts
            // are locked to their parent (Linux's `clone_mnt`); the top of a
            // bind is its own mount point, which `bind` unlocks.
            locks: AtomicU32::new(like.locks.load(Ordering::Acquire)),
            sb: Arc::clone(&like.sb),
            root,
            parent: SpinLock::new(parent),
            tree: Arc::downgrade(tree),
            parker: Arc::clone(&self.parker),
            _charge: charge,
        }))
    }

    /// A mount id no mount of the kernel has had.
    fn next_id(&self) -> u64 {
        self.shared.next_mount.fetch_add(1, Ordering::Relaxed)
    }

    /// Put `mounts` in the table, each on the place its parent pointer
    /// names, the first on a place nothing is mounted on. The change lock
    /// is held, and every mount was made already.
    ///
    /// # Errors
    ///
    /// `EBUSY` if something is mounted exactly where the first goes, and
    /// `ENOSPC` if they would take the namespace past [`MOUNT_MAX`];
    /// nothing is put in then.
    fn attach(&self, mounts: &[Arc<Mount>]) -> Result<()> {
        // Read before the table's lock, so nothing is allocated under it: no
        // parent of a mount not yet in the table can change meanwhile.
        let mut placed: Vec<Option<(Arc<Mount>, Arc<Dentry>)>> = Vec::new();
        placed
            .try_reserve_exact(mounts.len())
            .map_err(|_| Errno::ENOMEM)?;
        placed.extend(mounts.iter().map(|mount| mount.parent()));
        let mut table = self.tree.mounts.lock();
        if let Some(Some((above, covered))) = placed.first()
            && table.contains_key(&(above.id, covered.id()))
        {
            return Err(Errno::EBUSY);
        }
        // The root is the one mount the table does not hold.
        if table.len() + 1 + mounts.len() > MOUNT_MAX {
            return Err(Errno::ENOSPC);
        }
        for (mount, place) in mounts.iter().zip(placed) {
            if let Some((above, covered)) = place {
                let _ = table.insert((above.id, covered.id()), Arc::clone(mount));
                covered.add_mount();
            }
        }
        Ok(())
    }

    /// Whether `mount` is in this namespace's tree: its root, or in its
    /// table on the place its parent pointer names. A mount `MNT_DETACH`
    /// took out, a [`Location::detached`] one, or one of another namespace
    /// -- Linux's `check_mnt` -- is not.
    #[must_use]
    pub fn owns(&self, mount: &Arc<Mount>) -> bool {
        if Arc::ptr_eq(mount, &self.root) {
            return true;
        }
        if !core::ptr::eq(mount.tree.as_ptr(), Arc::as_ptr(&self.tree)) {
            return false;
        }
        let Some((above, covered)) = mount.parent() else {
            return false;
        };
        self.tree
            .mounts
            .lock()
            .get(&(above.id, covered.id()))
            .is_some_and(|found| Arc::ptr_eq(found, mount))
    }

    /// Whether every mount of `mount`'s filesystem, in any namespace, is one
    /// of this namespace's: the filesystem is this namespace's alone, made
    /// here or copied from nowhere. A plain `MS_REMOUNT` changes the
    /// filesystem for every mount of it, so a caller that does not own the
    /// whole of it has no business doing that (`docs/NAMESPACES.md` N5).
    #[must_use]
    pub fn sole_filesystem(&self, mount: &Arc<Mount>) -> bool {
        let here = self
            .tree
            .mounts
            .lock()
            .values()
            .filter(|other| Arc::ptr_eq(&other.sb, &mount.sb))
            .count();
        // Each mount holds one reference; this call holds none of its own.
        here > 0 && Arc::strong_count(&mount.sb) == here
    }

    /// Every mount of the table, copied out: the change lock is held, so
    /// the table cannot grow between its size being read and the copy.
    fn snapshot(&self) -> Result<Vec<Arc<Mount>>> {
        let len = self.tree.mounts.lock().len();
        let mut all = Vec::new();
        all.try_reserve_exact(len).map_err(|_| Errno::ENOMEM)?;
        all.extend(self.tree.mounts.lock().values().take(len).cloned());
        Ok(all)
    }

    /// The mounts below `top`, each after its parent: those on `top` whose
    /// mount point is at or below `within` (all of them without one), and
    /// then everything on those, all the way down. The change lock is held.
    fn descendants(
        &self,
        top: &Arc<Mount>,
        within: Option<&Arc<Dentry>>,
    ) -> Result<Vec<Arc<Mount>>> {
        let all = self.snapshot()?;
        let mut found: Vec<Arc<Mount>> = Vec::new();
        found.try_reserve(all.len()).map_err(|_| Errno::ENOMEM)?;
        for mount in &all {
            if let Some((above, covered)) = mount.parent()
                && Arc::ptr_eq(&above, top)
                && within.is_none_or(|place| place.is_ancestor_of(&covered))
            {
                found.push(Arc::clone(mount));
            }
        }
        let mut next = 0;
        while let Some(parent) = found.get(next).cloned() {
            next += 1;
            for mount in &all {
                if let Some((above, _)) = mount.parent()
                    && Arc::ptr_eq(&above, &parent)
                {
                    found.push(Arc::clone(mount));
                }
            }
        }
        Ok(found)
    }

    /// The mount whose root `at` is and every mount inside it, each after
    /// its parent: what [`Namespace::unmount_with`] with `detach` takes out,
    /// for a caller that must write their filesystems out first.
    ///
    /// # Errors
    ///
    /// `EINVAL` if `at` is not the root of a mount of this namespace's tree;
    /// `ENOMEM` when there is no memory to list them.
    pub fn subtree(&self, at: &Location) -> Result<Vec<Arc<Mount>>> {
        if !at.is_mount_root() {
            return Err(Errno::EINVAL);
        }
        let _changing = self.change.lock();
        if !self.owns(&at.mount) {
            return Err(Errno::EINVAL);
        }
        let inside = self.descendants(&at.mount, None)?;
        let mut all = Vec::new();
        all.try_reserve_exact(inside.len() + 1)
            .map_err(|_| Errno::ENOMEM)?;
        all.push(Arc::clone(&at.mount));
        all.extend(inside);
        Ok(all)
    }

    /// Give the mount whose root `at` is the flags `flags`: `MS_REMOUNT |
    /// MS_BIND`, which changes that one mount and no other mount of its
    /// filesystem. The flags replace the old ones whole; which to keep is the
    /// caller's to decide, as `mount(2)`'s rules say.
    ///
    /// # Errors
    ///
    /// `EINVAL` if `at` is not the root of a mount of this namespace's tree.
    pub fn remount(&self, at: &Location, flags: MountFlags) -> Result<()> {
        if !at.is_mount_root() || !self.owns(&at.mount) {
            return Err(Errno::EINVAL);
        }
        // A flag a copy locked stays set, and the access-time mode stays as
        // it is (Linux's `do_reconfigure_mnt`): a remount in a user namespace
        // cannot undo what an administrator's mount said (CVE-2014-5206, -5207).
        let locks = at.mount.locks.load(Ordering::Acquire);
        let old = at.mount.flags.load(Ordering::Acquire);
        let held = old & locks & LOCK_FLAGS;
        let atime = MountFlags::ATIME.bits();
        if flags.bits() & held != held
            || (locks & LOCK_ATIME != 0 && flags.bits() & atime != old & atime)
        {
            return Err(Errno::EPERM);
        }
        at.mount.flags.store(flags.bits(), Ordering::Release);
        Ok(())
    }

    /// A plain `MS_REMOUNT`: as [`Namespace::remount`], and the filesystem
    /// itself read-only or writable as `flags` says, so that every mount of
    /// it -- each bind, each copy -- refuses writes or takes them again
    /// (Linux's `do_remount`, which sets the superblock's `SB_RDONLY` and
    /// the mount's own flags).
    ///
    /// # Errors
    ///
    /// As [`Namespace::remount`].
    pub fn remount_filesystem(&self, at: &Location, flags: MountFlags) -> Result<()> {
        self.remount(at, flags)?;
        at.mount
            .sb
            .read_only
            .store(flags.contains(MountFlags::READ_ONLY), Ordering::Release);
        Ok(())
    }

    /// Unmount the filesystem whose root `at` is: `umount2` without
    /// `MNT_DETACH`.
    ///
    /// Lazy all the same, as every unmount here is: files already open on it
    /// keep working, and it goes away when the last of them closes.
    ///
    /// # Errors
    ///
    /// `EINVAL` if `at` is not the root of a mount of this namespace's tree;
    /// `EBUSY` if it is `/` -- the mount on the bottom one -- or something is
    /// mounted inside it.
    pub fn unmount(&self, at: &Location) -> Result<()> {
        self.unmount_with(at, false)
    }

    /// `umount2`, with `detach` for `MNT_DETACH`: then everything mounted
    /// inside it leaves the table with it, at once, rather than being
    /// `EBUSY`.
    ///
    /// Every mount that leaves loses its parent, as Linux's `umount_tree`
    /// disconnects each: `..` from inside one stops at its root, and nothing
    /// of it can be reached by a path from the tree it left. Each goes when
    /// its last user does.
    ///
    /// # Errors
    ///
    /// As [`Namespace::unmount`], less `EBUSY` with `detach`; `ENOMEM` when
    /// there is no memory to list what is inside it.
    pub fn unmount_with(&self, at: &Location, detach: bool) -> Result<()> {
        if !at.is_mount_root() {
            return Err(Errno::EINVAL);
        }
        let _changing = self.change.lock();
        let Some((above, _)) = at.mount.parent() else {
            return Err(Errno::EINVAL);
        };
        if !self.owns(&at.mount) || at.mount.is_locked() {
            return Err(Errno::EINVAL);
        }
        // `/` stays: every process of the namespace that has not moved its
        // root starts there, and the kernel's own walks do. Linux turns a
        // plain unmount of `/` into a read-only remount and lets
        // `MNT_DETACH` take it lazily; `pivot_root` is the way to move it.
        if Arc::ptr_eq(&above, &self.root) {
            return Err(Errno::EBUSY);
        }
        let mut going = Vec::new();
        going.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
        going.push(Arc::clone(&at.mount));
        if detach {
            let inside = self.descendants(&at.mount, None)?;
            going.try_reserve(inside.len()).map_err(|_| Errno::ENOMEM)?;
            going.extend(inside);
        }
        let mut old_parents = Vec::new();
        old_parents
            .try_reserve(going.len())
            .map_err(|_| Errno::ENOMEM)?;
        {
            let mut table = self.tree.mounts.lock();
            if !detach && table.keys().any(|&(on, _)| on == at.mount.id) {
                return Err(Errno::EBUSY);
            }
            for mount in &going {
                if let Some((above, covered)) = mount.parent() {
                    let _ = table.remove(&(above.id, covered.id()));
                    covered.remove_mount();
                }
                old_parents.push(mount.set_parent(None));
            }
        }
        // Outside the table's lock: a parent's last reference may go here,
        // and the cache takes its own lock.
        drop(old_parents);
        for mount in &going {
            self.forget_tree(&mount.root);
        }
        Ok(())
    }

    /// Drop the cache's references to every dentry of a tree that has just
    /// been unmounted: its root and anything whose parents lead back to it.
    ///
    /// Nothing can reach that tree by name any more, but a cached dentry holds
    /// its inode and, through its parents, every directory up to the root —
    /// so without this an unmounted filesystem stayed allocated, a dentry at a
    /// time, until the cache got round to evicting each one. A procfs or a
    /// devtmpfs mounted and unmounted again left one behind per cycle. Linux
    /// prunes a mount's dentries when it is unmounted for the same reason.
    ///
    /// A walk up each cached dentry's parents, paid only by unmount. Nothing is
    /// freed under the lock: the entries taken out are dropped after it is
    /// released, since the last reference to one releases its parent chain.
    fn forget_tree(&self, root: &Arc<Dentry>) {
        let released: Vec<Arc<Dentry>> = {
            let mut cache = self.shared.cache.lock();
            let mut released = Vec::new();
            cache.retain(|cached| {
                let gone = root.is_ancestor_of(cached);
                if gone {
                    released.push(Arc::clone(cached));
                }
                !gone
            });
            released
        };
        drop(released);
    }

    // -- Namespaces ---------------------------------------------------------

    /// A new namespace, a copy of this one: `unshare(CLONE_NEWNS)` and
    /// `clone(CLONE_NEWNS)` (`docs/NAMESPACES.md` §2.1, *Copying*).
    ///
    /// Every mount is given a new one -- the same filesystem, root dentry
    /// and flags, a new id -- on the copy of its parent, so the two trees
    /// look alike and share nothing a mount or an unmount changes. Each
    /// location of `places` on a mount of this namespace is moved to that
    /// mount's copy, the dentry kept: the caller's root and working
    /// directory. One on a mount outside the tree stays as it is, as an open
    /// descriptor keeps its own.
    ///
    /// The namespace and every copy are charged to the running task's job,
    /// all before anything is shared; a refusal leaves nothing made.
    ///
    /// # Errors
    ///
    /// `ENOMEM` for memory or past the job's memory limit.
    pub fn copy(&self, places: &mut [&mut Location]) -> Result<Namespace> {
        self.copy_as(places, None, false)
    }

    /// As [`Namespace::copy`], owned by `owner` (whatever the kernel says
    /// owns a namespace), and with `lock` -- when the owner is a user
    /// namespace that does not own this one -- every copied mount's flags
    /// locked and every mount locked to its parent, but the bottom, so that
    /// the new namespace's owner cannot undo or reveal anything the old one's
    /// administrator did (`docs/NAMESPACES.md` M3, M4).
    ///
    /// # Errors
    ///
    /// As [`Namespace::copy`].
    pub fn copy_as(
        &self,
        places: &mut [&mut Location],
        owner: Option<Arc<dyn Any + Send + Sync>>,
        lock: bool,
    ) -> Result<Namespace> {
        let charge = crate::charge(arc_footprint::<Namespace>() + arc_footprint::<Tree>())?;
        let _changing = self.change.lock();
        let below = self.descendants(&self.root, None)?;
        let mut originals: Vec<Arc<Mount>> = Vec::new();
        originals
            .try_reserve_exact(below.len() + 1)
            .map_err(|_| Errno::ENOMEM)?;
        originals.push(Arc::clone(&self.root));
        originals.extend(below);

        let tree = Arc::new(Tree {
            mounts: SpinLock::new(BTreeMap::new()),
        });
        // Each original's index by id, so that a parent's copy is found in
        // one lookup; every parent comes before its children.
        let mut index: BTreeMap<u64, usize> = BTreeMap::new();
        let mut made: Vec<Arc<Mount>> = Vec::new();
        made.try_reserve_exact(originals.len())
            .map_err(|_| Errno::ENOMEM)?;
        let mut table = BTreeMap::new();
        for (at, original) in originals.iter().enumerate() {
            let parent = match original.parent() {
                None => None,
                Some((above, covered)) => {
                    let copied_above = index
                        .get(&above.id)
                        .and_then(|&found| made.get(found))
                        .cloned()
                        .ok_or(Errno::EINVAL)?;
                    Some((copied_above, covered))
                }
            };
            let copy = self.copy_of(original, Arc::clone(&original.root), parent, &tree)?;
            if lock && at > 0 {
                let flags = original.flags().bits() & LOCK_FLAGS;
                let _ = copy
                    .locks
                    .fetch_or(flags | LOCK_ATIME | LOCK_PARENT, Ordering::AcqRel);
            }
            if let Some((above, covered)) = copy.parent() {
                let _ = table.insert((above.id, covered.id()), Arc::clone(&copy));
            }
            let _ = index.insert(original.id, at);
            made.push(copy);
        }
        let root = made.first().cloned().ok_or(Errno::EINVAL)?;
        for place in places.iter_mut() {
            // Ids are the kernel's, so only a mount of this tree is found.
            if let Some(copy) = index
                .get(&place.mount.id)
                .and_then(|&found| made.get(found))
            {
                place.mount = Arc::clone(copy);
            }
        }
        // Nothing can fail from here: the mount points are counted only for
        // a namespace that exists, so its teardown is what gives them back.
        for copy in made.iter().skip(1) {
            if let Some((_, covered)) = copy.parent() {
                covered.add_mount();
            }
        }
        *tree.mounts.lock() = table;
        self.shared.trees.lock().push(Arc::downgrade(&tree));
        Ok(Namespace {
            root,
            tree,
            change: SleepLock::new((), self.parker.as_ref()),
            shared: Arc::clone(&self.shared),
            parker: Arc::clone(&self.parker),
            id: self.shared.next_namespace.fetch_add(1, Ordering::Relaxed),
            _charge: charge,
            owner: SpinLock::new(owner),
        })
    }

    /// `pivot_root(new_root, put_old)` for a caller whose `/` is `root`:
    /// the mount whose root `new_root` is takes the place of the one whose
    /// root `root` is, and that one is mounted on `put_old`, in one step
    /// under the table's lock (Linux's `fs/namespace.c`).
    ///
    /// Moving every process whose root or working directory was `root` to
    /// `new_root` is the caller's (`chroot_fs_refs`): this knows no
    /// processes.
    ///
    /// For `pivot_root(".", ".")`, `put_old` is `new_root` itself, and the
    /// old root lands stacked on the new one, where the caller unmounts it.
    ///
    /// # Errors
    ///
    /// Linux's, in its order: `ENOTDIR` unless both are directories;
    /// `EINVAL` if `root` or `new_root` is not in this namespace, `ENOENT`
    /// if `new_root` was removed, `EBUSY` if `new_root` or `put_old` is on
    /// the caller's root mount, `EINVAL` if `root` or `new_root` is not a
    /// mount's root or its mount has no parent, or if `put_old` is not
    /// reachable from `new_root` nor `new_root` from `root`; `EBUSY` if
    /// something is already mounted on `put_old`.
    pub fn pivot_root(
        &self,
        root: &Location,
        new_root: &Location,
        put_old: &Location,
    ) -> Result<()> {
        if new_root.inode()?.metadata().kind != FileType::Directory
            || put_old.inode()?.metadata().kind != FileType::Directory
        {
            return Err(Errno::ENOTDIR);
        }
        let _changing = self.change.lock();
        if !self.owns(&root.mount) || !self.owns(&new_root.mount) || !self.owns(&put_old.mount) {
            return Err(Errno::EINVAL);
        }
        if new_root.dentry.is_unhashed() {
            return Err(Errno::ENOENT);
        }
        if Arc::ptr_eq(&new_root.mount, &root.mount) || Arc::ptr_eq(&put_old.mount, &root.mount) {
            return Err(Errno::EBUSY);
        }
        let (Some((root_parent, root_covered)), Some((ex_parent, ex_covered))) =
            (root.mount.parent(), new_root.mount.parent())
        else {
            return Err(Errno::EINVAL);
        };
        if !root.is_mount_root()
            || !new_root.is_mount_root()
            || new_root.mount.is_locked()
            || !reaches(put_old, new_root)
            || !reaches(new_root, root)
        {
            return Err(Errno::EINVAL);
        }
        let new_mount = &new_root.mount;
        let old_root = &root.mount;
        let (displaced_new, displaced_old) = {
            let mut table = self.tree.mounts.lock();
            if table.contains_key(&(put_old.mount.id, put_old.dentry.id())) {
                return Err(Errno::EBUSY);
            }
            let _ = table.remove(&(ex_parent.id, ex_covered.id()));
            ex_covered.remove_mount();
            let _ = table.remove(&(root_parent.id, root_covered.id()));
            // The new root first, onto the old root's place, then the old
            // root onto `put_old`: after each write the parents stay acyclic
            // (§2.1). `put_old` is below the new root, which is now below
            // `root_parent` alone, so the old root is on no path up from it.
            let displaced_new =
                new_mount.set_parent(Some((Arc::clone(&root_parent), Arc::clone(&root_covered))));
            let _ = table.insert((root_parent.id, root_covered.id()), Arc::clone(new_mount));
            let displaced_old = old_root.set_parent(Some((
                Arc::clone(&put_old.mount),
                Arc::clone(&put_old.dentry),
            )));
            let _ = table.insert(
                (put_old.mount.id, put_old.dentry.id()),
                Arc::clone(old_root),
            );
            put_old.dentry.add_mount();
            // The old root's lock goes to the new one, so that what the old
            // root covered stays hidden and the old root can be detached.
            if old_root.is_locked() {
                let _ = new_mount.locks.fetch_or(LOCK_PARENT, Ordering::AcqRel);
                let _ = old_root.locks.fetch_and(!LOCK_PARENT, Ordering::AcqRel);
            }
            (displaced_new, displaced_old)
        };
        // Outside the table's lock, as every parent is dropped.
        drop((displaced_new, displaced_old));
        Ok(())
    }
}

/// Whether `from` is at or below `to`: going `..` from it, across mount
/// roots, meets `to` before the top of its tree. Linux's
/// `is_path_reachable`.
fn reaches(from: &Location, to: &Location) -> bool {
    let mut here = from.clone();
    loop {
        if here.same(to) {
            return true;
        }
        let above = up(&here, None);
        if above.same(&here) {
            return false;
        }
        here = above;
    }
}

impl Drop for Namespace {
    /// The last context naming it has gone: every mount leaves its mount
    /// point and its parent, as Linux's `umount_tree` disconnects a dead
    /// namespace's, and goes when its last user does -- an open file keeps
    /// its own, with nothing mounted on it and `..` stopping at its root.
    ///
    /// A filesystem no other mount holds has its dentries dropped from the
    /// cache, as an unmount drops them, so that a tmpfs mounted only inside
    /// the namespace does not keep its pages until the cache evicts them.
    fn drop(&mut self) {
        let table = core::mem::take(&mut *self.tree.mounts.lock());
        for mount in table.values() {
            if let Some((_, covered)) = mount.parent() {
                covered.remove_mount();
            }
        }
        for mount in table.values() {
            drop(mount.set_parent(None));
        }
        for mount in table.values() {
            // Only mounts hold a superblock: one reference is this mount
            // alone, and nothing else of the filesystem is mounted anywhere.
            if Arc::strong_count(&mount.sb) == 1 {
                // What it was given is written out before it is let go, as
                // the last unmount of a filesystem does (F-53's class): a
                // disk mounted in this namespace alone would otherwise lose
                // what it wrote since its last commit.
                let _ = mount.sb.fs.sync();
                self.forget_tree(&mount.root);
            }
        }
        drop(table);
    }
}
