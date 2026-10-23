//! Stage 8's self-check of the block registry devfs keeps for stage 11.
//!
//! A mount will find its disk through [`devfs::block_device`], and a program
//! will find the same disk as a node in `/dev` and a row in
//! `/proc/partitions`. The check registers a small in-memory disk and holds
//! all three to what Linux shows: the node listed after the character nodes,
//! with a listing in pieces neither repeating nor skipping a name across a
//! registration; `statx` reporting a block device with the registered number
//! and, as Linux does, a size of zero; `O_PATH` opening it; the disk found
//! by number and its sectors read back; exactly its row in
//! `/proc/partitions`; clashing names and numbers refused, in order; and,
//! once the registration is dropped, the node, the lookup and the row gone
//! while the `Arc` still held answers `EIO` and does not panic.
//!
//! In between, the disk opened as a file (`fs::disk_file`, the installer's
//! way to a disk): `pread` of a range that starts and ends inside sectors,
//! `lseek` to the end, short reads at and over the end, the `BLK*` requests,
//! and `O_RDWR` refused because the disk is read-only. Then a writable disk
//! in memory: `pwrite` of a range that patches two sectors in part, a write
//! over the end cut short and one at the end refused, `fsync` reaching the
//! disk's flush, and every byte read back.
//!
//! The calls a program makes go in by number, from a process built for the
//! check, as `fs::check::run_calls` makes them. Like that check it runs
//! twice and counts frames and cached dentries across the second run, which
//! must leave neither behind. It assumes it is the only registrant at boot:
//! no driver has registered a disk before stage 8's checks run.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::offset_of;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    AT_FDCWD, BLKGETSIZE64, BLKROGET, BLKRRPART, BLKSSZGET, DT_BLK, DT_CHR, DT_DIR, DT_LNK,
    MAP_ANONYMOUS, MAP_PRIVATE, O_PATH, O_RDONLY, O_RDWR, PROT_READ, PROT_WRITE, S_IFBLK, SEEK_END,
    STATX_BASIC_STATS, Statx, TCGETS,
};
use ferrix_vfs::Errno;
use ferrix_vfs::dirent;
use ferrix_vfs::initramfs::makedev;

use crate::fs;
use crate::fs::block::BlockDevice;
use crate::fs::devfs::{self, BlockRefused};
use crate::mm;
use crate::syscall::check as syscall_check;
use crate::syscall::memory::{self, MmapRequest, OffsetUnit};
use crate::syscall::process::{self, Process};
use crate::syscall::{fd, uaccess};

/// What the check measured, for the boot log.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Report {
    /// The number the check's disk was registered with, major half.
    pub(crate) major: u32,
    /// And minor half.
    pub(crate) minor: u32,
    /// Sectors read back through the device `block_device` found.
    pub(crate) sectors: u64,
    /// Frames the second run did not give back. Zero, or the check fails.
    pub(crate) leaked: i64,
}

/// The check disk's name.
const NAME: &[u8] = b"ferrixcheck0";
/// A second disk's, registered part-way through a listing.
const SECOND: &[u8] = b"ferrixcheck1";
/// The check disk's major number: Linux's first local/experimental block
/// major, which no driver here takes.
const MAJOR: u32 = 254;
/// And its minor.
const MINOR: u32 = 250;
/// Sectors on the check disk.
const SECTORS: u64 = 64;
/// Bytes in each of them.
const SECTOR: u32 = 512;
/// The first sector read back.
const FIRST_READ: u64 = 3;
/// How many are read.
const READ_SECTORS: u64 = 2;

/// `/proc/partitions` with the check disk registered, as Linux would print
/// it: 64 sectors of 512 bytes are 32 KiB.
const PARTITIONS: &[u8] = b"major minor  #blocks  name\n\n 254      250         32 ferrixcheck0\n";

/// The character nodes, in the order `/dev` lists them.
const STATIC_NODES: [&[u8]; 9] = [
    b"null", b"zero", b"full", b"random", b"urandom", b"tty", b"console", b"ptmx", b"kmsg",
];

/// `shm`, which `/dev` lists after the disks and always: the one directory
/// there unconditionally, and the one this filesystem does not publish a
/// device for.
const SHM: &[u8] = b"shm";
/// The links every Linux `/dev` has, listed after `shm`, in this order.
const LINKS: [&[u8]; 4] = [b"fd", b"stdin", b"stdout", b"stderr"];

/// Where `/dev` is staged in the check's page.
const AT_DEV: u64 = 0;
/// The disk's node.
const AT_NODE: u64 = 16;
/// `/proc/partitions`.
const AT_PARTITIONS: u64 = 48;
/// Where `statx` writes.
const AT_STATX: u64 = 256;
/// Where `getdents64` writes.
const AT_LISTING: u64 = 768;
/// Where `/proc/partitions` is read to.
const AT_TEXT: u64 = 1024;
/// The writable disk's node.
const AT_WRITABLE_NODE: u64 = 96;
/// Where a disk opened as a file is read to and written from.
const AT_DATA: u64 = 2048;

const DEV: &[u8] = b"/dev\0";
const NODE: &[u8] = b"/dev/ferrixcheck0\0";
const WRITABLE_NODE: &[u8] = b"/dev/ferrixcheck2\0";

/// The writable disk's name and minor.
const WRITABLE: &[u8] = b"ferrixcheck2";
const WRITABLE_MINOR: u32 = MINOR + 4;
/// Its sectors: few, so a write over its end is cheap to set up.
const WRITABLE_SECTORS: u64 = 4;

/// A read of the check disk that starts 100 bytes into sector 3 and ends
/// part-way through sector 4.
const PARTIAL_AT: u64 = 3 * SECTOR as u64 + 100;
const PARTIAL_LEN: u64 = 700;
const PROC_PARTITIONS: &[u8] = b"/proc/partitions\0";

/// Room `getdents64` is given per call: the dot entries and three nodes, so
/// the first call stops part-way through the static nodes.
const LISTING_PIECE: u64 = 128;
/// Calls a listing of `/dev` may take before it is called endless.
const LISTING_CALLS: u32 = 64;
/// Bytes of `/proc/partitions` read per call.
const TEXT_PIECE: u64 = 256;

/// `AT_FDCWD`, as a register carries it.
const CWD: u64 = AT_FDCWD as i64 as u64;

/// A listed name: the name, `d_type` and `d_ino`.
type Listed = Vec<(Vec<u8>, u8, u64)>;

/// The check's disk: 64 sectors of a pattern, read-only, and a switch that
/// makes it gone.
#[derive(Debug, Default)]
struct CheckDisk {
    /// Whether it has gone away, as a disk whose driver died has.
    gone: AtomicBool,
}

/// The byte at `offset` in `sector`: different in every sector and along it.
fn pattern(sector: u64, offset: usize) -> u8 {
    (sector as u8).wrapping_mul(37) ^ (offset as u8) ^ ((offset >> 8) as u8).wrapping_mul(11)
}

impl BlockDevice for CheckDisk {
    fn read(&self, sector: u64, buf: &mut [u8]) -> Result<(), Errno> {
        if self.gone.load(Ordering::Acquire) {
            return Err(Errno::EIO);
        }
        let size = SECTOR as usize;
        if !buf.len().is_multiple_of(size) {
            return Err(Errno::EINVAL);
        }
        let count = (buf.len() / size) as u64;
        if sector.checked_add(count).is_none_or(|end| end > SECTORS) {
            return Err(Errno::EIO);
        }
        for (at, byte) in buf.iter_mut().enumerate() {
            *byte = pattern(sector + (at / size) as u64, at % size);
        }
        Ok(())
    }

    fn sectors(&self) -> u64 {
        SECTORS
    }

    fn sector_size(&self) -> u32 {
        SECTOR
    }

    fn read_only(&self) -> bool {
        true
    }
}

/// A writable disk in memory: [`WRITABLE_SECTORS`] sectors, zero until
/// written, counting its flushes.
#[derive(Debug)]
struct MemoryDisk {
    /// Its bytes.
    bytes: crate::sync::SpinLock<Vec<u8>>,
    /// How many times it was flushed.
    flushes: AtomicU32,
}

impl MemoryDisk {
    fn new() -> MemoryDisk {
        MemoryDisk {
            bytes: crate::sync::SpinLock::new(vec![
                0;
                (WRITABLE_SECTORS * u64::from(SECTOR)) as usize
            ]),
            flushes: AtomicU32::new(0),
        }
    }

    /// The byte range `sector` and `len` name, if it is whole sectors inside.
    fn range(sector: u64, len: usize) -> Result<core::ops::Range<usize>, Errno> {
        let size = SECTOR as usize;
        if len == 0 || !len.is_multiple_of(size) {
            return Err(Errno::EINVAL);
        }
        let start = usize::try_from(sector)
            .map_err(|_| Errno::EIO)?
            .checked_mul(size)
            .ok_or(Errno::EIO)?;
        let end = start.checked_add(len).ok_or(Errno::EIO)?;
        if end > (WRITABLE_SECTORS * u64::from(SECTOR)) as usize {
            return Err(Errno::EIO);
        }
        Ok(start..end)
    }
}

impl BlockDevice for MemoryDisk {
    fn read(&self, sector: u64, buf: &mut [u8]) -> Result<(), Errno> {
        let range = MemoryDisk::range(sector, buf.len())?;
        buf.copy_from_slice(self.bytes.lock().get(range).ok_or(Errno::EIO)?);
        Ok(())
    }

    fn sectors(&self) -> u64 {
        WRITABLE_SECTORS
    }

    fn sector_size(&self) -> u32 {
        SECTOR
    }

    fn read_only(&self) -> bool {
        false
    }

    fn write(&self, sector: u64, buf: &[u8]) -> Result<(), Errno> {
        let range = MemoryDisk::range(sector, buf.len())?;
        self.bytes
            .lock()
            .get_mut(range)
            .ok_or(Errno::EIO)?
            .copy_from_slice(buf);
        Ok(())
    }

    fn flush(&self) -> Result<(), Errno> {
        let _ = self.flushes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// The check disk as the registry takes it.
fn as_device(disk: &Arc<CheckDisk>) -> Arc<dyn BlockDevice> {
    Arc::clone(disk) as Arc<dyn BlockDevice>
}

/// Run it twice, measured on the second: the first grows the registry's list
/// and the heap to where they stay.
pub(crate) fn run() -> Result<Report, &'static str> {
    let process =
        process::new_for_check().map_err(|_| "could not make a process for the block check")?;
    let _warm = check_once(&process)?;
    let cached = fs::namespace().cached();
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let window = mm::FrameWindow::open();
    let sectors = check_once(&process)?;
    // devfs does not cache lookups, so walking to a disk that comes and goes
    // must not leave a dentry for it behind.
    let cache_growth = i64::try_from(fs::namespace().cached()).unwrap_or(i64::MAX)
        - i64::try_from(cached).unwrap_or(i64::MAX);
    if cache_growth != 0 {
        crate::console::println!("  devfs    dentry cache {cache_growth:+} across the second run");
        return Err("the block registry check left dentries behind in the cache");
    }
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let leaked = window.kept();
    if leaked != 0 {
        crate::console::println!("  devfs    {leaked} frames across the second run");
        window.report("devfs");
    }
    if leaked < 0 {
        return Err(
            "the free frame count rose across the block registry check: something outside it freed frames in the window",
        );
    }
    if leaked > 0 {
        return Err("the block registry check kept frames it did not give back");
    }
    Ok(Report {
        major: MAJOR,
        minor: MINOR,
        sectors,
        leaked,
    })
}

/// One run: stage the page, check, and clean up whatever happened. Any
/// registration still held is dropped on the way out, failure or not.
fn check_once(process: &Process) -> Result<u64, &'static str> {
    let page = memory::sys_mmap(
        process,
        &MmapRequest {
            addr: 0,
            len: PAGE_SIZE,
            prot: PROT_READ | PROT_WRITE,
            flags: MAP_ANONYMOUS | MAP_PRIVATE,
            fd: -1,
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    )
    .map_err(|_| "a page for the block registry check was refused")?;
    let page = u64::try_from(page).map_err(|_| "mmap returned an impossible address")?;
    let staged = [
        (AT_DEV, DEV),
        (AT_NODE, NODE),
        (AT_PARTITIONS, PROC_PARTITIONS),
        (AT_WRITABLE_NODE, WRITABLE_NODE),
    ]
    .into_iter()
    .try_for_each(|(at, bytes)| uaccess::copy_to_user(process.space(), page + at, bytes))
    .map_err(|_| "could not stage the block registry check");

    let disk = Arc::new(CheckDisk::default());
    let outcome = staged.and_then(|()| check_the_disk(process, page, &disk));

    for fd in 3..32 {
        let _ = fd::sys_close(process, fd);
    }
    let _ = memory::sys_munmap(process, page, PAGE_SIZE);
    outcome
}

/// Steps 1 to 7: registered, seen, read, opened as files, refused, gone,
/// dropped.
fn check_the_disk(
    process: &Process,
    page: u64,
    disk: &Arc<CheckDisk>,
) -> Result<u64, &'static str> {
    let registration = devfs::register_block(NAME, MAJOR, MINOR, as_device(disk))
        .map_err(|_| "the check disk's registration was refused")?;
    if registration.rdev() != makedev(MAJOR, MINOR) {
        return Err("a registration does not carry the number it was made with");
    }

    // A second disk comes part-way through a listing: every static node must
    // still be listed once, and both disks after them.
    let mut second = None;
    let listed = list_dev(process, page, &mut || {
        second = Some(
            devfs::register_block(SECOND, MAJOR, MINOR + 1, as_device(disk))
                .map_err(|_| "a second disk's registration was refused")?,
        );
        Ok(())
    })?;
    let ino = expect_listing(
        &listed,
        &[NAME, SECOND],
        "a /dev listing in pieces did not list each character node once and then both disks",
    )?;
    drop(second);

    check_the_node(process, page, ino)?;
    let sectors = check_found_by_number(disk)?;
    if partitions(process, page)? != PARTITIONS {
        return Err("/proc/partitions is not exactly the check disk's row, as Linux prints it");
    }
    check_read_as_a_file(process, page)?;
    check_written_as_a_file(process, page)?;
    check_refusals(disk)?;

    let held = devfs::block_device(registration.rdev())
        .ok_or("block_device lost the check disk before its registration was dropped")?;
    disk.gone.store(true, Ordering::Release);
    let mut buf = vec![0_u8; SECTOR as usize];
    if held.read(FIRST_READ, &mut buf) != Err(Errno::EIO) {
        return Err("a disk marked gone did not answer a read with EIO");
    }

    check_dropped(process, page, registration, &held)?;
    Ok(sectors)
}

/// Step 2: `statx` describes the node, and `O_PATH` opens it.
fn check_the_node(process: &Process, page: u64, listed_ino: u64) -> Result<(), &'static str> {
    let size = size_of::<Statx>();
    let basic = u64::from(STATX_BASIC_STATS);
    answers(
        by_number(
            process,
            Syscall::Statx,
            [CWD, page + AT_NODE, 0, basic, page + AT_STATX, 0],
        ),
        0,
        "statx of the check disk's node was refused",
    )?;
    let record = read_back(process, page + AT_STATX, size)?;
    let field = |at, width| le(&record, at, width).unwrap_or(u64::MAX);
    if field(offset_of!(Statx, stx_mode), 2) != u64::from(S_IFBLK | 0o660) {
        return Err("statx does not report the disk's node as a block device with mode 0660");
    }
    if field(offset_of!(Statx, stx_rdev_major), 4) != u64::from(MAJOR)
        || field(offset_of!(Statx, stx_rdev_minor), 4) != u64::from(MINOR)
    {
        return Err("statx does not report the number the disk was registered with");
    }
    // Linux reports a block special file's size and blocks as zero: the disk's
    // size is the disk's to answer, not its node's.
    if field(offset_of!(Statx, stx_size), 8) != 0 || field(offset_of!(Statx, stx_blocks), 8) != 0 {
        return Err("statx reports a size or blocks for a block node, which Linux reports as 0");
    }
    if field(offset_of!(Statx, stx_ino), 8) != listed_ino {
        return Err("the /dev listing and statx disagree about the disk's inode number");
    }

    let path = descriptor(
        by_number(
            process,
            Syscall::Openat,
            [CWD, page + AT_NODE, u64::from(O_PATH), 0, 0, 0],
        ),
        "O_PATH did not open the check disk's node",
    )?;
    answers(
        by_number(process, Syscall::Close, [path, 0, 0, 0, 0, 0]),
        0,
        "an O_PATH descriptor of the check disk's node would not close",
    )
}

/// Step 4a: the read-only check disk opened as a file. A read that starts
/// and ends inside sectors is the pattern's bytes; the end is its size; a
/// read at the end is empty and one over it short; the `BLK*` requests
/// answer its geometry, a request it does not know is `ENOTTY`; and it will
/// not open for writing.
fn check_read_as_a_file(process: &Process, page: u64) -> Result<(), &'static str> {
    let size = SECTORS * u64::from(SECTOR);
    let fd = descriptor(
        by_number(
            process,
            Syscall::Openat,
            [CWD, page + AT_NODE, u64::from(O_RDONLY), 0, 0, 0],
        ),
        "the check disk's node would not open for reading",
    )?;
    let outcome = (|| {
        answers(
            by_number(
                process,
                Syscall::Pread64,
                positioned(fd, page + AT_DATA, PARTIAL_LEN, PARTIAL_AT),
            ),
            PARTIAL_LEN as usize,
            "a pread inside the check disk did not read all it asked",
        )?;
        let got = read_back(process, page + AT_DATA, PARTIAL_LEN as usize)?;
        let size_of_sector = u64::from(SECTOR);
        let want = (PARTIAL_AT..PARTIAL_AT + PARTIAL_LEN)
            .map(|at| pattern(at / size_of_sector, (at % size_of_sector) as usize));
        if !got.iter().copied().eq(want) {
            return Err("a pread that starts and ends inside sectors read the wrong bytes");
        }
        answers(
            by_number(
                process,
                Syscall::Lseek,
                [fd, 0, u64::from(SEEK_END), 0, 0, 0],
            ),
            size as usize,
            "lseek to the end of the check disk did not land at its size",
        )?;
        answers(
            by_number(
                process,
                Syscall::Pread64,
                positioned(fd, page + AT_DATA, 16, size),
            ),
            0,
            "a pread at the end of the check disk was not empty",
        )?;
        answers(
            by_number(
                process,
                Syscall::Pread64,
                positioned(fd, page + AT_DATA, 100, size - 10),
            ),
            10,
            "a pread over the end of the check disk was not cut at the end",
        )?;
        let ask = |request: u32| {
            by_number(
                process,
                Syscall::Ioctl,
                [fd, u64::from(request), page + AT_DATA, 0, 0, 0],
            )
        };
        answers(
            ask(BLKGETSIZE64),
            0,
            "BLKGETSIZE64 on the check disk was refused",
        )?;
        if le(&read_back(process, page + AT_DATA, 8)?, 0, 8) != Some(size) {
            return Err("BLKGETSIZE64 did not answer the check disk's size in bytes");
        }
        answers(ask(BLKSSZGET), 0, "BLKSSZGET on the check disk was refused")?;
        if le(&read_back(process, page + AT_DATA, 4)?, 0, 4) != Some(u64::from(SECTOR)) {
            return Err("BLKSSZGET did not answer the check disk's sector size");
        }
        answers(ask(BLKROGET), 0, "BLKROGET on the check disk was refused")?;
        if le(&read_back(process, page + AT_DATA, 4)?, 0, 4) != Some(1) {
            return Err("BLKROGET did not say the read-only check disk is read-only");
        }
        refuses(
            ask(BLKRRPART),
            Errno::EINVAL,
            "BLKRRPART was not EINVAL with no partition tables",
        )?;
        refuses(
            ask(TCGETS),
            Errno::ENOTTY,
            "a terminal's request on a disk was not ENOTTY",
        )
    })();
    let closed = by_number(process, Syscall::Close, [fd, 0, 0, 0, 0, 0]);
    outcome?;
    answers(closed, 0, "the check disk's descriptor would not close")?;
    refuses(
        by_number(
            process,
            Syscall::Openat,
            [CWD, page + AT_NODE, u64::from(O_RDWR), 0, 0, 0],
        ),
        Errno::EACCES,
        "opening the read-only check disk for writing was not EACCES",
    )
}

/// Step 4b: a writable disk opened as a file. A write that patches two
/// sectors in part leaves the rest of them as they were; one over the end is
/// cut short, and one at the end is `ENOSPC`; `fsync` flushes the disk; and
/// what is read back is what was written.
fn check_written_as_a_file(process: &Process, page: u64) -> Result<(), &'static str> {
    const AT: u64 = 300;
    const LEN: u64 = 700;
    let size = WRITABLE_SECTORS * u64::from(SECTOR);
    let disk = Arc::new(MemoryDisk::new());
    let registration = devfs::register_block(
        WRITABLE,
        MAJOR,
        WRITABLE_MINOR,
        Arc::clone(&disk) as Arc<dyn BlockDevice>,
    )
    .map_err(|_| "the writable check disk's registration was refused")?;
    let written: Vec<u8> = (0..LEN).map(|at| (at as u8).wrapping_mul(7) | 1).collect();
    uaccess::copy_to_user(process.space(), page + AT_DATA, &written)
        .map_err(|_| "could not stage the bytes to write")?;
    let fd = descriptor(
        by_number(
            process,
            Syscall::Openat,
            [CWD, page + AT_WRITABLE_NODE, u64::from(O_RDWR), 0, 0, 0],
        ),
        "the writable check disk would not open for writing",
    )?;
    let outcome = (|| {
        answers(
            by_number(
                process,
                Syscall::Pwrite64,
                positioned(fd, page + AT_DATA, LEN, AT),
            ),
            LEN as usize,
            "a pwrite that patches two sectors in part did not write all it was given",
        )?;
        answers(
            by_number(
                process,
                Syscall::Pwrite64,
                positioned(fd, page + AT_DATA, 10, size - 4),
            ),
            4,
            "a pwrite over the end of a disk was not cut at the end",
        )?;
        refuses(
            by_number(
                process,
                Syscall::Pwrite64,
                positioned(fd, page + AT_DATA, 10, size),
            ),
            Errno::ENOSPC,
            "a pwrite at the end of a disk was not ENOSPC",
        )?;
        answers(
            by_number(process, Syscall::Fsync, [fd, 0, 0, 0, 0, 0]),
            0,
            "fsync of a disk was refused",
        )?;
        if disk.flushes.load(Ordering::Relaxed) == 0 {
            return Err("fsync of a disk did not flush it");
        }
        answers(
            by_number(
                process,
                Syscall::Pread64,
                positioned(fd, page + AT_DATA, size, 0),
            ),
            size as usize,
            "the writable check disk did not read back whole",
        )?;
        let got = read_back(process, page + AT_DATA, size as usize)?;
        let want = (0..size).map(|at| match at {
            at if (AT..AT + LEN).contains(&at) => {
                written.get((at - AT) as usize).copied().unwrap_or(0)
            }
            at if at >= size - 4 => written
                .get((at - (size - 4)) as usize)
                .copied()
                .unwrap_or(0),
            _ => 0,
        });
        if !got.iter().copied().eq(want) {
            return Err(
                "a disk written as a file did not read back as written, around the writes too",
            );
        }
        Ok(())
    })();
    let closed = by_number(process, Syscall::Close, [fd, 0, 0, 0, 0, 0]);
    drop(registration);
    outcome?;
    answers(
        closed,
        0,
        "the writable check disk's descriptor would not close",
    )
}

/// Step 3: `block_device` finds the disk by number, and its sectors read back.
fn check_found_by_number(disk: &Arc<CheckDisk>) -> Result<u64, &'static str> {
    let found = devfs::block_device(makedev(MAJOR, MINOR))
        .ok_or("block_device did not find the check disk by its number")?;
    if !core::ptr::addr_eq(Arc::as_ptr(&found), Arc::as_ptr(disk)) {
        return Err("block_device found some other device for the check disk's number");
    }
    if devfs::block_device(makedev(MAJOR, MINOR + 2)).is_some()
        || devfs::block_device(makedev(1, 3)).is_some()
    {
        return Err("block_device found a disk for a number no disk was registered with");
    }
    if found.sectors() != SECTORS || found.sector_size() != SECTOR || !found.read_only() {
        return Err("the device block_device found does not describe the check disk");
    }
    let size = SECTOR as usize;
    let mut buf = vec![0_u8; READ_SECTORS as usize * size];
    found
        .read(FIRST_READ, &mut buf)
        .map_err(|_| "reading sectors 3 and 4 through block_device failed")?;
    let expected =
        |(at, byte): (usize, &u8)| *byte == pattern(FIRST_READ + (at / size) as u64, at % size);
    if !buf.iter().enumerate().all(expected) {
        return Err("sectors 3 and 4 did not read back the check disk's pattern");
    }
    if found.read(SECTORS - 1, &mut buf).is_ok() {
        return Err("a read running past the check disk's end was not an error");
    }
    Ok(READ_SECTORS)
}

/// Step 5: a bad name, a name in use and a number in use are refused, and
/// in that order when more than one applies.
fn check_refusals(disk: &Arc<CheckDisk>) -> Result<(), &'static str> {
    let refused =
        |name: &[u8], minor: u32| devfs::register_block(name, MAJOR, minor, as_device(disk)).err();
    let too_long = [b'a'; 33];
    let cases: [(&[u8], u32, BlockRefused, &'static str); 8] = [
        (
            NAME,
            MINOR + 3,
            BlockRefused::NameInUse,
            "a disk's name in use was not NameInUse",
        ),
        (
            b"ferrixcheck9",
            MINOR,
            BlockRefused::NumberInUse,
            "a number in use was not NumberInUse",
        ),
        (
            b"null",
            MINOR + 3,
            BlockRefused::NameInUse,
            "a character node's name was not NameInUse",
        ),
        (
            NAME,
            MINOR,
            BlockRefused::NameInUse,
            "a name and a number in use was not NameInUse",
        ),
        (
            b"",
            MINOR + 3,
            BlockRefused::InvalidName,
            "an empty name was not InvalidName",
        ),
        (
            b"Vda",
            MINOR + 3,
            BlockRefused::InvalidName,
            "an uppercase name was not InvalidName",
        ),
        (
            &too_long,
            MINOR + 3,
            BlockRefused::InvalidName,
            "a 33-byte name was not InvalidName",
        ),
        (
            b"vd/a",
            MINOR,
            BlockRefused::InvalidName,
            "a bad name with a number in use was not InvalidName",
        ),
    ];
    for (name, minor, want, what) in cases {
        if refused(name, minor) != Some(want) {
            return Err(what);
        }
    }
    Ok(())
}

/// Step 7: the registration dropped part-way through a listing, the disk is
/// gone from it, from lookup, from `block_device` and from
/// `/proc/partitions`, and the device still held answers `EIO`.
fn check_dropped(
    process: &Process,
    page: u64,
    registration: devfs::BlockRegistration,
    held: &Arc<dyn BlockDevice>,
) -> Result<(), &'static str> {
    let rdev = registration.rdev();
    let mut registration = Some(registration);
    let listed = list_dev(process, page, &mut || {
        drop(registration.take());
        Ok(())
    })?;
    if registration.is_some() {
        return Err("a /dev listing ended before the check could drop the registration");
    }
    let _ = expect_listing(
        &listed,
        &[],
        "a /dev listing in pieces across a dropped registration did not list each character node once, and no disk",
    )?;
    let _ = expect_listing(
        &list_dev(process, page, &mut || Ok(()))?,
        &[],
        "/dev still lists the check disk once its registration was dropped",
    )?;
    refuses(
        by_number(
            process,
            Syscall::Openat,
            [CWD, page + AT_NODE, u64::from(O_PATH), 0, 0, 0],
        ),
        Errno::ENOENT,
        "the check disk's node is still found once its registration was dropped",
    )?;
    if devfs::block_device(rdev).is_some() {
        return Err("block_device still finds the check disk once its registration was dropped");
    }
    if !partitions(process, page)?.is_empty() {
        return Err(
            "/proc/partitions still has a row once the check disk's registration was dropped",
        );
    }
    let mut buf = vec![0_u8; SECTOR as usize];
    if held.read(FIRST_READ, &mut buf) != Err(Errno::EIO) {
        return Err("the device still held after its registration was dropped did not answer EIO");
    }
    Ok(())
}

/// List `/dev` by `getdents64`, a few entries a call, running `between` after
/// the first call. The dot entries are left out.
fn list_dev(
    process: &Process,
    page: u64,
    between: &mut dyn FnMut() -> Result<(), &'static str>,
) -> Result<Listed, &'static str> {
    let dir = descriptor(
        by_number(
            process,
            Syscall::Openat,
            [CWD, page + AT_DEV, u64::from(O_RDONLY), 0, 0, 0],
        ),
        "/dev did not open to be listed",
    )?;
    let listed = read_listing(process, page, dir, between);
    let closed = by_number(process, Syscall::Close, [dir, 0, 0, 0, 0, 0]);
    let listed = listed?;
    answers(closed, 0, "/dev would not close after listing")?;
    Ok(listed)
}

/// The `getdents64` calls of [`list_dev`].
fn read_listing(
    process: &Process,
    page: u64,
    dir: u64,
    between: &mut dyn FnMut() -> Result<(), &'static str>,
) -> Result<Listed, &'static str> {
    let mut listed = Vec::new();
    for call in 0..LISTING_CALLS {
        if call == 1 {
            between()?;
        }
        let used = by_number(
            process,
            Syscall::Getdents64,
            [dir, page + AT_LISTING, LISTING_PIECE, 0, 0, 0],
        )
        .map_err(|_| "getdents64 of /dev was refused")?;
        if used == 0 {
            return Ok(listed);
        }
        let records = read_back(process, page + AT_LISTING, used)?;
        listed.extend(
            dirent::records(&records)
                .filter(|record| record.name != b"." && record.name != b"..")
                .map(|record| (record.name.to_vec(), record.kind, record.ino)),
        );
    }
    Err("listing /dev a few entries at a time never ended")
}

/// Require a listing to be the character nodes, each once and in order, then
/// `disks`, each once and in order, then `shm` and the links, with the kinds
/// they have.
/// Returns the first disk's inode number, or 0 with none.
fn expect_listing(
    listed: &Listed,
    disks: &[&[u8]],
    what: &'static str,
) -> Result<u64, &'static str> {
    let names = listed.iter().map(|(name, _, _)| name.as_slice());
    let want = STATIC_NODES
        .iter()
        .chain(disks.iter())
        .chain(core::iter::once(&SHM))
        .chain(LINKS.iter())
        .copied();
    if !names.eq(want) {
        return Err(what);
    }
    let last = STATIC_NODES.len() + disks.len();
    let kinds_right = listed.iter().enumerate().all(|(at, (_, kind, _))| {
        *kind
            == if at < STATIC_NODES.len() {
                DT_CHR
            } else if at < last {
                DT_BLK
            } else if at == last {
                DT_DIR
            } else {
                DT_LNK
            }
    });
    if !kinds_right {
        return Err("a /dev listing gave a node the wrong d_type");
    }
    Ok(listed.get(STATIC_NODES.len()).map_or(0, |(_, _, ino)| *ino))
}

/// `/proc/partitions`, read by `openat` and `read` a piece at a time.
fn partitions(process: &Process, page: u64) -> Result<Vec<u8>, &'static str> {
    let file = descriptor(
        by_number(
            process,
            Syscall::Openat,
            [CWD, page + AT_PARTITIONS, u64::from(O_RDONLY), 0, 0, 0],
        ),
        "/proc/partitions did not open",
    )?;
    let text = read_text(process, page, file);
    let closed = by_number(process, Syscall::Close, [file, 0, 0, 0, 0, 0]);
    let text = text?;
    answers(closed, 0, "/proc/partitions would not close")?;
    Ok(text)
}

/// The `read` calls of [`partitions`].
fn read_text(process: &Process, page: u64, file: u64) -> Result<Vec<u8>, &'static str> {
    let mut text = Vec::new();
    for _ in 0..LISTING_CALLS {
        let count = by_number(
            process,
            Syscall::Read,
            [file, page + AT_TEXT, TEXT_PIECE, 0, 0, 0],
        )
        .map_err(|_| "/proc/partitions could not be read")?;
        if count == 0 {
            return Ok(text);
        }
        text.extend_from_slice(&read_back(process, page + AT_TEXT, count)?);
    }
    Err("reading /proc/partitions never reached its end")
}

/// Make `call` by its number, as a program on this architecture would.
fn by_number(process: &Process, call: Syscall, args: [u64; 6]) -> Result<usize, Errno> {
    syscall_check::call_by_number(process, call, args)
}

/// `pread64` and `pwrite64`'s registers as a program on this architecture
/// passes them: on a 32-bit one the 64-bit offset is a pair of registers
/// starting at an even one, so the fourth is padding.
fn positioned(fd: u64, buf: u64, len: u64, offset: u64) -> [u64; 6] {
    if size_of::<usize>() == 8 {
        [fd, buf, len, offset, 0, 0]
    } else {
        [fd, buf, len, 0, offset & 0xFFFF_FFFF, offset >> 32]
    }
}

/// Require a handler to have answered `want`.
fn answers(got: Result<usize, Errno>, want: usize, what: &'static str) -> Result<(), &'static str> {
    if got == Ok(want) { Ok(()) } else { Err(what) }
}

/// Require a handler to have refused with `errno`.
fn refuses(
    got: Result<usize, Errno>,
    errno: Errno,
    what: &'static str,
) -> Result<(), &'static str> {
    if got == Err(errno) { Ok(()) } else { Err(what) }
}

/// A descriptor a handler returned, as a register carries it.
fn descriptor(got: Result<usize, Errno>, what: &'static str) -> Result<u64, &'static str> {
    got.ok().map(|fd| fd as u64).ok_or(what)
}

/// `len` bytes of the process's memory at `at`.
fn read_back(process: &Process, at: u64, len: usize) -> Result<Vec<u8>, &'static str> {
    let mut out = vec![0_u8; len];
    uaccess::copy_from_user(process.space(), at, &mut out)
        .map_err(|_| "the block registry check could not read its own page back")?;
    Ok(out)
}

/// A little-endian field `width` bytes wide at `at`.
fn le(bytes: &[u8], at: usize, width: usize) -> Option<u64> {
    let mut word = [0_u8; 8];
    word.get_mut(..width)?
        .copy_from_slice(bytes.get(at..at.checked_add(width)?)?);
    Some(u64::from_le_bytes(word))
}
