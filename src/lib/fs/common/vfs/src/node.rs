//! What a filesystem implements.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;

use ferrix_linux_abi::errno::Errno;
pub use ferrix_linux_abi::types::Timespec;
use ferrix_linux_abi::types::{
    DT_BLK, DT_CHR, DT_DIR, DT_FIFO, DT_LNK, DT_REG, DT_SOCK, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO,
    S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK,
};

use crate::Result;

/// The kind of object an inode is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    /// A regular file.
    Regular,
    /// A directory.
    Directory,
    /// A symbolic link.
    Symlink,
    /// A character device.
    CharDevice,
    /// A block device.
    BlockDevice,
    /// A named pipe.
    Fifo,
    /// A socket's name in the filesystem.
    Socket,
}

impl FileType {
    /// The `S_IF*` bits `stat` reports for this kind.
    #[must_use]
    pub const fn mode_bits(self) -> u32 {
        match self {
            FileType::Regular => S_IFREG,
            FileType::Directory => S_IFDIR,
            FileType::Symlink => S_IFLNK,
            FileType::CharDevice => S_IFCHR,
            FileType::BlockDevice => S_IFBLK,
            FileType::Fifo => S_IFIFO,
            FileType::Socket => S_IFSOCK,
        }
    }

    /// The kind a mode word names, or `None` for bits no kind has.
    #[must_use]
    pub const fn from_mode(mode: u32) -> Option<FileType> {
        match mode & S_IFMT {
            S_IFREG => Some(FileType::Regular),
            S_IFDIR => Some(FileType::Directory),
            S_IFLNK => Some(FileType::Symlink),
            S_IFCHR => Some(FileType::CharDevice),
            S_IFBLK => Some(FileType::BlockDevice),
            S_IFIFO => Some(FileType::Fifo),
            S_IFSOCK => Some(FileType::Socket),
            _ => None,
        }
    }

    /// The `DT_*` value `getdents64` reports for this kind.
    #[must_use]
    pub const fn dirent_type(self) -> u8 {
        match self {
            FileType::Regular => DT_REG,
            FileType::Directory => DT_DIR,
            FileType::Symlink => DT_LNK,
            FileType::CharDevice => DT_CHR,
            FileType::BlockDevice => DT_BLK,
            FileType::Fifo => DT_FIFO,
            FileType::Socket => DT_SOCK,
        }
    }
}

/// What `stat` reports about an inode, less the device, which is the mount's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    /// Inode number, unique within the filesystem.
    pub ino: u64,
    /// What kind of object it is.
    pub kind: FileType,
    /// Permission and set-id bits, `0o7777` at most.
    pub permissions: u32,
    /// Number of names it has; for a directory, two plus its subdirectories.
    pub nlink: u32,
    /// Owning user.
    pub uid: u32,
    /// Owning group.
    pub gid: u32,
    /// Size in bytes; for a symbolic link, the length of its target.
    pub size: u64,
    /// The device a device node stands for, as Linux's `(major << 8) | minor`
    /// encoding widened to 64 bits. Zero for everything else.
    pub rdev: u64,
    /// Storage actually committed, in 512-byte blocks.
    pub blocks: u64,
    /// The block size a program should use for I/O.
    pub block_size: u32,
    /// Last access.
    pub atime: Timespec,
    /// Last modification of the contents.
    pub mtime: Timespec,
    /// Last change to the metadata or the contents.
    pub ctime: Timespec,
}

impl Metadata {
    /// The whole mode word: kind and permissions.
    #[must_use]
    pub const fn mode(&self) -> u32 {
        self.kind.mode_bits() | self.permissions
    }
}

/// What [`Inode::create`] is asked to make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewNode<'a> {
    /// An empty regular file.
    Regular,
    /// An empty directory.
    Directory,
    /// A symbolic link to the given target.
    Symlink(&'a [u8]),
    /// A character or block device node.
    Device {
        /// [`FileType::CharDevice`] or [`FileType::BlockDevice`].
        kind: FileType,
        /// The device it stands for.
        rdev: u64,
    },
    /// A named pipe.
    Fifo,
    /// A socket's name.
    Socket,
}

impl NewNode<'_> {
    /// The kind of inode this will be.
    #[must_use]
    pub const fn kind(&self) -> FileType {
        match self {
            NewNode::Regular => FileType::Regular,
            NewNode::Directory => FileType::Directory,
            NewNode::Symlink(_) => FileType::Symlink,
            NewNode::Device { kind, .. } => *kind,
            NewNode::Fifo => FileType::Fifo,
            NewNode::Socket => FileType::Socket,
        }
    }
}

/// A change to an inode's metadata. `None` leaves a field as it is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SetAttributes {
    /// New permission bits.
    pub permissions: Option<u32>,
    /// New owner.
    pub uid: Option<u32>,
    /// New group.
    pub gid: Option<u32>,
    /// New access time.
    pub atime: Option<Timespec>,
    /// New modification time.
    pub mtime: Option<Timespec>,
}

/// One entry a directory reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirEntry<'a> {
    /// The inode number the name refers to.
    pub ino: u64,
    /// What kind of object that is.
    pub kind: FileType,
    /// The name, without any terminator.
    pub name: &'a [u8],
    /// The cursor that resumes reading *after* this entry.
    pub next: u64,
}

/// The cursor at which a filesystem's own entries begin.
///
/// Cursors 0 and 1 are `.` and `..`, which the VFS reports itself because it
/// is the one that knows what a directory's parent is — across a mount point,
/// a filesystem's root directory has a parent the filesystem has never heard
/// of. A filesystem's [`Inode::read_dir`] is never asked for a cursor below
/// this.
pub const FIRST_CURSOR: u64 = 2;

/// What an open object is ready for, as `poll` and `select` report it.
///
/// Five booleans rather than `POLL*` bits, because the bits are the system
/// call layer's encoding and this crate does not encode anything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each is one independent poll(2) condition, and a bit set would be \
              the encoding this type exists to leave to the system call layer"
)]
pub struct Readiness {
    /// A read would not block: `POLLIN`.
    pub readable: bool,
    /// A write would not block: `POLLOUT`.
    pub writable: bool,
    /// The other end is gone: `POLLHUP`.
    pub hangup: bool,
    /// Writing would fail: `POLLERR`.
    pub error: bool,
    /// Something exceptional is to be read: `POLLPRI`, `EPOLLPRI`, and
    /// `select`'s exception set. False for everything but a file that
    /// announces a change this way, as `cgroup.events` does: from a change
    /// until the file is read again.
    pub priority: bool,
}

impl Readiness {
    /// Ready for both, which is what Linux reports for a regular file or a
    /// directory: neither ever makes a reader or a writer wait.
    pub const ALWAYS: Readiness = Readiness {
        readable: true,
        writable: true,
        hangup: false,
        error: false,
        priority: false,
    };
}

/// A queue an object wakes when what [`Inode::poll`] reports may have
/// changed, as [`Inode::poll_queues`] names it.
///
/// Opaque here: the queue is the kernel's, which this crate cannot name, so
/// it is handed over as `Any` for the kernel to recognise. A queue that lives
/// as long as the kernel is lent; one that lives inside an object that can go
/// is shared, so a wait holds it for as long as it waits.
#[derive(Clone)]
pub enum WakeSource {
    /// A queue that is never freed.
    Static(&'static (dyn Any + Send + Sync)),
    /// A queue held by reference count.
    Shared(Arc<dyn Any + Send + Sync>),
}

impl fmt::Debug for WakeSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            WakeSource::Static(_) => "WakeSource::Static",
            WakeSource::Shared(_) => "WakeSource::Shared",
        })
    }
}

/// What `statfs` reports about a mounted filesystem.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatFs {
    /// The filesystem's magic number, `f_type`: `TMPFS_MAGIC` and the like.
    /// Programs test it to find out what they are running on.
    pub magic: u64,
    /// The block size the counts below are in.
    pub block_size: u64,
    /// Blocks in total.
    pub blocks: u64,
    /// Blocks free.
    pub blocks_free: u64,
    /// Blocks free to an unprivileged user.
    pub blocks_available: u64,
    /// Inodes in total.
    pub files: u64,
    /// Inodes free.
    pub files_free: u64,
    /// The longest name a directory entry may have.
    pub name_max: u64,
}

/// Where time comes from, for the timestamps a filesystem keeps.
pub trait Clock: Send + Sync + fmt::Debug {
    /// The time now.
    fn now(&self) -> Timespec;
}

/// A mounted filesystem instance.
pub trait FileSystem: Send + Sync + fmt::Debug {
    /// Its root directory.
    fn root(&self) -> Arc<dyn Inode>;
    /// The name `/proc/mounts` would give its type: `tmpfs`, `proc`.
    fn name(&self) -> &'static str;
    /// The device number `stat` reports for every inode in it.
    fn device(&self) -> u64;

    /// Make everything written to this filesystem durable: `sync` and
    /// `syncfs`, and what unmounting one does before it lets go.
    ///
    /// The default is `Ok`, for a filesystem that keeps nothing back.
    fn sync(&self) -> Result<()> {
        Ok(())
    }

    /// Whether the filesystem itself takes no writes, whatever the mount
    /// says: what `mountinfo`'s last field shows as `ro`, Linux's
    /// `sb_rdonly`. The default is writable.
    fn read_only(&self) -> bool {
        false
    }

    /// What `statfs` reports.
    ///
    /// The default knows nothing: no magic number, no counts, and the name
    /// limit every filesystem here shares. A filesystem a program might test
    /// for — tmpfs, procfs — says what it is.
    fn statfs(&self) -> StatFs {
        StatFs {
            block_size: 4096,
            name_max: crate::path::NAME_MAX as u64,
            ..StatFs::default()
        }
    }
}

/// One file, directory, link or device, as a filesystem implements it.
///
/// Every operation has a default that refuses the way Linux refuses it for an
/// object of the wrong kind, so a filesystem implements only what its objects
/// are. A directory that does not override [`Inode::read_at`] is refused with
/// `EINVAL` from here, but the VFS turns a read of a directory into `EISDIR`
/// before asking; the defaults are the answer for a kind the VFS cannot see.
///
/// # The contract
///
/// * An operation may block. The VFS holds none of an open file description's
///   locks and no dentry lock across a call; the one VFS lock held across
///   calls is the namespace's rename lock, during `rename`, as the crate
///   documentation says. A filesystem's own locks are its own business, and a
///   filesystem that does I/O must not hold a spin lock across it.
/// * A name passed in is already validated: not empty, not `.` or `..`, no
///   `/`, no NUL, no longer than [`crate::path::NAME_MAX`].
/// * Errors are the ones Linux would return for the same operation.
pub trait Inode: Send + Sync + fmt::Debug {
    /// What `stat` reports.
    fn metadata(&self) -> Metadata;

    /// This inode as a value that can be downcast, so a filesystem can
    /// recognise its own inodes when `link` or `rename` hands it one.
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync>;

    /// Change the metadata.
    fn set_attributes(&self, change: &SetAttributes) -> Result<()> {
        let _ = change;
        Err(Errno::EPERM)
    }

    /// Read up to `buf.len()` bytes at `offset`, returning how many were read.
    /// Zero is end of file.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let _ = (offset, buf);
        Err(Errno::EINVAL)
    }

    /// Write `data` at `offset`, or at the end if `append`, returning the
    /// count written and the offset just past it.
    fn write_at(&self, offset: u64, data: &[u8], append: bool) -> Result<(usize, u64)> {
        let _ = (offset, data, append);
        Err(Errno::EINVAL)
    }

    /// Read from an object with no position, one whose [`Inode::is_stream`]
    /// is true.
    ///
    /// Separate from [`Inode::read_at`] because a stream is the kind of object
    /// that can make a reader wait, and whether it may is the open file's
    /// `O_NONBLOCK` -- which lives on the description, not the inode, and can
    /// change under `fcntl` between two reads. `nonblock` is that flag as it
    /// stands for this call. An object that would wait answers `EAGAIN`
    /// instead when it is set.
    ///
    /// The default reads at offset zero and ignores the flag, which is right
    /// for a stream that never asks a caller to wait on another program.
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize> {
        let _ = nonblock;
        self.read_at(0, buf)
    }

    /// Whether one read of this stream takes all it has, up to the length
    /// asked, rather than stopping after the first piece: Linux's
    /// `pipe_read` goes on through a pipe's buffers until the pipe is empty,
    /// and its memory devices fill the whole request -- measured on a 7.0
    /// host, `read` of 65536 from /dev/zero is 65536, and `readv` of a pipe
    /// holding six bytes into two segments of four is 6.
    ///
    /// The system call layer reads through a buffer of a page. After a piece
    /// that took something it reads on only from a stream that says yes, and
    /// then through [`Inode::read_on`] ([`crate::OpenFile::read_more`]),
    /// which never waits, so a reader that already has bytes is never made
    /// to wait for more.
    ///
    /// The default, no, is right for a stream whose one read is one record
    /// or one line -- an eventfd's counter, a terminal's line, a datagram --
    /// where reading on would take the next, which one Linux read never does.
    fn fills_reads(&self) -> bool {
        false
    }

    /// The next piece of a read that has already taken bytes, from a stream
    /// that [fills reads](Inode::fills_reads): what it has now, never
    /// waiting, and `Ok(0)` or `EAGAIN` where the read ends.
    ///
    /// The default is [`Inode::read_stream`] with `nonblock` set, which is
    /// right for a stream with no boundaries inside it, a pipe or a memory
    /// device. A stream whose one read stops at a boundary a second read
    /// would cross -- a Unix stream socket's ancillary data -- answers here
    /// what the read it goes on with would still have taken.
    fn read_on(&self, buf: &mut [u8]) -> Result<usize> {
        self.read_stream(buf, true)
    }

    /// Take back `bytes`, the start of what the last [`Inode::read_stream`]
    /// gave, which could not be delivered to the reader. The default drops
    /// them: most streams cannot put data back.
    fn unread_stream(&self, bytes: &[u8]) {
        let _ = bytes;
    }

    /// Write to an object with no position; see [`Inode::read_stream`].
    ///
    /// A write that would wait with `nonblock` set answers the count it
    /// managed, or `EAGAIN` if that was nothing.
    fn write_stream(&self, data: &[u8], nonblock: bool) -> Result<usize> {
        let _ = nonblock;
        self.write_at(0, data, false).map(|(count, _)| count)
    }

    /// Change a regular file's length.
    fn set_len(&self, len: u64) -> Result<()> {
        let _ = len;
        Err(Errno::EINVAL)
    }

    /// Make everything written to this file durable, and do not return until
    /// it is: `fsync`, or with `data_only` `fdatasync`, which need not write
    /// out metadata a reader does not need.
    ///
    /// The default is `Ok`, which is the truth for a filesystem that keeps
    /// nothing back — tmpfs, procfs, a pipe — and is what Linux answers for a
    /// file whose filesystem has no `fsync` operation. A filesystem that
    /// commits, as btrfs does, writes here.
    fn fsync(&self, data_only: bool) -> Result<()> {
        let _ = data_only;
        Ok(())
    }

    /// Lengthen a regular file to `len` if it is shorter, and leave it alone
    /// if it is not: what `fallocate` asks.
    ///
    /// Its own operation rather than a look at the size and a [`Inode::set_len`],
    /// because between the two another writer can extend the file, and the
    /// `set_len` would then cut off what it wrote. The default does exactly
    /// that look and is only as good as a filesystem nobody writes to
    /// concurrently; a filesystem with a lock over its length decides under it.
    fn grow_to(&self, len: u64) -> Result<()> {
        if self.metadata().size >= len {
            return Ok(());
        }
        self.set_len(len)
    }

    /// What the object is ready for.
    ///
    /// The default is [`Readiness::ALWAYS`], which is right for everything that
    /// never makes a caller wait. An object that can — a pipe, a terminal —
    /// reports its state, and wakes whoever waits on it when that changes.
    fn poll(&self) -> Readiness {
        Readiness::ALWAYS
    }

    /// A number that moves whenever what [`Inode::poll`] reports may have
    /// changed: data arrived, room was made, a peer left. `epoll`'s
    /// edge-triggered mode reports an object again when it moved, as Linux
    /// reports one again when the object wakes its wait queue.
    ///
    /// `None`, the default, for an object `epoll` refuses to watch: one that
    /// never makes a caller wait, as Linux refuses a regular file or a
    /// directory, which have no `poll` operation. An object that overrides
    /// [`Inode::poll`] to report a state of its own answers `Some`.
    fn poll_changes(&self) -> Option<u64> {
        None
    }

    /// The queues this object wakes when its readiness may have changed, each
    /// handed to `visit`, so a `poll` or `epoll_wait` can sleep on them rather
    /// than asking again every few milliseconds.
    ///
    /// Answers whether every change to what [`Inode::poll`] reports wakes one
    /// of them. A wait may then sleep long between its own looks; when not --
    /// the default, which names no queue -- it keeps looking often.
    ///
    /// **The contract `true` makes**, which an epoll set relies on more than
    /// a `poll` does, since it hooks the queues once and looks at the object
    /// again only when one of them is woken (the kernel's `fs::epoll`):
    ///
    /// * every change [`Inode::poll`] can report wakes a queue named here;
    /// * a change of *which* queues are named here -- a socket naming its
    ///   peer's once it connects -- wakes one of the queues it named before,
    ///   so a watcher looks again and finds the new ones.
    ///
    /// It is asked inside a wait's last look, with preemption held off: it
    /// may take spin locks, and must not block or take a sleeping lock.
    fn poll_queues(&self, visit: &mut dyn FnMut(WakeSource)) -> bool {
        let _ = visit;
        false
    }

    /// Whether [`Inode::poll_queues`] names the same queues, and gives the
    /// same answer, for this object's whole life: an epoll registration then
    /// hooks them once and never asks again. The default, no, makes it ask
    /// at every look, which an object whose queues change -- a socket that
    /// connects -- needs.
    fn poll_queues_fixed(&self) -> bool {
        false
    }

    /// Whether this object has no position: a terminal, a pipe. Offsets passed
    /// to it are ignored and `lseek` on it is `ESPIPE`, unless
    /// [`Inode::ignores_position`] or [`Inode::seek_is_noop`] says otherwise.
    fn is_stream(&self) -> bool {
        false
    }

    /// Whether this stream has a position that never moves, as Linux's
    /// memory devices have: `/dev/null`, `zero`, `full`, `random` and
    /// `urandom`. Their reads and writes are still a stream's, but `lseek`
    /// answers 0 for any offset and every whence, and `pread64` and
    /// `pwrite64` are an ordinary read and write, as they are on Linux
    /// (`null_lseek` and `noop_llseek` in `drivers/char/`). A program uses
    /// that: busybox `dd of=/dev/null seek=1` seeks its output and dies if
    /// the seek is refused.
    ///
    /// Meaningful only where [`Inode::is_stream`] is true. The default --
    /// no, `ESPIPE` -- is a pipe's, a socket's and a terminal's.
    fn ignores_position(&self) -> bool {
        false
    }

    /// Whether `lseek` on this stream answers 0, whatever it is asked, rather
    /// than `ESPIPE`: Linux's `noop_llseek`, which its eventfd, timerfd,
    /// signalfd, epoll and inotify files have. Measured on a 7.0 host,
    /// `lseek` on each is 0 for every offset and whence, a read between
    /// included, while `pread64` and `pwrite64` on them stay `ESPIPE`. So
    /// unlike [`Inode::ignores_position`] this says nothing about the offsets
    /// reads and writes are given.
    ///
    /// Meaningful only where [`Inode::is_stream`] is true. The default is
    /// [`Inode::ignores_position`]'s answer: a stream whose position never
    /// moves is at 0 too.
    fn seek_is_noop(&self) -> bool {
        self.ignores_position()
    }

    /// Whether `sendfile` and `splice` may take bytes out of this object:
    /// whether Linux's file for it has a `splice_read`, without which both
    /// calls are `EINVAL` for any count above zero, whatever the output.
    ///
    /// Most files have one, and the default says so. Measured on a 7.0 host,
    /// these do not: `/dev/null`; the event and DRM device nodes; the files
    /// of `/proc/net`; and every file of `/proc/<pid>` and
    /// `/proc/<pid>/task/<tid>` but `mounts` -- `status`, `stat`, `comm`,
    /// `cmdline`, `maps`, `cgroup`, `oom_score_adj` -- which read through
    /// `seq_read` rather than a `read_iter`. The rest of `/proc`, `/proc/sys`,
    /// sysfs and cgroupfs send. A pipe and the anonymous objects have none
    /// either; `sendfile` asks those by kind.
    fn splices_out(&self) -> bool {
        true
    }

    /// Called when the inode is opened. `Some` replaces the object that reads
    /// and writes go to for the life of that open file, while `stat` keeps
    /// reporting this inode.
    ///
    /// This is how a generated file — `/proc/self/maps` — is read
    /// consistently: it renders once, at open, and a program reading it in
    /// small pieces sees one snapshot rather than a different file each time.
    fn open(&self) -> Result<Option<Arc<dyn Inode>>> {
        Ok(None)
    }

    /// Whether the VFS may remember what [`Inode::lookup`] answers in this
    /// directory.
    ///
    /// True for every filesystem whose names change only through the VFS,
    /// which is what lets a cached hit or miss stand until the VFS itself
    /// changes the name. A directory whose names come and go behind its back
    /// answers false — `/proc`, where a process exiting removes a name and a
    /// process starting adds one, and `/proc/<pid>/fd`, where every `open`
    /// does — and every walk through it asks the filesystem afresh. Linux
    /// answers the same question per dentry with `d_revalidate`; one answer
    /// per directory is enough for the directories that exist.
    ///
    /// The cost is that nothing can be mounted inside such a directory: a
    /// mount point is a remembered dentry, and none is remembered here. A
    /// directory with one name that does not come and go can make that one
    /// name an exception with [`Inode::caches_lookup_of`].
    fn caches_lookups(&self) -> bool {
        true
    }

    /// Whether this one name may be remembered although the directory itself
    /// says its names are not.
    ///
    /// The exception a directory of coming-and-going names needs for the one
    /// name that never does. `/dev` is the case it was added for: its device
    /// names appear and vanish as drivers register, so it answers
    /// [`Inode::caches_lookups`] with false, but `shm` is always there and
    /// something has to be mounted on it. A mount point is a remembered
    /// dentry, so without this there is no way to put a filesystem anywhere
    /// inside such a directory.
    ///
    /// Answer true only for a name that is always present and always the same
    /// object, because nothing will ever look it up again: the cached answer
    /// stands for the life of the dentry, and there is no invalidation.
    fn caches_lookup_of(&self, name: &[u8]) -> bool {
        let _ = name;
        false
    }

    /// Whether this file holds pages in a page cache that reclaim can give
    /// back and read again, and still has a name: a disk filesystem's file
    /// with pages read or written. The dentry cache keeps the dentry of such
    /// a file past its bound until reclaim has taken the pages
    /// ([`crate::namespace::DEFAULT_CACHE`]), so that the count of names
    /// looked up never decides how much of the cache stays. False by default:
    /// a memory filesystem's files are its tree's to keep, and their pages
    /// are never reclaimed.
    ///
    /// Asked with no lock of the VFS's held; it takes only the file's own.
    fn holds_cached_pages(&self) -> bool {
        false
    }

    /// The object called `name` in this directory, or `ENOENT`.
    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn Inode>> {
        let _ = name;
        Err(Errno::ENOTDIR)
    }

    /// Make a new object called `name` in this directory; `EEXIST` if the name
    /// is taken.
    fn create(&self, name: &[u8], node: NewNode<'_>, permissions: u32) -> Result<Arc<dyn Inode>> {
        let _ = (name, node, permissions);
        Err(Errno::ENOTDIR)
    }

    /// Give `target` a further name in this directory.
    fn link(&self, name: &[u8], target: &Arc<dyn Inode>) -> Result<()> {
        let _ = (name, target);
        Err(Errno::ENOTDIR)
    }

    /// Remove the name `name`, which must not be a directory.
    fn unlink(&self, name: &[u8]) -> Result<()> {
        let _ = name;
        Err(Errno::ENOTDIR)
    }

    /// Remove the empty directory called `name`.
    fn rmdir(&self, name: &[u8]) -> Result<()> {
        let _ = name;
        Err(Errno::ENOTDIR)
    }

    /// Move `old` in this directory to `new` in `new_parent`, replacing what
    /// is there unless `replace` is false.
    fn rename(
        &self,
        old: &[u8],
        new_parent: &Arc<dyn Inode>,
        new: &[u8],
        replace: bool,
    ) -> Result<()> {
        let _ = (old, new_parent, new, replace);
        Err(Errno::ENOTDIR)
    }

    /// Report entries from `cursor` onwards, until `emit` returns `false` or
    /// there are none left. An entry `emit` refused has not been consumed.
    fn read_dir(&self, cursor: u64, emit: &mut dyn FnMut(DirEntry<'_>) -> bool) -> Result<()> {
        let _ = (cursor, emit);
        Err(Errno::ENOTDIR)
    }

    /// A symbolic link's target.
    fn read_link(&self) -> Result<Vec<u8>> {
        Err(Errno::EINVAL)
    }

    /// Where following this link leads, for a *magic* link: one that stands
    /// for an object rather than naming a path.
    ///
    /// `/proc/<pid>/exe` is the one that matters. A walk that follows it
    /// arrives at the file the process's program was loaded from, whatever
    /// [`Inode::read_link`] says and whether or not a path still reaches that
    /// file: `execve("/proc/self/exe")` runs the same program after its file
    /// was renamed, replaced or deleted, as Chrome does to start every child
    /// process. That is Linux's `nd_jump_link`.
    ///
    /// `None`, the default, for an ordinary symbolic link, whose target is
    /// the path [`Inode::read_link`] returns; and for a magic link with no
    /// object behind it, which is then followed as one.
    fn link_location(&self) -> Option<Result<crate::Location>> {
        None
    }

    /// The object a memory mapping of this file maps: in the kernel, the
    /// file's VMO, whose pages `read_at` copies out of too.
    ///
    /// `None`, the default, for an object that cannot be mapped, which `mmap`
    /// answers with `ENODEV`. A filesystem whose file contents live in a
    /// [`crate::tmpfs::Pages`] store answers that store's
    /// [`crate::tmpfs::Pages::object`], making it first if a mapping comes
    /// before any read.
    ///
    /// A store filled from a page source may be offered here only when a
    /// fault fills an absent page from the source as a read does: a page the
    /// source has not filled yet would otherwise be mapped as zeros, and a
    /// private mapping's first write would copy them over the file's data.
    /// The kernel's page cache does (`src/kernel/src/fs/pages.rs`, whose object
    /// carries the source into the fault); tmpfs's stores have no source.
    fn mapping(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        None
    }

    /// The object a memory mapping of this file *at byte `offset`* maps, and
    /// the byte of that object the mapping starts at.
    ///
    /// For every file whose pages are one object this is [`Inode::mapping`]
    /// and the same offset, which is the default. A device whose offsets are
    /// names rather than places -- a DRM render node, where each buffer
    /// object has pages of its own and `VIRTGPU_MAP` hands out an offset that
    /// stands for one -- answers the object the offset names instead.
    fn mapping_at(&self, offset: u64) -> Option<(Arc<dyn Any + Send + Sync>, u64)> {
        self.mapping().map(|object| (object, offset))
    }

    /// The seals on this file, as `fcntl(F_GET_SEALS)` reports them.
    ///
    /// `EINVAL`, the default, for a filesystem that cannot seal, which is
    /// Linux's answer for any file not on shmem.
    fn seals(&self) -> Result<u32> {
        Err(Errno::EINVAL)
    }

    /// Add `seals`, as `fcntl(F_ADD_SEALS)` does.
    ///
    /// A write seal is refused with `EBUSY` while a shared mapping may write
    /// the file. `writably_mapped` answers that, and is asked only after the
    /// seal has been stored: a mapping counts itself before it reads the seals,
    /// so one of the two always sees the other. `EPERM` once `F_SEAL_SEAL` is
    /// set; `EINVAL` for a filesystem that cannot seal.
    fn add_seals(&self, seals: u32, writably_mapped: &dyn Fn() -> bool) -> Result<()> {
        let _ = (seals, writably_mapped);
        Err(Errno::EINVAL)
    }
}
