//! Stage 10's self-check of a block ring for a device tree node: the SD
//! card's controller on an STM32MP15 board is one (`TREE_STM32_SDMMC`), and
//! its disk is named by the location word `DEVICE_NOT_PCI`
//! ([`super::TREE_LOCATION`]).
//!
//! No machine a boot gate runs has such a node -- QEMU's device tree nodes
//! are `virtio,mmio` transports, never a binding the kernel knows -- so the
//! check makes two of its own: unpublished, nothing minted, handed to a
//! process with `MANAGE` as devmgr would hand one. Through [`Side`], as
//! `check` does for a PCI function, it requires:
//!
//! * `block_ring_create` on a tree node answers a control channel;
//! * a HELLO naming any location but `DEVICE_NOT_PCI` is refused
//!   `WrongLocation`, and one naming it is answered READY and its disk
//!   published as `vda`;
//! * a second tree node is refused `ALREADY_BOUND` while the first holds the
//!   word: while its ring is served, and again after its driver died and
//!   the device was quiesced, while its disk is parked;
//! * the first node's next driver takes its parked disk up, and once it
//!   says STOPPED the second node is given a ring and its disk published;
//! * a `virtio,mmio` node, where the machine has one, is still refused
//!   `INVALID_ARGS`: it has no location a HELLO can name.
//!
//! [`Side`]: crate::object::check::Side

use alloc::sync::Arc;

use ferrix_blkring::Refusal;
use ferrix_blkring::kernel::Ending;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::status;
use ferrix_native_abi::types::TREE_STM32_SDMMC;
use ferrix_vfs::initramfs::makedev;

use super::check::{
    Counter, end, expect_ready, expect_refusal, hello, objects, published, refused, ring, send,
};
use super::{TREE_LOCATION, VIRTIO_BLK_MAJOR, forget_parked, is_parked};
use crate::console::println;
use crate::device::{self, DeviceNode, DmaShape};
use crate::fs::devfs;
use crate::object::check::{Side, device_handle, reg};

/// The two nodes' register addresses: no board's tree has a device there, so
/// the audit record of their quiesce names no real one. Nothing maps them.
const FIRST: u64 = 0xFFFF_E000;
const SECOND: u64 = 0xFFFF_F000;

/// A device tree node of the SD card's binding at `at`, as board support
/// would publish one, but published nowhere.
fn tree_node(at: u64) -> Result<Arc<DeviceNode>, &'static str> {
    let mut node = DeviceNode::empty(device::Location::Tree(at));
    node.bind_board(
        TREE_STM32_SDMMC,
        DmaShape {
            contiguous: false,
            coherent: false,
        },
    );
    crate::fallible::try_arc(node).map_err(|_| "no memory for a tree node")
}

/// Run the check and say what it found. `Err` names the first thing that
/// was not true.
///
/// # Errors
///
/// What was not true.
pub(crate) fn run() -> Result<(), &'static str> {
    let first = tree_node(FIRST)?;
    let second = tree_node(SECOND)?;
    let side = Side::new()?;
    let one = device_handle(&side, &first)?;
    let two = device_handle(&side, &second)?;
    let rdev = makedev(VIRTIO_BLK_MAJOR, 0);
    let mut counter = Counter::default();

    // A HELLO naming a PCI location for a tree node is refused, and the
    // node is free again after.
    let control = ring(&side, one)?;
    let elsewhere = ferrix_blkring::Location(0x0000_0008);
    send(&side, control, &hello(elsewhere)?, &objects(&side, true)?)?;
    expect_refusal(&side, control, Refusal::WrongLocation, &mut counter)?;
    super::check::settle()?;

    // The tree word: accepted, published.
    let control = accepted(&side, one, rdev)?;
    refused(
        side.call(nr::BLOCK_RING_CREATE, &[reg(two)]),
        status::ALREADY_BOUND,
        "a second tree node was given a ring while the first was served",
        &mut counter,
    )?;

    // Its driver dies; the device is quiesced, so the first node holds no
    // claim, and its disk is parked at the tree word.
    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(control)])
        .map_err(|_| "closing the control channel failed")?;
    let _ = side
        .call(nr::DEVICE_QUIESCE, &[reg(one)])
        .map_err(|_| "quiescing a tree node whose driver died failed")?;
    if !is_parked(TREE_LOCATION) || devfs::block_device(rdev).is_none() {
        return Err("a tree disk whose driver died was not parked for the next one");
    }
    refused(
        side.call(nr::BLOCK_RING_CREATE, &[reg(two)]),
        status::ALREADY_BOUND,
        "a second tree node was given a ring while the first one's disk was parked",
        &mut counter,
    )?;

    // The first node's next driver takes the parked disk up, then stops;
    // and then the second node may have the word.
    for device in [one, two] {
        let control = accepted(&side, device, rdev)?;
        let location = TREE_LOCATION;
        end(
            &side,
            device,
            control,
            rdev,
            location,
            Ending::Stopped,
            &mut counter,
        )?;
    }

    let mmio_checked = mmio_refused(&side, &mut counter)?;
    side.close_everything();
    super::check::settle()?;
    forget_parked(TREE_LOCATION);
    if devfs::block_device(rdev).is_some() || is_parked(TREE_LOCATION) {
        return Err("a tree disk outlived its driver's STOPPED");
    }
    println!(
        "  ring     tree nodes: {} calls and HELLOs refused as specified, {} disks published \
         under DEVICE_NOT_PCI, one tree node at a time{}",
        counter.refusals,
        counter.published,
        if mmio_checked {
            ", a virtio-mmio node refused"
        } else {
            ""
        },
    );
    Ok(())
}

/// A ring for `device`, its HELLO naming the tree word answered READY and
/// its disk published as `rdev`: the control channel.
fn accepted(side: &Side, device: Handle, rdev: u64) -> Result<Handle, &'static str> {
    let control = ring(side, device)?;
    send(side, control, &hello(TREE_LOCATION)?, &objects(side, true)?)?;
    expect_ready(side, control)?;
    published(rdev)?;
    Ok(control)
}

/// A virtio-mmio transport names no location a HELLO carries: refused
/// `INVALID_ARGS`, where the machine has one. Whether it had.
fn mmio_refused(side: &Side, counter: &mut Counter) -> Result<bool, &'static str> {
    let Some(node) = device::devices()
        .iter()
        .find(|node| matches!(node.location(), device::Location::VirtioMmio(_)))
        .cloned()
    else {
        return Ok(false);
    };
    let handle = device_handle(side, &node)?;
    refused(
        side.call(nr::BLOCK_RING_CREATE, &[reg(handle)]),
        status::INVALID_ARGS,
        "a virtio-mmio node was given a block ring",
        counter,
    )?;
    Ok(true)
}
