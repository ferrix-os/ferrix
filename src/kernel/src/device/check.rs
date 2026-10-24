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
use ferrix_bootinfo::PAGE_SIZE;

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
/// ([`check_msi`]); and the outcome of the held-off check enumeration ran
/// on its scratch node ([`check_held_off`]), if the machine had the case,
/// so that a failure there stops stage 10 as any other.
pub(super) fn check_vectors(
    published: &[Arc<DeviceNode>],
    report: &mut Report,
) -> Result<(), Failure> {
    report.msix_tables = published.iter().filter(|node| node.msix.is_some()).count();
    if let Some(node) = published.iter().find(|node| node.msix.is_some()) {
        check_msix(node, report)?;
    }
    check_msi(published, report)?;
    HELD_OFF.get().copied().unwrap_or(Ok(()))
}

// CHECKS-ONLY: device/config_check.rs; home: super
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
#[allow(
    clippy::disallowed_methods,
    reason = "the method's home: checks-only for the crates above, not here (ledger 1795, S3.2a)"
)]
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
#[allow(
    clippy::disallowed_methods,
    reason = "the method's home: checks-only for the crates above, not here (ledger 1795, S3.2a)"
)]
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
#[allow(
    clippy::disallowed_methods,
    reason = "the method's home: checks-only for the crates above, not here (ledger 1795, S3.2a)"
)]
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
    // A function in the build's MSI-preferred list whose MSI-X was held off
    // has its table pages granted (L.device.35); every other function's are
    // refused -- an unlisted one whatever the node says of its MSI-X, and a
    // listed one whose hold did not read back.
    let granted = node.msix_off.is_some()
        && node.pci_function().is_some_and(|function| {
            ferrix_pci::msix::msi_preferred(function.vendor, function.device)
        });
    for &(start, len) in node.interrupt_tables.iter().filter(|_| !granted) {
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
/// On the first published function with a configuration space that is not a
/// bridge; a machine with none has nothing to switch.
///
/// Verifies: L.device.9
#[allow(
    clippy::disallowed_methods,
    reason = "the method's home: checks-only for the crates above, not here (ledger 1795, S3.2a)"
)]
pub(super) fn check_dma_switch(nodes: &[Arc<DeviceNode>]) -> Result<(), Failure> {
    // Not a bridge (base class 06): an Intel host bridge's bus master enable
    // is hardwired on, so on the N150 box 00:00.0 failed this check; QEMU's
    // takes the write. A bridge is no device whose DMA a quiesce stops.
    let is_bridge = |node: &&Arc<DeviceNode>| {
        node.pci
            .as_ref()
            .is_some_and(|function| function.class >> 16 == 0x06)
    };
    let Some(node) = nodes
        .iter()
        .find(|node| node.config_phys().is_some() && !is_bridge(node))
    else {
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

/// PCI's refusal to turn a function's memory decoding on: its own BARs,
/// every other function's, and the reserved ranges, to why it is refused.
pub type DecodingRefusal = fn(&[(u64, u64)], &[(u64, u64)], &Reserved) -> Option<&'static str>;

/// The item's discovery policy that [`check_decoding_refusals`] and
/// [`check_lent_framebuffer`] hold the nodes to: PCI's refusal to turn memory
/// decoding on, and the firmware framebuffer's lending (`discovery::pci`,
/// `discovery::firmware`). The core may not name the item, so the item hands
/// it to [`super::publish`] (`docs/KERNEL-CRATES.md`, ledger 3294).
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// `discovery::pci::decoding_refusal`.
    pub decoding_refusal: DecodingRefusal,
    /// `discovery::firmware::may_lend`.
    pub may_lend: fn() -> bool,
    /// `discovery::firmware::refuses`.
    pub refuses: fn(u64, u64, crate::user::memory_type::MemoryType) -> bool,
    /// `discovery::firmware::KERNEL_MAPPING`.
    pub kernel_mapping: crate::user::memory_type::MemoryType,
}

/// What holds of the nodes taken together rather than one at a time:
/// [`check_exclusive`], [`check_input_functions`] and
/// [`check_lent_framebuffer`].
pub(super) fn check_together(
    nodes: &[DeviceNode],
    reserved: &Reserved,
    policy: &Policy,
) -> Result<(), Failure> {
    check_exclusive(nodes)?;
    check_widening()
        .and_then(|()| check_decoding_refusals(policy))
        .map_err(|what| Failure {
            location: Location::Tree(0),
            what,
        })?;
    check_input_functions()?;
    check_intel_gpu_yield(nodes).map_err(|what| Failure {
        location: Location::Tree(0),
        what,
    })?;
    check_lent_framebuffer(nodes, reserved, policy)
}

/// The Intel integrated GPU is not driven without `ferrix.intelgpu`
/// (`docs/INTEL-GPU.md` §4.4 (b)): on made-up nodes at `00:02.0`, Intel's
/// display function is yielded when the option was not said and not when it
/// was, while another vendor's display function and Intel's display
/// function anywhere else are never yielded for it; and on the published
/// nodes, every Intel GPU is yielded exactly when the option was not said.
///
/// Verifies: L.device.44
fn check_intel_gpu_yield(nodes: &[DeviceNode]) -> Result<(), &'static str> {
    let made = |bus, vendor| -> Result<DeviceNode, &'static str> {
        let address = Address::new(0, bus, 2, 0).ok_or("a check's PCI address is out of range")?;
        let mut node = DeviceNode::empty(Location::Pci(address));
        node.pci = Some(PciFunction {
            vendor,
            device: 0x46d4,
            class: 0x03_00_00,
            revision: 0,
            subsystem_vendor: 0,
            subsystem: 0,
            secondary_bus: None,
            config_phys: None,
            virtio: None,
            msix_table_size: 0,
            host_visible: None,
        });
        Ok(node)
    };
    for allowed in [false, true] {
        let mut made = [made(0, 0x8086)?, made(0, 0x1234)?, made(1, 0x8086)?];
        super::yield_intel_gpu(&mut made, allowed);
        let yielded = made.each_ref().map(DeviceNode::yielded);
        if yielded != [!allowed, false, false] {
            return Err("the Intel GPU was yielded other than exactly without ferrix.intelgpu");
        }
    }
    let allowed = super::intel_gpu_allowed();
    if nodes.iter().any(|node| {
        node.intel_gpu()
            && node.yielded() == allowed
            && node.framebuffer_mode().is_none()
            && !node.holds_framebuffer
    }) {
        return Err("a published Intel GPU's yield does not follow ferrix.intelgpu");
    }
    Ok(())
}

/// `publish`'s widening of a BAR shorter than a page, on made-up nodes: a
/// 2 KiB BAR alone in its page is widened to the page, and one whose page
/// also holds another node's aperture, another node's withheld MSI-X span, an
/// undecoded function's assigned BAR, or a reserved range, is left as it was.
/// Nothing here is published.
///
/// Verifies: L.device.5
fn check_widening() -> Result<(), &'static str> {
    use super::{Aperture, Source, occupants, widen_lone_partial_bars};

    // Far above any machine's RAM or MMIO this kernel boots on; the
    // ranges are only compared, never mapped.
    const AT: u64 = 0x7f00_0000_0000;
    const BAR: u64 = 2048;
    let address = Address::new(0, 0, 0, 0).ok_or("no PCI address")?;
    let candidate = || {
        let mut node = DeviceNode::empty(Location::Pci(address));
        node.assigned[5] = Some((AT, BAR));
        // FATAL-ALLOC: boot only: stage 10 builds the device registry once, before any program runs.
        node.apertures.push(Aperture {
            phys: AT,
            len: BAR,
            cacheable: false,
            source: Source::Bar {
                slot: 5,
                offset: 0,
                wide: false,
            },
        });
        node
    };
    let widened = |mut nodes: Vec<DeviceNode>, reserved: &Reserved| {
        let occupied = occupants(&nodes);
        widen_lone_partial_bars(&mut nodes, &occupied, reserved, false);
        nodes
            .first()
            .and_then(|node| node.apertures.first())
            .is_some_and(|aperture| aperture.len == PAGE_SIZE)
    };
    let nothing = Reserved::synthetic(None, &[]).map_err(|_| "no memory")?;
    if !widened(alloc::vec![candidate()], &nothing) {
        return Err("a BAR shorter than a page and alone in it was not widened to the page");
    }
    let neighbour = AT + BAR;
    let mut tree = DeviceNode::empty(Location::Tree(neighbour));
    tree.mint(neighbour, 256, false, &nothing);
    if widened(alloc::vec![candidate(), tree], &nothing) {
        return Err("a BAR was widened over another node's aperture");
    }
    let mut table = DeviceNode::empty(Location::Pci(address));
    // FATAL-ALLOC: boot only: stage 10 builds the device registry once, before any program runs.
    table.interrupt_tables.push((neighbour, BAR));
    if widened(alloc::vec![candidate(), table], &nothing) {
        return Err("a BAR was widened over another function's withheld MSI-X table");
    }
    let mut undecoded = DeviceNode::empty(Location::Pci(address));
    undecoded.assigned[0] = Some((neighbour, BAR));
    undecoded.undecoded = true;
    if widened(alloc::vec![candidate(), undecoded], &nothing) {
        return Err("a BAR was widened over an undecoded function's assigned BAR");
    }
    let reserved = Reserved::synthetic(None, &[(neighbour, 256)]).map_err(|_| "no memory")?;
    if widened(alloc::vec![candidate()], &reserved) {
        return Err("a BAR was widened over a reserved range");
    }
    Ok(())
}

/// PCI enumeration's refusal to turn memory decoding on, on made-up BARs: a
/// function whose memory BARs are assigned, clear of reserved memory and of
/// every other function's, may have it turned on; one with a BAR unassigned,
/// one overlapping a reserved range (RAM among them), and one overlapping
/// another function's BAR may not.
///
/// Verifies: L.device.1
fn check_decoding_refusals(policy: &Policy) -> Result<(), &'static str> {
    let decoding_refusal = policy.decoding_refusal;

    const AT: u64 = 0x7f00_0000_0000;
    const SIZE: u64 = 0x4000;
    let own = [(AT, SIZE), (AT + SIZE, PAGE_SIZE)];
    let elsewhere = [(AT + 0x10_0000, SIZE), (0, SIZE)];
    let nothing = Reserved::synthetic(None, &[]).map_err(|_| "no memory")?;
    if decoding_refusal(&own, &elsewhere, &nothing).is_some() {
        return Err("decoding was refused to assigned BARs clear of everything");
    }
    if decoding_refusal(&[(AT, SIZE), (0, PAGE_SIZE)], &elsewhere, &nothing).is_none() {
        return Err("decoding was allowed with a memory BAR unassigned");
    }
    let ram = Reserved::synthetic(None, &[(AT + SIZE + 0x800, 0x1000)]).map_err(|_| "no memory")?;
    if decoding_refusal(&own, &elsewhere, &ram).is_none() {
        return Err("decoding was allowed over a reserved range");
    }
    if decoding_refusal(&own, &[(AT + 0x2000, PAGE_SIZE)], &nothing).is_none() {
        return Err("decoding was allowed over another function's BAR");
    }
    Ok(())
}

/// [`check_framebuffer`] on the published nodes, then [`check_lending`]
/// against made-up ones.
fn check_lent_framebuffer(
    nodes: &[DeviceNode],
    reserved: &Reserved,
    policy: &Policy,
) -> Result<(), Failure> {
    check_framebuffer(nodes, reserved, policy)?;
    check_lending().map_err(|what| Failure {
        location: Location::Tree(0),
        what,
    })
}

/// The firmware framebuffer's node, when `ferrix.efifb` made one
/// (`docs/DISPLAY.md` §8): it holds exactly one aperture, wholly inside the
/// boot framebuffer [`Reserved`] holds and starting at its first byte; no
/// vector; nothing that reaches memory, so `vmo_pin` refuses it; and it is the
/// only node whose aperture overlaps the framebuffer -- which `check_node`'s
/// reserved test already holds of every other. At most one such node.
///
/// Verifies: L.device.1, L.device.38, L.device.39, L.device.40, L.device.41, L.device.43
fn check_framebuffer(
    nodes: &[DeviceNode],
    reserved: &Reserved,
    policy: &Policy,
) -> Result<(), Failure> {
    let mut seen = 0;
    // Only the framebuffer's own node holds a lent aperture, or, under
    // `ferrix.intelgpu`, the Intel GPU holding it, in place of that node
    // (K4, `L.device.46`).
    for node in nodes
        .iter()
        .filter(|node| node.framebuffer_mode().is_none())
    {
        let lent = node
            .apertures
            .iter()
            .any(|aperture| aperture.source == super::Source::Framebuffer);
        let to_intel = super::lend_target(nodes, super::intel_gpu_allowed())
            .and_then(|index| nodes.get(index))
            .is_some_and(|target| core::ptr::eq(target, node))
            && nodes.iter().all(|other| other.framebuffer_mode().is_none());
        if lent && !to_intel {
            return Err(Failure {
                location: node.location,
                what: "a node other than the framebuffer's holds the framebuffer",
            });
        }
    }
    for node in nodes
        .iter()
        .filter(|node| node.framebuffer_mode().is_some())
    {
        let fail = |what| Failure {
            location: node.location,
            what,
        };
        seen += 1;
        if seen > 1 {
            return Err(fail("a second firmware framebuffer node"));
        }
        if !(policy.may_lend)() {
            return Err(fail(
                "a firmware framebuffer node without ferrix.efifb on x86-64",
            ));
        }
        let Some((start, end)) = reserved.framebuffer() else {
            return Err(fail("a firmware framebuffer node with no boot framebuffer"));
        };
        let [aperture] = node.apertures.as_slice() else {
            return Err(fail(
                "the firmware framebuffer node does not hold exactly one aperture",
            ));
        };
        let visible = mode_bytes(node);
        if aperture.phys != start
            || aperture.end() > end
            || !aperture.whole_pages()
            || aperture.len != visible.div_ceil(PAGE_SIZE) * PAGE_SIZE
        {
            return Err(fail(
                "the firmware framebuffer node's aperture is not the framebuffer",
            ));
        }
        if !node.vectors.is_empty() {
            return Err(fail("the firmware framebuffer node has a vector"));
        }
        if node.reaches_memory()
            || node.domain().is_ok()
            || node.domain_made().is_some()
            || node.dma_on.load(Ordering::Acquire)
        {
            return Err(fail("the firmware framebuffer node would take pins"));
        }
        // The kernel's type hold: write-combining, the type of its own
        // window; an uncached mapping is refused, a write-combining one is
        // not.
        let refuses = policy.refuses;
        use crate::user::memory_type::MemoryType;
        if !refuses(aperture.phys, aperture.len, MemoryType::Device)
            || !refuses(aperture.phys, aperture.len, MemoryType::Cached)
            || refuses(aperture.phys, aperture.len, MemoryType::Combining)
        {
            return Err(fail(
                "the framebuffer's memory type is not held as the kernel's",
            ));
        }
        check_combining(aperture, policy).map_err(fail)?;
        // The boot console, which draws through a slice it holds alone, has
        // stopped: a driver can map the window from now on.
        if crate::console::screen::drawing() {
            return Err(fail("the boot console still draws on the lent framebuffer"));
        }
        // The functions that hold it -- found again from their BARs, not
        // from the mark minting left -- are driven by nobody while it is lent.
        let holds = |other: &DeviceNode| {
            other
                .bars
                .iter()
                .flatten()
                .any(|bar| bar.address < end && start < bar.address.saturating_add(bar.size))
        };
        if let Some(holder) = nodes.iter().find(|other| holds(other) && !other.yielded) {
            return Err(Failure {
                location: holder.location,
                what: "the function holding the lent framebuffer may still be driven",
            });
        }
    }
    Ok(())
}

/// The firmware framebuffer's exemption from [`Reserved`], on every boot and
/// whatever the command line says: against made-up ranges, a framebuffer of
/// 1024 x 768 is lent exactly its visible lines as one aperture of its own
/// source; not when another reserved range lies inside it, nor when it is
/// memory the allocator owns, nor when its size is one page short of its
/// lines; and an ordinary window at the same address is still withheld.
/// A framebuffer whose size is exactly its visible bytes, ending inside a
/// page (800 x 600), is lent its lines in whole pages, unless its size is
/// one byte short of them or another range reaches into that last page.
/// Nothing here is published.
///
/// Verifies: L.device.38
fn check_lending() -> Result<(), &'static str> {
    use ferrix_bootinfo::{Framebuffer, PixelFormat};

    const AT: u64 = 0x8000_0000;
    const SIZE: u64 = 16 * 1024 * 1024;
    const VISIBLE: u64 = 1024 * 768 * 4;
    let screen = Framebuffer {
        phys: AT,
        size: SIZE,
        width: 1024,
        height: 768,
        stride: 1024,
        format: PixelFormat::Bgrx8888,
        reclaimable: 0,
        reserved: 0,
    };
    let memory = (0, AT);
    let lone = Reserved::synthetic(Some((AT, SIZE)), &[memory]).map_err(|_| "no memory")?;
    let node = DeviceNode::firmware_framebuffer(&screen, &lone)
        .map_err(|_| "a framebuffer clear of everything else was not lent")?;
    match node.apertures.as_slice() {
        [aperture]
            if aperture.phys == AT
                && aperture.len == VISIBLE
                && aperture.source == super::Source::Framebuffer
                && aperture.cacheable => {}
        _ => return Err("the framebuffer was not lent exactly its visible lines"),
    }
    let crossed = Reserved::synthetic(Some((AT, SIZE)), &[memory, (AT + VISIBLE / 2, 4096)])
        .map_err(|_| "no memory")?;
    if DeviceNode::firmware_framebuffer(&screen, &crossed).is_ok() {
        return Err("the exemption lent a range another reservation holds");
    }
    let owned = Framebuffer {
        reclaimable: 1,
        ..screen
    };
    if DeviceNode::firmware_framebuffer(&owned, &lone).is_ok() {
        return Err("a framebuffer the allocator owns was lent");
    }
    let short = Framebuffer {
        size: VISIBLE - 4096,
        ..screen
    };
    if DeviceNode::firmware_framebuffer(&short, &lone).is_ok() {
        return Err("a framebuffer one page shorter than its lines was lent");
    }
    let mut window = DeviceNode::empty(Location::Tree(AT));
    window.mint(AT, VISIBLE, false, &lone);
    if window.apertures_minted() != 0 {
        return Err("an ordinary window over the framebuffer was minted");
    }
    check_lending_exact(AT)?;
    check_lending_intel(&screen, &lone, AT)?;
    check_lending_refusals(&screen, &lone, AT, SIZE)
}

/// K4 (`docs/INTEL-GPU.md` §5; ledger 2352 K4 (1)-(3)), on made-up nodes at
/// `00:02.0` whose BAR holds the framebuffer at `at`: the lend goes to the
/// Intel GPU only when the option was said and only to it -- not to
/// another vendor's display function holding it, nor to the Intel GPU that
/// does not -- and mints it the framebuffer's visible lines, exempt from
/// the reservation; the Intel GPU lent it takes no configuration write and
/// may be handed a copying card, which another vendor's function lent an
/// aperture by any path may not; and a lend whose hold is refused leaves
/// the Intel GPU holding none of the aperture.
///
/// Verifies: L.device.46, L.device.47, L.device.48
fn check_lending_intel(
    screen: &ferrix_bootinfo::Framebuffer,
    reserved: &Reserved,
    at: u64,
) -> Result<(), &'static str> {
    let made = |vendor, holds| -> Result<DeviceNode, &'static str> {
        let address = Address::new(0, 0, 2, 0).ok_or("a check's PCI address is out of range")?;
        let mut node = DeviceNode::empty(Location::Pci(address));
        node.holds_framebuffer = holds;
        node.pci = Some(PciFunction {
            vendor,
            device: 0x46d4,
            class: 0x03_00_00,
            revision: 0,
            subsystem_vendor: 0,
            subsystem: 0,
            secondary_bus: None,
            config_phys: None,
            virtio: None,
            msix_table_size: 0,
            host_visible: None,
        });
        Ok(node)
    };
    let nodes = [made(0x8086, true)?];
    if super::lend_target(&nodes, false).is_some() || super::lend_target(&nodes, true) != Some(0) {
        return Err(
            "the framebuffer was lent to the Intel GPU other than exactly under ferrix.intelgpu",
        );
    }
    let others = [made(0x1234, true)?, made(0x8086, false)?];
    if super::lend_target(&others, true).is_some() {
        return Err("the framebuffer was lent to a function that is not the Intel GPU holding it");
    }
    let [mut other, mut idle] = others;
    if other.lend_framebuffer_here(screen, reserved, true).is_ok()
        || idle.lend_framebuffer_here(screen, reserved, true).is_ok()
    {
        return Err("a function that is not the Intel GPU holding the framebuffer was lent it");
    }
    let [mut intel] = nodes;
    if intel.lend_framebuffer_here(screen, reserved, false).is_ok() {
        return Err("the Intel GPU was lent the framebuffer without ferrix.intelgpu");
    }
    intel
        .lend_framebuffer_here(screen, reserved, true)
        .map_err(|_| "the Intel GPU holding the framebuffer was not lent it")?;
    let visible = u64::from(screen.stride) * u64::from(screen.height) * 4;
    match intel.apertures.as_slice() {
        [aperture]
            if aperture.phys == at
                && aperture.len == visible
                && aperture.source == super::Source::Framebuffer => {}
        _ => return Err("the Intel GPU was not lent exactly the framebuffer's visible lines"),
    }
    if intel.config_write(0x04, 2, 0) != Err(super::ConfigRefusal::Denied) {
        return Err("the Intel GPU lent the framebuffer took a configuration write");
    }
    if !intel.may_copy_lent() || other.may_copy_lent() {
        return Err("a copying card over the lent framebuffer follows another rule than identity");
    }
    other.apertures = intel.apertures.clone();
    if other.may_copy_lent() {
        return Err("another vendor's function with the framebuffer's aperture may copy");
    }
    let mut refused = made(0x8086, true)?;
    if refused
        .lend_framebuffer_held(screen, reserved, true, |_, _| Err("a hold refused"))
        .is_ok()
        || refused.holds_lent_framebuffer()
        || refused.may_copy_lent()
    {
        return Err("the Intel GPU kept the framebuffer's aperture after its hold was refused");
    }
    Ok(())
}

/// [`check_lending`]'s framebuffer that ends inside a page: 800 x 600 with
/// firmware's size exactly its 1,920,000 visible bytes is lent 469 whole
/// pages, refused when its size is one byte short of them, and withheld when
/// another range starts in the last of its pages.
///
/// Verifies: L.device.38
fn check_lending_exact(at: u64) -> Result<(), &'static str> {
    use ferrix_bootinfo::{Framebuffer, PixelFormat};

    const VISIBLE: u64 = 800 * 600 * 4;
    const PAGES: u64 = VISIBLE.div_ceil(PAGE_SIZE) * PAGE_SIZE;
    let small = Framebuffer {
        phys: at,
        size: VISIBLE,
        width: 800,
        height: 600,
        stride: 800,
        format: PixelFormat::Bgrx8888,
        reclaimable: 0,
        reserved: 0,
    };
    let memory = (0, at);
    let lone = Reserved::synthetic(Some((at, VISIBLE)), &[memory]).map_err(|_| "no memory")?;
    let node = DeviceNode::firmware_framebuffer(&small, &lone)
        .map_err(|_| "an 800 x 600 framebuffer of exactly its lines was not lent")?;
    match node.apertures.as_slice() {
        [aperture] if aperture.phys == at && aperture.len == PAGES => {}
        _ => return Err("an 800 x 600 framebuffer was not lent its lines in whole pages"),
    }
    let short = Framebuffer {
        size: VISIBLE - 1,
        ..small
    };
    let short_lone =
        Reserved::synthetic(Some((at, VISIBLE - 1)), &[memory]).map_err(|_| "no memory")?;
    if DeviceNode::firmware_framebuffer(&short, &short_lone).is_ok() {
        return Err("an 800 x 600 framebuffer one byte short of its lines was lent");
    }
    let tail = Reserved::synthetic(Some((at, VISIBLE)), &[memory, (at + VISIBLE, 64)])
        .map_err(|_| "no memory")?;
    if DeviceNode::firmware_framebuffer(&small, &tail).is_ok() {
        return Err("a framebuffer was lent a last page another range reaches into");
    }
    Ok(())
}

/// [`check_lending`]'s other refusals, against the same framebuffer
/// `screen` of `size` bytes at `at` and its ranges `lone`: a layout other
/// than four bytes a pixel, a first pixel off a page, and the two clauses of
/// [`Reserved::lends`] -- a lent aperture reaching past the framebuffer, and
/// one inside a reserved range that is not the framebuffer, are withheld.
///
/// Verifies: L.device.38
fn check_lending_refusals(
    screen: &ferrix_bootinfo::Framebuffer,
    lone: &Reserved,
    at: u64,
    size: u64,
) -> Result<(), &'static str> {
    use ferrix_bootinfo::{Framebuffer, PixelFormat};

    let unknown = Framebuffer {
        format: PixelFormat::Unknown,
        ..*screen
    };
    if DeviceNode::firmware_framebuffer(&unknown, lone).is_ok() {
        return Err("a framebuffer of an unknown pixel layout was lent");
    }
    let off_page = Framebuffer {
        phys: at + 16,
        ..*screen
    };
    if DeviceNode::firmware_framebuffer(&off_page, lone).is_ok() {
        return Err("a framebuffer off a page boundary was lent");
    }
    let mut past = DeviceNode::empty(Location::Tree(at));
    past.mint_from(
        at,
        size + PAGE_SIZE,
        false,
        super::Source::Framebuffer,
        lone,
    );
    if past.apertures_minted() != 0 {
        return Err("a lent aperture reaching past the framebuffer was minted");
    }
    let mut elsewhere = DeviceNode::empty(Location::Tree(PAGE_SIZE));
    elsewhere.mint_from(
        PAGE_SIZE,
        PAGE_SIZE,
        false,
        super::Source::Framebuffer,
        lone,
    );
    if elsewhere.apertures_minted() != 0 {
        return Err(
            "a lent aperture inside a reserved range other than the framebuffer was minted",
        );
    }
    Ok(())
}

/// The bytes of the firmware framebuffer node's visible lines, from the mode
/// it reports.
fn mode_bytes(node: &DeviceNode) -> u64 {
    node.framebuffer_mode().map_or(0, |mode| {
        u64::from(mode.stride) * u64::from(mode.height) * 4
    })
}

/// What [`check_held_off`] found, kept from stage 10's enumeration, which
/// runs it, until [`super::publish`] reports it: `None` where no function
/// was its case (no QEMU `e1000e`, or `ferrix.checks=skip`).
pub static HELD_OFF: ferrix_sync::Once<Result<(), Failure>> = ferrix_sync::Once::new();

/// The held-off path, on a scratch node: QEMU's `e1000e` with QEMU's
/// subsystem (`ferrix_pci::msix::held_off_check_case`), built by
/// enumeration a second time as if it were in `MSI_PREFERRED` and never
/// published. Its MSI-X was held off through enumeration's own
/// configuration space before the node was built
/// (`ferrix_pci::msix::hold_off`), and `table` is where its MSI-X table and
/// pending-bit array lie, `(phys, len)` each. Required:
///
/// * MSI-X Enable reads 0 and Function Mask 1;
/// * the table's and the pending-bit array's whole ranges, and their first
///   and last bytes, lie inside the node's apertures -- granted, not
///   withheld -- as do the table pages the node records;
/// * the node offers no MSI-X table and one vector, its MSI message;
/// * that message mints once (the same vector asked again), on a number
///   with no handler, masked as minted;
/// * with Enable written 1 -- the mirror case, a driver or its firmware
///   turning MSI-X on behind the kernel -- the mint is refused and
///   [`DeviceNode::verify_config`] reports MSI-X control and refuses the
///   node.
///
/// No delivery is raised: QEMU's `msix_notify` sends nothing while MSI-X is
/// disabled (`hw/pci/msix.c`: `msix_function_masked` is set whenever Enable
/// is clear, and a masked vector only sets its pending bit), and `e1000e`
/// takes the MSI-X path only when MSI-X is enabled (`hw/net/e1000e_core.c`),
/// so a message written into the table could never be sent (the
/// consultant's V3, ledger line 1110). No domain is made and bus mastering
/// is never turned on.
///
/// What it leaves behind: the one MSI vector, and on a machine that remaps
/// interrupts its remapping entry, spent for good, as `edu`'s are; the
/// scratch node's mapping of configuration space, kept for the machine's
/// life as every node's is; and the MSI address, data and mask it
/// programmed, left in the capability with MSI Enable clear, where nothing
/// sends them.
///
/// The writes are stage 10's, before any node is published: enumeration
/// restores the function's command register, MSI and MSI-X control after
/// this returns, and builds the published node from fresh reads.
///
/// Verifies: L.device.35, L.device.36, L.device.37
#[allow(
    clippy::disallowed_methods,
    reason = "the method's home: checks-only for the crates above, not here (ledger 1795, S3.2a)"
)]
pub fn check_held_off(node: &DeviceNode, table: [(u64, u64); 2]) -> Result<(), Failure> {
    let fail = |what| Failure {
        location: node.location(),
        what,
    };
    let capability = node
        .msix_off
        .ok_or_else(|| fail("a held-off function's node does not hold its MSI-X off"))?;
    let control = {
        let config = node
            .config_writes()
            .map_err(|_| fail("a held-off function's configuration space could not be mapped"))?;
        config.read16(
            ConfigWrites::FUNCTION,
            capability + super::CAPABILITY_CONTROL,
        )
    };
    if control & super::CONTROL_ENABLE != 0 || control & super::CONTROL_FUNCTION_MASK == 0 {
        return Err(fail(
            "a held-off function's MSI-X did not read Enable 0 and Function Mask 1",
        ));
    }
    let granted = |phys: u64| {
        node.apertures()
            .iter()
            .any(|aperture| aperture.phys() <= phys && phys < aperture.end())
    };
    for (phys, len) in table {
        let last = phys.saturating_add(len.max(1) - 1);
        let whole = (phys..=last).step_by(4).all(granted);
        if !granted(phys) || !granted(last) || !whole {
            return Err(fail(
                "a held-off function's MSI-X table or pending-bit array was not granted",
            ));
        }
    }
    if node.interrupt_tables.is_empty()
        || !node
            .interrupt_tables
            .iter()
            .all(|&(start, len)| granted(start) && granted(start + (len - 1)))
    {
        return Err(fail("a held-off function's table pages were not granted"));
    }
    let offered = node.pci_function().map(|function| function.msix_table_size);
    if node.msix.is_some() || offered != Some(0) || node.vector_count() != 1 {
        return Err(fail("a held-off function offered its MSI-X table"));
    }
    let msi = node
        .msi
        .as_ref()
        .ok_or_else(|| fail("a held-off function offered no MSI vector"))?;
    let first = msi
        .mint(node)
        .map_err(|_| fail("a held-off function's MSI vector could not be minted"))?;
    if msi.mint(node).ok() != Some(first) {
        return Err(fail(
            "a held-off function's MSI was minted twice as different vectors",
        ));
    }
    if irq::is_registered(first.number()) {
        return Err(fail(
            "a held-off function's MSI was minted on a number with a handler",
        ));
    }
    if msi.is_masked(node) != Some(true) {
        return Err(fail("a held-off function's minted MSI was not masked"));
    }
    // The mirror case.
    {
        let mut config = node
            .config_writes()
            .map_err(|_| fail("a held-off function's configuration space could not be mapped"))?;
        let at = capability + super::CAPABILITY_CONTROL;
        let control = config.read16(ConfigWrites::FUNCTION, at);
        config.write16(ConfigWrites::FUNCTION, at, control | super::CONTROL_ENABLE);
    }
    if msi.mint(node).is_ok() {
        return Err(fail(
            "a held-off function's MSI was minted with its MSI-X reading enabled",
        ));
    }
    match node.verify_config() {
        Err(breach) if breach.register == super::Register::MsixControl && node.is_refused() => {}
        _ => {
            return Err(fail(
                "a held-off function's MSI-X turned on was not refused by verify_config",
            ));
        }
    }
    crate::println!(
        "  msix-off {} held off: Enable 0, table and pending bits granted, one MSI vector \
         minted, MSI-X turned on refused",
        node.location()
    );
    Ok(())
}

/// The lent framebuffer is write-combining wherever it is mapped
/// (`docs/DISPLAY.md` §8), read back rather than timed: its aperture is
/// prefetchable, so `io_mapping_map_combining` maps it; every processor
/// programmed its PAT, and this one's reads `X86_PAT` whole, whose entry 1,
/// the one a write-combining leaf selects, is write-combining; and every
/// page of the kernel's own window on it reads back write-combining, not
/// device, writable and not user's -- the type the kernel's hold keeps,
/// compared with what the leaves say rather than taken from the constant.
/// W8 reads a driver's mapping of it back the same way.
///
/// Verifies: L.device.38, L.device.39, L.device.43
fn check_combining(aperture: &super::Aperture, policy: &Policy) -> Result<(), &'static str> {
    use crate::user::memory_type::MemoryType;
    if !aperture.cacheable {
        return Err("the firmware framebuffer's aperture is not prefetchable");
    }
    if crate::arch::write_combining_processors() != Some(crate::smp::count())
        || crate::arch::pat() != Some(ferrix_paging::X86_PAT)
    {
        return Err("a processor's PAT is not the table with entry 1 write-combining");
    }
    let (window, mapped) =
        crate::panic::screen::window().ok_or("the kernel has no window on the lent framebuffer")?;
    let root = crate::mm::root_table();
    for page in 0..mapped.div_ceil(PAGE_SIZE) {
        let flags = crate::mm::flags_in(root, window + page * PAGE_SIZE)
            .ok_or("the kernel's window on the lent framebuffer has a hole")?;
        let kind = MemoryType::of(false, flags.write_combining && !flags.device);
        if kind != MemoryType::Combining
            || kind != policy.kernel_mapping
            || !flags.write
            || flags.user
        {
            return Err("the kernel's window on the lent framebuffer is not write-combining");
        }
    }
    Ok(())
}
