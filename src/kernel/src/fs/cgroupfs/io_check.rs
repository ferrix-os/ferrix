//! Stage 13's `io` controller, landing I (`docs/CGROUPS.md` §13, `fs::blkio`).
//!
//! A disk of 1 MiB in memory is registered as a driver's disk is, wrapped so
//! that its reads and writes are charged. Then, as tasks of cgroups:
//!
//! * `/check-i` reads three 4 KiB pieces and writes two: its `io.stat` says
//!   `rbytes=12288 wbytes=8192 rios=3 wios=2` for the disk, in Linux's format;
//!   the root's counts those too, and a sibling `/check-is` that read one
//!   sector has only its own;
//! * `io.max` reads back what was written, keeps the words a write leaves
//!   out, is `ENODEV` for a disk that is not there and `EINVAL` and `ERANGE`
//!   as `tg_set_limit` answers, and the root has none;
//! * with `rbps=16384`, four reads of 4 KiB take the better part of a
//!   second, and with the limit lifted they take a blink;
//! * a limit on a parent holds for its child's reads (`riops=4`), and the
//!   parent's `io.stat` counts them;
//! * `io` is listed in `cgroup.controllers`, and once the cgroups and their
//!   processes are gone every quota slot they held is back.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_vfs::initramfs::makedev;

use super::reclaim_check::Group;
use super::{Checked, Harness};
use crate::fs::blkio;
use crate::fs::block::BlockDevice;
use crate::fs::devfs::{self, Origin};
use crate::sync::SpinLock;

/// The disk's major number: one no driver of Ferrix's uses.
const MAJOR: u32 = 251;
/// Its minor.
const MINOR: u32 = 0;
/// Its sectors, of 512 bytes: 1 MiB.
const SECTORS: u64 = 2048;
/// One piece of I/O: 4 KiB.
const PIECE: usize = 4096;

/// A disk in memory.
#[derive(Debug)]
struct RamDisk {
    /// Its bytes.
    bytes: SpinLock<Vec<u8>>,
}

impl BlockDevice for RamDisk {
    fn read(&self, sector: u64, buf: &mut [u8]) -> Result<(), Errno> {
        let from = usize::try_from(sector * 512).map_err(|_| Errno::EIO)?;
        let bytes = self.bytes.lock();
        let source = bytes.get(from..from + buf.len()).ok_or(Errno::EIO)?;
        buf.copy_from_slice(source);
        Ok(())
    }

    fn sectors(&self) -> u64 {
        SECTORS
    }

    fn sector_size(&self) -> u32 {
        512
    }

    fn read_only(&self) -> bool {
        false
    }

    fn write(&self, sector: u64, buf: &[u8]) -> Result<(), Errno> {
        let from = usize::try_from(sector * 512).map_err(|_| Errno::EIO)?;
        let mut bytes = self.bytes.lock();
        let target = bytes.get_mut(from..from + buf.len()).ok_or(Errno::EIO)?;
        target.copy_from_slice(buf);
        Ok(())
    }
}

/// The disk as a mount would find it.
fn disk() -> Checked<Arc<dyn BlockDevice>> {
    devfs::block_device(makedev(MAJOR, MINOR)).ok_or("io check: the disk is not registered")
}

/// `pieces` reads of one piece each, as the task of `group`, and how long they
/// took, in milliseconds.
fn reads(group: &Group, disk: &Arc<dyn BlockDevice>, pieces: usize) -> Checked<u64> {
    let started = crate::timer::now_nanos();
    group
        .as_task(|| {
            let mut buf = [0_u8; PIECE];
            (0..pieces).try_for_each(|piece| disk.read((piece * PIECE / 512) as u64, &mut buf))
        })
        .map_err(|_| "io check: a read of the disk failed")?;
    Ok((crate::timer::now_nanos() - started) / 1_000_000)
}

/// `io.stat` of the cgroup at `tail`, the line for our disk.
fn line(harness: &Harness, tail: &[u8]) -> Checked<Vec<u8>> {
    let text = harness
        .read(tail)
        .map_err(|_| "io check: io.stat did not read")?;
    Ok(text
        .split(|&byte| byte == b'\n')
        .find(|line| line.starts_with(b"251:0 "))
        .map(<[u8]>::to_vec)
        .unwrap_or_default())
}

/// Run it. How many requests the controller charged.
///
/// # Errors
///
/// The first thing that was not as `docs/CGROUPS.md` §13 has it, by name.
pub(super) fn run(harness: &mut Harness) -> Checked<u32> {
    let slots = crate::object::quota::live_slots();
    let ram = Arc::new(RamDisk {
        bytes: SpinLock::new(alloc::vec![0; (SECTORS * 512) as usize]),
    });
    let registration = devfs::register_block_from(
        b"iochk0",
        MAJOR,
        MINOR,
        blkio::account_disk(ram, MAJOR, MINOR),
        Origin::default(),
    )
    .map_err(|_| "io check: the disk would not register")?;
    let _ = harness
        .write(b"/cgroup.subtree_control", b"+io\n")
        .map_err(|_| "the root refused to enable io for the io check")?;
    let outcome = requests(harness);
    let disabled = harness.write(b"/cgroup.subtree_control", b"-io\n");
    drop(registration);
    let counted = outcome?;
    let _ = disabled.map_err(|_| "the root refused to disable io after the io check")?;
    if crate::object::quota::live_slots() != slots {
        return Err("the io check's cgroups are gone and their quota slots are not");
    }
    Ok(counted)
}

/// Everything the check claims, in its cgroups.
fn requests(harness: &mut Harness) -> Checked<u32> {
    let listed = harness
        .read(b"/cgroup.controllers")
        .map_err(|_| "io check: cgroup.controllers did not read")?;
    if listed != b"cpu io memory pids\n" {
        return Err("the root's cgroup.controllers is not cpu io memory pids");
    }
    let job = Group::make(harness, b"/check-i")?;
    let sibling = Group::make(harness, b"/check-is")?;
    let disk = disk()?;
    let outcome = accounting(harness, &job, &sibling, &disk)
        .and_then(|counted| Ok((counted, limits(harness, &job, &disk)?)));
    let rest = outcome.and_then(|(counted, spaced)| {
        let parent = hierarchy(harness, &disk)?;
        Ok(counted + spaced + parent)
    });
    let _ = job.end(harness);
    let _ = sibling.end(harness);
    rest
}

/// `io.stat` in the job, its sibling and the root.
fn accounting(
    harness: &Harness,
    job: &Group,
    sibling: &Group,
    disk: &Arc<dyn BlockDevice>,
) -> Checked<u32> {
    if !harness.exists(&job.file("io.stat")) || !harness.exists(b"/io.stat") {
        return Err("a cgroup with io enabled, or the root, has no io.stat");
    }
    if harness.exists(b"/io.max") || !harness.exists(&job.file("io.max")) {
        return Err("io.max is on the root, or missing from a cgroup with io enabled");
    }
    if !harness.reads(&job.file("io.stat"), b"") {
        return Err("a cgroup that has done no I/O has an io.stat");
    }
    let before = line(harness, b"/io.stat")?;
    let _ = reads(job, disk, 3)?;
    job.as_task(|| {
        let page = [0xa5_u8; PIECE];
        (0..2).try_for_each(|piece| disk.write((piece * PIECE / 512) as u64, &page))
    })
    .map_err(|_| "io check: a write to the disk failed")?;
    let mut one = [0_u8; 512];
    sibling
        .as_task(|| disk.read(0, &mut one))
        .map_err(|_| "io check: the sibling's read failed")?;
    let expected = b"251:0 rbytes=12288 wbytes=8192 rios=3 wios=2 dbytes=0 dios=0";
    if line(harness, &job.file("io.stat"))? != expected {
        crate::console::println!(
            "  io       io.stat: {}",
            core::str::from_utf8(&line(harness, &job.file("io.stat"))?).unwrap_or("?")
        );
        return Err("io.stat does not count what a cgroup read and wrote of a disk");
    }
    if line(harness, &sibling.file("io.stat"))?
        != b"251:0 rbytes=512 wbytes=0 rios=1 wios=0 dbytes=0 dios=0"
    {
        return Err("a sibling cgroup's io.stat counts another cgroup's I/O, or not its own");
    }
    let now = line(harness, b"/io.stat")?;
    if now == before || !now.starts_with(b"251:0 rbytes=") {
        return Err("the root's io.stat does not count the machine's I/O");
    }
    Ok(6)
}

/// `io.max` in the job: its text, its refusals, and its rate.
fn limits(harness: &mut Harness, job: &Group, disk: &Arc<dyn BlockDevice>) -> Checked<u32> {
    job.set(harness, "io.max", b"251:0 rbps=16384 riops=max\n")?;
    if !harness.reads(
        &job.file("io.max"),
        b"251:0 rbps=16384 wbps=max riops=max wiops=max\n",
    ) {
        return Err("io.max does not read back what was written");
    }
    job.set(harness, "io.max", b"251:0 wiops=50\n")?;
    if !harness.reads(
        &job.file("io.max"),
        b"251:0 rbps=16384 wbps=max riops=max wiops=50\n",
    ) {
        return Err("a write to io.max that left out rbps changed it");
    }
    for (bad, errno, what) in [
        (
            &b"251:99 rbps=9999\n"[..],
            Errno::ENODEV,
            "io.max took a disk that is not there",
        ),
        (
            b"251:0 rbps=0\n",
            Errno::ERANGE,
            "io.max took a limit of zero",
        ),
        (
            b"251:0 rbps=fast\n",
            Errno::EINVAL,
            "io.max took a limit that is no number",
        ),
        (
            b"251:0 bogus=5\n",
            Errno::EINVAL,
            "io.max took a key it has not got",
        ),
        (
            b"251:0 rbps\n",
            Errno::EINVAL,
            "io.max took a word with no value",
        ),
        (b"everything\n", Errno::EINVAL, "io.max took no disk"),
    ] {
        let refused = harness.write(&job.file("io.max"), bad);
        harness.refused(refused.err(), errno, what)?;
    }
    job.set(harness, "io.max", b"251:0 wiops=max\n")?;

    // Four pieces at 16 KiB a second start 0, 0.25, 0.5 and 0.75 seconds in.
    let slow = reads(job, disk, 4)?;
    if !(650..=2000).contains(&slow) {
        crate::console::println!("  io       four reads under rbps=16384 took {slow} ms");
        return Err("four reads under io.max rbps=16384 were not spaced out to its rate");
    }
    job.set(harness, "io.max", b"251:0 rbps=max\n")?;
    if !harness.reads(&job.file("io.max"), b"") {
        return Err("io.max still lists a disk with no limit");
    }
    let fast = reads(job, disk, 4)?;
    if fast > 300 {
        return Err("four reads with io.max lifted were still held back");
    }
    Ok(8)
}

/// A limit on a parent holds for its child, and the parent counts its I/O.
fn hierarchy(harness: &mut Harness, disk: &Arc<dyn BlockDevice>) -> Checked<u32> {
    let parent = Group::bare(harness, b"/check-ip")?;
    parent.set(harness, "cgroup.subtree_control", b"+io\n")?;
    let child = Group::make(harness, b"/check-ip/c")?;
    parent.set(harness, "io.max", b"251:0 riops=4\n")?;
    // Three reads at four a second: 0, 0.25 and 0.5 seconds in.
    let held = reads(&child, disk, 3)?;
    let counted = line(harness, &parent.file("io.stat"))?;
    let _ = child.end(harness);
    let _ = parent.end(harness);
    if !(400..=1500).contains(&held) {
        crate::console::println!("  io       three reads under a parent's riops=4 took {held} ms");
        return Err("a parent's io.max did not hold for its child's reads");
    }
    if counted != b"251:0 rbytes=12288 wbytes=0 rios=3 wios=0 dbytes=0 dios=0" {
        return Err("a parent's io.stat does not count its child's I/O");
    }
    Ok(3)
}
