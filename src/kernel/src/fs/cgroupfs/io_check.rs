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
//! * `io.weight` reads `default 100`, takes a default and a disk's own
//!   weight as `ioc_weight_write` does (`EINVAL` out of 1 to 10,000,
//!   `ENODEV` for a disk that is not there), and `io.latency` a disk's
//!   target as `iolatency_set_limit` does;
//! * on a disk that takes 2 ms a read, a task of a cgroup at `io.weight` 400
//!   reads more than twice what one beside it at 100 does in the same time,
//!   and a task of a cgroup missing its `io.latency` target reads more than
//!   half again what one beside it with none does, which still reads at
//!   least a twentieth of the reads;
//! * `io` is listed in `cgroup.controllers`, and once the cgroups and their
//!   processes are gone every quota slot they held is back.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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
    slots_back(
        slots,
        "the io check's cgroups are gone and their quota slots are not",
    )?;
    Ok(counted)
}

/// Wait for the quota slots to come down to `slots`. A program an earlier
/// check killed may be reaped meanwhile, and give a slot back: more than were
/// there before is what a leak is.
fn slots_back(slots: u64, leak: &'static str) -> Checked<()> {
    let deadline = crate::timer::now_nanos().saturating_add(5_000_000_000);
    while crate::object::quota::live_slots() > slots {
        if crate::timer::now_nanos() >= deadline {
            return Err(leak);
        }
        crate::sched::sleep_for(1_000_000);
    }
    Ok(())
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
        let gone = after_its_job_went(harness, &disk)?;
        shape_files(harness, &job)?;
        let shared = shares(harness)?;
        Ok(counted + spaced + parent + gone + shared)
    });
    let _ = job.end(harness);
    let _ = sibling.end(harness);
    rest
}

/// `io.weight` and `io.latency` in the job: their text and their refusals.
fn shape_files(harness: &mut Harness, job: &Group) -> Checked<()> {
    if harness.exists(b"/io.weight") || harness.exists(b"/io.latency") {
        return Err("io.weight or io.latency is on the root");
    }
    if !harness.reads(&job.file("io.weight"), b"default 100\n")
        || !harness.reads(&job.file("io.latency"), b"")
    {
        return Err("a new cgroup's io.weight is not default 100, or it has an io.latency");
    }
    job.set(harness, "io.weight", b"250\n")?;
    job.set(harness, "io.weight", b"251:0 400\n")?;
    if !harness.reads(&job.file("io.weight"), b"default 250\n251:0 400\n") {
        return Err("io.weight does not read back a default and a disk's own weight");
    }
    job.set(harness, "io.weight", b"251:0 default\n")?;
    job.set(harness, "io.weight", b"default 100\n")?;
    if !harness.reads(&job.file("io.weight"), b"default 100\n") {
        return Err("io.weight did not go back to the default when written so");
    }
    for (bad, errno, what) in [
        (&b"0\n"[..], Errno::EINVAL, "io.weight took 0"),
        (b"10001\n", Errno::EINVAL, "io.weight took 10001"),
        (
            b"9:9 50\n",
            Errno::ENODEV,
            "io.weight took a disk that is not there",
        ),
    ] {
        let refused = harness.write(&job.file("io.weight"), bad);
        harness.refused(refused.err(), errno, what)?;
    }
    job.set(harness, "io.latency", b"251:0 target=2000\n")?;
    if !harness.reads(&job.file("io.latency"), b"251:0 target=2000\n") {
        return Err("io.latency does not read back a target");
    }
    let refused = harness.write(&job.file("io.latency"), b"251:0 lots=1\n");
    harness.refused(
        refused.err(),
        Errno::EINVAL,
        "io.latency took a word it has no key for",
    )?;
    job.set(harness, "io.latency", b"251:0 target=max\n")?;
    if !harness.reads(&job.file("io.latency"), b"") {
        return Err("io.latency's target=max did not take the target away");
    }
    Ok(())
}

/// The slow disk's minor.
const SLOW_MINOR: u32 = 1;
/// How long the slow disk takes a read.
const SLOW_NANOS: u64 = 2_000_000;
/// How long the two readers read for.
const SHARE_WINDOW: u64 = 600_000_000;

/// A disk in memory that takes [`SLOW_NANOS`] a read, as a disk with one
/// request in flight at a time would.
#[derive(Debug)]
struct SlowDisk;

impl BlockDevice for SlowDisk {
    fn read(&self, _sector: u64, buf: &mut [u8]) -> Result<(), Errno> {
        crate::sched::sleep_for(SLOW_NANOS);
        buf.fill(0x5a);
        Ok(())
    }

    fn sectors(&self) -> u64 {
        SECTORS
    }

    fn sector_size(&self) -> u32 {
        512
    }

    fn read_only(&self) -> bool {
        true
    }

    fn write(&self, _sector: u64, _buf: &[u8]) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }
}

/// Reads each reader made, by its argument.
static READ: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Whether the readers are to stop.
static STOP: AtomicBool = AtomicBool::new(false);

/// A reader of the slow disk, counting into `READ[which]` until told to stop.
fn reader(which: usize) {
    let Some(disk) = devfs::block_device(makedev(MAJOR, SLOW_MINOR)) else {
        return;
    };
    let mut buf = [0_u8; PIECE];
    while !STOP.load(Ordering::Acquire) {
        if disk.read(0, &mut buf).is_err() {
            return;
        }
        if let Some(count) = READ.get(which) {
            let _ = count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Two readers of the slow disk, one in each of `a` and `b`, for
/// [`SHARE_WINDOW`]: what each read.
fn race(a: &Group, b: &Group) -> Checked<(u64, u64)> {
    STOP.store(false, Ordering::Release);
    for count in &READ {
        count.store(0, Ordering::Relaxed);
    }
    let first = crate::sched::spawn_in_group("io-share-a", reader, 0, 0, a.slot())?;
    let second = crate::sched::spawn_in_group("io-share-b", reader, 1, 0, b.slot());
    if second.is_ok() {
        crate::sched::sleep_for(SHARE_WINDOW);
    }
    STOP.store(true, Ordering::Release);
    let first_gone = crate::sched::wait_until_gone(&first, 5_000_000_000);
    let second_gone = second.and_then(|task| crate::sched::wait_until_gone(&task, 5_000_000_000));
    first_gone?;
    second_gone?;
    Ok((
        READ[0].load(Ordering::Relaxed),
        READ[1].load(Ordering::Relaxed),
    ))
}

/// `io.weight` and `io.latency` at work, on the slow disk; and with their
/// cgroups gone, no disk keeps a fair queue.
fn shares(harness: &mut Harness) -> Checked<u32> {
    let registration = devfs::register_block_from(
        b"iochk1",
        MAJOR,
        SLOW_MINOR,
        blkio::account_disk(Arc::new(SlowDisk), MAJOR, SLOW_MINOR),
        Origin::default(),
    )
    .map_err(|_| "io check: the slow disk would not register")?;
    let light = Group::make(harness, b"/check-iwa")?;
    let heavy = Group::make(harness, b"/check-iwb")?;
    let outcome = shares_in(harness, &light, &heavy);
    let _ = light.end(harness);
    let _ = heavy.end(harness);
    drop(registration);
    let shares = outcome?;
    // `/check-i` set and took back its own; with these two gone, nobody has a
    // weight or a target, and the rest of the boot is served unqueued.
    let deadline = crate::timer::now_nanos().saturating_add(5_000_000_000);
    while blkio::shaped() {
        if crate::timer::now_nanos() >= deadline {
            return Err("a disk kept its fair queue once no cgroup had an io.weight or io.latency");
        }
        crate::sched::sleep_for(1_000_000);
    }
    Ok(shares)
}

/// [`shares`]' races, in its two cgroups.
fn shares_in(harness: &mut Harness, light: &Group, heavy: &Group) -> Checked<u32> {
    heavy.set(harness, "io.weight", b"400\n")?;
    let (low, high) = race(light, heavy)?;
    crate::console::println!(
        "  io       io.weight 100 and 400 read {low} and {high} times from a disk of 2 ms a read"
    );
    if low == 0 || high < 2 * low {
        return Err("a cgroup at io.weight 400 did not read twice what one at 100 did beside it");
    }
    heavy.set(harness, "io.weight", b"100\n")?;
    // A target no read of this disk can meet: the cgroup is always missing
    // it, and is served first.
    light.set(harness, "io.latency", b"251:1 target=1000\n")?;
    let (protected, other) = race(light, heavy)?;
    light.set(harness, "io.latency", b"251:1 target=max\n")?;
    crate::console::println!(
        "  io       a cgroup missing its io.latency target read {protected} times, one beside \
         it with none {other}"
    );
    if protected <= other.saturating_mul(3) / 2 {
        return Err("a cgroup missing its io.latency target was not served before one beside it");
    }
    // At most seven urgent requests in a row while it waits: about one read
    // in nine is its own (27 of about 230 on every architecture), against two
    // in all with no cap. A twentieth is the line between them.
    if other.saturating_mul(20) < protected.saturating_add(other) {
        return Err("a cgroup beside one missing its io.latency target was starved");
    }
    Ok(2)
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

/// A task that is still in the kernel when its job goes -- one asleep in the
/// wait `io.max` makes it, say, whose process moved and whose cgroup was
/// removed -- may charge the disk when it wakes, and that must not start
/// state for a job that is gone: it would hold the job's quota slot, and every
/// slot above it, for ever. Done as the task would: the check's own task
/// holds the slot as a task of the job, the cgroup is removed, and then it
/// reads.
///
/// Verifies: L.object.110
fn after_its_job_went(harness: &mut Harness, disk: &Arc<dyn BlockDevice>) -> Checked<u32> {
    let before = crate::object::quota::live_slots();
    let group = Group::make(harness, b"/check-iw")?;
    let own = crate::sched::running_group();
    crate::sched::set_current_group(group.slot());
    let gone = group.end(harness);
    let mut sector = [0_u8; 512];
    let read = disk.read(0, &mut sector);
    crate::sched::set_current_group(own);
    gone?;
    read.map_err(|_| "io check: a read of the disk failed")?;
    slots_back(
        before,
        "a disk read by a task of a job that had gone kept the job's quota slot",
    )?;
    Ok(1)
}
