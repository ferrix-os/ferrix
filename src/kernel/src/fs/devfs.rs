//! devfs: the device nodes every program expects to find in `/dev`.
//!
//! A fixed table, not a directory nodes can be created in, and after it the
//! disks drivers have registered. What is in `/dev` is the kernel's statement
//! of which devices exist: the three memory devices a C library and a shell
//! reach for, the two random devices, the console under both of its names —
//! and, as block drivers come and go, their disks. Here the node *is* the
//! device, which is why this is a filesystem of its own rather than a tmpfs
//! with nodes unpacked into it.
//!
//! # Nodes elsewhere
//!
//! A character device node made by `mknod` in a tmpfs, or unpacked from the
//! initramfs, names a device by number. [`attach_device`] opens it as the
//! device this table gives that number, exactly as the devfs node would open:
//! `/tmp/null` made as `c 1 3` is `/dev/null` to every read and write, and its
//! own inode to `stat`. A number the table does not have is `ENXIO` on open,
//! which is Linux's answer for a character number no driver registered; so is
//! every block device node, including the ones this filesystem lists.
//!
//! # Numbers
//!
//! Linux's, from `Documentation/admin-guide/devices.txt`: major 1 is the
//! memory devices — `null` 3, `zero` 5, `full` 7, `random` 8, `urandom` 9 —
//! and major 5 the alternate TTY devices, `tty` 0 and `console` 1. Programs
//! compare them: `ttyname` walks `/dev` matching `st_rdev`, and a careful
//! daemon checks that what it was handed as `/dev/null` is 1:3 before writing
//! to it. A node with the right name and the wrong number is a wrong answer.
//!
//! # Block devices
//!
//! Stage 11 mounts a btrfs volume from a disk a ring-3 driver serves, and
//! `mount(2)` names that disk by its node here. The node does not open —
//! `ENXIO`, as Linux answers for a block number with no driver behind it — so
//! a mount resolves the node's `st_rdev` with [`block_device`] instead, and
//! reads through the [`BlockDevice`] that returns. The kernel's driver glue
//! calls [`register_block`] when a driver says hello, with the name devmgr
//! chose and numbers derived from it; this registry numbers nothing itself.
//! Block numbers are a namespace of their own: a disk at 1:3 is not
//! `/dev/null`.
//!
//! The registration is a value, and dropping it takes the node, the name and
//! the number away again. A mount that took the device's `Arc` keeps the
//! device, whose reads fail with `EIO` once it has gone away.
//!
//! Three properties a program can see, and how they are kept:
//!
//! - **Listings stay stable across a registration.** The first
//!   [`DEVICES`]`.len()` cursors after the VFS's dot entries are the static
//!   nodes by index, and no registration moves them. The cursors past those
//!   name disks by their registration serial — a number every registration
//!   takes one higher than the last — not by where the disk is in the list.
//!   Disks are listed in serial order, so a listing resumed at a cursor lists
//!   exactly the disks with a serial at least that high that are registered
//!   then: a registration or a drop between two `getdents64` calls neither
//!   repeats nor skips a static node, nor any disk registered throughout.
//! - **Inode numbers are never reused.** A disk's is 2³² plus its serial, far
//!   above the static nodes' index-plus-two. A serial is never handed out
//!   twice, so a name looked up, or opened with `O_PATH`, before its disk went
//!   away can never share a number with a disk registered after — `find` and
//!   `du` take two names with one number to be one file.
//! - **A name registered after a miss is found.** Names here now come and go
//!   behind the VFS's back, so the directory answers
//!   [`Inode::caches_lookups`] with `false`, as procfs does, and every walk
//!   asks the table. Invalidating instead would need the registry to reach
//!   every devtmpfs mount's dentries, which `src/lib/fs/vfs` offers no way to do; and
//!   what not caching costs here is small: a scan of seven names and a short
//!   locked list. The one thing it would have cost is a mount point inside
//!   `/dev`, which `/dev/shm` needs; `Inode::caches_lookup_of` buys that one
//!   name back, and only that one, because `shm` is the only name here that
//!   never comes or goes.
//!
//! # `/dev/shm`
//!
//! A directory, not a device, and the only name here that is not one. POSIX
//! shared memory and named semaphores live in it: ferrousli's `sem_open` makes
//! a file there and maps it shared, and a program whose `memfd_create` is not
//! available falls back to it. The node this filesystem carries is an empty
//! directory with nothing behind it; [`crate::fs::init`] mounts a tmpfs over
//! it at boot, which is where the files actually are, exactly as a Linux init
//! script mounts one there. A devtmpfs mounted somewhere else by a program has
//! the node and not the tmpfs, which is what Linux gives that program too.
//!
//! # Why it says it is devtmpfs
//!
//! `/proc/mounts` and `/proc/filesystems` name a filesystem by the word
//! `mount -t` takes, and programs decide by that word. An init script greps
//! `/proc/filesystems` for `devtmpfs` before it mounts `/dev`, and a service
//! manager looks in `/proc/mounts` for a `devtmpfs` on `/dev` before deciding
//! whether to mount one. `mount -t devtmpfs` is how a program asks for this
//! filesystem, so the type it reads back is the type it asked for, and the
//! boot's own `/dev` reads the same as one a script mounted. Linux has had no
//! type called `devfs` since 2.6.18, so that name would be a word no program
//! looks for and no `mount` takes.
//!
//! The difference from Linux's is that nothing can be created in it: Linux's
//! `devtmpfs` is a tmpfs the kernel populates, and `mknod` in it works. Here
//! the table is the filesystem, so a program that tries is refused, which it
//! can see, rather than given a node no device answers.
//!
//! # Streams
//!
//! Every node reports [`Inode::is_stream`], so the VFS passes no offsets and
//! reads and writes take no position. What `lseek` answers is Linux's, device
//! by device, because programs seek these and die if refused: busybox
//! `dd of=/dev/null seek=1` seeks its output before it writes.
//!
//! - The five memory devices -- `null`, `zero`, `full`, `random`, `urandom`
//!   -- report [`Inode::ignores_position`]. `lseek` answers 0 for every
//!   offset and whence, as Linux's `null_lseek` (`null`, `zero`, `full`) and
//!   `noop_llseek` over a position no read moves (`random`, `urandom`) do,
//!   and `pread64` and `pwrite64` are a plain read and write. Measured on a
//!   Linux 7.0 host: `SEEK_SET` 100, `SEEK_CUR` -5, `SEEK_END` 10, a
//!   negative `SEEK_SET`, `SEEK_DATA` and `SEEK_HOLE` all answer 0, and a
//!   `pread` at 1000 reads. They also [fill reads](Inode::fills_reads): a
//!   `read` of 65536 from `zero` or `urandom` is 65536 there, and `readv`
//!   of `zero` into two segments of eight is 16.
//! - The terminals -- `tty`, `console`, `ptmx` and what it opens, the
//!   `pts` slaves -- are `ESPIPE`, as they are on Linux.
//! - The DRM nodes, `dri/card<N>` and `dri/renderD<N>`, are `ESPIPE` to
//!   `lseek` but take `pread64` as a `read`, as Linux's do on this host:
//!   what their open files answer, and why, is in `crate::interfaces::display::drm`.

pub(crate) mod check;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use ferrix_vfs::initramfs::makedev;
use ferrix_vfs::{
    DirEntry, Errno, FIRST_CURSOR, FileSystem, FileType, Inode, Metadata, OpenFile, Result,
    Timespec,
};

use crate::fs;
use crate::fs::block::BlockDevice;
use crate::fs::console::console_inode;
use crate::fs::pty;
use crate::sync::SpinLock;
use crate::syscall::time;

/// What a node does with reads and writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Behaviour {
    /// Reads are at end of file; writes are swallowed.
    Null,
    /// Reads give zeros; writes are swallowed.
    Zero,
    /// Reads give zeros; writes find the device full.
    Full,
    /// Reads give the generator's bytes; writes are accepted and discarded,
    /// where Linux would mix them into a pool this kernel does not have.
    Random,
    /// A node that opens the console: `/dev/tty`, which has a number of its
    /// own and so cannot be the console's inode.
    Console,
    /// `/dev/ptmx`: opening it makes a pseudoterminal pair and gives back
    /// the master, which is a different object each time.
    Ptmx,
    /// The console's own inode: `/dev/console`. Never a devfs node — the name
    /// resolves to [`console_inode`] itself.
    ConsoleItself,
}

/// One node.
#[derive(Debug)]
struct Device {
    /// Its name in `/dev`.
    name: &'static [u8],
    /// Its device number's major half.
    major: u32,
    /// And minor half.
    minor: u32,
    /// Its permission bits.
    permissions: u32,
    /// What it does.
    behaviour: Behaviour,
}

/// Every character node in `/dev`, in the order a listing reports it.
static DEVICES: [Device; 8] = [
    Device {
        name: b"null",
        major: 1,
        minor: 3,
        permissions: 0o666,
        behaviour: Behaviour::Null,
    },
    Device {
        name: b"zero",
        major: 1,
        minor: 5,
        permissions: 0o666,
        behaviour: Behaviour::Zero,
    },
    Device {
        name: b"full",
        major: 1,
        minor: 7,
        permissions: 0o666,
        behaviour: Behaviour::Full,
    },
    Device {
        name: b"random",
        major: 1,
        minor: 8,
        permissions: 0o666,
        behaviour: Behaviour::Random,
    },
    Device {
        name: b"urandom",
        major: 1,
        minor: 9,
        permissions: 0o666,
        behaviour: Behaviour::Random,
    },
    Device {
        name: b"tty",
        major: 5,
        minor: 0,
        permissions: 0o666,
        behaviour: Behaviour::Console,
    },
    Device {
        name: b"console",
        major: 5,
        minor: 1,
        permissions: 0o600,
        behaviour: Behaviour::ConsoleItself,
    },
    Device {
        name: b"ptmx",
        major: 5,
        minor: 2,
        permissions: 0o666,
        behaviour: Behaviour::Ptmx,
    },
];

/// The root directory's inode number. A device's is its index plus two.
const ROOT_INO: u64 = 1;

/// The longest name a disk may be registered under.
const BLOCK_NAME_MAX: usize = 32;

/// Every block node's permission bits: read and write for root and the disk
/// group, as a Linux `/dev` gives its disks.
const BLOCK_PERMISSIONS: u32 = 0o660;

/// A disk's inode number is this plus its registration serial.
const BLOCK_INO_BASE: u64 = 1 << 32;

/// The directory cursor that lists the disks from serial 0: the one after
/// the last static node's.
const BLOCK_CURSOR_BASE: u64 = FIRST_CURSOR + DEVICES.len() as u64;

/// `/dev/dri`'s inode number.
const DRI_INO: u64 = 1 << 36;

/// The root's cursor for `/dev/dri`, after every disk's.
const DRI_CURSOR: u64 = 1 << 48;

/// The name of the directory cards are in.
const DRI: &[u8] = b"dri";

/// `/dev/input`'s inode number.
const INPUT_INO: u64 = 1 << 37;

/// The root's cursor for `/dev/input`, after `/dev/dri`'s.
const INPUT_CURSOR: u64 = (1 << 48) + 2;

/// The name of the directory input devices are in.
const INPUT: &[u8] = b"input";

/// `/dev/pts`'s inode number.
const PTS_INO: u64 = 1 << 38;

/// The root's cursor for `/dev/pts`, after `/dev/input`'s.
const PTS_CURSOR: u64 = (1 << 48) + 4;

/// The name of the directory pseudoterminal slaves are in.
const PTS: &[u8] = b"pts";

/// `/dev/snd`'s inode number.
const SND_INO: u64 = 1 << 42;

/// The root's cursor for `/dev/snd`, after `/dev/shm`'s.
const SND_CURSOR: u64 = (1 << 48) + 8;

/// The name of the directory sound cards' nodes are in.
const SND: &[u8] = b"snd";

/// `/dev/shm`'s inode number.
const SHM_INO: u64 = 1 << 39;

/// The root's cursor for `/dev/shm`, after `/dev/pts`'s.
const SHM_CURSOR: u64 = (1 << 48) + 6;

/// The name of the directory POSIX shared memory and named semaphores live
/// in.
const SHM: &[u8] = b"shm";

/// The symbolic links every Linux `/dev` has, and where each leads: the
/// caller's descriptor table in procfs. Userspace makes them on Linux --
/// udev, systemd, an initramfs's `init` -- after mounting devtmpfs; this
/// `/dev` cannot be written, so it carries them, as it carries `shm`. Bash
/// opens `/dev/fd/63` for a process substitution, `<(...)` and `>(...)`,
/// and a script writes to `/dev/stderr` by name.
static LINKS: [(&[u8], &[u8]); 4] = [
    (b"fd", b"/proc/self/fd"),
    (b"stdin", b"/proc/self/fd/0"),
    (b"stdout", b"/proc/self/fd/1"),
    (b"stderr", b"/proc/self/fd/2"),
];

/// `LINKS[index]`'s inode number is this plus its index.
const LINK_INO_BASE: u64 = 1 << 45;

/// The root's cursor for `LINKS[0]`, after `/dev/snd`'s; the rest follow it.
const LINK_CURSOR: u64 = (1 << 48) + 10;

/// A devfs instance.
#[derive(Debug)]
pub(crate) struct Devfs {
    /// `st_dev` for everything in it.
    device: u64,
    /// The directory.
    root: Arc<Node>,
}

impl Devfs {
    /// A devfs, stamped with the time it was made.
    pub(crate) fn new() -> Devfs {
        Devfs {
            device: fs::anonymous_device(),
            root: Arc::new(Node {
                place: Place::Root,
                made: fs::clock().now(),
            }),
        }
    }
}

impl FileSystem for Devfs {
    fn root(&self) -> Arc<dyn Inode> {
        Arc::clone(&self.root) as Arc<dyn Inode>
    }

    fn name(&self) -> &'static str {
        "devtmpfs"
    }

    fn device(&self) -> u64 {
        self.device
    }
}

// -- The block registry ---------------------------------------------------------

/// One registered disk.
#[derive(Debug, Clone)]
struct Disk {
    /// Which registration it is: one higher than the one before, never reused.
    serial: u64,
    /// Its name in `/dev`, the first `name_len` bytes of this.
    name: [u8; BLOCK_NAME_MAX],
    /// How long the name is.
    name_len: usize,
    /// Its device number's major half.
    major: u32,
    /// And minor half.
    minor: u32,
    /// The disk.
    device: Arc<dyn BlockDevice>,
    /// Where it came from, for sysfs.
    origin: Origin,
}

/// Where a disk came from: the device node its driver serves, and what the
/// driver said the disk is called. sysfs shows the disk inside the node's
/// directory and its serial in `serial`; nothing else here reads either.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Origin {
    /// The device node, by its index in `device::devices()`; `None` for a
    /// disk no node backs, which sysfs puts under `devices/virtual`.
    pub(crate) node: Option<usize>,
    /// The serial number the driver reported, NUL-padded; all zeros for
    /// none.
    pub(crate) serial: [u8; 20],
}

/// A registered disk, copied out for sysfs.
#[derive(Debug, Clone)]
pub(crate) struct DiskInfo {
    /// Its registration serial: never reused, so a name that goes and comes
    /// back is another disk.
    pub(crate) registration: u64,
    /// Its name in `/dev`.
    pub(crate) name: Vec<u8>,
    /// Its device number's major half.
    pub(crate) major: u32,
    /// And minor half.
    pub(crate) minor: u32,
    /// The disk.
    pub(crate) device: Arc<dyn BlockDevice>,
    /// Where it came from.
    pub(crate) origin: Origin,
}

/// Every registered disk, in registration order, copied out so that the
/// caller may ask each device anything with no lock held.
pub(crate) fn disks() -> Vec<DiskInfo> {
    disks_from(0)
        .into_iter()
        .map(|disk| DiskInfo {
            registration: disk.serial,
            name: disk.name().to_vec(),
            major: disk.major,
            minor: disk.minor,
            origin: disk.origin,
            device: disk.device,
        })
        .collect()
}

/// A static node of the table: its name, numbers and permission bits, for
/// sysfs's `devices/virtual` and `/sys/dev/char`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CharNode {
    /// Its name in `/dev`.
    pub(crate) name: &'static [u8],
    /// Its number.
    pub(crate) major: u32,
    /// And minor half.
    pub(crate) minor: u32,
    /// Its permission bits.
    pub(crate) permissions: u32,
}

/// The table's nodes, in the table's order.
pub(crate) fn char_nodes() -> impl Iterator<Item = CharNode> {
    DEVICES.iter().map(|device| CharNode {
        name: device.name,
        major: device.major,
        minor: device.minor,
        permissions: device.permissions,
    })
}

impl Disk {
    /// Its name in `/dev`.
    fn name(&self) -> &[u8] {
        self.name.get(..self.name_len).unwrap_or_default()
    }

    /// What a node for it reports: its serial and number, none of which asks
    /// the device, since a block node's `stat` carries no size.
    fn node(&self) -> BlockNode {
        BlockNode {
            serial: self.serial,
            major: self.major,
            minor: self.minor,
        }
    }
}

/// The disks, in registration order, and the serial the next one takes.
#[derive(Debug)]
struct Registry {
    /// The serial the next registration is given.
    next_serial: u64,
    /// Every registered disk, serials ascending.
    disks: Vec<Disk>,
}

/// Every disk `/dev` lists, for every devtmpfs mounted anywhere.
static BLOCKS: SpinLock<Registry> = SpinLock::new(Registry {
    next_serial: 0,
    disks: Vec::new(),
});

/// Why [`register_block`] refused, in the order it checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockRefused {
    /// The name is not 1 to 32 bytes of lowercase letters and digits.
    InvalidName,
    /// A node in `/dev` already has the name: a disk, or a character device.
    NameInUse,
    /// A registered disk already has the number.
    NumberInUse,
}

/// A disk's place in `/dev`, held for as long as the node should exist.
///
/// Not `Clone`, so exactly one drop unpublishes.
#[derive(Debug)]
#[must_use = "dropping the registration takes the node out of /dev at once"]
pub(crate) struct BlockRegistration {
    /// The number registered, major half.
    major: u32,
    /// And minor half.
    minor: u32,
    /// Which registration this is, so that the drop removes this one and no
    /// other.
    serial: u64,
}

impl BlockRegistration {
    /// The device number the disk was registered with.
    pub(crate) fn rdev(&self) -> u64 {
        makedev(self.major, self.minor)
    }
}

impl Drop for BlockRegistration {
    /// Unpublish the node and free the name and the number. The registry's
    /// reference to the device is dropped after the lock is released, since
    /// it may be the last one.
    fn drop(&mut self) {
        let removed = {
            let mut registry = BLOCKS.lock();
            registry
                .disks
                .iter()
                .position(|disk| disk.serial == self.serial)
                .map(|at| registry.disks.remove(at))
        };
        drop(removed);
    }
}

/// Publish a block device node until the returned registration is dropped.
/// `major`/`minor` are derived by the caller from the name; mode is 0660.
///
/// `major` and `minor` are derived by the caller (the kernel's driver glue)
/// from the name devmgr chose; this registry does not number disks.
///
/// # Errors
///
/// In this order: [`BlockRefused::InvalidName`] for a name that is not 1 to
/// 32 bytes of lowercase letters and digits, [`BlockRefused::NameInUse`] for
/// a name any node in `/dev` has, and [`BlockRefused::NumberInUse`] for a
/// number another registered disk has.
pub(crate) fn register_block(
    name: &[u8],
    major: u32,
    minor: u32,
    device: Arc<dyn BlockDevice>,
) -> core::result::Result<BlockRegistration, BlockRefused> {
    register_block_from(name, major, minor, device, Origin::default())
}

/// [`register_block`], saying where the disk came from.
///
/// # Errors
///
/// As [`register_block`].
pub(crate) fn register_block_from(
    name: &[u8],
    major: u32,
    minor: u32,
    device: Arc<dyn BlockDevice>,
    origin: Origin,
) -> core::result::Result<BlockRegistration, BlockRefused> {
    let valid = !name.is_empty()
        && name
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
    let mut stored = [0_u8; BLOCK_NAME_MAX];
    match stored.get_mut(..name.len()) {
        Some(slot) if valid => slot.copy_from_slice(name),
        _ => return Err(BlockRefused::InvalidName),
    }
    if DEVICES.iter().any(|static_node| static_node.name == name)
        || name == SHM
        || name == DRI
        || name == INPUT
        || name == PTS
    {
        return Err(BlockRefused::NameInUse);
    }
    let mut registry = BLOCKS.lock();
    if registry.disks.iter().any(|disk| disk.name() == name) {
        return Err(BlockRefused::NameInUse);
    }
    if registry
        .disks
        .iter()
        .any(|disk| disk.major == major && disk.minor == minor)
    {
        return Err(BlockRefused::NumberInUse);
    }
    // Saturating, not wrapping: 2^64 registrations is out of reach, and
    // a serial is never handed out below one already given.
    let serial = registry.next_serial;
    registry.next_serial = serial.saturating_add(1);
    registry.disks.push(Disk {
        serial,
        name: stored,
        name_len: name.len(),
        major,
        minor,
        device,
        origin,
    });
    Ok(BlockRegistration {
        major,
        minor,
        serial,
    })
}

/// The registered disk whose number is `rdev`, for a mount to read through.
///
/// The `Arc` is cloned out, so the registry's lock is not held across any
/// read, and it keeps the device for as long as it is held, registered or not.
pub(crate) fn block_device(rdev: u64) -> Option<Arc<dyn BlockDevice>> {
    BLOCKS
        .lock()
        .disks
        .iter()
        .find(|disk| makedev(disk.major, disk.minor) == rdev)
        .map(|disk| Arc::clone(&disk.device))
}

/// Visit every registered disk, in registration order, with its name,
/// numbers and device. The disks are copied out first, so `visit` runs with
/// no lock held and may ask the device anything.
pub(crate) fn for_each_block(mut visit: impl FnMut(&[u8], u32, u32, &dyn BlockDevice)) {
    for disk in disks_from(0) {
        visit(disk.name(), disk.major, disk.minor, disk.device.as_ref());
    }
}

/// The registered disks with a serial of at least `serial`, copied out.
fn disks_from(serial: u64) -> Vec<Disk> {
    BLOCKS
        .lock()
        .disks
        .iter()
        .filter(|disk| disk.serial >= serial)
        .cloned()
        .collect()
}

/// The registered disk called `name`, copied out.
fn disk_named(name: &[u8]) -> Option<Disk> {
    BLOCKS
        .lock()
        .disks
        .iter()
        .find(|disk| disk.name() == name)
        .cloned()
}

// -- Nodes ----------------------------------------------------------------------

/// What a block node knows of its disk, taken when the name was looked up:
/// enough to answer `stat` after the disk has gone, and nothing that keeps
/// the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockNode {
    /// The registration's serial.
    serial: u64,
    /// The number's major half.
    major: u32,
    /// And minor half.
    minor: u32,
}

/// Which object a node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// `/dev` itself.
    Root,
    /// `DEVICES[index]`.
    Device(usize),
    /// A registered disk.
    Block(BlockNode),
    /// `/dev/dri`, while a card is published.
    Dri,
    /// `/dev/dri/card<N>`.
    Card(u32),
    /// `/dev/dri/renderD<N>`, the same card's other conversation.
    Render(u32),
    /// `/dev/input`, while an input device is published.
    Input,
    /// `/dev/input/event<N>`.
    Event(u32),
    /// `/dev/snd`, while a sound card is published.
    Snd,
    /// `/dev/snd/controlC<N>`.
    SoundControl(u32),
    /// `/dev/snd/pcmC<N>D0p`.
    SoundPcm(u32),
    /// A node of major 195 a ring-3 driver serves through the chardev core,
    /// by its minor (`docs/NVIDIA.md` §4.4).
    Chardev(u16),
    /// `/dev/pts`, the directory a pseudoterminal's slave is in.
    Pts,
    /// `/dev/pts/<N>`.
    Slave(u32),
    /// `/dev/shm`, the directory a tmpfs is mounted on. Empty in itself: the
    /// boot mounts the filesystem that holds the files, and what this node is
    /// for is being somewhere to mount it.
    Shm,
    /// `LINKS[index]`.
    Link(usize),
}

/// A devfs inode.
#[derive(Debug)]
struct Node {
    /// Which object it is.
    place: Place,
    /// Every timestamp: nothing here is ever modified.
    made: Timespec,
}

impl Node {
    /// The character device this node is, or `None` for the directory and a
    /// disk.
    fn device(&self) -> Option<&'static Device> {
        match self.place {
            Place::Device(index) => DEVICES.get(index),
            Place::Root
            | Place::Block(_)
            | Place::Dri
            | Place::Card(_)
            | Place::Render(_)
            | Place::Input
            | Place::Event(_)
            | Place::Snd
            | Place::SoundControl(_)
            | Place::SoundPcm(_)
            | Place::Chardev(_)
            | Place::Pts
            | Place::Slave(_)
            | Place::Shm
            | Place::Link(_) => None,
        }
    }
}

/// A device's inode number.
fn ino_of(index: usize) -> u64 {
    (index as u64).saturating_add(ROOT_INO + 1)
}

/// A disk's inode number.
fn block_ino(serial: u64) -> u64 {
    BLOCK_INO_BASE.saturating_add(serial)
}

/// What opening `/dev/tty` gives, as Linux's `tty_open_current_tty`: the
/// console when it is the controlling terminal of the caller's session,
/// else the pty slave that is, else `ENXIO`. The console is decided under
/// the terminal's lock and the pty under its pair's, the locks that guard
/// each session field, never both at once.
fn controlling_terminal() -> Result<Arc<dyn Inode>> {
    terminal_of_session(crate::syscall::process::current().map_or(0, |process| process.sid()))
}

/// [`controlling_terminal`] for `session`: the rule itself, which the boot's
/// self-check asks directly, since a check runs on no process's task.
pub(crate) fn terminal_of_session(session: u32) -> Result<Arc<dyn Inode>> {
    if session == 0 {
        return Err(Errno::ENXIO);
    }
    let console = fs::terminal::with(|terminal| terminal.session == session);
    if console {
        return Ok(fs::console::open_file());
    }
    let slave: Arc<dyn Inode> = pty::open_slave_of_session(session)?;
    Ok(slave)
}

impl Inode for Node {
    fn metadata(&self) -> Metadata {
        let directory = Metadata {
            ino: ROOT_INO,
            kind: FileType::Directory,
            permissions: 0o755,
            nlink: 2,
            uid: 0,
            gid: 0,
            size: 0,
            rdev: 0,
            blocks: 0,
            block_size: 4096,
            atime: self.made,
            mtime: self.made,
            ctime: self.made,
        };
        match (self.place, self.device()) {
            (Place::Device(index), Some(device)) => Metadata {
                ino: ino_of(index),
                kind: FileType::CharDevice,
                permissions: device.permissions,
                nlink: 1,
                rdev: makedev(device.major, device.minor),
                ..directory
            },
            // Size and blocks zero, as Linux reports a block special file: the
            // disk's size is asked of the disk (`/proc/partitions`, and
            // `BLKGETSIZE64` once a block node opens), not of its node.
            (Place::Block(disk), _) => Metadata {
                ino: block_ino(disk.serial),
                kind: FileType::BlockDevice,
                permissions: BLOCK_PERMISSIONS,
                nlink: 1,
                rdev: makedev(disk.major, disk.minor),
                ..directory
            },
            (Place::Dri, _) => Metadata {
                ino: DRI_INO,
                ..directory
            },
            (Place::Input, _) => Metadata {
                ino: INPUT_INO,
                ..directory
            },
            (Place::Snd | Place::SoundControl(_) | Place::SoundPcm(_), _) => {
                sound_metadata(self.place, self.made, directory)
            }
            (Place::Pts, _) => Metadata {
                ino: PTS_INO,
                ..directory
            },
            // Sticky and writable by everyone, as `/tmp` is and as every
            // `/dev/shm` is: the mode a program checks before trusting the
            // directory. The tmpfs mounted over it carries the same mode, so
            // the answer does not change when the mount is stepped onto.
            (Place::Shm, _) => Metadata {
                ino: SHM_INO,
                permissions: 0o1777,
                ..directory
            },
            (Place::Link(index), _) => link_metadata(index, directory),
            (Place::Chardev(minor), _) => Metadata {
                atime: self.made,
                mtime: self.made,
                ctime: self.made,
                ..crate::interfaces::chardev::file::metadata(minor)
            },
            (Place::Slave(number), _) => Metadata {
                atime: self.made,
                mtime: self.made,
                ctime: self.made,
                ..pty::slave_metadata(number)
            },
            (Place::Event(index), _) => Metadata {
                atime: self.made,
                mtime: self.made,
                ctime: self.made,
                ..crate::interfaces::input::evdev::metadata(index)
            },
            (Place::Card(index), _) => crate::interfaces::display::card(index).map_or(
                Metadata {
                    kind: FileType::CharDevice,
                    rdev: makedev(crate::interfaces::display::DRM_MAJOR, index),
                    ..directory
                },
                |card| Metadata {
                    atime: self.made,
                    mtime: self.made,
                    ctime: self.made,
                    ..card.metadata()
                },
            ),
            // The same major as a card, with the node's own number for its
            // minor, as Linux numbers `renderD128` 226:128.
            (Place::Render(index), _) => {
                let served = crate::interfaces::render::renderer(index)
                    .map(|renderer| renderer.metadata())
                    .or_else(|| {
                        crate::interfaces::chardev::render_control(index)
                            .map(|_| crate::interfaces::chardev::file::render_metadata(index))
                    });
                served.map_or(
                    Metadata {
                        kind: FileType::CharDevice,
                        rdev: makedev(crate::interfaces::display::DRM_MAJOR, index),
                        ..directory
                    },
                    |served| Metadata {
                        atime: self.made,
                        mtime: self.made,
                        ctime: self.made,
                        ..served
                    },
                )
            }
            _ => directory,
        }
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        !matches!(
            self.place,
            Place::Root
                | Place::Dri
                | Place::Input
                | Place::Snd
                | Place::Pts
                | Place::Shm
                | Place::Link(_)
        )
    }

    /// The memory devices, whose position is always 0: see the module's
    /// documentation. The terminals are not among them.
    fn ignores_position(&self) -> bool {
        self.device().is_some_and(|device| {
            matches!(
                device.behaviour,
                Behaviour::Null | Behaviour::Zero | Behaviour::Full | Behaviour::Random
            )
        })
    }

    /// The memory devices fill a read whatever its length, as Linux's
    /// `read_iter_zero` and `urandom_read_iter` do; see the module's
    /// documentation.
    fn fills_reads(&self) -> bool {
        self.ignores_position()
    }

    /// All but `/dev/null`, which Linux's `null_fops` give no `splice_read`,
    /// so `sendfile` and `splice` from it are `EINVAL` for a count above
    /// zero where a `read` is end of file. The nodes that open an object of
    /// their own -- a card, a render node, an event device, a terminal --
    /// answer through it.
    fn splices_out(&self) -> bool {
        self.device()
            .is_none_or(|device| device.behaviour != Behaviour::Null)
    }

    /// Disks are registered and dropped without the VFS being told, so a miss
    /// must not be remembered; the module's documentation says why this
    /// rather than invalidating.
    fn caches_lookups(&self) -> bool {
        false
    }

    /// Except `shm` in the root, which is always there and always the same
    /// node. Nothing else in `/dev` is, and a mount point has to be a
    /// remembered dentry, so this is what lets the boot put a tmpfs on
    /// `/dev/shm`; see [`Inode::caches_lookup_of`].
    fn caches_lookup_of(&self, name: &[u8]) -> bool {
        self.place == Place::Root && name == SHM
    }

    /// The console's own inode takes the reads and writes, so that whatever
    /// the console layer keys on — its object — is what an open of either
    /// name reaches, while `stat` still reports this node's number. A disk's
    /// node does not open: a mount reaches the disk by number.
    fn open(&self) -> Result<Option<Arc<dyn Inode>>> {
        // A disk is opened by [`attach_device`], which knows whether the open
        // is for writing: a read-only disk refuses only that.
        if let Place::Block(_) = self.place {
            return Ok(None);
        }
        if let Place::Card(index) = self.place {
            let card = crate::interfaces::display::card(index).ok_or(Errno::ENXIO)?;
            let file: Arc<dyn Inode> = crate::interfaces::display::drm::CardFile::open(card)?;
            return Ok(Some(file));
        }
        // Every open of a render node is its own, and there may be any
        // number: a render node is not the display's master, which is what
        // Linux has them for and what `docs/GPU.md` §3.3 keeps.
        // A chardev driver's render node is its driver's to answer, as its
        // other nodes are (`docs/NVIDIA.md` §4.4, N3b).
        if let Place::Render(index) = self.place {
            let file: Arc<dyn Inode> = match crate::interfaces::render::renderer(index) {
                Some(renderer) => crate::interfaces::render::node::RenderFile::open(renderer)?,
                None => crate::interfaces::chardev::file::ChardevFile::open_render(index)?,
            };
            return Ok(Some(file));
        }
        // Every open of an input device gets its own object, with its own
        // queue, clock and grab: `docs/INPUT.md` §3.3 allows any number,
        // as Linux does.
        if let Place::Event(index) = self.place {
            let device = crate::interfaces::input::device(index).ok_or(Errno::ENXIO)?;
            let file: Arc<dyn Inode> = crate::interfaces::input::evdev::EventFile::open(device)?;
            return Ok(Some(file));
        }
        // A sound card's nodes: the playback node one open at a time, the
        // control node any number (`docs/AUDIO.md` §3.1).
        if let Place::SoundPcm(index) = self.place {
            let card = crate::interfaces::audio::card(index).ok_or(Errno::ENXIO)?;
            let file: Arc<dyn Inode> = crate::interfaces::audio::pcm::PcmFile::open(card)?;
            return Ok(Some(file));
        }
        if let Place::SoundControl(index) = self.place {
            let card = crate::interfaces::audio::card(index).ok_or(Errno::ENXIO)?;
            let file: Arc<dyn Inode> = crate::interfaces::audio::pcm::ControlFile::open(card)?;
            return Ok(Some(file));
        }
        // Every open of a chardev node is a file of its own, which its
        // driver is told of and agrees to (`docs/NVIDIA.md` §4.4).
        if let Place::Chardev(minor) = self.place {
            let file: Arc<dyn Inode> = crate::interfaces::chardev::file::ChardevFile::open(minor)?;
            return Ok(Some(file));
        }
        // A slave is one object a pair, shared by every open of it, as a
        // terminal is: two programs with the same terminal open read from
        // one queue.
        if let Place::Slave(number) = self.place {
            let file: Arc<dyn Inode> = pty::open_slave(number)?;
            return Ok(Some(file));
        }
        // Every open of `/dev/ptmx` is a pair of its own, which is the whole
        // point of the multiplexer.
        if self
            .device()
            .is_some_and(|device| device.behaviour == Behaviour::Ptmx)
        {
            let file: Arc<dyn Inode> = pty::open_master()?;
            return Ok(Some(file));
        }
        // `/dev/tty`: the caller's controlling terminal, not the console to
        // anyone (`docs/AUTH.md` §1).
        if self
            .device()
            .is_some_and(|device| device.behaviour == Behaviour::Console)
        {
            return controlling_terminal().map(Some);
        }
        Ok(None)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if let Place::Block(_)
        | Place::Card(_)
        | Place::Render(_)
        | Place::Event(_)
        | Place::SoundControl(_)
        | Place::SoundPcm(_)
        | Place::Chardev(_)
        | Place::Slave(_) = self.place
        {
            return Err(Errno::ENXIO);
        }
        let device = self.device().ok_or(Errno::EISDIR)?;
        match device.behaviour {
            Behaviour::Null => Ok(0),
            Behaviour::Zero | Behaviour::Full => {
                buf.fill(0);
                Ok(buf.len())
            }
            Behaviour::Random => {
                time::fill_random(buf);
                Ok(buf.len())
            }
            // Never reached for a node that was opened, whose reads go to
            // what its open gave; refused by the device itself.
            Behaviour::Console | Behaviour::ConsoleItself => console_inode().read_at(offset, buf),
            // The node is never the object: opening it makes a pair and
            // gives back the master, which is what reads and writes.
            Behaviour::Ptmx => Err(Errno::ENXIO),
        }
    }

    fn write_at(&self, offset: u64, data: &[u8], append: bool) -> Result<(usize, u64)> {
        if let Place::Block(_)
        | Place::Card(_)
        | Place::Render(_)
        | Place::Event(_)
        | Place::SoundControl(_)
        | Place::SoundPcm(_)
        | Place::Chardev(_)
        | Place::Slave(_) = self.place
        {
            return Err(Errno::ENXIO);
        }
        let device = self.device().ok_or(Errno::EISDIR)?;
        match device.behaviour {
            Behaviour::Null | Behaviour::Zero | Behaviour::Random => Ok((data.len(), offset)),
            Behaviour::Full => Err(Errno::ENOSPC),
            Behaviour::Console | Behaviour::ConsoleItself => {
                console_inode().write_at(offset, data, append)
            }
            Behaviour::Ptmx => Err(Errno::ENXIO),
        }
    }

    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn Inode>> {
        if self.place == Place::Dri {
            // `renderD<N>` first: a card's name cannot be mistaken for one,
            // and a render node is not a card with another name.
            if let Some(index) = crate::interfaces::render::node::render_number(name) {
                let _served = crate::interfaces::render::node_device(index).ok_or(Errno::ENOENT)?;
                return Ok(Arc::new(Node {
                    place: Place::Render(index),
                    made: self.made,
                }));
            }
            let index = card_number(name).ok_or(Errno::ENOENT)?;
            let _card = crate::interfaces::display::card(index).ok_or(Errno::ENOENT)?;
            return Ok(Arc::new(Node {
                place: Place::Card(index),
                made: self.made,
            }));
        }
        if self.place == Place::Input {
            let index = crate::interfaces::input::evdev::event_number(name).ok_or(Errno::ENOENT)?;
            let _device = crate::interfaces::input::device(index).ok_or(Errno::ENOENT)?;
            return Ok(Arc::new(Node {
                place: Place::Event(index),
                made: self.made,
            }));
        }
        if self.place == Place::Snd {
            let (index, pcm) = sound_node(name).ok_or(Errno::ENOENT)?;
            let _card = crate::interfaces::audio::card(index).ok_or(Errno::ENOENT)?;
            return Ok(Arc::new(Node {
                place: if pcm {
                    Place::SoundPcm(index)
                } else {
                    Place::SoundControl(index)
                },
                made: self.made,
            }));
        }
        if self.place == Place::Pts {
            // A slave's name is its number and nothing else, as devpts
            // names them.
            let number = slave_number(name).ok_or(Errno::ENOENT)?;
            let _pair = pty::pair(number).ok_or(Errno::ENOENT)?;
            return Ok(Arc::new(Node {
                place: Place::Slave(number),
                made: self.made,
            }));
        }
        if self.place != Place::Root {
            return Err(Errno::ENOTDIR);
        }
        if name == DRI
            && !(crate::interfaces::display::card_indices().is_empty()
                && crate::interfaces::render::node_indices().is_empty())
        {
            return Ok(Arc::new(Node {
                place: Place::Dri,
                made: self.made,
            }));
        }
        if name == INPUT && !crate::interfaces::input::device_indices().is_empty() {
            return Ok(Arc::new(Node {
                place: Place::Input,
                made: self.made,
            }));
        }
        if name == SND && !crate::interfaces::audio::card_indices().is_empty() {
            return Ok(Arc::new(Node {
                place: Place::Snd,
                made: self.made,
            }));
        }
        // `/dev/pts` is there once anything has opened `/dev/ptmx`, which is
        // when it has anything in it: Linux's devpts is a mount, and this
        // is the same directory without one.
        if name == PTS && pty::made() > 0 {
            return Ok(Arc::new(Node {
                place: Place::Pts,
                made: self.made,
            }));
        }
        if name == SHM {
            return Ok(Arc::new(Node {
                place: Place::Shm,
                made: self.made,
            }));
        }
        if let Some(index) = LINKS.iter().position(|(link, _)| *link == name) {
            return Ok(Arc::new(Node {
                place: Place::Link(index),
                made: self.made,
            }));
        }
        if let Some(node) = chardev_node(name, self.made) {
            return Ok(node);
        }
        if let Some(index) = DEVICES.iter().position(|device| device.name == name) {
            return Ok(node(index, self.made));
        }
        let disk = disk_named(name).ok_or(Errno::ENOENT)?;
        Ok(Arc::new(Node {
            place: Place::Block(disk.node()),
            made: self.made,
        }))
    }

    /// The static nodes by index, then the disks by serial; the module's
    /// documentation says why that keeps a listing in pieces stable.
    fn read_dir(&self, cursor: u64, emit: &mut dyn FnMut(DirEntry<'_>) -> bool) -> Result<()> {
        if self.place == Place::Dri {
            return read_dri(cursor, emit);
        }
        if self.place == Place::Input {
            return crate::interfaces::input::evdev::read_dir(cursor, emit);
        }
        if self.place == Place::Snd {
            return read_snd(cursor, emit);
        }
        if self.place == Place::Pts {
            return read_pts(cursor, emit);
        }
        // Nothing of its own is ever in it: a listing of `/dev/shm` is a
        // listing of the tmpfs mounted over it, and this node is only what
        // the walk sees if that mount is not there.
        if self.place == Place::Shm {
            return Ok(());
        }
        if self.place != Place::Root {
            return Err(Errno::ENOTDIR);
        }
        if cursor < BLOCK_CURSOR_BASE {
            let first = usize::try_from(cursor.saturating_sub(FIRST_CURSOR)).unwrap_or(usize::MAX);
            for (index, device) in DEVICES.iter().enumerate().skip(first) {
                let ino = if device.behaviour == Behaviour::ConsoleItself {
                    console_inode().metadata().ino
                } else {
                    ino_of(index)
                };
                let entry = DirEntry {
                    ino,
                    kind: FileType::CharDevice,
                    name: device.name,
                    next: FIRST_CURSOR.saturating_add(index as u64 + 1),
                };
                if !emit(entry) {
                    return Ok(());
                }
            }
        }
        // Copied out, so `emit` — which may copy to a program's memory and
        // sleep on a fault — runs with the registry unlocked.
        for disk in disks_from(cursor.saturating_sub(BLOCK_CURSOR_BASE)) {
            let entry = DirEntry {
                ino: block_ino(disk.serial),
                kind: FileType::BlockDevice,
                name: disk.name(),
                next: BLOCK_CURSOR_BASE
                    .saturating_add(disk.serial)
                    .saturating_add(1),
            };
            if !emit(entry) {
                return Ok(());
            }
        }
        if cursor <= DRI_CURSOR && !crate::interfaces::display::card_indices().is_empty() {
            let kept = emit(DirEntry {
                ino: DRI_INO,
                kind: FileType::Directory,
                name: DRI,
                next: DRI_CURSOR + 1,
            });
            if !kept {
                return Ok(());
            }
        }
        if cursor <= INPUT_CURSOR && !crate::interfaces::input::device_indices().is_empty() {
            let kept = emit(DirEntry {
                ino: INPUT_INO,
                kind: FileType::Directory,
                name: INPUT,
                next: INPUT_CURSOR + 1,
            });
            if !kept {
                return Ok(());
            }
        }
        if cursor <= PTS_CURSOR && pty::made() > 0 {
            let kept = emit(DirEntry {
                ino: PTS_INO,
                kind: FileType::Directory,
                name: PTS,
                next: PTS_CURSOR + 1,
            });
            if !kept {
                return Ok(());
            }
        }
        // Unconditional, unlike the three above: `shm` does not wait for a
        // device to be published, because nothing publishes it. It is there
        // from the first listing.
        if cursor <= SHM_CURSOR {
            let kept = emit(DirEntry {
                ino: SHM_INO,
                kind: FileType::Directory,
                name: SHM,
                next: SHM_CURSOR + 1,
            });
            if !kept {
                return Ok(());
            }
        }
        if emit_snd(cursor, emit) && emit_links(cursor, emit) {
            emit_chardevs(cursor, emit);
        }
        Ok(())
    }

    fn read_link(&self) -> Result<Vec<u8>> {
        match self.place {
            Place::Link(index) => LINKS
                .get(index)
                .map(|(_, target)| target.to_vec())
                .ok_or(Errno::ENOENT),
            _ => Err(Errno::EINVAL),
        }
    }

    /// A card's pages, which `MODE_MAP_DUMB`'s offsets are into.
    fn mapping(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        let Place::Card(index) = self.place else {
            return None;
        };
        let card = crate::interfaces::display::card(index)?;
        let vmo: Arc<dyn Any + Send + Sync> = Arc::clone(&card.vmo) as Arc<dyn Any + Send + Sync>;
        Some(vmo)
    }
}

/// The number a slave's name is, with no leading zero: devpts names a slave
/// by its number and nothing else.
fn slave_number(name: &[u8]) -> Option<u32> {
    if name.is_empty() || (name.len() > 1 && name.first() == Some(&b'0')) {
        return None;
    }
    core::str::from_utf8(name).ok()?.parse::<u32>().ok()
}

/// `/dev/pts`'s listing: every pair that exists, by number.
fn read_pts(cursor: u64, emit: &mut dyn FnMut(DirEntry<'_>) -> bool) -> Result<()> {
    let first = cursor.saturating_sub(FIRST_CURSOR);
    for number in pty::numbers() {
        if u64::from(number) < first {
            continue;
        }
        let name = alloc::format!("{number}");
        let entry = DirEntry {
            ino: pty::SLAVE_INO_BASE + u64::from(number),
            kind: FileType::CharDevice,
            name: name.as_bytes(),
            next: FIRST_CURSOR
                .saturating_add(u64::from(number))
                .saturating_add(1),
        };
        if !emit(entry) {
            return Ok(());
        }
    }
    Ok(())
}

/// The card number `card<N>` names, with no leading zero.
fn card_number(name: &[u8]) -> Option<u32> {
    let digits = name.strip_prefix(b"card")?;
    if digits.is_empty() || (digits.len() > 1 && digits.first() == Some(&b'0')) {
        return None;
    }
    core::str::from_utf8(digits).ok()?.parse::<u32>().ok()
}

/// `/dev/dri`'s listing: every published card, by number.
fn read_dri(cursor: u64, emit: &mut dyn FnMut(DirEntry<'_>) -> bool) -> Result<()> {
    let first = cursor.saturating_sub(FIRST_CURSOR);
    for index in crate::interfaces::display::card_indices() {
        if u64::from(index) < first {
            continue;
        }
        let name = alloc::format!("card{index}");
        let entry = DirEntry {
            ino: (1u64 << 40) + u64::from(index),
            kind: FileType::CharDevice,
            name: name.as_bytes(),
            next: FIRST_CURSOR + u64::from(index) + 1,
        };
        if !emit(entry) {
            break;
        }
    }
    // Then the render nodes, which are numbered from 128 upwards where a
    // card's number is small, so one cursor counts through both in order.
    for index in crate::interfaces::render::node_indices() {
        if u64::from(index) < first {
            continue;
        }
        let name = alloc::format!("renderD{index}");
        let entry = DirEntry {
            ino: (1u64 << 41) + u64::from(index),
            kind: FileType::CharDevice,
            name: name.as_bytes(),
            next: FIRST_CURSOR + u64::from(index) + 1,
        };
        if !emit(entry) {
            break;
        }
    }
    Ok(())
}

/// The inode `DEVICES[index]` is, stamped `made`.
///
/// `/dev/console` is the console itself rather than a node standing for it.
/// `fs::console::open_console` names a process's descriptors `/dev/console`
/// only when that name reaches this very inode, and `stat` then reports the
/// console's own number and identity.
fn node(index: usize, made: Timespec) -> Arc<dyn Inode> {
    if DEVICES
        .get(index)
        .is_some_and(|device| device.behaviour == Behaviour::ConsoleItself)
    {
        return console_inode();
    }
    Arc::new(Node {
        place: Place::Device(index),
        made,
    })
}

/// A node of devfs's appeared (`created`) or went: `IN_CREATE` or
/// `IN_DELETE` on its directory for anyone watching it with inotify, as
/// Linux's devtmpfs tells udev. `path` is under `/dev`, `dri/card0` say.
/// A subdirectory that came with its first node, or went with its last, is
/// told to `/dev` the same way, so a watch there sees `dri` arrive before
/// it can watch `dri` itself. init's `.device` units are ordered on these
/// (`docs/INIT.md` §4.2).
///
/// Called with no lock held, and never from interrupt context: the walk to
/// the directory may sleep, and devfs's lookups read the driver cores'
/// lists. A core announces a node after it is in its list and its going
/// after it has left it, so a watcher that looks on the event finds what
/// the event says. Only the first namespace's `/dev` is told (`fs::namespace`).
pub(crate) fn announce(path: &[u8], created: bool) {
    if !fs::inotify::watching() {
        return;
    }
    let (dir, name) = match path.iter().rposition(|&byte| byte == b'/') {
        Some(at) => (
            path.get(..at).unwrap_or_default(),
            path.get(at + 1..).unwrap_or_default(),
        ),
        None => (&b""[..], path),
    };
    let namespace = fs::namespace();
    let context = namespace.context();
    let mask = if created {
        fs::inotify::IN_CREATE
    } else {
        fs::inotify::IN_DELETE
    };
    let mut at = Vec::with_capacity(5 + dir.len());
    at.extend_from_slice(b"/dev");
    if !dir.is_empty() {
        at.push(b'/');
        at.extend_from_slice(dir);
    }
    let subdirectory = namespace.resolve(&context, None, &at, true);
    // The subdirectory's own coming, before its entry's, and its going, after.
    let tell_dir = |mask| {
        if let (false, Ok(root)) = (
            dir.is_empty(),
            namespace.resolve(&context, None, b"/dev", true),
        ) {
            fs::inotify::dir_event(&root, dir, mask, 0, true);
        }
    };
    if created {
        tell_dir(fs::inotify::IN_CREATE);
    }
    if let Ok(directory) = &subdirectory {
        fs::inotify::dir_event(directory, name, mask, 0, false);
    }
    if !created && (dir.is_empty() || subdirectory.is_err()) {
        tell_dir(fs::inotify::IN_DELETE);
    }
}

/// What reads and writes of the character device numbered `rdev` go to: the
/// devfs node with that number, opened as an open of it in `/dev` would be.
///
/// # Errors
///
/// `ENXIO` for a number no device here has.
pub(crate) fn open_char_device(rdev: u64) -> Result<Arc<dyn Inode>> {
    let index = DEVICES
        .iter()
        .position(|device| makedev(device.major, device.minor) == rdev)
        .ok_or(Errno::ENXIO)?;
    // The stamp is never seen: `stat` reports the node that was opened, and
    // this inode only takes its reads and writes.
    let device = node(index, Timespec::default());
    Ok(device.open()?.unwrap_or(device))
}

/// An open file of a device node, made to read and write the device its
/// number names. Anything else -- a devfs node, which already is its device,
/// and a node opened with `O_PATH`, which is a handle on the name -- comes
/// back as it was.
///
/// The open keeps its location and inode, so `fstat` reports the node that
/// was opened, with its own inode number and `st_rdev`, as `/dev/tty` reports
/// its own while reading and writing the console.
///
/// # Errors
///
/// `ENXIO` for a character or block number no device here has, and
/// `EACCES` for a disk that refuses writes opened for writing. A block node
/// opens as its disk (`fs::disk_file`), wherever the node is.
pub(crate) fn attach_device(file: Arc<OpenFile>) -> Result<Arc<OpenFile>> {
    if file.is_path() {
        return Ok(file);
    }
    match file.kind() {
        FileType::CharDevice => {}
        FileType::BlockDevice => {
            let rdev = file.inode().metadata().rdev;
            let disk = fs::disk_file::DiskFile::open(rdev, file.writable())?;
            return file.with_io(disk);
        }
        _ => return Ok(file),
    }
    let inode = file.inode();
    if Arc::clone(inode).into_any().is::<Node>() || Arc::ptr_eq(inode, &console_inode()) {
        return Ok(file);
    }
    let device = open_char_device(inode.metadata().rdev)?;
    file.with_io(device)
}

/// Mount a devfs on `/dev`, making the directory if the archive had none.
pub(crate) fn mount() -> Result<()> {
    let ns = fs::namespace();
    let ctx = ns.context();
    match ns.mkdir(&ctx, None, b"/dev", 0o755) {
        Ok(()) | Err(Errno::EEXIST) => {}
        Err(errno) => return Err(errno),
    }
    let at = ns.resolve(&ctx, None, b"/dev", true)?;
    ns.mount(Arc::new(Devfs::new()), &at).map(drop)
}

/// A name in `/dev/snd`: `controlC<N>`, or `pcmC<N>D0p` (`true`).
fn sound_node(name: &[u8]) -> Option<(u32, bool)> {
    let number = |digits: &[u8]| -> Option<u32> {
        if digits.is_empty() || (digits.len() > 1 && digits.first() == Some(&b'0')) {
            return None;
        }
        core::str::from_utf8(digits).ok()?.parse().ok()
    };
    if let Some(rest) = name.strip_prefix(b"controlC") {
        return number(rest).map(|index| (index, false));
    }
    let rest = name.strip_prefix(b"pcmC")?.strip_suffix(b"D0p")?;
    number(rest).map(|index| (index, true))
}

/// `/dev/snd`'s entries: each card's control node, then its playback node.
fn read_snd(cursor: u64, emit: &mut dyn FnMut(DirEntry<'_>) -> bool) -> Result<()> {
    let mut next = FIRST_CURSOR;
    for index in crate::interfaces::audio::card_indices() {
        let names = [
            (
                alloc::format!("controlC{index}"),
                crate::interfaces::audio::pcm::control_metadata(index).ino,
            ),
            (
                alloc::format!("pcmC{index}D0p"),
                crate::interfaces::audio::pcm::pcm_metadata(index).ino,
            ),
        ];
        for (name, ino) in &names {
            let at = next;
            next += 1;
            if at < cursor {
                continue;
            }
            let kept = emit(DirEntry {
                ino: *ino,
                kind: FileType::CharDevice,
                name: name.as_bytes(),
                next,
            });
            if !kept {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// `/dev/snd`'s metadata, and its nodes'.
fn sound_metadata(place: Place, made: Timespec, directory: Metadata) -> Metadata {
    let node = match place {
        Place::SoundControl(index) => crate::interfaces::audio::pcm::control_metadata(index),
        Place::SoundPcm(index) => crate::interfaces::audio::pcm::pcm_metadata(index),
        _ => {
            return Metadata {
                ino: SND_INO,
                ..directory
            };
        }
    };
    Metadata {
        atime: made,
        mtime: made,
        ctime: made,
        ..node
    }
}

/// `LINKS[index]`'s metadata: `directory`'s times and owner, a link's kind.
fn link_metadata(index: usize, directory: Metadata) -> Metadata {
    Metadata {
        ino: LINK_INO_BASE.saturating_add(index as u64),
        kind: FileType::Symlink,
        permissions: 0o777,
        nlink: 1,
        size: LINKS
            .get(index)
            .map_or(0, |(_, target)| target.len() as u64),
        ..directory
    }
}

/// The root's entries for [`LINKS`], from `cursor` on; whether the listing
/// may go on.
fn emit_links(cursor: u64, emit: &mut dyn FnMut(DirEntry<'_>) -> bool) -> bool {
    let first = usize::try_from(cursor.saturating_sub(LINK_CURSOR)).unwrap_or(usize::MAX);
    for (index, (name, _)) in LINKS.iter().enumerate().skip(first) {
        let entry = DirEntry {
            ino: LINK_INO_BASE.saturating_add(index as u64),
            kind: FileType::Symlink,
            name,
            next: LINK_CURSOR.saturating_add(index as u64 + 1),
        };
        if !emit(entry) {
            return false;
        }
    }
    true
}

/// The chardev node `name` is in `/dev`, if a driver serves it
/// (`docs/NVIDIA.md` §4.4).
fn chardev_node(name: &[u8], made: Timespec) -> Option<Arc<dyn Inode>> {
    let minor = ferrix_chardevctl::node::minor_of(name)?;
    let _served = crate::interfaces::chardev::published(minor)?;
    Some(Arc::new(Node {
        place: Place::Chardev(minor),
        made,
    }))
}

/// The root's cursor for the chardev nodes, after the links: one past it
/// for each minor.
const CHARDEV_CURSOR: u64 = (1 << 48) + 16;

/// The root's entries for the published chardev nodes, by minor, last in a
/// listing (`docs/NVIDIA.md` §4.4).
fn emit_chardevs(cursor: u64, emit: &mut dyn FnMut(DirEntry<'_>) -> bool) {
    for minor in crate::interfaces::chardev::published_minors() {
        let at = CHARDEV_CURSOR.saturating_add(u64::from(minor));
        if at < cursor {
            continue;
        }
        let Some(name) = ferrix_chardevctl::node::name(minor) else {
            continue;
        };
        let entry = DirEntry {
            ino: crate::interfaces::chardev::file::INO_BASE.saturating_add(u64::from(minor)),
            kind: FileType::CharDevice,
            name: name.as_bytes(),
            next: at.saturating_add(1),
        };
        if !emit(entry) {
            return;
        }
    }
}

/// The root's entry for `/dev/snd`, while a card is published; whether the
/// listing may go on.
fn emit_snd(cursor: u64, emit: &mut dyn FnMut(DirEntry<'_>) -> bool) -> bool {
    if cursor <= SND_CURSOR && !crate::interfaces::audio::card_indices().is_empty() {
        return emit(DirEntry {
            ino: SND_INO,
            kind: FileType::Directory,
            name: SND,
            next: SND_CURSOR + 1,
        });
    }
    true
}
