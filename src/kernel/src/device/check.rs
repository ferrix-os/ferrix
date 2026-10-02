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

use ferrix_pci::header::{COMMAND, COMMAND_BUS_MASTER, COMMAND_MEMORY_SPACE};

use super::{
    DeviceNode, FIRST_SHARED_INTERRUPT, Failure, LEGACY_CONFIG_BYTES, Location, Report, Reserved,
    Trigger, Vector,
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

/// QEMU's `edu` test device: MSI and no MSI-X, and a register that raises
/// its interrupt on request (QEMU's `docs/specs/edu.rst`).
const EDU: (u16, u16) = (0x1234, 0x11E8);
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
    let _ = vmap::unmap_device(registers);
    report.msi_delivered += delivered.map_err(fail)?;
    Ok(())
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
pub(super) fn check_dma_switch(nodes: &[Arc<DeviceNode>]) -> Result<(), Failure> {
    let Some((node, config_phys)) = nodes.iter().find_map(|node| {
        let phys = node
            .pci
            .as_ref()
            .and_then(|function| function.config_phys)?;
        Some((node, phys))
    }) else {
        return Ok(());
    };
    let fail = |what| Failure {
        location: node.location(),
        what,
    };
    let unmappable = fail("a function's configuration space could not be mapped to read it");
    let config = vmap::map_device(config_phys, LEGACY_CONFIG_BYTES).map_err(|_| unmappable)?;
    let registers = Mmio::at(config);
    let at = u64::from(COMMAND);
    let found = registers.read16(at);
    let was_on = node.dma_on.load(Ordering::Acquire);

    let switched = (|| {
        node.enable_dma()
            .map_err(|_| fail("a function's DMA could not be turned on"))?;
        let on = registers.read16(at);
        if on & COMMAND_BUS_MASTER == 0 || on & COMMAND_MEMORY_SPACE == 0 {
            return Err(fail(
                "DMA turned on left the function's bus mastering or decoding off",
            ));
        }
        node.disable_dma()
            .map_err(|_| fail("a function's DMA could not be turned off"))?;
        let off = registers.read16(at);
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

    registers.write16(at, found);
    node.dma_on.store(was_on, Ordering::Release);
    let _ = vmap::unmap_device(config);
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
pub(crate) fn reserved(reserved: &Reserved) -> (usize, usize, u64) {
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
pub(crate) fn order(nodes: &[Arc<DeviceNode>]) -> (usize, u64) {
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
