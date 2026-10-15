//! The boot check that starts the ring-3 virtio-blk driver and reads the
//! test disk through it: stage 10's exit, the half that is a driver in ring
//! 3 reading sectors with the IOMMU on, and stage 11's disk.
//!
//! `devmgr` will start block drivers; until it lands, and for the boot check
//! either way, the kernel starts one itself the way `devmgr` will: a process
//! made from `/sbin/blk` in the initramfs, given the device with `MANAGE`
//! and the driver's end of a ring's control channel over its bootstrap
//! channel, in the one START message `docs/BLOCK-RING.md` §6 fixes and
//! `block_ring::start_for` fills from the device node. Every call the
//! starter makes goes through the native dispatcher from a process of its
//! own, as `block_ring::check` and stage 9's process checks do, so nothing
//! here reaches the kernel by a path a program could not.
//!
//! A driver is started for every virtio-blk function, in PCI order, named
//! `vda`, `vdb` and so on as `devmgr` would name them; the second serves the
//! btrfs fixture stage 11's exit mounts next. What the check requires: each
//! driver's HELLO accepted and its disk published under the name START gave
//! it, within the patience; then sectors of the first disk
//! read through the registry's [`BlockDevice`] — the same path a mount takes
//! — come back exactly as `xtask` wrote the test disk, whose layout is fixed
//! in `tools/common/xtask/src/test_disk.rs` and repeated here in [`expected`]. A driver
//! that exits before publishing fails the check with its exit status printed,
//! which names the step it stopped at.
//!
//! A boot that skips its self-checks (`ferrix.checks=skip`) still starts
//! every driver and waits for every disk, since what mounts next needs them,
//! but reads nothing back: its first disk is whatever the machine was given,
//! a desktop's volume say, and not the test disk.
//!
//! The driver is left running. Its disk stays in `/dev` for what comes after
//! this check — a btrfs mount of the next disk is stage 11's exit — and the
//! process ends with the machine.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_blkring::control::Message;
use ferrix_blkring::identity::DiskName;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals;
use ferrix_vfs::initramfs::makedev;
use ferrix_vma::VmaFlags;

use super::{VIRTIO_BLK_MAJOR, start_for};
use crate::checks;
use crate::device;
use crate::fs;
use crate::fs::block::BlockDevice;
use crate::object::Object;
use crate::object::check::{Side, device_handle, reg};
use crate::object::job::Job;
use crate::sched;
use crate::timer;

/// virtio's PCI vendor, and virtio-blk's modern and transitional device ids.
const VIRTIO_VENDOR: u16 = 0x1AF4;
const VIRTIO_BLK_IDS: [u16; 2] = [0x1042, 0x1001];

/// The driver, as the initramfs carries it.
const PROGRAM: &[u8] = b"/lib/drivers/blk";

/// What the child is listed as.
const NAME: &[u8] = b"blk";

/// A page, for the starter's own staging.
const PAGE_SIZE: u64 = 4096;

/// Where the starter stages what its calls read: START, handles, the name,
/// a key, a channel pair and an offset. A region of its own,
/// beside the one `Side::new` maps for `object::check`'s constants.
const STAGE: u64 = 0x5100_0000;
const OUTBOX: u64 = STAGE;
const OUT_HANDLES: u64 = STAGE + 0x100;
const NAME_AT: u64 = STAGE + 0x200;
const KEY: u64 = STAGE + 0x300;
const PAIR: u64 = STAGE + 0x310;
const OFFSET: u64 = STAGE + 0x320;

/// Where the child's image is staged before it goes into a VMO.
const IMAGE_AT: u64 = 0x5200_0000;

/// The port key the child's end is watched with.
const ENDED: u64 = 0x1e;

/// How long the driver has to bring the device up and be published: the
/// device library resets and negotiates with the device, and the kernel's
/// ring task publishes on HELLO; ten seconds is generous under TCG.
const PATIENCE_NANOS: u64 = 10_000_000_000;

/// The test disk's layout, as `tools/common/xtask/src/test_disk.rs` fixes it.
const SECTOR_SIZE: usize = 512;
const MAGIC: &[u8; 9] = b"FERRIXBLK";
const VERSION: u8 = 1;
const FILL_MULTIPLIER: u64 = 0x9e37_79b9_7f4a_7c15;

/// Sectors read one at a time, chosen to cover the start, the end and the
/// middle of the 64 MiB disk.
const SINGLE_SECTORS: [u64; 5] = [0, 1, 255, 4096, 131_071];

/// A run read in one request: sixteen sectors, so a request spans pages.
const RUN_START: u64 = 1000;
const RUN_SECTORS: usize = 16;

/// What the check found.
#[derive(Debug)]
pub struct Report {
    /// The disks' names in `/dev`, space-separated: one per virtio-blk
    /// function, in PCI order.
    pub names: &'static str,
    /// The first disk's size in sectors, as its driver announced it.
    pub sectors: u64,
    /// Sectors of the first disk read back as written: none on a boot that
    /// skips its self-checks.
    pub read: u32,
    /// Why nothing was checked, on a machine without the device.
    pub skipped: Option<&'static str>,
}

/// Start a driver per virtio-blk function and read the test disk through the
/// first.
///
/// # Errors
///
/// What did not happen, as a sentence.
pub fn run(started_by_devmgr: bool) -> Result<Report, &'static str> {
    let nodes: Vec<Arc<device::DeviceNode>> = device::devices()
        .iter()
        .filter(|node| {
            node.pci_function().is_some_and(|function| {
                function.vendor == VIRTIO_VENDOR && VIRTIO_BLK_IDS.contains(&function.device)
            })
        })
        .cloned()
        .collect();
    if nodes.is_empty() {
        return Ok(Report {
            names: "",
            sectors: 0,
            read: 0,
            skipped: Some("no virtio-blk function on this machine"),
        });
    }

    if started_by_devmgr {
        // devmgr started the drivers: only the disks are checked here.
        let mut first = None;
        for index in 0..nodes.len() {
            let rdev = makedev(VIRTIO_BLK_MAJOR, u32::try_from(index).unwrap_or(0) * 16);
            let location = nodes.get(index).and_then(|node| super::location_of(node));
            let disk = published_by_devmgr(rdev, location)?;
            if first.is_none() {
                first = Some(disk);
            }
        }
        let disk = first.ok_or("no disk was published")?;
        let read = if checks::run() {
            let read = read_back(disk.as_ref())?;
            no_nudge_lost()?;
            super::hop_check::run(&disk)?;
            no_nudge_lost()?;
            print_nudges();
            read
        } else {
            0
        };
        return Ok(Report {
            names: names_str(nodes.len()),
            sectors: disk.sectors(),
            read,
            skipped: None,
        });
    }

    let side = Side::new()?;
    let _ = side
        .process
        .space()
        .map_anonymous(STAGE, PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "could not map the starter's staging region")?;
    let file = fs::read_file(&fs::namespace().context(), None, PROGRAM)
        .map_err(|_| "the initramfs carries no /lib/drivers/blk")?;
    let image = image_vmo(&side, &file)?;
    let root = Job::new_root().map_err(|_| "no memory for the driver's job")?;
    let job = side
        .process
        .with_handles(|table| table.insert(Object::Job(root), Rights::JOB))
        .map_err(|_| "no room for the driver's job")?;
    side.put(NAME_AT, NAME)?;

    // One driver at a time: each is started and its disk waited for before
    // the next is started, so the registry lists the disks in PCI order,
    // vda before vdb, as devmgr will register them and as /proc/partitions
    // prints them. Started together, two drivers' HELLOs race and the order
    // is whichever the kernel accepts first.
    let mut first = None;
    for (index, node) in nodes.iter().enumerate() {
        let index = u32::try_from(index).map_err(|_| "too many disks to name")?;
        let name = DiskName::for_index(index).ok_or("the crate has no name for this disk")?;
        let child = start_driver(&side, node, name, image, job)?;
        let disk = published(&side, child, makedev(VIRTIO_BLK_MAJOR, index * 16))?;
        if first.is_none() {
            first = Some(disk);
        }
    }

    // Sectors through the first disk, the path a mount takes.
    let disk = first.ok_or("no disk was published")?;
    let read = if checks::run() {
        let read = read_back(disk.as_ref())?;
        no_nudge_lost()?;
        super::hop_check::run(&disk)?;
        no_nudge_lost()?;
        print_nudges();
        read
    } else {
        0
    };
    Ok(Report {
        names: names_str(nodes.len()),
        sectors: disk.sectors(),
        read,
        skipped: None,
    })
}

/// Start `/sbin/blk` on `node` as the disk called `name`: a ring on the
/// device, a process from `image` in `job`, START over its bootstrap
/// channel, its end watched. The child's handle is the answer.
fn start_driver(
    side: &Side,
    node: &Arc<device::DeviceNode>,
    name: DiskName,
    image: Handle,
    job: Handle,
) -> Result<Handle, &'static str> {
    let start = start_for(node, name).ok_or("a virtio-blk function has no virtio blocks")?;
    let device = device_handle(side, node)?;
    // The ring check's last round leaves its device bound; quiescing it
    // releases the claim, and does nothing on a device nobody claimed.
    let _ = side.call(nr::DEVICE_QUIESCE, &[reg(device)]);
    let control = side.handle(
        nr::BLOCK_RING_CREATE,
        &[reg(device)],
        "block_ring_create for a driver failed",
    )?;
    let child = side.handle(
        nr::PROCESS_CREATE,
        &[reg(job), reg(image), NAME_AT, NAME.len() as u64],
        "process_create for a driver failed",
    )?;

    // START over the bootstrap channel: the device, and the control channel.
    let _ = side
        .call(nr::CHANNEL_CREATE, &[PAIR])
        .map_err(|_| "channel_create for a bootstrap failed")?;
    let near = Handle(side.get_u32(PAIR)?);
    let far = Handle(side.get_u32(PAIR + 4)?);
    let for_child = device_handle(side, node)?;
    let encoded = Message::Start(start).encode();
    side.put(OUTBOX, encoded.as_bytes())?;
    let words: Vec<u8> = [for_child, control]
        .iter()
        .flat_map(|handle| handle.0.to_ne_bytes())
        .collect();
    side.put(OUT_HANDLES, &words)?;
    let _ = side
        .call(
            nr::CHANNEL_WRITE,
            &[
                reg(near),
                OUTBOX,
                encoded.as_bytes().len() as u64,
                OUT_HANDLES,
                2,
            ],
        )
        .map_err(|_| "sending START to a driver failed")?;

    // Hear the child end, then start it.
    let port = side.handle(nr::PORT_CREATE, &[], "port_create failed")?;
    side.put(KEY, &ENDED.to_ne_bytes())?;
    let _ = side
        .call(
            nr::OBJECT_WAIT_ASYNC,
            &[reg(child), reg(port), u64::from(Signals::TERMINATED.0), KEY],
        )
        .map_err(|_| "watching a driver failed")?;
    let _ = side
        .call(nr::PROCESS_START, &[reg(child), reg(far)])
        .map_err(|_| "process_start for a driver failed")?;
    Ok(child)
}

/// Read sectors through `disk`, the path a mount takes, and compare each
/// with what `xtask` wrote. Answers how many matched.
fn read_back(disk: &dyn BlockDevice) -> Result<u32, &'static str> {
    let mut read = 0;
    let mut sector = [0_u8; SECTOR_SIZE];
    for number in SINGLE_SECTORS {
        disk.read(number, &mut sector)
            .map_err(|_| "a sector read through the ring failed")?;
        if sector != expected(number) {
            return Err("a sector read through the ring came back wrong");
        }
        read += 1;
    }
    let mut run = [0_u8; SECTOR_SIZE * RUN_SECTORS];
    disk.read(RUN_START, &mut run)
        .map_err(|_| "a sixteen-sector read through the ring failed")?;
    for (i, chunk) in run.chunks_exact(SECTOR_SIZE).enumerate() {
        if chunk != expected(RUN_START + i as u64) {
            return Err("a sector of a sixteen-sector read came back wrong");
        }
        read += 1;
    }
    Ok(read)
}

/// The child's image, written into a VMO through `vmo_write`.
fn image_vmo(side: &Side, file: &[u8]) -> Result<Handle, &'static str> {
    let len = file.len() as u64;
    let _ = side
        .process
        .space()
        .map_anonymous(
            IMAGE_AT,
            len.div_ceil(PAGE_SIZE) * PAGE_SIZE,
            VmaFlags::READ_WRITE,
        )
        .map_err(|_| "could not map room for the driver's image")?;
    side.put(IMAGE_AT, file)?;
    let image = side.handle(
        nr::VMO_CREATE,
        &[len],
        "vmo_create for the driver's image failed",
    )?;
    side.put(OFFSET, &0_u64.to_ne_bytes())?;
    let _ = side
        .call(nr::VMO_WRITE, &[reg(image), IMAGE_AT, len, OFFSET])
        .map_err(|_| "writing the driver's image into a VMO failed")?;
    Ok(image)
}

/// Wait for the driver's disk to appear in the registry, or for the driver
/// to end first, which is reported with its exit status.
fn published(side: &Side, child: Handle, rdev: u64) -> Result<Arc<dyn BlockDevice>, &'static str> {
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    loop {
        if let Some(disk) = fs::devfs::block_device(rdev) {
            return Ok(disk);
        }
        if let Some(status) = exit_status(side, child) {
            crate::console::println!(
                "  driver   /sbin/blk exited with status {status} before publishing its disk"
            );
            return Err("the driver exited before publishing its disk; its status names the step");
        }
        if timer::now_nanos() >= deadline {
            return Err("the driver published no disk within the patience");
        }
        sched::sleep_for(2_000_000);
    }
}

/// The disk `devmgr`'s driver published, or why not within the patience.
fn published_by_devmgr(
    rdev: u64,
    location: Option<ferrix_blkring::Location>,
) -> Result<Arc<dyn BlockDevice>, &'static str> {
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    loop {
        if let Some(disk) = fs::devfs::block_device(rdev) {
            return Ok(disk);
        }
        // A driver that died is named, with how it died, rather than left to
        // look like a disk that never came.
        if let Some((status, signal)) = location.and_then(crate::discovery::devmgr::driver_ending) {
            let raw = location.map_or(0, ferrix_blkring::Location::raw);
            match signal {
                Some(signal) => crate::console::println!(
                    "  driver   the blk driver of {raw:#010x} was ended by signal {signal} \
                     before publishing its disk"
                ),
                None => crate::console::println!(
                    "  driver   the blk driver of {raw:#010x} exited with status {status} \
                     ({}) before publishing its disk",
                    blk_step(status)
                ),
            }
            return Err(DRIVER_DIED);
        }
        if timer::now_nanos() >= deadline {
            return Err("a disk devmgr reported started is not in the registry");
        }
        sched::sleep_for(2_000_000);
    }
}

/// What the check fails with when a driver devmgr started died before its disk
/// was published; the line before it says how.
const DRIVER_DIED: &str = "a blk driver devmgr started died before publishing its disk";

/// The step `/sbin/blk`'s exit status names: its `Step` enum, in
/// `src/user/system/native/drivers/block/virtio-blk/src/main.rs`, whose numbers are its exit statuses, and
/// from 20 up its `fault_status`, the fault that stopped it serving.
fn blk_step(status: i32) -> &'static str {
    match status {
        1 => "at its start",
        2 => "checking its identity",
        3 => "mapping its registers",
        4 => "making, pinning or mapping memory",
        5 => "bringing the device up",
        6 => "setting up the ring",
        7 => "arranging its events",
        8 => "serving a broken ring or device",
        9 => "stopping a device that would not reset",
        10 => "on its control channel",
        20 => "serving a ring that was corrupt",
        21 => "serving, the ring and the device disagreeing about a request",
        22..=27 => "serving a device that broke the protocol",
        30..=39 => "serving a virtqueue whose used ring said something impossible",
        _ => "at no step it names",
    }
}

/// The child's exit status, if it has ended.
fn exit_status(side: &Side, child: Handle) -> Option<i32> {
    side.process.with_handles(|table| match table.get(child) {
        Ok((Object::Process(process), _)) => process.exit_status(),
        _ => None,
    })
}

/// What `xtask` wrote into sector `number` of the test disk.
fn expected(number: u64) -> [u8; SECTOR_SIZE] {
    let seed = number.wrapping_mul(FILL_MULTIPLIER);
    let mut bytes = [0_u8; SECTOR_SIZE];
    for (offset, byte) in bytes.iter_mut().enumerate() {
        *byte = (seed >> (8 * (offset % 8))) as u8 ^ offset as u8;
    }
    let header = number
        .to_le_bytes()
        .into_iter()
        .chain(*MAGIC)
        .chain([VERSION]);
    for (byte, value) in bytes.iter_mut().zip(header) {
        *byte = value;
    }
    bytes
}

/// The disks' names for the report, in order.
fn names_str(count: usize) -> &'static str {
    match count {
        1 => "vda",
        2 => "vda vdb",
        3 => "vda vdb vdc",
        _ => "vda ...",
    }
}

/// Require that no caller's nudge to a ring's task was lost: the task, waking
/// for any reason, never found something it could have dispatched that was
/// queued while it slept without a nudge on its way (`block_ring`'s
/// `Serving::wake_up`). A lost nudge does not fail a read -- the task's
/// recheck finds the request 50 ms later -- so only this count shows it.
fn no_nudge_lost() -> Result<(), &'static str> {
    if super::NUDGES_LOST.load(core::sync::atomic::Ordering::Relaxed) != 0 {
        print_nudges();
        return Err(
            "a request waited for the ring task's recheck: the nudge that should have woken it was lost",
        );
    }
    Ok(())
}

/// The nudge counts, for the boot log.
fn print_nudges() {
    let load =
        |count: &core::sync::atomic::AtomicU64| count.load(core::sync::atomic::Ordering::Relaxed);
    crate::console::println!(
        "  nudge    {} requests found the ring's task asleep and nudged it, {} found it awake \
         or nudged already, {} nudges lost",
        load(&super::NUDGES_SENT),
        load(&super::NUDGES_SPARED),
        load(&super::NUDGES_LOST),
    );
}
