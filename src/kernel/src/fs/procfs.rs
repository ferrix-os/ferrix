//! procfs: the kernel describing itself and its processes as files.
//!
//! Programs learn about themselves through `/proc` rather than through system
//! calls. `ps` is a listing of it, a C library counts processors in
//! `/proc/cpuinfo` and memory in `/proc/meminfo`, a sanitizer and a garbage
//! collector read `/proc/self/maps` to find their own stack, and a program
//! that wants its own path reads `/proc/self/exe`. None of these is optional
//! for the software the exit criterion runs, and the byte formats are pinned
//! in `src/lib/fs/procfs`, where the host tests hold them against lines a real
//! Linux printed.
//!
//! # Rendered at open
//!
//! Every file is generated when it is opened, by [`Inode::open`] handing the
//! VFS a snapshot, and every read of that open file reads the snapshot. A
//! program reading `/proc/self/maps` a few bytes at a time — `busybox cat`
//! reads in pages, a `getline` loop in whatever its buffer is — sees one
//! consistent file, even though its own reads allocate and change the map
//! they are reading. Rendering at each read would hand it the first half of
//! one map and the second half of another.
//!
//! # Nothing is remembered
//!
//! Names here change without the VFS being told: a process starting adds
//! `/proc/<pid>`, one exiting removes it, and every `open` adds a name to
//! `/proc/<pid>/fd`. So every directory answers [`Inode::caches_lookups`]
//! with `false`, and a walk asks this filesystem each time. `/proc/self` is a
//! symbolic link whose target is the caller's pid, computed when the link is
//! read, which is what makes one name mean a different directory to every
//! process.
//!
//! # Extending it
//!
//! The top level and a process's directory are both tables of [`Entry`]: a
//! name, permission bits, and what the name is — a file rendered at open and
//! optionally writable, a link rendered when read, the descriptor directory,
//! or a directory of more entries. A new file is a row in [`TOP`],
//! [`PER_PROCESS`] or one of the `/proc/sys` tables and one function; nothing
//! else here needs to learn about it.
//!
//! # `/proc/sys`
//!
//! A tree of fixed directories under [`TOP`], each value one line. It holds
//! only the sysctls whose value something in the kernel already keeps — what
//! `uname` reports, the pid registry's limit, the descriptor table's — and
//! the host and domain names are written through the setters `sethostname`
//! and `setdomainname` use. Any other value is refused for writing with
//! `EACCES` when it is opened, by [`refuse_write_open`] on `openat`'s way
//! out, and a write that reaches one anyway is refused with the same errno.
//! That is where Linux's sysctls differ from the rest of its `/proc`.
//!
//! # Inode numbers
//!
//! Computed from what a node is, so the same file has the same number every
//! time without a table of numbers to keep, and all of them below 2³², as
//! Linux's are: a 32-bit program's `readdir` and `stat` without large-file
//! support fail with `EOVERFLOW` on any number wider, and the Steam client,
//! which is one, then sees no processes at all.
//!
//! The 2³² numbers are cut into [`registry::PID_MAX`] blocks of 2¹⁷, one per
//! pid. Block 0 is the top of the tree and `/proc/sys`, numbered densely in
//! the order a listing walks them ([`Tree::ordinal`]); no process has pid 0.
//! Block `pid` holds `/proc/<pid>` at its start, the process's entries after
//! it, and from `0x200` to its end the links in `/proc/<pid>/fd`. A thread's
//! directory and entries are in its *tid's* block, at `0x100`: thread ids and
//! pids are one table's numbers, so a tid's block is its process's own for
//! the first thread and belongs to no process for every other. So every name
//! a process can have gets a number of its own, except that descriptors
//! above 130,560 share numbers with those 130,560 below them -- no pid block
//! can hold `fd::MAX_LIMIT`'s million, and 2³² has no room for 32,768 blocks
//! that could -- which `find` and `du` only notice as two links being one.
//!
//! Nothing else about the numbers is promised, as nothing is on Linux, whose
//! `/proc` numbers come from a counter and change when a name is looked up
//! afresh.

pub(crate) mod check;
pub(crate) mod loadavg;
mod render;

/// The kernel command line, kept at boot for `/proc/cmdline`: the loader's
/// boot info, where it arrived, is memory the kernel takes back.
static COMMAND_LINE: ferrix_sync::Once<Vec<u8>> = ferrix_sync::Once::new();

/// Keep the command line the loader passed, for `/proc/cmdline`. Once, at
/// bring-up, from the crate root.
pub(crate) fn remember_command_line(line: &str) {
    // FATAL-ALLOC: boot only: the command line is copied once, as the kernel comes up.
    let _ = COMMAND_LINE.call_once(|| Vec::from(line.as_bytes()));
}

/// The command line [`remember_command_line`] kept, or nothing before it:
/// `/proc/cmdline`'s, and what the display core reads `drm.edid_firmware`
/// from (`crate::interfaces::display::edid`).
pub(crate) fn command_line() -> &'static [u8] {
    COMMAND_LINE.get().map_or(&[][..], Vec::as_slice)
}

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;

use ferrix_kmem::{Charge, arc_footprint, buffer_footprint};
use ferrix_vfs::{
    DirEntry, Errno, FIRST_CURSOR, FileSystem, FileType, Inode, Location, Metadata, OpenFile,
    Result, StatFs, Timespec,
};

use crate::fs;
use crate::panic::{catalog, fatal};
use crate::syscall::process::Process;
use crate::syscall::registry;

/// What an entry's functions are handed at the top level: nothing, because a
/// top-level file describes the kernel rather than a process.
pub(crate) type Kernel = ();

/// What a file in `/proc/<pid>/task/<tid>` is handed: the process, and which
/// of its threads the directory is.
pub(crate) struct ThreadOf {
    /// The process the thread belongs to.
    pub(crate) process: Arc<Process>,
    /// The thread's id.
    pub(crate) tid: u32,
}

/// A write into a file in a table: the context, the bytes, and how many
/// were consumed.
pub(crate) type WriteFn<T> = fn(&T, &[u8]) -> Result<usize>;

/// What a name in a table is.
pub(crate) enum Content<T: 'static> {
    /// A regular file, rendered when opened, and written through `write` if
    /// it has one. A file with none refuses writes with `EINVAL`, as Linux
    /// does for a `/proc` file with no write handler.
    File {
        /// Produce the contents.
        render: fn(&T) -> Result<Vec<u8>>,
        /// Take a write, returning how much was consumed.
        write: Option<WriteFn<T>>,
    },
    /// A symbolic link, whose target is rendered each time it is read.
    Link(fn(&T) -> Result<Vec<u8>>),
    /// `/proc/<pid>/uid_map`, `gid_map` or `setgroups`: read as the reader's
    /// namespace shows it, written once by the rules of
    /// `docs/NAMESPACES.md` §2.2 (U2 to U4), judged against who opened the
    /// file and who writes it.
    IdMap(MapFile),
    /// `/proc/<pid>/fd`: a link per open descriptor.
    Descriptors,
    /// `/proc/<pid>/ns`: a link per namespace the process is in: `mnt`,
    /// `user`, `uts`, `ipc` and `cgroup` (`docs/NAMESPACES.md` §2.4).
    Namespaces,
    /// `/proc/<pid>/task`: a directory per thread, each holding
    /// [`PER_THREAD`].
    Threads,
    /// A directory whose names are fixed, as the table's are: `/proc/sys` and
    /// the directories under it. Only [`TOP`]'s tree may hold one; see
    /// [`Tree`].
    Directory {
        /// What is in it.
        entries: &'static [Entry<T>],
        /// What a write to a file in it that takes none is refused with.
        refusal: Errno,
    },
}

/// Which of a user namespace's three files an [`Content::IdMap`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MapFile {
    /// `uid_map`.
    Uid,
    /// `gid_map`.
    Gid,
    /// `setgroups`.
    Setgroups,
}

/// The namespaces under `/proc/<pid>/ns`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NamespaceKind {
    /// `mnt`.
    Mount = 0,
    /// `user`.
    User = 1,
    /// `uts`.
    Uts = 2,
    /// `ipc`.
    Ipc = 3,
    /// `cgroup`.
    Cgroup = 4,
    /// `net`.
    Net = 5,
}

impl NamespaceKind {
    /// Every one, in the order `ns` lists them, with its name.
    const ALL: [(NamespaceKind, &'static [u8]); 6] = [
        (NamespaceKind::Mount, b"mnt"),
        (NamespaceKind::User, b"user"),
        (NamespaceKind::Uts, b"uts"),
        (NamespaceKind::Ipc, b"ipc"),
        (NamespaceKind::Cgroup, b"cgroup"),
        (NamespaceKind::Net, b"net"),
    ];
}

/// One name in a table.
pub(crate) struct Entry<T: 'static> {
    /// The name.
    pub(crate) name: &'static [u8],
    /// Permission bits, as `stat` reports them.
    pub(crate) permissions: u32,
    /// What it is.
    pub(crate) content: Content<T>,
}

impl<T> Entry<T> {
    /// Whether this entry is a file that takes writes.
    const fn takes_writes(&self) -> bool {
        matches!(
            self.content,
            Content::File { write: Some(_), .. } | Content::IdMap(_)
        )
    }

    /// The kind of object this entry is.
    const fn kind(&self) -> FileType {
        match self.content {
            Content::File { .. } | Content::IdMap(_) => FileType::Regular,
            Content::Link(_) => FileType::Symlink,
            Content::Descriptors
            | Content::Namespaces
            | Content::Threads
            | Content::Directory { .. } => FileType::Directory,
        }
    }
}

/// A file rendered at open that nothing may write.
const fn file<T>(name: &'static [u8], render: fn(&T) -> Result<Vec<u8>>) -> Entry<T> {
    Entry {
        name,
        permissions: 0o444,
        content: Content::File {
            render,
            write: None,
        },
    }
}

/// A directory under `/proc/sys`.
///
/// A write to a file in it that takes none is `EACCES`, not the `EINVAL` of
/// the rest of `/proc`: Linux checks a sysctl's mode bits in
/// `proc_sys_permission`, for root as for anyone, rather than finding no
/// handler when the write arrives. `openat` refuses the open through
/// [`refuse_write_open`]; the refusal of the write itself is the backstop for
/// an open made without it, such as the kernel's own through the namespace.
const fn sysctl_directory(name: &'static [u8], entries: &'static [Entry<Kernel>]) -> Entry<Kernel> {
    Entry {
        name,
        permissions: 0o555,
        content: Content::Directory {
            entries,
            refusal: Errno::EACCES,
        },
    }
}

/// A value under `/proc/sys` that a program may set.
const fn sysctl_setting(
    name: &'static [u8],
    render: fn(&Kernel) -> Result<Vec<u8>>,
    write: WriteFn<Kernel>,
) -> Entry<Kernel> {
    Entry {
        name,
        permissions: 0o644,
        content: Content::File {
            render,
            write: Some(write),
        },
    }
}

/// What a write-only file renders: nothing, so a read is end of file.
fn nothing(_: &Kernel) -> Result<Vec<u8>> {
    Ok(Vec::new())
}

/// `/proc/sysrq-trigger`: writing `c` panics the kernel, on purpose.
///
/// The one request of Linux's magic `SysRq` set that is implemented, because it
/// is the one somebody working on the failure path needs: a way to reach the
/// report, the backtrace, the screen and its QR code from a shell, without
/// building a kernel with a fault in it. Linux looks only at the first byte,
/// and so does this. Any other byte is accepted and does nothing.
fn sysrq_trigger(_: &Kernel, data: &[u8]) -> Result<usize> {
    if data.first() == Some(&b'c') {
        fatal!(
            catalog::SYSRQ_CRASH,
            "panic requested through /proc/sysrq-trigger"
        );
    }
    Ok(data.len())
}

/// A directory of `/proc` whose files only report, so a write to one is
/// `EACCES` rather than a missing handler.
const fn reporting_directory(
    name: &'static [u8],
    entries: &'static [Entry<Kernel>],
) -> Entry<Kernel> {
    Entry {
        name,
        permissions: 0o555,
        content: Content::Directory {
            entries,
            refusal: Errno::EACCES,
        },
    }
}

/// `/proc/net`: what `route`, `netstat`, `arp` and `ifconfig` read.
///
/// On Linux this is a symbolic link to `/proc/self/net`, because each network
/// namespace has its own. Ferrix has one network namespace, so it is a
/// directory; a program that follows the link finds the same files either way.
static NET: [Entry<Kernel>; 7] = [
    file(b"arp", render::net_arp),
    file(b"dev", render::net_dev),
    file(b"route", render::net_route),
    file(b"tcp", render::net_tcp),
    file(b"tcp6", render::net_tcp6),
    file(b"udp", render::net_udp),
    file(b"udp6", render::net_udp6),
];

/// `/proc`, less the process directories that follow these in a listing.
pub(crate) static TOP: [Entry<Kernel>; 15] = [
    Entry {
        name: b"self",
        permissions: 0o777,
        content: Content::Link(render::self_link),
    },
    file(b"cmdline", render::cmdline_file),
    file(b"cpuinfo", render::cpuinfo),
    file(b"ferrix-seam", render::seam),
    file(b"filesystems", render::filesystems),
    file(b"loadavg", render::loadavg),
    file(b"meminfo", render::meminfo),
    file(b"mounts", render::mounts),
    reporting_directory(b"net", &NET),
    file(b"stat", render::kstat),
    file(b"partitions", render::partitions),
    file(b"uptime", render::uptime),
    file(b"version", render::version),
    sysctl_directory(b"sys", &SYS),
    Entry {
        name: b"sysrq-trigger",
        permissions: 0o200,
        content: Content::File {
            render: nothing,
            write: Some(sysrq_trigger),
        },
    },
];

/// `/proc/sys`: the sysctls this kernel has a source for, and no others. A
/// value is added here when something in the kernel holds it, not before.
static SYS: [Entry<Kernel>; 3] = [
    sysctl_directory(b"fs", &SYS_FS),
    sysctl_directory(b"kernel", &SYS_KERNEL),
    sysctl_directory(b"vm", &SYS_VM),
];

/// `/proc/sys/vm`: the memory policy a program can ask about.
static SYS_VM: [Entry<Kernel>; 1] = [file(b"overcommit_memory", render::overcommit_memory)];

/// `/proc/sys/fs`.
static SYS_FS: [Entry<Kernel>; 3] = [
    file(b"file-max", render::file_max),
    file(b"mount-max", render::mount_max),
    file(b"nr_open", render::nr_open),
];

/// `/proc/sys/kernel`: what `uname` reports, the pid limit, and the ids a
/// namespace shows for one it has no mapping of.
static SYS_KERNEL: [Entry<Kernel>; 8] = [
    sysctl_setting(b"domainname", render::domainname, render::set_domainname),
    sysctl_setting(b"hostname", render::hostname, render::set_hostname),
    file(b"osrelease", render::osrelease),
    file(b"ostype", render::ostype),
    file(b"overflowgid", render::overflowgid),
    file(b"overflowuid", render::overflowuid),
    file(b"pid_max", render::pid_max),
    file(b"version", render::sys_version),
];

/// `/proc/<pid>`.
pub(crate) static PER_PROCESS: [Entry<Process>; 18] = [
    Entry {
        name: b"fd",
        permissions: 0o500,
        content: Content::Descriptors,
    },
    Entry {
        name: b"task",
        permissions: 0o555,
        content: Content::Threads,
    },
    file(b"status", render::status),
    file(b"comm", render::comm),
    file(b"cmdline", render::cmdline),
    file(b"stat", render::stat),
    file(b"maps", render::maps),
    file(b"mounts", render::process_mounts),
    file(b"mountinfo", render::mountinfo),
    Entry {
        name: b"ns",
        permissions: 0o511,
        content: Content::Namespaces,
    },
    file(b"cgroup", render::cgroup),
    Entry {
        name: b"uid_map",
        permissions: 0o644,
        content: Content::IdMap(MapFile::Uid),
    },
    Entry {
        name: b"gid_map",
        permissions: 0o644,
        content: Content::IdMap(MapFile::Gid),
    },
    Entry {
        name: b"setgroups",
        permissions: 0o644,
        content: Content::IdMap(MapFile::Setgroups),
    },
    Entry {
        name: b"oom_score_adj",
        permissions: 0o644,
        content: Content::File {
            render: render::oom_score_adj,
            write: Some(render::set_oom_score_adj),
        },
    },
    Entry {
        name: b"cwd",
        permissions: 0o777,
        content: Content::Link(render::cwd),
    },
    Entry {
        name: b"exe",
        permissions: 0o777,
        content: Content::Link(render::exe),
    },
    Entry {
        name: b"root",
        permissions: 0o777,
        content: Content::Link(render::root),
    },
];

/// `/proc/<pid>/task/<tid>`: what a thread has something true to say in. A
/// thread shares its process's memory, files and ids, so these are the
/// process's own files with the thread's id where Linux puts one.
pub(crate) static PER_THREAD: [Entry<ThreadOf>; 3] = [
    file(b"status", render::thread_status),
    file(b"stat", render::thread_stat),
    file(b"comm", render::thread_comm),
];

/// Levels of [`TOP`]'s tree a [`Tree`] can name: a byte of its `u32` each.
const TREE_LEVELS: u32 = 4;

/// Entries a directory in [`TOP`]'s tree may hold: one byte's worth, less the
/// zero that ends a [`Tree`].
const TREE_WIDTH: usize = 255;

/// Whether `table`, found `depth` levels down, and every directory in it fit
/// what a [`Tree`] can name.
const fn fits(table: &[Entry<Kernel>], depth: u32) -> bool {
    if table.len() > TREE_WIDTH || depth > TREE_LEVELS {
        return false;
    }
    let Some((first, rest)) = table.split_first() else {
        return true;
    };
    let inside = match first.content {
        Content::Directory { entries, .. } => fits(entries, depth + 1),
        _ => true,
    };
    inside && fits(rest, depth)
}

/// Whether a table has no [`Content::Directory`]: a process's directory
/// names its entries by index alone, so it cannot hold one.
const fn flat<T>(table: &[Entry<T>]) -> bool {
    match table.split_first() {
        None => true,
        Some((first, rest)) => !matches!(first.content, Content::Directory { .. }) && flat(rest),
    }
}

const _: () = assert!(
    fits(&TOP, 1),
    "/proc's tree is deeper or wider than a Tree names"
);
const _: () = assert!(
    flat(&PER_PROCESS),
    "a process's directory cannot hold a directory"
);
const _: () = assert!(
    flat(&PER_THREAD),
    "a thread's directory cannot hold a directory"
);

/// Where an entry is in [`TOP`]'s tree: its index at each level plus one, a
/// byte a level from the low end, so `/proc/<TOP[i]>` is `i + 1` and
/// `/proc/sys/kernel` is `(sys + 1) | (kernel + 1) << 8`. Zero is `/proc`
/// itself. No two entries share a value, because no byte of one is zero
/// below its highest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tree(u32);

impl Tree {
    /// `/proc`.
    const ROOT: Tree = Tree(0);

    /// Levels below `/proc`.
    const fn depth(self) -> u32 {
        (u32::BITS - self.0.leading_zeros()).div_ceil(8)
    }

    /// The `index`th entry of this directory, if a `Tree` can name it.
    fn child(self, index: usize) -> Option<Tree> {
        let depth = self.depth();
        if depth >= TREE_LEVELS || index >= TREE_WIDTH {
            return None;
        }
        let byte = u32::try_from(index).ok()?.checked_add(1)?;
        Some(Tree(self.0 | byte << (8 * depth)))
    }

    /// Where this is in a walk of [`TOP`]'s tree that lists a directory
    /// before what it holds: 0 for `/proc`, and below [`nodes`] of it for
    /// every name. Dense, where the value itself is a byte a level, so the
    /// tree's inode numbers take a few hundred numbers rather than 2^32.
    fn ordinal(self) -> u32 {
        let mut table: &'static [Entry<Kernel>] = &TOP;
        let mut ordinal = 0_u32;
        let mut rest = self.0;
        while let Some(index) = (rest & 0xff).checked_sub(1) {
            let index = usize::try_from(index).unwrap_or(usize::MAX);
            let Some((before, Some(entry))) =
                table.split_at_checked(index).map(|(b, a)| (b, a.first()))
            else {
                return ordinal;
            };
            ordinal = ordinal.saturating_add(1).saturating_add(nodes(before));
            rest >>= 8;
            match entry.content {
                Content::Directory { entries, .. } => table = entries,
                _ => return ordinal,
            }
        }
        ordinal
    }

    /// The entries of the directory this names: [`TOP`] for the root.
    fn entries(self) -> Option<&'static [Entry<Kernel>]> {
        if self == Tree::ROOT {
            return Some(&TOP);
        }
        match self.entry()?.0.content {
            Content::Directory { entries, .. } => Some(entries),
            _ => None,
        }
    }

    /// The entry this names, and what a write to it is refused with if it
    /// takes none: the refusal of the directory it is in.
    fn entry(self) -> Option<(&'static Entry<Kernel>, Errno)> {
        let mut table: &'static [Entry<Kernel>] = &TOP;
        let mut refusal = Errno::EINVAL;
        let mut rest = self.0;
        loop {
            let index = usize::try_from((rest & 0xff).checked_sub(1)?).ok()?;
            let entry = table.get(index)?;
            rest >>= 8;
            if rest == 0 {
                return Some((entry, refusal));
            }
            match entry.content {
                Content::Directory {
                    entries,
                    refusal: inner,
                } => {
                    table = entries;
                    refusal = inner;
                }
                _ => return None,
            }
        }
    }
}

/// Where the process directories start in the root's cursor space: above
/// every cursor [`TOP`] could use, so a listing resumed after the last table
/// entry begins at the lowest pid, and a cursor never names a pid and a table
/// entry both.
///
/// Every cursor stays below 2³¹, as Linux's `/proc` offsets do: a cursor is
/// the entry's `d_off`, and a 32-bit program's `readdir` without large-file
/// support fails with `EOVERFLOW` on one that does not fit its `off_t`. At
/// 2³² every process's entry had such a cursor, and the Steam client, which
/// is one, listed `/proc` up to its first pid and found no processes.
const PID_CURSORS: u64 = 1 << 16;

const _: () = assert!(
    FIRST_CURSOR + TOP.len() as u64 <= PID_CURSORS,
    "/proc's own entries run into the process directories' cursors"
);
const _: () = assert!(
    PID_CURSORS + (registry::PID_MAX as u64) < i32::MAX as u64,
    "a process directory's cursor does not fit a 32-bit off_t"
);

/// `statfs`'s `f_type` for procfs.
const PROC_SUPER_MAGIC: u64 = 0x9fa0;

/// A procfs instance.
#[derive(Debug)]
pub(crate) struct Procfs {
    /// `st_dev` for everything in it, and the timestamps.
    shared: Arc<Shared>,
}

/// What every node of one instance shares.
#[derive(Debug)]
struct Shared {
    /// `st_dev`.
    device: u64,
    /// Every timestamp: when it was mounted.
    made: Timespec,
}

impl Procfs {
    /// A procfs, stamped with the time it was made.
    pub(crate) fn new() -> Procfs {
        Procfs {
            shared: Arc::new(Shared {
                device: fs::anonymous_device(),
                made: fs::clock().now(),
            }),
        }
    }
}

impl FileSystem for Procfs {
    fn root(&self) -> Arc<dyn Inode> {
        Arc::new(Node {
            place: Place::Root,
            shared: Arc::clone(&self.shared),
        })
    }

    fn name(&self) -> &'static str {
        "proc"
    }

    fn device(&self) -> u64 {
        self.shared.device
    }

    /// `PROC_SUPER_MAGIC`, from `include/uapi/linux/magic.h`, which is how a
    /// program asks whether `/proc` really is procfs before trusting it.
    fn statfs(&self) -> StatFs {
        StatFs {
            magic: PROC_SUPER_MAGIC,
            block_size: 4096,
            name_max: 255,
            ..StatFs::default()
        }
    }
}

/// Which object a node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// `/proc`.
    Root,
    /// An entry in [`TOP`]'s tree: a top-level name, or one under `/proc/sys`.
    Top(Tree),
    /// `/proc/<pid>`.
    Process(u32),
    /// `PER_PROCESS[index]` of the process.
    Entry(u32, usize),
    /// `/proc/<pid>/fd/<fd>`.
    Descriptor(u32, i32),
    /// `/proc/<pid>/ns/<kind>`.
    Namespace(u32, NamespaceKind),
    /// `/proc/<pid>/task/<tid>`.
    Thread(u32, u32),
    /// `PER_THREAD[index]` of the thread: `/proc/<pid>/task/<tid>/<name>`.
    ThreadEntry(u32, u32, usize),
}

/// Inode numbers per pid: `/proc/<pid>` and everything below it are numbered
/// in the block `[pid * PID_INODES, (pid + 1) * PID_INODES)`, and the top of
/// the tree in pid 0's, which no process has. See the module documentation.
const PID_INODES: u64 = 1 << 17;

/// Where in its pid's block a process's entries start: `/proc/<pid>` itself
/// is the block's first number, and `PER_PROCESS[i]` is `1 + i`.
const ENTRY_INODES: u64 = 1;

/// `/proc/<pid>/ns/mnt`'s number in its pid's block: after the process's
/// entries, before its thread's.
const NAMESPACE_INODES: u64 = 0x80;

/// Where in a tid's block its thread's numbers start: `/proc/<pid>/task/<tid>`
/// is here, and `PER_THREAD[i]` is one above it and on.
const THREAD_INODES: u64 = 0x100;

/// Where in its pid's block the descriptors' links start, and how many
/// distinct numbers they have: every number to the end of the block.
const DESCRIPTOR_INODES: u64 = 0x200;

/// See [`DESCRIPTOR_INODES`].
const DESCRIPTOR_SPAN: u64 = PID_INODES - DESCRIPTOR_INODES;

const _: () = assert!(
    PER_PROCESS.len() as u64 + ENTRY_INODES <= NAMESPACE_INODES,
    "a process's entries run into its namespace links' inode numbers"
);
const _: () = assert!(
    NAMESPACE_INODES < THREAD_INODES,
    "a process's namespace links run into its thread's inode numbers"
);
const _: () = assert!(
    PER_THREAD.len() as u64 + THREAD_INODES < DESCRIPTOR_INODES,
    "a thread's entries run into the descriptors' inode numbers"
);
const _: () = assert!(
    nodes(&TOP) as u64 + 2 <= PID_INODES,
    "/proc's own tree has more names than pid 0's block of inode numbers"
);
const _: () = assert!(
    registry::PID_MAX as u64 * PID_INODES <= 1 << 32,
    "the last pid's inode numbers do not fit 32 bits"
);

/// The names in `table` and everything under it: a [`Tree`]'s number below
/// [`Tree::ordinal`] counts them.
const fn nodes(table: &[Entry<Kernel>]) -> u32 {
    let Some((first, rest)) = table.split_first() else {
        return 0;
    };
    let inside = match first.content {
        Content::Directory { entries, .. } => nodes(entries),
        _ => 0,
    };
    1 + inside + nodes(rest)
}

impl Place {
    /// The inode number; see the module documentation.
    fn ino(self) -> u64 {
        let block = |id: u32| u64::from(id) * PID_INODES;
        match self {
            Place::Root => 1,
            Place::Top(tree) => 1 + u64::from(tree.ordinal()),
            Place::Process(id) => block(id),
            Place::Entry(id, index) => block(id) + ENTRY_INODES + index as u64,
            Place::Descriptor(id, fd) => {
                block(id) + DESCRIPTOR_INODES + u64::from(fd.unsigned_abs()) % DESCRIPTOR_SPAN
            }
            Place::Namespace(id, kind) => block(id) + NAMESPACE_INODES + kind as u64,
            Place::Thread(_, tid) => block(tid) + THREAD_INODES,
            Place::ThreadEntry(_, tid, index) => block(tid) + THREAD_INODES + 1 + index as u64,
        }
    }

    /// What a write to this is refused with, if it is a file that takes none.
    fn refusal(self) -> Errno {
        match self {
            Place::Top(tree) => tree.entry().map_or(Errno::EINVAL, |(_, refusal)| refusal),
            _ => Errno::EINVAL,
        }
    }

    /// The kind and permission bits.
    fn kind(self) -> (FileType, u32) {
        let of = |kind: FileType, permissions: u32| (kind, permissions);
        match self {
            Place::Root | Place::Process(_) | Place::Thread(..) => of(FileType::Directory, 0o555),
            Place::Top(tree) => tree.entry().map_or(of(FileType::Regular, 0), |(entry, _)| {
                of(entry.kind(), entry.permissions)
            }),
            Place::Entry(_, index) => PER_PROCESS
                .get(index)
                .map_or(of(FileType::Regular, 0), |entry| {
                    of(entry.kind(), entry.permissions)
                }),
            Place::Descriptor(..) => of(FileType::Symlink, 0o700),
            Place::Namespace(..) => of(FileType::Symlink, 0o777),
            Place::ThreadEntry(_, _, index) => PER_THREAD
                .get(index)
                .map_or(of(FileType::Regular, 0), |entry| {
                    of(entry.kind(), entry.permissions)
                }),
        }
    }
}

/// A procfs inode.
#[derive(Debug)]
struct Node {
    /// Which object it is.
    place: Place,
    /// The instance's device and timestamps.
    shared: Arc<Shared>,
}

impl Node {
    /// Another node of the same instance.
    fn at(&self, place: Place) -> Arc<dyn Inode> {
        Arc::new(Node {
            place,
            shared: Arc::clone(&self.shared),
        })
    }

    /// Render this node's file, if it is one.
    fn snapshot(&self) -> Result<Option<Snapshot>> {
        let metadata = self.metadata();
        let refusal = self.place.refusal();
        let splices = self.splices();
        match self.place {
            Place::Top(tree) => match tree.entry().map(|(entry, _)| &entry.content) {
                Some(Content::File { render, write }) => Snapshot::new(
                    metadata,
                    render(&())?,
                    write.map(|write| -> Writer { Box::new(move |data| write(&(), data)) }),
                    refusal,
                    splices,
                )
                .map(Some),
                _ => Ok(None),
            },
            Place::Entry(pid, index) => match PER_PROCESS.get(index).map(|entry| &entry.content) {
                Some(Content::IdMap(file)) => {
                    let process = alive(pid)?;
                    let bytes = render::id_map(&process, *file);
                    // Who opened it is judged at each write, beside who
                    // writes (rule U3).
                    let opener = crate::syscall::userns::acting()
                        .map(|opener| opener.with_credentials(|held| held.clone()));
                    let file = *file;
                    let writer: Writer = Box::new(move |data| {
                        render::write_id_map(&process, file, opener.as_ref(), data)
                    });
                    Snapshot::new(metadata, bytes, Some(writer), refusal, splices)
                        .map(|snapshot| Some(snapshot.only_at_start()))
                }
                Some(Content::File { render, write }) => {
                    let process = alive(pid)?;
                    let bytes = render(&process)?;
                    Snapshot::new(
                        metadata,
                        bytes,
                        write
                            .map(|write| -> Writer { Box::new(move |data| write(&process, data)) }),
                        refusal,
                        splices,
                    )
                    .map(Some)
                }
                _ => Ok(None),
            },
            Place::ThreadEntry(pid, tid, index) => {
                match PER_THREAD.get(index).map(|entry| &entry.content) {
                    Some(Content::File { render, write }) => {
                        let of = thread_alive(pid, tid)?;
                        let bytes = render(&of)?;
                        Snapshot::new(
                            metadata,
                            bytes,
                            write.map(|write| -> Writer { Box::new(move |data| write(&of, data)) }),
                            refusal,
                            splices,
                        )
                        .map(Some)
                    }
                    _ => Ok(None),
                }
            }
            _ => Ok(None),
        }
    }

    /// Whether `sendfile` and `splice` may read an open of this file
    /// ([`Inode::splices_out`]), as they may read Linux's where its file has
    /// a `splice_read`. Measured on a 7.0 host: every top-level file and
    /// every value under `/proc/sys` sends but those of `/proc/net`, which
    /// are `EINVAL`; of a process's files only `mounts` and `mountinfo` send,
    /// as Linux's `seq_read_iter` lets them, and of a
    /// thread's none. Linux reads the ones that do not through `seq_read`,
    /// which has no `read_iter` to splice from.
    fn splices(&self) -> bool {
        match self.place {
            Place::Top(tree) => tree
                .entry()
                .is_some_and(|(entry, _)| !NET.iter().any(|net| core::ptr::eq(net, entry))),
            Place::Entry(_, index) => PER_PROCESS
                .get(index)
                .is_some_and(|entry| matches!(entry.name, b"mounts" | b"mountinfo")),
            _ => false,
        }
    }

    /// Whether this node is a file in a table that takes writes.
    fn writable(&self) -> bool {
        match self.place {
            Place::Top(tree) => tree.entry().is_some_and(|(entry, _)| entry.takes_writes()),
            Place::Entry(_, index) => PER_PROCESS.get(index).is_some_and(Entry::takes_writes),
            Place::ThreadEntry(_, _, index) => {
                PER_THREAD.get(index).is_some_and(Entry::takes_writes)
            }
            _ => false,
        }
    }

    /// Whether this node is a process's `task` directory, and whose.
    fn threads_of(&self) -> Option<u32> {
        match self.place {
            Place::Entry(pid, index) => PER_PROCESS
                .get(index)
                .filter(|entry| matches!(entry.content, Content::Threads))
                .map(|_| pid),
            _ => None,
        }
    }

    /// Whether this node is a process's `ns` directory, and whose.
    fn namespaces_of(&self) -> Option<u32> {
        match self.place {
            Place::Entry(pid, index) => PER_PROCESS
                .get(index)
                .filter(|entry| matches!(entry.content, Content::Namespaces))
                .map(|_| pid),
            _ => None,
        }
    }

    /// Whether this node is a process's descriptor directory, and whose.
    fn descriptors_of(&self) -> Option<u32> {
        match self.place {
            Place::Entry(pid, index) => PER_PROCESS
                .get(index)
                .filter(|entry| matches!(entry.content, Content::Descriptors))
                .map(|_| pid),
            _ => None,
        }
    }
}

/// The live process with this pid, or `ENOENT`: a directory a program is
/// holding for a process that has since exited is empty rather than an
/// error on every call.
fn alive(pid: u32) -> Result<Arc<Process>> {
    registry::find(pid).ok_or(Errno::ENOENT)
}

/// Where following `/proc/<pid>/fd/<fd>` leads: the open file's own location,
/// whatever it is, as Linux's `proc_fd_link` jumps to the file's own path.
///
/// For a file a path reached, that is the file, so one renamed or deleted
/// since it was opened opens again. For a pipe it is the end the descriptor
/// holds, which opens as a new end of the same pipe (`fs::pipe`): bash's
/// `<(...)` opens `/dev/fd/63` that way. For a memfd it is the file itself,
/// which opens again as Linux's shmem file does.
///
/// For a socket, and for an eventfd, epoll or other anonymous file, it is the
/// object's detached location on sockfs or `anon_inodefs`. `stat` through the
/// link then describes the object as `fstat` of the descriptor does: a
/// socket's `S_IFSOCK` and its sockfs inode number, which is how `lsof`
/// matches a `/proc/net/tcp` row's inode to the processes holding it. Opening
/// one again is refused with `ENXIO` by [`refuse_reopen`], as Linux's
/// `sock_no_open` and `no_open` refuse it.
fn descriptor_location(pid: u32, fd: i32) -> Option<Result<Location>> {
    let process = match alive(pid) {
        Ok(process) => process,
        Err(errno) => return Some(Err(errno)),
    };
    let file = process.files().lock().get(fd).map(Arc::clone);
    let Ok(file) = file else {
        return Some(Err(Errno::ENOENT));
    };
    Some(Ok(file.location().clone()))
}

/// Where following `/proc/<pid>/ns/<kind>` leads: the namespace as an nsfs
/// file, so that `open` of the link is a descriptor `setns` takes. Refused
/// `EACCES` to a caller who is neither the same person nor root, as Linux
/// refuses it without `ptrace_may_access` (`fs/nsfs.rs`).
fn namespace_location(pid: u32, kind: NamespaceKind) -> Result<Location> {
    let process = alive(pid)?;
    if !fs::nsfs::may_open(&process) {
        return Err(Errno::EACCES);
    }
    let kind = match kind {
        NamespaceKind::Mount => fs::nsfs::Kind::Mount,
        NamespaceKind::User => fs::nsfs::Kind::User,
        NamespaceKind::Uts => fs::nsfs::Kind::Uts,
        NamespaceKind::Ipc => fs::nsfs::Kind::Ipc,
        NamespaceKind::Cgroup => fs::nsfs::Kind::Cgroup,
        // Followed before this is asked, through `net::netns_file`.
        NamespaceKind::Net => return Err(Errno::EINVAL),
    };
    fs::nsfs::location(fs::nsfs::Handle::of(&process, kind))
}

/// The ids of a process's threads that have not begun to end, in order; its
/// own pid alone for a process the kernel made without listing a thread, as
/// `render::thread_count` counts it.
pub(crate) fn thread_ids(process: &Process) -> Vec<u32> {
    let mut ids: Vec<u32> = process
        .threads()
        .iter()
        .map(|thread| thread.tid())
        .collect();
    if ids.is_empty() {
        ids.push(process.pid());
    }
    ids.sort_unstable();
    ids
}

/// Thread `tid` of live process `pid`, or `ENOENT`, as [`alive`] answers for
/// a process.
fn thread_alive(pid: u32, tid: u32) -> Result<ThreadOf> {
    let process = alive(pid)?;
    if !thread_ids(&process).contains(&tid) {
        return Err(Errno::ENOENT);
    }
    Ok(ThreadOf { process, tid })
}

impl Inode for Node {
    fn metadata(&self) -> Metadata {
        let (kind, permissions) = self.place.kind();
        let made = self.shared.made;
        // A process's directory and everything in it belong to the ids the
        // process acts as, as Linux's `task_dump_owner` gives them, so a user
        // may list its own descriptors; the rest of /proc is root's.
        let (uid, gid) = match self.place {
            Place::Process(pid)
            | Place::Entry(pid, _)
            | Place::Descriptor(pid, _)
            | Place::Namespace(pid, _)
            | Place::Thread(pid, _)
            | Place::ThreadEntry(pid, ..) => registry::find(pid).map_or((0, 0), |process| {
                process.with_credentials(|ids| (ids.user.effective, ids.group.effective))
            }),
            Place::Root | Place::Top(_) => (0, 0),
        };
        // A directory's links are two and one per directory in it, as Linux
        // counts them. For `/proc/<pid>/task` that is one per thread, and
        // Chrome counts its threads that way before it will fork: its
        // sandbox helper `CHECK`s the count through `fstatat`, and every
        // child process it started died there while this said two.
        let nlink = match self.place {
            Place::Entry(pid, index)
                if PER_PROCESS
                    .get(index)
                    .is_some_and(|entry| matches!(entry.content, Content::Threads)) =>
            {
                registry::find(pid).map_or(2, |process| {
                    2_u32.saturating_add(u32::try_from(thread_ids(&process).len()).unwrap_or(0))
                })
            }
            _ if kind == FileType::Directory => 2,
            _ => 1,
        };
        Metadata {
            ino: self.place.ino(),
            kind,
            permissions,
            nlink,
            uid,
            gid,
            // Linux reports zero for every generated file, and a program that
            // sized its buffer by `st_size` learns to read to end of file.
            size: 0,
            rdev: 0,
            blocks: 0,
            block_size: 4096,
            atime: made,
            mtime: made,
            ctime: made,
        }
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn caches_lookups(&self) -> bool {
        false
    }

    /// Except the names of [`TOP`]'s tree: `/proc/sys`, `/proc/sysrq-trigger`
    /// and the rest are always there and always the same place, and a mount
    /// point has to be a remembered dentry. bubblewrap as root binds
    /// `/proc/sys` and `/proc/sysrq-trigger` onto themselves and remounts
    /// them read-only (`docs/NAMESPACES.md` §1.4), which a walk can only
    /// cross if it finds the dentry the bind was made on. A process's
    /// directory comes and goes with it, and is never one.
    fn caches_lookup_of(&self, name: &[u8]) -> bool {
        match self.place {
            Place::Root => named_in(Tree::ROOT, name).is_some(),
            Place::Top(tree) => tree.entries().is_some() && named_in(tree, name).is_some(),
            _ => false,
        }
    }

    fn open(&self) -> Result<Option<Arc<dyn Inode>>> {
        Ok(self
            .snapshot()?
            .map(|snapshot| Arc::new(snapshot) as Arc<dyn Inode>))
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        match self.snapshot()? {
            Some(snapshot) => snapshot.read_at(offset, buf),
            None if self.place.kind().0 == FileType::Directory => Err(Errno::EISDIR),
            None => Err(Errno::EINVAL),
        }
    }

    fn write_at(&self, offset: u64, data: &[u8], append: bool) -> Result<(usize, u64)> {
        match self.snapshot()? {
            Some(snapshot) => snapshot.write_at(offset, data, append),
            None => Err(Errno::EINVAL),
        }
    }

    /// Accepted and ignored on a file that takes writes, as Linux does for its
    /// `/proc` files: a shell's `>` opens with `O_TRUNC`, and a generated file
    /// has no length of its own to cut. Refused on everything else, with
    /// `EACCES` under `/proc/sys`, where Linux refuses the open itself.
    fn set_len(&self, _len: u64) -> Result<()> {
        if self.writable() {
            Ok(())
        } else {
            Err(self.place.refusal())
        }
    }

    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn Inode>> {
        match self.place {
            Place::Root => {
                if let Some(child) = named_in(Tree::ROOT, name) {
                    return Ok(self.at(Place::Top(child)));
                }
                let pid = number(name).ok_or(Errno::ENOENT)?;
                let _ = alive(pid)?;
                Ok(self.at(Place::Process(pid)))
            }
            Place::Top(tree) => {
                let _ = tree.entries().ok_or(Errno::ENOTDIR)?;
                let child = named_in(tree, name).ok_or(Errno::ENOENT)?;
                Ok(self.at(Place::Top(child)))
            }
            Place::Process(pid) => {
                let _ = alive(pid)?;
                let index = PER_PROCESS
                    .iter()
                    .position(|entry| entry.name == name)
                    .ok_or(Errno::ENOENT)?;
                Ok(self.at(Place::Entry(pid, index)))
            }
            Place::Thread(pid, tid) => {
                let _ = thread_alive(pid, tid)?;
                let index = PER_THREAD
                    .iter()
                    .position(|entry| entry.name == name)
                    .ok_or(Errno::ENOENT)?;
                Ok(self.at(Place::ThreadEntry(pid, tid, index)))
            }
            _ if self.namespaces_of().is_some() => {
                let pid = self.namespaces_of().ok_or(Errno::ENOTDIR)?;
                let _ = alive(pid)?;
                let (kind, _) = NamespaceKind::ALL
                    .iter()
                    .find(|(_, known)| *known == name)
                    .ok_or(Errno::ENOENT)?;
                Ok(self.at(Place::Namespace(pid, *kind)))
            }
            _ if self.threads_of().is_some() => {
                let pid = self.threads_of().ok_or(Errno::ENOTDIR)?;
                let tid = number(name).ok_or(Errno::ENOENT)?;
                let _ = thread_alive(pid, tid)?;
                Ok(self.at(Place::Thread(pid, tid)))
            }
            _ => {
                let pid = self.descriptors_of().ok_or(Errno::ENOTDIR)?;
                let fd = descriptor_number(name).ok_or(Errno::ENOENT)?;
                let open = alive(pid)?.files().lock().get(fd).is_ok();
                if !open {
                    return Err(Errno::ENOENT);
                }
                Ok(self.at(Place::Descriptor(pid, fd)))
            }
        }
    }

    fn read_dir(&self, cursor: u64, emit: &mut dyn FnMut(DirEntry<'_>) -> bool) -> Result<()> {
        match self.place {
            Place::Root => list_root(cursor, emit),
            Place::Top(tree) => {
                let entries = tree.entries().ok_or(Errno::ENOTDIR)?;
                let ino = |index| tree.child(index).map_or(0, |child| Place::Top(child).ino());
                let _ = list_table(entries, cursor, ino, emit);
                Ok(())
            }
            Place::Process(pid) => {
                let _ = alive(pid)?;
                let ino = |index| Place::Entry(pid, index).ino();
                let _ = list_table(&PER_PROCESS, cursor, ino, emit);
                Ok(())
            }
            Place::Thread(pid, tid) => {
                let _ = thread_alive(pid, tid)?;
                let ino = |index| Place::ThreadEntry(pid, tid, index).ino();
                let _ = list_table(&PER_THREAD, cursor, ino, emit);
                Ok(())
            }
            _ if self.namespaces_of().is_some() => {
                let pid = self.namespaces_of().ok_or(Errno::ENOTDIR)?;
                let _ = alive(pid)?;
                for (index, (kind, name)) in NamespaceKind::ALL.iter().enumerate() {
                    let position = FIRST_CURSOR.saturating_add(index as u64);
                    if cursor <= position
                        && !emit(DirEntry {
                            ino: Place::Namespace(pid, *kind).ino(),
                            kind: FileType::Symlink,
                            name,
                            next: position.saturating_add(1),
                        })
                    {
                        break;
                    }
                }
                Ok(())
            }
            _ if self.threads_of().is_some() => {
                let pid = self.threads_of().ok_or(Errno::ENOTDIR)?;
                list_threads(pid, cursor, emit)
            }
            _ => {
                let pid = self.descriptors_of().ok_or(Errno::ENOTDIR)?;
                list_descriptors(pid, cursor, emit)
            }
        }
    }

    fn read_link(&self) -> Result<Vec<u8>> {
        match self.place {
            Place::Top(tree) => match tree.entry().map(|(entry, _)| &entry.content) {
                Some(Content::Link(target)) => target(&()),
                _ => Err(Errno::EINVAL),
            },
            Place::Entry(pid, index) => match PER_PROCESS.get(index).map(|entry| &entry.content) {
                Some(Content::Link(target)) => target(&*alive(pid)?),
                _ => Err(Errno::EINVAL),
            },
            Place::Descriptor(pid, fd) => render::descriptor(&*alive(pid)?, fd),
            Place::Namespace(pid, NamespaceKind::Mount) => render::mount_namespace(&*alive(pid)?),
            Place::Namespace(pid, NamespaceKind::User) => render::user_namespace(&*alive(pid)?),
            Place::Namespace(pid, NamespaceKind::Uts) => render::uts_namespace(&*alive(pid)?),
            Place::Namespace(pid, NamespaceKind::Ipc) => render::ipc_namespace(&*alive(pid)?),
            Place::Namespace(pid, NamespaceKind::Cgroup) => render::cgroup_namespace(&*alive(pid)?),
            Place::Namespace(pid, NamespaceKind::Net) => render::net_namespace(&*alive(pid)?),
            _ => Err(Errno::EINVAL),
        }
    }

    /// `/proc/<pid>/exe` is a magic link: followed, it is the file the
    /// process's program was loaded from, not whatever its text names now.
    /// A process whose program came from no file -- the kernel's own checks,
    /// a built-in init -- has no file to lead to, and its link is followed as
    /// the text it reads as.
    ///
    /// `/proc/<pid>/fd/<n>` is one too, as Linux's `proc_fd_link` makes it:
    /// followed, it is where the open file is, whatever name reaches it now.
    /// See [`descriptor_location`].
    fn link_location(&self) -> Option<Result<Location>> {
        if let Place::Descriptor(pid, fd) = self.place {
            return descriptor_location(pid, fd);
        }
        // Following `/proc/<pid>/ns/net` opens the namespace as a file.
        if let Place::Namespace(pid, NamespaceKind::Net) = self.place {
            return Some(alive(pid).and_then(|process| {
                if !crate::net::netns_file::may_open(&process) {
                    return Err(Errno::EACCES);
                }
                crate::net::netns_file::location(process.net_ns())
            }));
        }
        if let Place::Namespace(pid, kind) = self.place {
            return Some(namespace_location(pid, kind));
        }
        let Place::Entry(pid, index) = self.place else {
            return None;
        };
        if PER_PROCESS.get(index)?.name != b"exe" {
            return None;
        }
        match alive(pid) {
            Ok(process) => process.exe_location().map(Ok),
            Err(errno) => Some(Err(errno)),
        }
    }
}

/// The entry called `name` in the directory `tree` names, if there is one
/// and a [`Tree`] can name it.
fn named_in(tree: Tree, name: &[u8]) -> Option<Tree> {
    let index = tree
        .entries()?
        .iter()
        .position(|entry| entry.name == name)?;
    tree.child(index)
}

/// Report a table's entries from `cursor` on, each with the inode number
/// `ino` gives its index.
fn list_table<T>(
    table: &[Entry<T>],
    cursor: u64,
    ino: impl Fn(usize) -> u64,
    emit: &mut dyn FnMut(DirEntry<'_>) -> bool,
) -> bool {
    let first = usize::try_from(cursor.saturating_sub(FIRST_CURSOR)).unwrap_or(usize::MAX);
    for (index, entry) in table.iter().enumerate().skip(first) {
        let accepted = emit(DirEntry {
            ino: ino(index),
            kind: entry.kind(),
            name: entry.name,
            next: FIRST_CURSOR.saturating_add(index as u64 + 1),
        });
        if !accepted {
            return false;
        }
    }
    true
}

/// `/proc`: the table, then a directory per live process in pid order.
fn list_root(cursor: u64, emit: &mut dyn FnMut(DirEntry<'_>) -> bool) -> Result<()> {
    let ino = |index| {
        Tree::ROOT
            .child(index)
            .map_or(0, |child| Place::Top(child).ino())
    };
    if cursor < PID_CURSORS && !list_table(&TOP, cursor, ino, emit) {
        return Ok(());
    }
    let from = cursor.saturating_sub(PID_CURSORS);
    let mut digits = [0_u8; 20];
    for process in registry::live()? {
        let pid = u64::from(process.pid());
        if pid < from {
            continue;
        }
        let accepted = emit(DirEntry {
            ino: Place::Process(process.pid()).ino(),
            kind: FileType::Directory,
            name: decimal(pid, &mut digits),
            next: PID_CURSORS.saturating_add(pid).saturating_add(1),
        });
        if !accepted {
            break;
        }
    }
    Ok(())
}

/// `/proc/<pid>/task`: a directory per thread, in id order.
fn list_threads(pid: u32, cursor: u64, emit: &mut dyn FnMut(DirEntry<'_>) -> bool) -> Result<()> {
    let process = alive(pid)?;
    let from = cursor.saturating_sub(FIRST_CURSOR);
    let mut digits = [0_u8; 20];
    for tid in thread_ids(&process) {
        let at = u64::from(tid);
        if at < from {
            continue;
        }
        let accepted = emit(DirEntry {
            ino: Place::Thread(pid, tid).ino(),
            kind: FileType::Directory,
            name: decimal(at, &mut digits),
            next: FIRST_CURSOR.saturating_add(at).saturating_add(1),
        });
        if !accepted {
            break;
        }
    }
    Ok(())
}

/// `/proc/<pid>/fd`: a link per open descriptor, in descriptor order.
fn list_descriptors(
    pid: u32,
    cursor: u64,
    emit: &mut dyn FnMut(DirEntry<'_>) -> bool,
) -> Result<()> {
    let process = alive(pid)?;
    let from = cursor.saturating_sub(FIRST_CURSOR);
    // Collected first: `emit` copies into a buffer the caller owns, and the
    // table's lock is no place to do that from.
    let open: Vec<i32> = process.files().lock().iter().map(|(fd, _)| fd).collect();
    let mut digits = [0_u8; 20];
    for fd in open {
        let at = u64::from(fd.unsigned_abs());
        if at < from {
            continue;
        }
        let accepted = emit(DirEntry {
            ino: Place::Descriptor(pid, fd).ino(),
            kind: FileType::Symlink,
            name: decimal(at, &mut digits),
            next: FIRST_CURSOR.saturating_add(at).saturating_add(1),
        });
        if !accepted {
            break;
        }
    }
    Ok(())
}

/// A name that is a number as Linux writes one: decimal digits, no sign, no
/// leading zero, and not zero itself. `/proc/042` is not `/proc/42`.
fn number(name: &[u8]) -> Option<u32> {
    if name.first() == Some(&b'0') {
        return None;
    }
    let text = core::str::from_utf8(name).ok()?;
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// A descriptor's name: as [`number`], except that `0` is one. Every process
/// has a descriptor 0, and a listing of `/proc/<pid>/fd` says so; a name the
/// listing reports that a walk then refuses is what `ls -R` prints an error
/// for.
fn descriptor_number(name: &[u8]) -> Option<i32> {
    if name == b"0" {
        return Some(0);
    }
    number(name).and_then(|fd| i32::try_from(fd).ok())
}

/// `value` in decimal, in `digits`.
fn decimal(value: u64, digits: &mut [u8; 20]) -> &[u8] {
    let mut at = digits.len();
    let mut rest = value;
    loop {
        at -= 1;
        if let Some(slot) = digits.get_mut(at) {
            *slot = b'0' + (rest % 10) as u8;
        }
        rest /= 10;
        if rest == 0 || at == 0 {
            break;
        }
    }
    digits.get(at..).unwrap_or_default()
}

/// A write into a file rendered at open.
pub(crate) type Writer = Box<dyn Fn(&[u8]) -> Result<usize> + Send + Sync>;

/// A generated file's contents as they are now, answering reads from them
/// and writes through `write`, or with `refusal` when there is none: how a
/// pseudo-filesystem other than this one -- cgroupfs -- answers
/// [`Inode::open`] the way procfs does.
pub(crate) fn snapshot(
    metadata: Metadata,
    bytes: Vec<u8>,
    write: Option<Writer>,
    refusal: Errno,
) -> Result<Arc<dyn Inode>> {
    // kernfs gives every sysfs and cgroupfs file a `splice_read`.
    let snapshot = Snapshot::new(metadata, bytes, write, refusal, true)?;
    Ok(Arc::new(snapshot))
}

/// One open of a generated file: its contents as they were at open.
struct Snapshot {
    /// What `stat` would report, less the size, which is the snapshot's.
    metadata: Metadata,
    /// The contents.
    bytes: Vec<u8>,
    /// Where writes go, for a file that takes them.
    write: Option<Writer>,
    /// What a write is refused with, for a file that takes none.
    refusal: Errno,
    /// Whether `sendfile` and `splice` may read it, as `Node::splices` says.
    splices: bool,
    /// Whether a write not at the start is refused `EINVAL`, as Linux refuses
    /// one to a map file.
    at_start_only: bool,
    /// Its heap, the contents included, charged to the job that opened it
    /// (F-37): a process's `maps` grows with its regions, and every open
    /// holds a copy.
    _charge: Charge,
}

impl Snapshot {
    /// Contents rendered at open, charged to the running task's job.
    ///
    /// # Errors
    ///
    /// `ENOMEM` past the job's memory limit.
    fn new(
        metadata: Metadata,
        bytes: Vec<u8>,
        write: Option<Writer>,
        refusal: Errno,
        splices: bool,
    ) -> Result<Snapshot> {
        let charge = Charge::bytes(
            arc_footprint::<Snapshot>().saturating_add(buffer_footprint::<u8>(bytes.capacity())),
        )
        .map_err(|_| Errno::ENOMEM)?;
        Ok(Snapshot {
            metadata,
            bytes,
            write,
            refusal,
            splices,
            at_start_only: false,
            _charge: charge,
        })
    }

    /// This snapshot, taking writes at its start only.
    fn only_at_start(mut self) -> Snapshot {
        self.at_start_only = true;
        self
    }
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("procfs::Snapshot")
            .field("ino", &self.metadata.ino)
            .field("len", &self.bytes.len())
            .field("writable", &self.write.is_some())
            .finish()
    }
}

impl Inode for Snapshot {
    /// The generated file's metadata with the snapshot's length, so that
    /// `SEEK_END` on an open file finds the end of what it can read.
    fn metadata(&self) -> Metadata {
        Metadata {
            size: self.bytes.len() as u64,
            ..self.metadata
        }
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let rest = self.bytes.get(start..).unwrap_or_default();
        let count = rest.len().min(buf.len());
        let (Some(to), Some(from)) = (buf.get_mut(..count), rest.get(..count)) else {
            return Ok(0);
        };
        to.copy_from_slice(from);
        Ok(count)
    }

    fn write_at(&self, offset: u64, data: &[u8], _append: bool) -> Result<(usize, u64)> {
        let write = self.write.as_ref().ok_or(self.refusal)?;
        if self.at_start_only && offset != 0 {
            return Err(Errno::EINVAL);
        }
        let count = write(data)?;
        Ok((count, offset.saturating_add(count as u64)))
    }

    fn splices_out(&self) -> bool {
        self.splices
    }
}

/// An open file `openat` made, refused with `EACCES` if it is a value under
/// `/proc/sys` that takes no writes and was opened for writing.
///
/// Linux refuses that open in `proc_sys_permission`, whether the access mode
/// is `O_WRONLY` or `O_RDWR`. [`Inode::open`] is not told the access mode, so
/// the refusal is made here, where `openat` already looks at the open file it
/// made; an open with `O_TRUNC` has already been refused by
/// [`Inode::set_len`] with the same errno. Every other file is returned as it
/// is.
pub(crate) fn refuse_write_open(file: Arc<OpenFile>) -> Result<Arc<OpenFile>> {
    if !file.writable() {
        return Ok(file);
    }
    let Ok(node) = Arc::clone(file.inode()).into_any().downcast::<Node>() else {
        return Ok(file);
    };
    let read_only_value = node.place.kind().0 == FileType::Regular
        && node.place.refusal() == Errno::EACCES
        && !node.writable();
    if read_only_value {
        return Err(Errno::EACCES);
    }
    Ok(file)
}

/// An open file `openat` made, refused with `ENXIO` if it is a socket or an
/// anonymous file -- an eventfd, epoll, timerfd, signalfd, inotify, pidfd --
/// which only a walk through `/proc/<pid>/fd/<n>` reaches by a path.
///
/// Linux gives those objects no second opening: a socket's `open` is
/// `sock_no_open` and an anonymous inode's is `no_open`, both `ENXIO`. An
/// `O_PATH` handle opens nothing, so it is allowed, as on Linux. The open file
/// the namespace made is dropped unused: no socket or anonymous inode has an
/// [`Inode::open`] of its own, so making it did nothing to the object.
pub(crate) fn refuse_reopen(file: Arc<OpenFile>) -> Result<Arc<OpenFile>> {
    if !file.is_path() && (fs::socket::holds(&file) || fs::anon::holds(&file)) {
        return Err(Errno::ENXIO);
    }
    Ok(file)
}

/// Mount a procfs on `/proc`, making the directory if the archive had none.
pub(crate) fn mount() -> Result<()> {
    let ns = fs::namespace();
    let ctx = ns.context();
    match ns.mkdir(&ctx, None, b"/proc", 0o555) {
        Ok(()) | Err(Errno::EEXIST) => {}
        Err(errno) => return Err(errno),
    }
    let at = ns.resolve(&ctx, None, b"/proc", true)?;
    ns.mount(Arc::new(Procfs::new()), &at).map(drop)
}
