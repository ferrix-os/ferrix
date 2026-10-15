//! Stage 10's device node checks, run by [`super::publish`] on every boot
//! before the nodes are published: each node hands out exactly the
//! apertures and vectors it has and nothing past them, no two nodes share
//! memory or an interrupt, and the first MSI-X table's vector behaves as one.
//!
//! Moved here from `device.rs`, the bodies unchanged, so that each can name
//! the requirement it verifies (`docs/sysml/20-device-requirements.sysml`):
//! check code in a product file can carry no tag.

use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::sync::atomic::Ordering;

use ferrix_pci::ConfigSpace as _;
use ferrix_pci::header::{COMMAND, COMMAND_BUS_MASTER, COMMAND_MEMORY_SPACE};

use super::{
    Address, ConfigWrites, DeviceNode, DmaShape, FIRST_SHARED_INTERRUPT, Failure, Location,
    MsiFunction, PCI_CLASS_XHCI, PciFunction, RegisterBlock, Report, Reserved, TREE_STM32_USBH,
    Trigger, USB_INPUT_FUNCTIONS, Vector, VirtioBlocks,
};
use crate::mmio::Mmio;
use crate::object::interrupt::Interrupt;
use crate::{irq, timer, vmap};

/// Mint one PCI node's first MSI-X vector and require it to behave as one:
/// the same vector when asked again, no handler already on its number, an
/// entry that reads back unmasked and masked as told, and nothing minted past
/// the table.
///
/// Runs on a published node, because minting needs the node's place in
/// [`devices`], and on one only, because the vector is spent for good. The
/// entry is left masked; a driver that asks for entry 0 gets this vector.
///
/// Verifies: L.device.6, L.device.7
fn check_msix(node: &DeviceNode, report: &mut Report) -> Result<(), Failure> {
    let fail = |what| Failure {
        location: node.location(),
        what,
    };
    let Some(table) = &node.msix else {
        return Ok(());
    };
    if node.vector(node.vector_count()).is_some() {
        return Err(fail("a vector past the MSI-X table was minted"));
    }
    report.refusals += 1;
    let Some(first) = node.vector(0) else {
        // No vector to give, as on a machine without an MSI frame: nothing
        // to check, and no rule broken.
        return Ok(());
    };
    report.msix_minted += 1;
    if node.vector(0) != Some(first) {
        return Err(fail("an MSI-X entry was minted twice as different vectors"));
    }
    if irq::is_registered(first.number()) {
        return Err(fail(
            "an MSI-X vector was minted on a number with a handler",
        ));
    }
    if table.is_masked(0) != Some(true) {
        return Err(fail("a minted MSI-X entry was not masked"));
    }
    let unmasked = first.set_masked(false).map(|()| table.is_masked(0));
    let masked = first.set_masked(true).map(|()| table.is_masked(0));
    if unmasked != Ok(Some(false)) || masked != Ok(Some(true)) {
        return Err(fail("a minted MSI-X entry did not mask as told"));
    }
    Ok(())
}

/// The message-signalled vector checks, on the published nodes: the first
/// MSI-X table's ([`check_msix`]) and QEMU's `edu` device's MSI
/// ([`check_msi`]).
pub(super) fn check_vectors(
    published: &[Arc<DeviceNode>],
    report: &mut Report,
) -> Result<(), Failure> {
    report.msix_tables = published.iter().filter(|node| node.msix.is_some()).count();
    if let Some(node) = published.iter().find(|node| node.msix.is_some()) {
        check_msix(node, report)?;
    }
    check_msi(published, report)
}

// CHECKS-ONLY: device/config_check.rs
/// QEMU's `edu` test device: MSI and no MSI-X, and a register that raises
/// its interrupt on request (QEMU's `docs/specs/edu.rst`).
#[doc(hidden)]
pub const EDU: (u16, u16) = (0x1234, 0x11E8);
/// `edu`'s register that raises its interrupt with the bits written.
const EDU_RAISE: u64 = 0x60;
/// `edu`'s register that clears the bits written from its interrupt status.
const EDU_ACKNOWLEDGE: u64 = 0x64;
/// How long a raised MSI may take to arrive, or a masked one to stay away.
const MSI_WAIT_NANOS: u64 = 50_000_000;

/// Whether `interrupt` goes pending within [`MSI_WAIT_NANOS`].
fn arrives(interrupt: &Interrupt) -> bool {
    let deadline = timer::now_nanos().saturating_add(MSI_WAIT_NANOS);
    while timer::now_nanos() <= deadline {
        if interrupt.is_pending() {
            return true;
        }
        core::hint::spin_loop();
    }
    interrupt.is_pending()
}

/// Mint the one MSI vector of QEMU's `edu` device, if the machine has one,
/// and require it to behave as one: the same vector when asked again, none
/// past the one message, no handler already on its number, masked as minted,
/// unmasked and masked as told, delivered when the device raises it, and not
/// delivered while masked -- `edu` cannot mask its vector, so that is its
/// enable bit, with `INTx` off so that nothing arrives by a pin either.
///
/// Only `edu`: a vector is spent for good and the function's interrupt
/// configuration is written, which on a real machine belongs to the driver
/// that will hold it (`docs/NVIDIA.md` §2.3). The device is left with MSI
/// off and bus mastering off, as it was found.
///
/// Verifies: L.device.22
fn check_msi(nodes: &[Arc<DeviceNode>], report: &mut Report) -> Result<(), Failure> {
    report.msi_functions = nodes.iter().filter(|node| node.msi.is_some()).count();
    let Some(node) = nodes.iter().find(|node| {
        node.msi.is_some()
            && node
                .pci_function()
                .is_some_and(|function| (function.vendor, function.device) == EDU)
    }) else {
        return Ok(());
    };
    let fail = |what| Failure {
        location: node.location(),
        what,
    };
    if node.vector_count() != 1 || node.vector(1).is_some() {
        return Err(fail("a vector past the one MSI message was minted"));
    }
    report.refusals += 1;
    let Some(first) = node.vector(0) else {
        return Ok(());
    };
    report.msi_minted += 1;
    if node.vector(0) != Some(first) {
        return Err(fail("an MSI message was minted twice as different vectors"));
    }
    if irq::is_registered(first.number()) {
        return Err(fail("an MSI vector was minted on a number with a handler"));
    }
    if first.reads_masked() != Some(true) {
        return Err(fail("a minted MSI vector was not masked"));
    }
    // INTx off, so that a function masked by its enable bit raises nothing
    // by a pin either.
    if MsiFunction::intx_off(node) != Some(true) {
        return Err(fail("a minted MSI vector left INTx on"));
    }
    let unmasked = first.set_masked(false).map(|()| first.reads_masked());
    let masked = first.set_masked(true).map(|()| first.reads_masked());
    if unmasked != Ok(Some(false)) || masked != Ok(Some(true)) {
        return Err(fail("a minted MSI vector did not mask as told"));
    }

    let registers = node
        .apertures()
        .first()
        .and_then(|aperture| vmap::map_device(aperture.phys(), 0x1000).ok())
        .ok_or_else(|| fail("edu's registers could not be mapped"))?;
    let delivered = deliver(node, Mmio::at(registers), first);
    let forged = delivered
        .is_ok()
        .then(|| check_forged(node, Mmio::at(registers), nodes));
    let _ = vmap::unmap_device(registers);
    report.msi_delivered += delivered.map_err(fail)?;
    if let Some(forged) = forged
        && let Some(line) = forged.map_err(fail)?
    {
        crate::println!("  remap    {line}");
    }
    Ok(())
}

/// The data of a compatibility-format message in delivery mode NMI (bits
/// 10:8 = 100): its vector is ignored.
const DATA_NMI: u16 = 0b100 << 8;

/// How long check R3 holds the other function's vector after its forged
/// message before it requires that nothing arrived: a VT-d unit's patience.
const R3_PATIENCE_NANOS: u64 = 100_000_000;

/// How long after a refused message's fault is read a delivery is still
/// looked for.
const GRACE_NANOS: u64 = 2_000_000;

/// Checks R1 to R4 of `docs/NVIDIA.md` §12.3, on QEMU's `edu`, once its
/// own vector has been delivered remapped: `None` where its message is not
/// in remappable format, so no unit remaps it.
///
/// * **R1**: `edu`'s MSI capability programmed in compatibility format,
///   aimed at the check vector 0xFC, then raised: fault 0x25 from `edu`'s
///   source ID, and nothing delivered.
/// * **R2**: the same in delivery mode NMI: fault 0x25, and no NMI taken.
/// * **R3**: `edu`'s message programmed in remappable format naming another
///   function's entry -- the first MSI-X table's entry 0, minted by
///   [`check_msix`] -- then raised: fault 0x26 from `edu`, and nothing
///   delivered on that function's vector.
/// * **R4** is [`deliver`]'s, on `edu`'s own remapped vector, which this
///   requires to be remappable.
///
/// `edu`'s minted message is programmed back, masked, after each.
///
/// Verifies: `L.x86_64.129`, L.iommu.54, H.DMA.9
#[allow(clippy::disallowed_methods, reason = "a check of the core, calling the device's methods made pub for a raised check")]
fn check_forged(
    node: &DeviceNode,
    edu: Mmio,
    nodes: &[Arc<DeviceNode>],
) -> Result<Option<alloc::string::String>, &'static str> {
    let msi = node.msi.as_ref().ok_or("edu has no MSI capability")?;
    let Some((minted, minted_data)) = node.config_writes()?.state.msi else {
        return Err("edu's minted message was not recorded");
    };
    if minted >> 20 != 0xFEE || minted & 1 << 4 == 0 {
        return Ok(None);
    }
    let domain = node.domain().map_err(|_| "no memory for edu's domain")?;
    node.enable_dma()?;
    let result = (|| {
        let compatibility = ferrix_pci::msix::local_apic_message(
            u8::try_from(crate::arch::hardware_id()).unwrap_or(0),
            crate::iommu::CHECK_VECTOR,
        );
        // R1.
        let before = crate::iommu::open_check_window();
        let r1 = forge(
            node,
            edu,
            msi,
            &domain,
            compatibility.address,
            compatibility.data as u16,
            0x25,
            8,
            || crate::iommu::open_check_window() != before,
        );
        let seen = crate::iommu::close_check_window() - before;
        restore(node, msi, minted, minted_data)?;
        if !r1? || seen != 0 {
            return Err(if seen == 0 {
                "a compatibility-format message was not refused with fault 0x25"
            } else {
                "a compatibility-format message to 0xfc was delivered"
            });
        }
        // R2.
        let nmis = crate::iommu::nmis();
        let r2 = forge(
            node,
            edu,
            msi,
            &domain,
            compatibility.address,
            DATA_NMI,
            0x25,
            16,
            || crate::iommu::nmis() != nmis,
        );
        restore(node, msi, minted, minted_data)?;
        if crate::iommu::nmis() != nmis {
            return Err("a compatibility-format NMI was delivered");
        }
        if !r2? {
            return Err("a compatibility-format NMI was not refused with fault 0x25");
        }
        // R3.
        let (other, vector, address) = minted_elsewhere(node, nodes)?;
        let interrupt = Interrupt::new(vector)
            .map_err(|_| "the other function's vector could not be claimed")?;
        let r3 = forge(node, edu, msi, &domain, address, 0, 0x26, 32, || {
            interrupt.is_pending()
        });
        restore(node, msi, minted, minted_data)?;
        // Held for the unit's whole patience before it is looked at, so a
        // late delivery on the other function's vector is still caught
        // (condition 3 of the C6 review).
        let late = timer::now_nanos().saturating_add(R3_PATIENCE_NANOS);
        while timer::now_nanos() <= late && !interrupt.is_pending() {
            core::hint::spin_loop();
        }
        if interrupt.is_pending() {
            return Err("a message naming another function's entry was delivered");
        }
        if !r3? {
            return Err(
                "a message naming another function's entry was not refused with fault 0x26",
            );
        }
        drop(interrupt);
        Ok(Some(alloc::format!(
            "edu's forged messages refused: a compatibility-format one to 0xfc and an NMI each \
             faulted 0x25 and arrived nowhere, one naming {}'s entry faulted 0x26 and arrived \
             nowhere; its own remapped message delivered",
            other.location()
        )))
    })();
    node.disable_dma()?;
    result
}

/// Program `edu`'s MSI capability with `address` and `data`, raise its
/// interrupt with status bit `bit`, and say whether its unit refused it
/// with fault `reason` from `edu`'s stream before `arrived` said it was
/// delivered. The fault is registered as provoked first.
///
/// # Errors
///
/// The capability could not be written or masked.
#[expect(
    clippy::too_many_arguments,
    reason = "one forged message, every part of it named"
)]
#[allow(clippy::disallowed_methods, reason = "a check of the core, calling the device's methods made pub for a raised check")]
fn forge(
    node: &DeviceNode,
    edu: Mmio,
    msi: &MsiFunction,
    domain: &crate::iommu::Domain,
    address: u64,
    data: u16,
    reason: u8,
    bit: u32,
    arrived: impl Fn() -> bool,
) -> Result<bool, &'static str> {
    domain.provoke_interrupt(reason);
    {
        let mut config = node.config_writes()?;
        msi.msi
            .program(&mut config, ConfigWrites::FUNCTION, address, data);
    }
    msi.set_masked(node, false)?;
    edu.write32(EDU_RAISE, bit);
    let deadline = timer::now_nanos().saturating_add(MSI_WAIT_NANOS);
    let mut faulted = false;
    while timer::now_nanos() <= deadline && !arrived() {
        if let Some(fault) = domain.take_fault() {
            faulted = domain.stream() == Some(fault.stream)
                && matches!(fault.cause, crate::iommu::Cause::Interrupt { reason: seen, .. } if seen == reason);
            break;
        }
        core::hint::spin_loop();
    }
    let grace = timer::now_nanos().saturating_add(GRACE_NANOS);
    while timer::now_nanos() <= grace && !arrived() {
        core::hint::spin_loop();
    }
    edu.write32(EDU_ACKNOWLEDGE, bit);
    msi.set_masked(node, true)?;
    Ok(faulted && !arrived())
}

/// Program `edu`'s minted message back, masked.
#[allow(clippy::disallowed_methods, reason = "a check of the core, calling the device's methods made pub for a raised check")]
fn restore(
    node: &DeviceNode,
    msi: &MsiFunction,
    address: u64,
    data: u16,
) -> Result<(), &'static str> {
    msi.set_masked(node, true)?;
    let mut config = node.config_writes()?;
    msi.msi
        .program(&mut config, ConfigWrites::FUNCTION, address, data);
    Ok(())
}

/// Another function's minted MSI-X entry 0: the node, its vector and the
/// message address that names its entry.
fn minted_elsewhere<'a>(
    edu: &DeviceNode,
    nodes: &'a [Arc<DeviceNode>],
) -> Result<(&'a Arc<DeviceNode>, Vector, u64), &'static str> {
    nodes
        .iter()
        .filter(|other| !core::ptr::eq(other.as_ref(), edu))
        .find_map(|other| {
            let table = other.msix.as_ref()?;
            let &(_, address, _) = table.minted.lock().get(&0)?;
            Some((other, other.vector(0)?, address))
        })
        .ok_or("no other function has a minted MSI-X entry")
}

/// Raise `edu`'s interrupt with its vector claimed, masked and unmasked:
/// the deliveries seen, which must be the two unmasked ones.
fn deliver(node: &DeviceNode, edu: Mmio, vector: Vector) -> Result<usize, &'static str> {
    // A message is a write the device makes: bus mastering on, and nothing
    // pinned, so that it reaches nothing else.
    node.enable_dma()?;
    let result = (|| {
        let interrupt =
            Interrupt::new(vector).map_err(|_| "edu's MSI vector could not be claimed")?;
        let mut seen = 0;
        for (round, masked) in [(1u32, false), (2, true), (4, false)] {
            if masked {
                vector.mask()?;
            }
            edu.write32(EDU_RAISE, round);
            let arrived = arrives(&interrupt);
            edu.write32(EDU_ACKNOWLEDGE, round);
            if masked {
                vector.unmask()?;
            }
            match (masked, arrived) {
                (false, true) => seen += 1,
                (false, false) => return Err("edu raised its MSI and nothing arrived"),
                (true, true) => return Err("edu's MSI arrived while it was masked"),
                (true, false) => {}
            }
            interrupt.acknowledge()?;
        }
        Ok(seen)
    })();
    node.disable_dma()?;
    result
}

/// Require a USB host's node to allow eight input claims and every other
/// node one: an STM32 board's USB host by its binding and a PCI function of
/// exactly xHCI's class answer [`USB_INPUT_FUNCTIONS`]; a PCI function of
/// EHCI's class -- one programming interface away -- and a virtio one answer
/// one. The nodes are built here, not found, so every machine checks all
/// four.
///
/// Verifies: L.device.21
pub(super) fn check_input_functions() -> Result<(), Failure> {
    let function = |class, virtio| PciFunction {
        vendor: 0x1B36,
        device: 0x000D,
        class,
        revision: 0,
        subsystem_vendor: 0,
        subsystem: 0,
        secondary_bus: None,
        config_phys: None,
        virtio,
        msix_table_size: 0,
        host_visible: None,
    };
    let block = RegisterBlock {
        phys: 0,
        offset: 0,
        length: 0,
    };
    let virtio = VirtioBlocks {
        common: block,
        notify: block,
        isr: block,
        device: None,
        notify_multiplier: 0,
    };
    let pci = |class, virtio| {
        let location = Location::Pci(Address::new(0, 0, 0, 0)?);
        let mut node = DeviceNode::empty(location);
        node.pci = Some(function(class, virtio));
        Some(node)
    };
    let mut host = DeviceNode::empty(Location::Tree(0));
    host.bind_board(TREE_STM32_USBH, DmaShape::ORDINARY);
    let wanted = [
        (
            Some(host),
            USB_INPUT_FUNCTIONS,
            "an STM32 USB host's node does not allow eight input claims",
        ),
        (
            pci(PCI_CLASS_XHCI, None),
            USB_INPUT_FUNCTIONS,
            "an xHCI function's node does not allow eight input claims",
        ),
        (
            pci(PCI_CLASS_EHCI, None),
            1,
            "an EHCI function's node allows more than one input claim",
        ),
        (
            pci(PCI_CLASS_INPUT, Some(virtio)),
            1,
            "a virtio function's node allows more than one input claim",
        ),
    ];
    for (node, functions, what) in wanted {
        // A row that could not be built is a check that did not run.
        let Some(node) = node else {
            return Err(Failure {
                location: Location::Tree(0),
                what: "a node of the input claims check could not be built",
            });
        };
        if node.input_functions() != functions {
            return Err(Failure {
                location: node.location,
                what,
            });
        }
    }
    Ok(())
}

/// A USB host controller with EHCI's programming interface, 20, where xHCI's
/// is 30: the nearest class that must not be taken for a USB host's.
const PCI_CLASS_EHCI: u32 = 0x000C_0320;
/// An input device controller, as a virtio-input function reports itself.
const PCI_CLASS_INPUT: u32 = 0x0009_8000;

/// Require no two apertures anywhere to overlap, no vector to be held twice,
/// and every device tree vector to be a shared peripheral interrupt.
///
/// A second pass over what `publish` built rather than a restatement of how it
/// built it: this sorts every aperture of every node and compares neighbours.
///
/// Verifies: L.device.5
pub(super) fn check_exclusive(nodes: &[DeviceNode]) -> Result<(), Failure> {
    let mut all: Vec<(u64, u64, Location)> = nodes
        .iter()
        .flat_map(|node| {
            node.apertures
                .iter()
                .map(move |aperture| (aperture.phys, aperture.end(), node.location))
        })
        // FATAL-ALLOC: boot only: stage 10 builds the device registry once, before any program runs.
        .collect();
    all.sort_unstable_by_key(|&(start, ..)| start);
    for pair in all.windows(2) {
        if let [(_, end, _), (start, _, location)] = pair
            && start < end
        {
            return Err(Failure {
                location: *location,
                what: "two apertures overlap",
            });
        }
    }

    let mut numbers = BTreeSet::new();
    for node in nodes {
        for vector in &node.vectors {
            if matches!(node.location, Location::VirtioMmio(_) | Location::Tree(_))
                && vector.number < FIRST_SHARED_INTERRUPT
            {
                return Err(Failure {
                    location: node.location,
                    what: "a vector is not a shared peripheral interrupt",
                });
            }
            // FATAL-ALLOC: boot only: stage 10 builds the device registry once, before any program runs.
            if !numbers.insert(vector.number) {
                return Err(Failure {
                    location: node.location,
                    what: "two nodes hold one vector",
                });
            }
        }
    }
    Ok(())
}

/// Require one node's tokens to follow the rule.
///
/// Verifies: L.device.1, L.device.2, L.device.3, L.device.4
pub(super) fn check_node(
    node: &DeviceNode,
    reserved: &Reserved,
    report: &mut Report,
) -> Result<(), Failure> {
    let fail = |what| Failure {
        location: node.location(),
        what,
    };
    report.withheld += node.withheld;
    report.vectors_withheld += node.withheld_vectors;
    if node.undecoded {
        report.undecoded += 1;
    }

    for &whole in node.apertures() {
        report.apertures += 1;
        if !whole.whole_pages() {
            report.partial_pages += 1;
        }
        if reserved.covers(whole) {
            return Err(fail("an aperture overlaps memory the kernel uses"));
        }
        if node.aperture(whole.phys(), whole.len()) != Some(whole) {
            return Err(fail("an aperture did not authorise itself"));
        }
        let last = whole.end() - 1;
        if node.aperture(last, 1).is_none() {
            return Err(fail("an aperture's last byte was refused"));
        }
        // Past the end: one byte inside and one outside, which must be refused
        // even when another aperture starts at the next byte — a mapping lies
        // inside one aperture or it is not a mapping of this device.
        let refused = [
            node.aperture(last, 2),
            node.aperture(whole.phys(), 0),
            whole
                .phys()
                .checked_sub(1)
                .and_then(|below| node.aperture(below, 2)),
        ];
        if refused.iter().any(Option::is_some) {
            return Err(fail("an aperture was granted past its edge"));
        }
        report.refusals += refused.len();
    }
    if node.aperture(u64::MAX, 2).is_some() {
        return Err(fail("an aperture wrapped the address space"));
    }
    report.refusals += 1;

    // A PCI node's vectors are minted on demand and only once it is
    // published, so asking here would neither mint nor check anything;
    // `check_msix` does that after publishing.
    if node.msix.is_none() {
        for (index, &vector) in node.vectors.iter().enumerate() {
            report.vectors += 1;
            if vector.trigger() == Some(Trigger::Edge) {
                report.edge += 1;
            }
            if node.vector(index) != Some(vector) {
                return Err(fail("a vector was not handed out as recorded"));
            }
        }
        if node.vector(node.vector_count()).is_some() {
            return Err(fail("a vector past the end was handed out"));
        }
        report.refusals += 1;
    }
    for &(start, len) in &node.interrupt_tables {
        report.msix_withheld += 1;
        // The whole range, its first byte and its last: a driver that could
        // reach any of them could rewrite which interrupt the device raises.
        let refused = [
            node.aperture(start, len),
            node.aperture(start, 1),
            node.aperture(start + (len - 1), 1),
        ];
        if refused.iter().any(Option::is_some) {
            return Err(fail("an MSI-X table or pending-bit page was granted"));
        }
        report.refusals += refused.len();
    }
    Ok(())
}

/// A PCI function's bus mastering is what `enable_dma` and `disable_dma`
/// say, read back from its command register rather than taken from the
/// node's own flag: set after the one, with memory decoding on; clear after
/// the other, with memory decoding still on. So a quiesce's `disable_dma`
/// does stop the device reaching memory. The register and the flag are left
/// as they were found, whatever firmware set.
///
/// On the first published function with a configuration space; a machine
/// with none has nothing to switch.
///
/// Verifies: L.device.9
#[allow(clippy::disallowed_methods, reason = "a check of the core, calling the device's methods made pub for a raised check")]
pub(super) fn check_dma_switch(nodes: &[Arc<DeviceNode>]) -> Result<(), Failure> {
    let Some(node) = nodes.iter().find(|node| node.config_phys().is_some()) else {
        return Ok(());
    };
    let fail = |what| Failure {
        location: node.location(),
        what,
    };
    let unmappable = fail("a function's configuration space could not be mapped to read it");
    // As found, and what the kernel's state says of it, to put both back
    // under the configuration lock after.
    let (found, state) = {
        let config = node.config_writes().map_err(|_| unmappable)?;
        let found = config.read16(ConfigWrites::FUNCTION, COMMAND);
        (found, (config.state.memory, config.state.bus_master))
    };
    let command = || {
        node.mapped_config()
            .map(|config| config.read16(COMMAND))
            .ok_or(unmappable)
    };
    let was_on = node.dma_on.load(Ordering::Acquire);

    let switched = (|| {
        node.enable_dma()
            .map_err(|_| fail("a function's DMA could not be turned on"))?;
        let on = command()?;
        if on & COMMAND_BUS_MASTER == 0 || on & COMMAND_MEMORY_SPACE == 0 {
            return Err(fail(
                "DMA turned on left the function's bus mastering or decoding off",
            ));
        }
        node.disable_dma()
            .map_err(|_| fail("a function's DMA could not be turned off"))?;
        let off = command()?;
        if off & COMMAND_BUS_MASTER != 0 {
            return Err(fail("DMA turned off left the function's bus mastering on"));
        }
        if off & COMMAND_MEMORY_SPACE == 0 {
            return Err(fail(
                "turning a function's DMA off turned its memory decoding off",
            ));
        }
        Ok(())
    })();

    if let Ok(mut config) = node.config_writes() {
        config.write16(ConfigWrites::FUNCTION, COMMAND, found);
        (config.state.memory, config.state.bus_master) = state;
    }
    node.dma_on.store(was_on, Ordering::Release);
    switched
}

/// How many reserved ranges came from the memory map, how many did not, and
/// a digest of the latter that does not depend on their order: what the
/// `reserved` boot line prints, so that a change to how they are gathered is
/// compared by it.
///
/// The memory map's ranges are counted and left out of the digest: the
/// loader places them around the kernel image, so they move whenever the
/// image changes size, whatever gathered them.
pub fn reserved(reserved: &Reserved) -> (usize, usize, u64) {
    let (first, last) = reserved.map;
    let mut others = 0;
    // FNV-1a over each range's two ends, summed, so that gathering the same
    // ranges in another order gives the same digest.
    let digest = reserved
        .ranges
        .iter()
        .enumerate()
        .filter(|&(index, _)| index < first || index >= last)
        .fold(0_u64, |sum, (_, &(low, high))| {
            others += 1;
            let mut hash = 0xcbf2_9ce4_8422_2325_u64;
            for byte in low.to_le_bytes().into_iter().chain(high.to_le_bytes()) {
                hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
            }
            sum.wrapping_add(hash)
        });
    (last - first, others, digest)
}

/// How many nodes were published, and a digest of their locations in the
/// order they were published: what the `nodes` boot line prints, so that a
/// change to how nodes are found shows when it reorders them, which a count
/// cannot.
pub fn order(nodes: &[Arc<DeviceNode>]) -> (usize, u64) {
    let mut hasher = Fnv(0xcbf2_9ce4_8422_2325);
    for node in nodes {
        // Writing to an FNV sum cannot fail.
        let _ = write!(hasher, "{};", node.location);
    }
    (nodes.len(), hasher.0)
}

/// FNV-1a over everything written to it, in order.
struct Fnv(u64);

impl Write for Fnv {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
        }
        Ok(())
    }
}
