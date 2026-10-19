//! Stage 12's exit, the other half: a machine switched off in the middle of
//! a transaction, and a mount that has to make sense of what is on the disk.
//!
//! The host drives it. `cargo xtask test-powerfail` boots with
//! `ferrix.btrfs=churn`, waits for the marker below, kills QEMU at a random
//! moment, runs host `btrfs check` over the image, boots the same image again
//! with `ferrix.btrfs=replay` — which mounts it, and mounting replays
//! whatever log the crash left — and runs `btrfs check` once more, which must
//! find nothing at all.
//!
//! # What the guest can judge by itself
//!
//! Not much about *which* version of a file it should find: it is a fresh
//! machine, and everything it knew died with the last one. So the churn
//! writes files that can be judged on their own. Each pass writes a file's
//! body, makes it durable, and only then appends a 32-byte trailer naming
//! the body's length and its CRC-32C, durable in its turn. A file that ends
//! in a well-formed trailer is therefore a file whose body was promised
//! before that trailer was: its bytes must be the ones the trailer counts,
//! and if they are not, something rolled a completed promise back. A file
//! with no trailer was being written when the power went, and POSIX promises
//! nothing about it, so nothing is asked of it.
//!
//! That is the same rule the host-side crash tests in `src/lib/fs/btrfs/write`
//! apply, at a thousand cuts a second rather than one a boot; this half is
//! what puts a real driver, a real block ring and a real host filesystem's
//! flushes under it.

use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use ferrix_bootinfo::BootView;
use ferrix_btrfs::crc32c::crc32c_update;
use ferrix_vfs::initramfs::makedev;
use ferrix_vfs::{Errno, NewNode};

use crate::console::println;
use crate::fs;
use crate::interfaces::block_ring::VIRTIO_BLK_MAJOR;

/// Where the writable disk is mounted.
const MOUNT_POINT: &[u8] = b"/mnt-pf";

/// The writable disk: the third virtio-blk function, `vdc`.
const DISK_INDEX: u32 = 2;

/// How many files the churn keeps.
const FILES: u64 = 4;

/// The longest body it writes, and the size of a piece of one.
const MAX_BODY: u64 = 192 * 1024;
const PIECE: usize = 32 * 1024;

/// The trailer: a magic word, the body's length, the pass that wrote it, and
/// the CRC-32C of the body.
const TRAILER: usize = 32;
const MAGIC: u32 = 0x5046_5831;

/// Printed once the churn is writing, which is what the host waits for
/// before it starts the clock on the kill.
const CHURNING: &str = "  btrfs-pf churning on vdc";

/// The command-line options: the mode, and the seed that picks the churn.
const MODE_OPTION: &str = "ferrix.btrfs";
const SEED_OPTION: &str = "ferrix.btrfs.seed";

/// What the boot does with the writable disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Stage 12's ordinary check: build a tree, remount, read it back.
    Check,
    /// Write until the host pulls the plug.
    Churn,
    /// Mount what a crash left, and judge it.
    Replay,
}

/// The mode, as [`init`] read it: 0 check, 1 churn, 2 replay.
static MODE: AtomicU8 = AtomicU8::new(0);
static SEED: AtomicU64 = AtomicU64::new(1);

/// Read `ferrix.btrfs` and `ferrix.btrfs.seed` from the command line, once,
/// early, so a typo is reported at the top of the log.
pub(crate) fn init(view: &BootView<'_>) {
    match view.option(MODE_OPTION) {
        None => {}
        Some("churn") => MODE.store(1, Ordering::Relaxed),
        Some("replay") => MODE.store(2, Ordering::Relaxed),
        Some(other) => {
            println!("  btrfs-pf {MODE_OPTION}={other} is not understood; the ordinary check runs");
        }
    }
    if let Some(seed) = view.option(SEED_OPTION).and_then(|s| s.parse::<u64>().ok()) {
        SEED.store(seed, Ordering::Relaxed);
    }
}

/// The mode [`init`] read.
pub(crate) fn mode() -> Mode {
    match MODE.load(Ordering::Relaxed) {
        1 => Mode::Churn,
        2 => Mode::Replay,
        _ => Mode::Check,
    }
}

/// The seed [`init`] read.
pub(crate) fn seed() -> u64 {
    SEED.load(Ordering::Relaxed)
}

/// What a replay found.
#[derive(Debug)]
pub(crate) struct Report {
    /// Files the volume held after the replay.
    pub(crate) files: u32,
    /// Of those, the ones ending in a well-formed trailer, whose body was
    /// therefore promised and was checked against it.
    pub(crate) checked: u32,
    /// Whether the crash left a log for the mount to replay: the primary
    /// superblock's `log_root`, read before the mount replays and clears it.
    /// A cut between a log commit and the next commit leaves one.
    pub(crate) logged: bool,
    /// Why nothing was done: no third disk on this machine.
    pub(crate) skipped: Option<&'static str>,
}

/// What ends a file whose body was made durable: its length and its
/// CRC-32C, behind a magic word. The pass that wrote it is in there too, for
/// whoever reads an image by hand; the replay has no use for it.
struct Trailer {
    len: u64,
    crc: u32,
}

impl Trailer {
    /// The 32 bytes: magic, 4 spare, length, pass, CRC, 4 spare.
    fn encode(&self, seq: u64) -> [u8; TRAILER] {
        let mut out = [0u8; TRAILER];
        let fields: [(usize, &[u8]); 4] = [
            (0, &MAGIC.to_le_bytes()),
            (8, &self.len.to_le_bytes()),
            (16, &seq.to_le_bytes()),
            (24, &self.crc.to_le_bytes()),
        ];
        for (at, bytes) in fields {
            if let Some(slot) = out.get_mut(at..at.saturating_add(bytes.len())) {
                slot.copy_from_slice(bytes);
            }
        }
        out
    }

    /// A trailer, if `bytes` is exactly one with the right magic.
    fn decode(bytes: &[u8]) -> Option<Trailer> {
        let field =
            |at: usize| -> Option<[u8; 8]> { bytes.get(at..at.saturating_add(8))?.try_into().ok() };
        let word =
            |at: usize| -> Option<[u8; 4]> { bytes.get(at..at.saturating_add(4))?.try_into().ok() };
        if bytes.len() != TRAILER || u32::from_le_bytes(word(0)?) != MAGIC {
            return None;
        }
        Some(Trailer {
            len: u64::from_le_bytes(field(8)?),
            crc: u32::from_le_bytes(word(24)?),
        })
    }
}

/// The path of file `which`.
fn path(which: u64) -> Vec<u8> {
    let mut out = MOUNT_POINT.to_vec();
    out.extend_from_slice(b"/f");
    out.push(b'0'.wrapping_add((which % 10) as u8));
    out
}

/// xorshift64*, so a seed on the command line picks the whole run.
fn next(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// The byte at `offset` of the body written by pass `seq`.
fn byte(seq: u64, offset: usize) -> u8 {
    let mixed = (offset as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ seq.wrapping_mul(0x9E37);
    (mixed >> 24) as u8
}

/// Mount the writable disk, making the mount point if it is not there.
///
/// Mounting is where a log left by a crash is replayed, so this is the whole
/// of the recovery: everything after it reads a volume that is already whole.
fn mount() -> Result<(), &'static str> {
    let rdev = makedev(VIRTIO_BLK_MAJOR, DISK_INDEX * 16);
    let ns = fs::namespace();
    let ctx = ns.context();
    match ns.mkdir(&ctx, None, MOUNT_POINT, 0o755) {
        Ok(()) | Err(Errno::EEXIST) => {}
        Err(_) => return Err("the mount point could not be made"),
    }
    let at = ns
        .resolve(&ctx, None, MOUNT_POINT, true)
        .map_err(|_| "the mount point could not be resolved")?;
    let volume = fs::btrfs::mount_rw(rdev).map_err(|_| "the disk would not mount writable")?;
    let _ = ns
        .mount(volume, &at)
        .map_err(|_| "the volume could not be mounted")?;
    Ok(())
}

/// Whether this machine has the disk at all.
fn served() -> bool {
    fs::devfs::block_device(makedev(VIRTIO_BLK_MAJOR, DISK_INDEX * 16)).is_some()
}

/// Make file `which` hold `len` bytes of pass `seq`, and make that durable.
///
/// The body first, made durable on its own, and then the trailer: a reader
/// that finds the trailer knows the body under it was promised first.
fn pass(which: u64, seq: u64, len: u64) -> Result<(), &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    let name = path(which);
    match ns.mknod(&ctx, None, &name, NewNode::Regular, 0o644) {
        Ok(()) | Err(Errno::EEXIST) => {}
        Err(_) => return Err("a file could not be created"),
    }
    let at = ns
        .resolve(&ctx, None, &name, true)
        .map_err(|_| "a file of the churn is gone")?;
    ns.truncate(&at, 0).map_err(|_| "a file would not empty")?;
    let inode = at.inode().map_err(|_| "a file of the churn has no inode")?;
    let mut register = !0u32;
    let mut done = 0u64;
    while done < len {
        let take = PIECE.min((len - done) as usize);
        let piece: Vec<u8> = (0..take).map(|i| byte(seq, done as usize + i)).collect();
        let (written, _) = inode
            .write_at(done, &piece, false)
            .map_err(|_| "a write of the churn failed")?;
        if written != take {
            return Err("a write of the churn was short");
        }
        register = crc32c_update(register, &piece);
        done = done.saturating_add(take as u64);
    }
    inode
        .fsync(false)
        .map_err(|_| "a body could not be made durable")?;
    let trailer = Trailer {
        len,
        crc: !register,
    }
    .encode(seq);
    let (written, _) = inode
        .write_at(len, &trailer, false)
        .map_err(|_| "a trailer could not be written")?;
    if written != TRAILER {
        return Err("a trailer was written short");
    }
    inode
        .fsync(false)
        .map_err(|_| "a trailer could not be made durable")
}

/// Write until the power goes, which is what the host is about to do.
///
/// # Errors
///
/// Anything that fails before the kill, which is a failure of the write path
/// and not of the test.
pub(crate) fn churn(seed: u64) -> Result<Report, &'static str> {
    if !served() {
        return Ok(Report {
            files: 0,
            checked: 0,
            logged: false,
            skipped: Some("no third disk is served"),
        });
    }
    mount()?;
    println!("{CHURNING}");
    let mut state = seed | 1;
    let mut seq = 0u64;
    loop {
        seq = seq.saturating_add(1);
        let which = next(&mut state) % FILES;
        let len = next(&mut state) % MAX_BODY;
        pass(which, seq, len)?;
        // Now and then the whole volume is committed rather than logged, so
        // the cut lands inside both kinds of promise.
        if seq.is_multiple_of(8) {
            let ns = fs::namespace();
            let ctx = ns.context();
            let place = ns
                .resolve(&ctx, None, MOUNT_POINT, true)
                .map_err(|_| "the mount point could not be resolved")?;
            place
                .mount
                .filesystem()
                .sync()
                .map_err(|_| "the volume could not be synced")?;
        }
    }
}

/// Mount what the crash left — which replays its log — and judge every file
/// that ends in a trailer.
///
/// # Errors
///
/// A body that does not match the trailer over it: a promise rolled back.
pub(crate) fn replay() -> Result<Report, &'static str> {
    let mut report = Report {
        files: 0,
        checked: 0,
        logged: false,
        skipped: None,
    };
    if !served() {
        report.skipped = Some("no third disk is served");
        return Ok(report);
    }
    // The disk is untouched until here, which the host reads as: a boot that
    // died before this line may be tried again.
    println!("  btrfs-pf replaying vdc");
    report.logged = log_root()? != 0;
    mount()?;
    let ns = fs::namespace();
    let ctx = ns.context();
    for which in 0..FILES {
        let name = path(which);
        let Ok(at) = ns.resolve(&ctx, None, &name, true) else {
            continue;
        };
        report.files = report.files.saturating_add(1);
        let inode = at.inode().map_err(|_| "a file of the churn has no inode")?;
        let size = inode.metadata().size;
        if size < TRAILER as u64 {
            continue;
        }
        let body = size - TRAILER as u64;
        let mut trailer = vec![0u8; TRAILER];
        let read = inode
            .read_at(body, &mut trailer)
            .map_err(|_| "a trailer could not be read")?;
        let found = trailer.get(..read).and_then(Trailer::decode);
        let Some(found) = found.filter(|found| found.len == body) else {
            // No trailer: the file was being written when the power went,
            // and nothing was promised about it.
            continue;
        };
        if checksum(&inode, body)? != found.crc {
            return Err("a file's bytes are not the ones its trailer promised");
        }
        report.checked = report.checked.saturating_add(1);
    }
    // Leave the volume as a clean mount leaves it, so host `btrfs check`
    // judges a filesystem nobody is holding open.
    let place = ns
        .resolve(&ctx, None, MOUNT_POINT, true)
        .map_err(|_| "the mount point could not be resolved")?;
    place
        .mount
        .filesystem()
        .sync()
        .map_err(|_| "the volume could not be synced")?;
    ns.unmount(&place)
        .map_err(|_| "the volume could not be unmounted")?;
    Ok(report)
}

/// The primary superblock's `log_root`, read off the disk itself.
fn log_root() -> Result<u64, &'static str> {
    /// The primary superblock's sector, and `log_root`'s offset in it.
    const SUPERBLOCK_SECTOR: u64 = 0x1_0000 / 512;
    const LOG_ROOT: usize = 96;
    let device = fs::devfs::block_device(makedev(VIRTIO_BLK_MAJOR, DISK_INDEX * 16))
        .ok_or("the disk went away")?;
    let mut block = vec![0u8; 4096];
    device
        .read(SUPERBLOCK_SECTOR, &mut block)
        .map_err(|_| "the superblock could not be read")?;
    let field = block
        .get(LOG_ROOT..LOG_ROOT + 8)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or("the superblock is short")?;
    Ok(u64::from_le_bytes(field))
}

/// The CRC-32C of the first `len` bytes of a file, a piece at a time.
fn checksum(
    inode: &alloc::sync::Arc<dyn ferrix_vfs::Inode>,
    len: u64,
) -> Result<u32, &'static str> {
    let mut buffer = vec![0u8; PIECE];
    let mut register = !0u32;
    let mut at = 0u64;
    while at < len {
        let take = PIECE.min((len - at) as usize);
        let read = inode
            .read_at(at, buffer.get_mut(..take).unwrap_or_default())
            .map_err(|_| "a file of the churn could not be read")?;
        if read == 0 {
            return Err("a file of the churn is shorter than its trailer says");
        }
        register = crc32c_update(register, buffer.get(..read).unwrap_or_default());
        at = at.saturating_add(read as u64);
    }
    Ok(!register)
}
