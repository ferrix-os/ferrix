//! Stage 10's self-check of the block ring's control plane.
//!
//! A process that has been given a device asks for a ring, sends HELLOs and
//! reads what comes back, the way a driver's process will — every call through
//! [`native::dispatch`] with raw registers, from stage 9's [`Side`]. The kernel
//! serves nothing here: the ring is taken up to a published disk and let go
//! again, and no request is ever put on it. Reading sectors through a ring is
//! the ring-3 driver's check, which needs the native runtime and a program in
//! the image, and comes after this.
//!
//! What is required, on a machine with a PCI function:
//!
//! * `block_ring_create` refuses a device handle without `MANAGE`, a handle
//!   that is not a device, and a device that already has a ring;
//! * a HELLO for another ring version, one whose handles carry more rights
//!   than the protocol allows, and one naming another device's location are
//!   each answered with the refusal `docs/BLOCK-RING.md` §6.2 specifies, and
//!   the kernel's end of the control channel closes after it;
//! * a HELLO as specified is answered with READY carrying the completion port
//!   with `WRITE` alone, and the disk it describes is in the registry under
//!   the name and numbers §6.1 gives it — and gone again once the driver says
//!   STOPPED, after which the next round's ring finds the device free;
//! * the second round of all that gives every frame back;
//! * a driver that closes the channel without STOPPED leaves its disk parked
//!   -- still published, for the next driver -- and the device bound until
//!   `device_quiesce`, which turns the device off and frees it for the next
//!   ring -- asked after the ring has ended, and asked the instant the driver
//!   is gone, repeatedly, since the quiesce then has to wait for the ring's
//!   task to notice; the next round's accepted HELLO takes the parked disk up
//!   under its name, and the check forgets the last one;
//! * `device_info` describes the device as enumeration found it, every virtio
//!   block it names inside an aperture, and the START the kernel would build
//!   from it agrees; `device_quiesce` is refused without `MANAGE` and while a
//!   driver serves the device.
//!
//! [`native::dispatch`]: crate::syscall::native::dispatch

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_blkring::kernel::Ending;
use ferrix_blkring::{Device, DeviceFlags, DiskName, Hello, Identity, Message, Refusal};
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{DEVICE_INFO_BYTES, DEVICE_VIRTIO_PCI};
use ferrix_pci::header::{COMMAND, COMMAND_BUS_MASTER};
use ferrix_vfs::initramfs::makedev;

use super::{VIRTIO_BLK_MAJOR, forget_parked, is_parked, location_of, start_for};
use crate::device::{self, DeviceNode};
use crate::fs::devfs;
use crate::object::check::{SCRATCH, Side, device_handle, reg};
use crate::{mm, object, sched, timer};

/// Where this check keeps its buffers: the second page of [`SCRATCH`], which
/// stage 9's checks leave alone.
const HERE: u64 = SCRATCH + PAGE_SIZE;
/// A message being sent.
const OUTBOX: u64 = HERE;
/// The handles it carries.
const OUT_HANDLES: u64 = HERE + 0x100;
/// A message received.
const INBOX: u64 = HERE + 0x200;
/// The handles it carried.
const IN_HANDLES: u64 = HERE + 0x300;
/// A `ReadActual`.
const ACTUAL: u64 = HERE + 0x320;
/// A wait's deadline.
const DEADLINE: u64 = HERE + 0x330;
/// The signals a wait observed.
const OBSERVED: u64 = HERE + 0x338;
/// A VMO offset.
const OFFSET: u64 = HERE + 0x340;
/// The ring header being written.
const HEADER: u64 = HERE + 0x400;
/// A `DeviceInfo`.
const INFO: u64 = HERE + 0x600;

/// How long the check waits for the ring's task to answer, or to notice the
/// channel closed and unpublish the disk.
const PATIENCE_NANOS: u64 = 10_000_000_000;

/// Whether a death round quiesces the instant the driver's end closes, as
/// `devmgr` does on `TERMINATED`, rather than after the ring has ended.
static QUIESCE_AT_ONCE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// The disk the accepted HELLO describes: eight sectors of 512 bytes, one per
/// request, through a one-page data VMO.
const BLOCK_SIZE: u32 = 512;
/// See [`BLOCK_SIZE`].
const CAPACITY: u64 = 8;
/// See [`BLOCK_SIZE`].
const MAX_SECTORS: u32 = 1;
/// The ring: the fewest entries the header allows.
const ENTRIES: u32 = 2;

/// What the check measured, for the boot log.
#[derive(Debug)]
pub(crate) struct Report {
    /// Calls and HELLOs refused with exactly the answer they had to get.
    pub(crate) refusals: u32,
    /// Disks published from an accepted HELLO and unpublished again.
    pub(crate) published: u32,
    /// Frames the second round did not give back.
    pub(crate) leaked: i64,
    /// Fields of a posted completion a cheating driver rewrote after the
    /// kernel read them, none of which it acted on ([`super::reread_check`]).
    pub(crate) rewrites: u32,
    /// Why nothing was checked, on a machine with no PCI function.
    pub(crate) skipped: Option<&'static str>,
}

/// Counts what happened, so the report is a measurement and not a claim.
#[derive(Debug, Default)]
pub(super) struct Counter {
    /// See [`Report::refusals`].
    pub(super) refusals: u32,
    /// See [`Report::published`].
    pub(super) published: u32,
}

/// Run the check. `Err` names the first thing that was not true.
///
/// # Errors
///
/// What was not true.
///
/// Verifies: L.quiesce.3
pub(crate) fn run() -> Result<Report, &'static str> {
    // No device needed: that ring is a page of the kernel's own.
    let rewrites = super::reread_check::run()?.rewrites;
    // Nor here: its two tree nodes are its own, and it leaves `vda` free.
    super::tree_check::run()?;
    let Some(node) = device::devices()
        .iter()
        .find(|node| matches!(node.location(), device::Location::Pci(_)))
        .cloned()
    else {
        return Ok(Report {
            refusals: 0,
            published: 0,
            leaked: 0,
            rewrites,
            skipped: Some("no PCI function to make a ring for"),
        });
    };
    // Twice, measured on the second, for the reason `syscall::check::run`
    // gives: the heap keeps a page of each size class the first run touched.
    // Each ring's task leaves a kernel stack for the scheduler to reap, so the
    // count waits for the reaper on both sides of the window.
    let mut warm = Counter::default();
    round(&node, &mut warm, Ending::Stopped)?;
    settle()?;
    let window = mm::FrameWindow::open();
    let mut counter = Counter::default();
    round(&node, &mut counter, Ending::Stopped)?;
    settle()?;
    let leaked = window.kept();
    if leaked != 0 {
        mm::print_frame_delta("ring", leaked);
        window.report("ring");
        return Err("the block ring check did not give every frame back");
    }
    // Outside the window: the deaths, each settled before the next so the
    // ring made after the quiesce has let the device go again.
    round(&node, &mut counter, Ending::DriverDied)?;
    for _ in 0..3 {
        settle()?;
        QUIESCE_AT_ONCE.store(true, core::sync::atomic::Ordering::Relaxed);
        let outcome = round(&node, &mut counter, Ending::DriverDied);
        QUIESCE_AT_ONCE.store(false, core::sync::atomic::Ordering::Relaxed);
        outcome?;
    }
    // Nobody serves the check's disk after it: its parked node goes.
    let location = location_of(&node).ok_or("the node is not a PCI function after all")?;
    forget_parked(location);
    if devfs::block_device(makedev(VIRTIO_BLK_MAJOR, 0)).is_some() || is_parked(location) {
        return Err("a parked disk outlived its forgetting");
    }
    Ok(Report {
        refusals: counter.refusals,
        published: counter.published,
        leaked,
        rewrites,
        skipped: None,
    })
}

/// Wait until every ring task of the rounds so far has stopped and the reaper
/// has freed what they left: the edge of a frame-count window. A ring's task
/// runs on after the check has seen the kernel's end close, so the stops are
/// waited for first, and only then the reaper.
pub(super) fn settle() -> Result<(), &'static str> {
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    super::wait_until_tasks_stopped(deadline)?;
    sched::wait_until_reaper_quiet(sched::REAPER_PATIENCE_NANOS)
}

/// One round: every refusal, then an accepted HELLO and its disk, ended by
/// STOPPED or by closing the channel as a dying driver would. A driver that
/// died leaves a device the quiesce answered success for, and that device's
/// bus mastering, turned on while its driver served it as the driver's first
/// pin would, reads off.
///
/// Verifies: L.quiesce.2, H.DEV.2, H.DEV.4
fn round(
    node: &Arc<DeviceNode>,
    counter: &mut Counter,
    ending: Ending,
) -> Result<(), &'static str> {
    let side = Side::new()?;
    let device = device_handle(&side, node)?;
    let location = location_of(node).ok_or("the node is not a PCI function after all")?;
    described(&side, node, device, location)?;
    refusals(&side, node, device, location, counter)?;

    let control = ring(&side, device)?;
    send(&side, control, &hello(location)?, &objects(&side, true)?)?;
    expect_ready(&side, control)?;
    let rdev = makedev(VIRTIO_BLK_MAJOR, 0);
    published(rdev)?;
    refused(
        side.call(nr::DEVICE_QUIESCE, &[reg(device)]),
        status::BAD_STATE,
        "a device was quiesced under the driver serving it",
        counter,
    )?;
    // As the driver's first pin would: its rings' pages are pinned for the
    // device, and the function masters the bus from then on.
    node.enable_dma()
        .map_err(|_| "a served device's DMA could not be turned on")?;
    if bus_mastering(node)? == Some(false) {
        return Err("a device whose DMA was turned on did not have bus mastering on");
    }
    end(&side, device, control, rdev, location, ending, counter)?;
    if ending == Ending::DriverDied && bus_mastering(node)? != Some(false) {
        return Err("a device quiesced after its driver died still had bus mastering on");
    }
    side.close_everything();
    Ok(())
}

/// Whether `node`'s bus mastering is on, read from its command register:
/// `None` for a node with no configuration space.
fn bus_mastering(node: &DeviceNode) -> Result<Option<bool>, &'static str> {
    let Some(config_phys) = node
        .pci_function()
        .and_then(|function| function.config_phys)
    else {
        return Ok(None);
    };
    let config = crate::vmap::map_device(config_phys, 256)
        .map_err(|_| "a function's configuration space could not be mapped to read it")?;
    let command = crate::mmio::Mmio::at(config).read16(u64::from(COMMAND));
    let _ = crate::vmap::unmap_device(config);
    Ok(Some(command & COMMAND_BUS_MASTER != 0))
}

/// Every refusal: of the call, then of three HELLOs, each on a ring of its
/// own, since a refused ring is over.
///
/// Verifies: L.quiesce.1
fn refusals(
    side: &Side,
    node: &Arc<DeviceNode>,
    device: Handle,
    location: ferrix_blkring::Location,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    // A device handle carries no DUPLICATE, so a second one is placed the way
    // the first was and then reduced.
    let full = device_handle(side, node)?;
    let bare = side.handle(
        nr::HANDLE_REPLACE,
        &[reg(full), u64::from(Rights::TRANSFER.0)],
        "reducing a device handle to one without MANAGE failed",
    )?;
    refused(
        side.call(nr::BLOCK_RING_CREATE, &[reg(bare)]),
        status::ACCESS_DENIED,
        "a ring was made through a device handle without MANAGE",
        counter,
    )?;
    refused(
        side.call(nr::DEVICE_QUIESCE, &[reg(bare)]),
        status::ACCESS_DENIED,
        "a device was quiesced through a handle without MANAGE",
        counter,
    )?;
    let vmo = side.handle(nr::VMO_CREATE, &[PAGE_SIZE], "vmo_create failed")?;
    refused(
        side.call(nr::BLOCK_RING_CREATE, &[reg(vmo)]),
        status::WRONG_TYPE,
        "a ring was made on a VMO",
        counter,
    )?;
    refused(
        side.call(nr::DEVICE_INFO, &[reg(vmo), INFO]),
        status::WRONG_TYPE,
        "a VMO was described as a device",
        counter,
    )?;
    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(vmo)])
        .map_err(|_| "closing a VMO failed")?;

    let control = ring(side, device)?;
    refused(
        side.call(nr::BLOCK_RING_CREATE, &[reg(device)]),
        status::ALREADY_BOUND,
        "a device was given a second ring",
        counter,
    )?;
    let mut wrong_version = hello(location)?;
    wrong_version.version = 2;
    send(side, control, &wrong_version, &[])?;
    expect_refusal(side, control, Refusal::Version, counter)?;

    let control = ring(side, device)?;
    let unreduced = objects(side, false)?;
    send(side, control, &hello(location)?, &unreduced)?;
    expect_refusal(side, control, Refusal::Rights, counter)?;

    let control = ring(side, device)?;
    let mut elsewhere = hello(location)?;
    elsewhere.location = elsewhere.location.wrapping_add(1);
    send(side, control, &elsewhere, &objects(side, true)?)?;
    expect_refusal(side, control, Refusal::WrongLocation, counter)
}

/// Require the accepted HELLO's disk to be in the registry as described.
pub(super) fn published(rdev: u64) -> Result<(), &'static str> {
    let Some(disk) = devfs::block_device(rdev) else {
        return Err("an accepted HELLO published no disk under its numbers");
    };
    if disk.sectors() != CAPACITY || disk.sector_size() != BLOCK_SIZE || disk.read_only() {
        return Err("the published disk is not the one HELLO described");
    }
    drop(disk);
    let mut named = false;
    devfs::for_each_block(|name, major, minor, _| {
        named |= name == b"vda" && major == VIRTIO_BLK_MAJOR && minor == 0;
    });
    if named {
        Ok(())
    } else {
        Err("the published disk is not listed as vda")
    }
}

/// End the ring as `ending` says: after STOPPED require its disk to go, and
/// after a death require the device to stay bound and the disk to stay
/// published, parked for the next driver.
pub(super) fn end(
    side: &Side,
    device: Handle,
    control: Handle,
    rdev: u64,
    location: ferrix_blkring::Location,
    ending: Ending,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    if ending == Ending::Stopped {
        let stopped = Message::Stopped.encode();
        side.put(OUTBOX, stopped.as_bytes())?;
        let _ = side
            .call(
                nr::CHANNEL_WRITE,
                &[
                    reg(control),
                    OUTBOX,
                    stopped.as_bytes().len() as u64,
                    OUT_HANDLES,
                    0,
                ],
            )
            .map_err(|_| "sending STOPPED failed")?;
        let observed = wait(side, control, Signals::PEER_CLOSED)?;
        if !observed.intersects(Signals::PEER_CLOSED) {
            return Err("the kernel kept its end of a stopped ring open");
        }
    }
    let at_once =
        ending == Ending::DriverDied && QUIESCE_AT_ONCE.load(core::sync::atomic::Ordering::Relaxed);
    if at_once {
        // As devmgr does on TERMINATED: before the ring's task has had a
        // chance to see the closed channel. The quiesce waits for it. Both
        // while another processor drains disposed objects, as though one did:
        // a close used to queue the channel's end behind that drain and
        // return with it open, and the quiesce was refused (FX-1004).
        let _ = object::as_if_draining_elsewhere(|| {
            let _ = side
                .call(nr::HANDLE_CLOSE, &[reg(control)])
                .map_err(|_| "closing the control channel failed")?;
            side.call(nr::DEVICE_QUIESCE, &[reg(device)])
                .map_err(|error| match error {
                    status::BAD_STATE => {
                        "quiescing the instant a driver died was refused: its channel was still \
                         open"
                    }
                    status::TIMED_OUT => {
                        "quiescing the instant a driver died timed out waiting for its ring to end"
                    }
                    _ => "quiescing the instant a driver died failed",
                })
        })?;
    } else {
        let _ = side
            .call(nr::HANDLE_CLOSE, &[reg(control)])
            .map_err(|_| "closing the control channel failed")?;
    }
    if ending == Ending::Stopped {
        let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
        while devfs::block_device(rdev).is_some() {
            if timer::now_nanos() > deadline {
                return Err("the disk stayed published after its driver stopped");
            }
            sched::yield_now();
        }
    }
    counter.published += 1;
    if ending == Ending::DriverDied {
        if !at_once {
            // Nothing reset the device: no ring until devmgr quiesces it.
            refused(
                side.call(nr::BLOCK_RING_CREATE, &[reg(device)]),
                status::ALREADY_BOUND,
                "a device whose driver died without a reset was given a new ring",
                counter,
            )?;
            let _ = side
                .call(nr::DEVICE_QUIESCE, &[reg(device)])
                .map_err(|_| "quiescing a device whose driver died failed")?;
        }
        // The quiesce waited for the ring's task to end, so the disk is
        // parked by now, and still published for the next driver.
        if devfs::block_device(rdev).is_none() || !is_parked(location) {
            return Err("a disk whose driver died was not kept published for the next one");
        }
        // And one again after; its task ends with the round's handles.
        let _ = ring(side, device)?;
        return Ok(());
    }
    // STOPPED said the device was reset: the next round's first ring shows
    // it free. A round ends only once the kernel's end has closed, which
    // comes after the device is released, so the next ring cannot race it.
    Ok(())
}

/// Require `device_info` to describe `node` as enumeration found it, every
/// virtio block inside one of its apertures, and the kernel's own START for
/// the node to agree with it.
///
/// Verifies: L.device.10
fn described(
    side: &Side,
    node: &Arc<DeviceNode>,
    device: Handle,
    location: ferrix_blkring::Location,
) -> Result<(), &'static str> {
    let _ = side
        .call(nr::DEVICE_INFO, &[reg(device), INFO])
        .map_err(|_| "device_info on a device handle failed")?;
    let bytes = side.get(INFO, DEVICE_INFO_BYTES)?;
    let word = |at: usize| -> Result<u32, &'static str> {
        bytes
            .get(at..at + 4)
            .and_then(|word| <[u8; 4]>::try_from(word).ok())
            .map(u32::from_ne_bytes)
            .ok_or("a short device_info")
    };
    let half = |at: usize| -> Result<u16, &'static str> {
        bytes
            .get(at..at + 2)
            .and_then(|word| <[u8; 2]>::try_from(word).ok())
            .map(u16::from_ne_bytes)
            .ok_or("a short device_info")
    };
    let long = |at: usize| -> Result<u64, &'static str> {
        bytes
            .get(at..at + 8)
            .and_then(|word| <[u8; 8]>::try_from(word).ok())
            .map(u64::from_ne_bytes)
            .ok_or("a short device_info")
    };
    if word(64)? != location.raw() {
        return Err("device_info named another location than the node's");
    }
    if word(72)? as usize != node.apertures().len() || word(76)? as usize != node.vector_count() {
        return Err("device_info counted the node's apertures or vectors wrongly");
    }
    let function = node
        .pci_function()
        .ok_or("a PCI node has no function record")?;
    if half(84)? != function.vendor || half(86)? != function.device {
        return Err("device_info named another vendor or device than enumeration read");
    }
    let virtio = half(90)?;
    let start = start_for(node, DiskName::for_index(0).ok_or("no name for disk 0")?);
    match (virtio, function.virtio, start) {
        (0, None, None) => return Ok(()),
        (DEVICE_VIRTIO_PCI, Some(_), Some(_)) => {}
        _ => return Err("device_info and the kernel's START disagree about a virtio transport"),
    }
    let start = start.ok_or("no START for a virtio node")?;
    for (at, block) in [
        (0, start.common),
        (16, start.notify),
        (32, start.isr),
        (48, start.device),
    ] {
        if long(at)? != block.phys
            || word(at + 8)? != block.offset
            || word(at + 12)? != block.length
        {
            return Err("device_info and the kernel's START place a virtio block differently");
        }
        if block.length == 0 {
            continue;
        }
        let span =
            (u64::from(block.offset) + u64::from(block.length)).div_ceil(PAGE_SIZE) * PAGE_SIZE;
        if node.aperture(block.phys, span).is_none() {
            return Err(
                "a virtio block device_info names is not inside one of the node's apertures",
            );
        }
    }
    if word(80)? != start.notify_off_multiplier || half(88)? != start.msix_table_size {
        return Err("device_info and the kernel's START disagree about the transport's numbers");
    }
    Ok(())
}

/// Make a ring on `device`, answering the driver's end of its control channel.
pub(super) fn ring(side: &Side, device: Handle) -> Result<Handle, &'static str> {
    side.handle(
        nr::BLOCK_RING_CREATE,
        &[reg(device)],
        "block_ring_create on a device with MANAGE failed",
    )
}

/// The HELLO for the disk this check describes, at `location`.
///
/// # Errors
///
/// A disk the crate no longer accepts, or a name it no longer numbers: a
/// change in `ferrix-blkring` the boot then reports.
pub(super) fn hello(location: ferrix_blkring::Location) -> Result<Hello, &'static str> {
    let device = Device::new(
        BLOCK_SIZE,
        CAPACITY,
        MAX_SECTORS,
        DeviceFlags::default(),
        PAGE_SIZE,
    )
    .map_err(|_| "the crate refused the check's disk")?;
    let name = DiskName::for_index(0).ok_or("the crate has no name for disk 0")?;
    let identity = Identity {
        location,
        serial: [0; ferrix_blkring::identity::SERIAL_BYTES],
        name,
    };
    Ok(Hello::new(&device, &identity))
}

/// The objects a HELLO carries, made in `side`: the ring VMO with its header
/// written, a data VMO, and a port; with exactly the rights the protocol
/// specifies when `reduced`, and as created otherwise.
pub(super) fn objects(side: &Side, reduced: bool) -> Result<[Handle; 3], &'static str> {
    let ring = side.handle(
        nr::VMO_CREATE,
        &[PAGE_SIZE],
        "vmo_create for the ring failed",
    )?;
    // The header as BLOCK-RING.md section 3.1 lays it out: magic, version 1,
    // no flags, the entry count, and where each array starts.
    let mut header = [0_u8; 64];
    header[0..4].copy_from_slice(b"FXBR");
    header[4..6].copy_from_slice(&1_u16.to_le_bytes());
    header[8..12].copy_from_slice(&ENTRIES.to_le_bytes());
    header[12..16].copy_from_slice(&64_u32.to_le_bytes());
    header[16..20].copy_from_slice(&(64 + ENTRIES * 32).to_le_bytes());
    side.put(HEADER, &header)?;
    side.put(OFFSET, &0_u64.to_ne_bytes())?;
    let _ = side
        .call(
            nr::VMO_WRITE,
            &[reg(ring), HEADER, header.len() as u64, OFFSET],
        )
        .map_err(|_| "writing the ring header failed")?;
    let data = side.handle(
        nr::VMO_CREATE,
        &[PAGE_SIZE],
        "vmo_create for the data failed",
    )?;
    let port = side.handle(nr::PORT_CREATE, &[], "port_create failed")?;
    if !reduced {
        return Ok([ring, data, port]);
    }
    let vmo_rights = u64::from(ferrix_blkring::control::VMO_RIGHTS.0);
    let port_rights = u64::from(ferrix_blkring::control::PORT_RIGHTS.0);
    Ok([
        side.handle(
            nr::HANDLE_REPLACE,
            &[reg(ring), vmo_rights],
            "reducing the ring VMO's rights failed",
        )?,
        side.handle(
            nr::HANDLE_REPLACE,
            &[reg(data), vmo_rights],
            "reducing the data VMO's rights failed",
        )?,
        side.handle(
            nr::HANDLE_REPLACE,
            &[reg(port), port_rights],
            "reducing the port's rights failed",
        )?,
    ])
}

/// Send `hello` with `handles` on `control`.
pub(super) fn send(
    side: &Side,
    control: Handle,
    hello: &Hello,
    handles: &[Handle],
) -> Result<(), &'static str> {
    let encoded = Message::Hello(*hello).encode();
    side.put(OUTBOX, encoded.as_bytes())?;
    let words: Vec<u8> = handles
        .iter()
        .flat_map(|handle| handle.0.to_ne_bytes())
        .collect();
    side.put(OUT_HANDLES, &words)?;
    let _ = side
        .call(
            nr::CHANNEL_WRITE,
            &[
                reg(control),
                OUTBOX,
                encoded.as_bytes().len() as u64,
                OUT_HANDLES,
                handles.len() as u64,
            ],
        )
        .map_err(|_| "sending a HELLO failed")?;
    Ok(())
}

/// Wait for `signals` on `control`, within the patience.
fn wait(side: &Side, control: Handle, signals: Signals) -> Result<Signals, &'static str> {
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    side.put(DEADLINE, &deadline.to_ne_bytes())?;
    let _ = side
        .call(
            nr::OBJECT_WAIT_ONE,
            &[reg(control), u64::from(signals.0), DEADLINE, OBSERVED],
        )
        .map_err(|_| "the ring's task did not answer in time")?;
    Ok(Signals(side.get_u32(OBSERVED)?))
}

/// Read the next message on `control`, with the handles it carried.
fn receive(side: &Side, control: Handle) -> Result<(Message, u32), &'static str> {
    let _ = wait(side, control, Signals::READABLE | Signals::PEER_CLOSED)?;
    let _ = side
        .call(
            nr::CHANNEL_READ,
            &[
                reg(control),
                INBOX,
                ferrix_blkring::control::MAX_BYTES as u64,
                IN_HANDLES,
                4,
                ACTUAL,
            ],
        )
        .map_err(|_| "reading the ring's answer failed")?;
    let bytes = side.get_u32(ACTUAL)? as usize;
    let handles = side.get_u32(ACTUAL + 4)?;
    let message = Message::decode(&side.get(INBOX, bytes)?)
        .map_err(|_| "the ring's answer is not a control message")?;
    Ok((message, handles))
}

/// Require REFUSED with `wanted`, then the kernel's end to close.
pub(super) fn expect_refusal(
    side: &Side,
    control: Handle,
    wanted: Refusal,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    match receive(side, control)? {
        (Message::Refused(reason), 0) if reason == wanted.raw() => {}
        (Message::Refused(_), _) => return Err("a HELLO was refused for the wrong reason"),
        _ => return Err("a HELLO that had to be refused was not"),
    }
    let observed = wait(side, control, Signals::PEER_CLOSED)?;
    if !observed.intersects(Signals::PEER_CLOSED) {
        return Err("the kernel kept its end of a refused ring open");
    }
    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(control)])
        .map_err(|_| "closing a refused ring's channel failed")?;
    counter.refusals += 1;
    Ok(())
}

/// Require READY carrying the completion port with `WRITE` alone.
pub(super) fn expect_ready(side: &Side, control: Handle) -> Result<(), &'static str> {
    match receive(side, control)? {
        (Message::Ready, 1) => {}
        (Message::Refused(reason), _) => return Err(refused_because(reason)),
        _ => return Err("READY did not carry exactly the completion port"),
    }
    let port = Handle(side.get_u32(IN_HANDLES)?);
    let rights = side
        .process
        .with_handles(|table| table.get(port).map(|(_, rights)| rights))
        .map_err(|_| "the completion port's handle is not in the table")?;
    if rights != Rights::WRITE {
        return Err("the completion port arrived with rights other than WRITE");
    }
    Ok(())
}

/// Why a HELLO as specified was refused, for the boot's report.
fn refused_because(reason: u32) -> &'static str {
    match Refusal::from_raw(reason) {
        Some(Refusal::Version) => "a HELLO as specified was refused: ring version mismatch",
        Some(Refusal::Queues) => "a HELLO as specified was refused: queues is not 1",
        Some(Refusal::Header) => "a HELLO as specified was refused: the ring header is invalid",
        Some(Refusal::Rights) => "a HELLO as specified was refused: a handle has the wrong rights",
        Some(Refusal::Malformed) => "a HELLO as specified was refused: malformed HELLO",
        Some(Refusal::Device) => "a HELLO as specified was refused: unusable device description",
        Some(Refusal::Name) => "a HELLO as specified was refused: malformed disk name",
        Some(Refusal::NameInUse) => "a HELLO as specified was refused: the disk name is in use",
        Some(Refusal::LocationInUse) => {
            "a HELLO as specified was refused: another driver serves this device"
        }
        Some(Refusal::WrongLocation) => {
            "a HELLO as specified was refused: the location is not the ring's own device"
        }
        None => "a HELLO as specified was refused for a reason this kernel does not know",
    }
}

/// Require a call to be refused with exactly `wanted`.
pub(super) fn refused(
    result: Result<usize, Errno>,
    wanted: Errno,
    what: &'static str,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    if result == Err(wanted) {
        counter.refusals += 1;
        Ok(())
    } else {
        Err(what)
    }
}
