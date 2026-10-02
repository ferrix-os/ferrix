//! Stage 10: where the IOMMUs are, which one each PCI function's DMA arrives
//! at, and the domains that confine it there.
//!
//! Each firmware describes this its own way:
//!
//! * **The DMAR**, on x86-64: each VT-d unit's register block, and the
//!   endpoints behind it by bus, device and function. A device arrives at its
//!   unit as its requester ID.
//! * **The IORT**, on AArch64 under ACPI: a root complex's ID mappings,
//!   followed one hop to the `SMMUv3` that translates a requester ID, and the
//!   stream ID it arrives as.
//! * **The device tree**, on ARMv7-A: `arm,smmu-v3` nodes, and each ECAM host's
//!   `iommu-map` from requester IDs to a phandle and a stream ID.
//!
//! Every unit's registers are kept out of every driver's apertures, whether
//! or not the kernel programs it.
//!
//! # What cannot be followed is said, not guessed
//!
//! A DMAR scope is followed through the bridges enumeration found, down to
//! a function behind a root port or a switch. A path through something that
//! is not a bridge, a function whose DMA arrives under a PCIe-to-PCI bridge's
//! alias, a mapping that points at a node or phandle that is not there, and a
//! device tree host this cannot tell apart from another are reported as
//! unresolved rather than as behind no IOMMU. The difference matters: a
//! function counted as bypassing is one a domain will never be built for.
//!
//! # Domains
//!
//! A [`Domain`] is the memory one device's DMA may reach, and the device
//! addresses it reaches it by: a driver pins pages into its device's domain
//! and gives the device the addresses the pin returns.
//!
//! [`bring_up`] turns translation on, before any device is given DMA, for
//! every VT-d unit the DMAR describes and, under ACPI, every `SMMUv3` the IORT
//! does. A function behind one of those gets a translated domain
//! ([`domain_for`]): its unit lets it reach only the pages pinned into it,
//! each at its own physical address, and stops everything else, as it stops
//! every access of a function behind it that has no domain. `vtd` and
//! `smmuv3` program the units and their tables; [`Domain::pin`] and
//! [`Domain::unpin`] are the same for both.
//!
//! A domain is untranslated -- a device address is the physical address and
//! the device can reach all of memory -- for a function behind no unit, behind
//! one that would not come up, or behind an SMMU only a device tree describes,
//! as ARMv7-A's is, which is left alone (see `bring_up_smmu`). That is the
//! degraded trusted mode `docs/ARCHITECTURE.md` §7 requires the kernel to
//! announce, and the first pin into such a domain does.
//!
//! # Faults
//!
//! A unit that refuses an access records it. [`audit_faults`] reads every
//! translating unit's records last in the boot and fails it (FX-1007) on any
//! fault no check provoked; the out-of-domain probe in `pci/virtio.rs`
//! registers the one fault it provokes with [`Domain::provoke`].

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_acpi::dmar::{self, Structure};
use ferrix_acpi::iort;
use ferrix_bootinfo::{BootView, PAGE_SIZE};
use ferrix_fdt::{EcamHost, Fdt};
use ferrix_paging::{MapError, MapFlags};
use ferrix_pci::Address;
use ferrix_pci::topology::{self, Bridge};
use ferrix_sync::Once;

use crate::device::{DeviceNode, Location};
use crate::discovery::description::{self, Description};
use crate::sync::SpinLock;
use crate::{arch, mm, println};

mod check;
mod gate;
mod smmuv3;
mod vtd;

pub(crate) use check::{check_dma_faults, check_iommu, run as check_gate};

use gate::Gate;

/// Bytes of a VT-d unit's registers kept from drivers: the first page, which
/// holds every register a legacy-mode driver uses. A DRHD gives no length.
const VTD_WINDOW: u64 = 0x1000;

/// Bytes of an `SMMUv3`'s registers: its two 64 KiB register pages. The IORT
/// gives no length.
const SMMU_V3_WINDOW: u64 = 0x2_0000;

/// A DMAR device scope for everything below a bridge.
const SCOPE_PCI_SUB_HIERARCHY: u8 = 0x02;

/// What kind of IOMMU a unit is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    /// An Intel VT-d remapping unit.
    VtD,
    /// An Arm `SMMUv3`.
    SmmuV3,
}

/// One IOMMU.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Unit {
    /// What kind.
    pub(crate) kind: Kind,
    /// Physical address of its register block.
    pub(crate) phys: u64,
    /// Bytes of it no driver may be given.
    pub(crate) len: u64,
}

/// Where a function's DMA arrives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Placement {
    /// The function.
    pub(crate) function: Address,
    /// The unit, as an index into [`units`]' answer.
    pub(crate) unit: usize,
    /// What the unit sees it as: a VT-d source ID or an `SMMUv3` stream ID.
    pub(crate) stream: u32,
}

/// What firmware says about one function.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Behind {
    /// A unit translates its DMA, seeing it as this stream.
    Unit {
        /// The unit's index.
        unit: usize,
        /// The stream or source ID.
        stream: u32,
    },
    /// No unit does.
    Nothing,
    /// Firmware says something this cannot follow.
    Unresolved,
}

/// Every IOMMU the machine describes, each once.
pub(crate) fn units(view: &BootView<'_>) -> Vec<Unit> {
    let mut units = Vec::new();
    let machine = description::of(view);
    if let Description::Acpi(firmware) = machine {
        let tables = firmware.acpi();
        if let Ok(table) = tables.dmar() {
            for structure in table.structures() {
                if let Structure::Drhd(unit) = structure {
                    add(&mut units, Kind::VtD, unit.register_base, VTD_WINDOW);
                }
            }
        }
        if let Ok(table) = tables.iort() {
            for node in table.nodes() {
                if let Some(smmu) = node.smmu_v3() {
                    add(&mut units, Kind::SmmuV3, smmu.base_address, SMMU_V3_WINDOW);
                }
            }
        }
    } else if let Description::Tree(tree) = machine {
        for smmu in tree.smmu_v3s() {
            add(
                &mut units,
                Kind::SmmuV3,
                smmu.region.address,
                smmu.region.size,
            );
        }
    }
    units
}

/// Record a unit, unless it is empty or already recorded.
fn add(units: &mut Vec<Unit>, kind: Kind, phys: u64, len: u64) {
    if phys != 0 && len != 0 && !units.iter().any(|unit| unit.phys == phys) {
        // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
        units.push(Unit { kind, phys, len });
    }
}

/// The index of the unit whose registers are at `phys`.
fn unit_at(units: &[Unit], phys: u64) -> Option<usize> {
    units.iter().position(|unit| unit.phys == phys)
}

/// A placement in `units`, or unresolved when the unit firmware named is not
/// among them.
fn behind(units: &[Unit], phys: u64, stream: u32) -> Behind {
    unit_at(units, phys).map_or(Behind::Unresolved, |unit| Behind::Unit { unit, stream })
}

/// Where the DMAR puts `function`.
///
/// An endpoint scope names a function by its path from a start bus, and a
/// sub-hierarchy scope names a bridge, the bridge and everything below it
/// (VT-d 4.0 §8.3.1). Paths longer than one hop, and the buses below a
/// bridge, are followed through the bridges enumeration found
/// (`ferrix_pci::topology`). A function below a named bridge is placed only
/// when its own requester ID is what arrives there -- every bridge on the way
/// a PCIe port -- because a domain is built for that ID. One whose DMA
/// arrives under a bridge's alias, and a path through something that is not
/// a bridge, leave it unresolved rather than bypassing: it may be one of
/// those.
fn place_dmar(
    table: &dmar::Dmar<'_>,
    units: &[Unit],
    bridges: &[Bridge],
    function: Address,
) -> Behind {
    let mut unfollowed = false;
    for structure in table.structures() {
        let Structure::Drhd(unit) = structure else {
            continue;
        };
        if unit.segment != function.segment() {
            continue;
        }
        for scope in unit.device_scopes() {
            if scope.kind != dmar::SCOPE_PCI_ENDPOINT && scope.kind != SCOPE_PCI_SUB_HIERARCHY {
                continue;
            }
            let Some(named) =
                topology::follow_path(function.segment(), scope.start_bus, scope.path(), bridges)
            else {
                unfollowed = true;
                continue;
            };
            let own = named == function
                || (scope.kind == SCOPE_PCI_SUB_HIERARCHY
                    && match topology::behind(function, named, bridges) {
                        topology::Behind::Own => true,
                        topology::Behind::Aliased => {
                            unfollowed = true;
                            false
                        }
                        topology::Behind::No => false,
                    });
            if own {
                return behind(
                    units,
                    unit.register_base,
                    u32::from(function.requester_id()),
                );
            }
        }
    }
    if unfollowed {
        Behind::Unresolved
    } else {
        Behind::Nothing
    }
}

/// The bridges PCI enumeration found, every host's, for following DMAR
/// scopes through them: filled once at stage 10, before any function on a
/// bus behind one is examined, and only read from then on.
static BRIDGES: SpinLock<Vec<Bridge>> = SpinLock::new(Vec::new());

/// Record a bridge PCI enumeration found (`discovery::pci`).
pub(crate) fn learn_bridge(bridge: Bridge) {
    // FATAL-ALLOC: boot only: PCI enumeration runs once, at stage 10, before any program runs.
    BRIDGES.lock().push(bridge);
}

/// Where the IORT puts `function`: its segment's root complex, one mapping
/// on. A mapping to an ITS group is a function no `SMMUv3` translates.
fn place_iort(table: &iort::Iort<'_>, units: &[Unit], function: Address) -> Behind {
    let root = table.nodes().find(|node| {
        node.root_complex()
            .is_some_and(|complex| complex.segment == u32::from(function.segment()))
    });
    let Some(root) = root else {
        return Behind::Nothing;
    };
    let Some((stream, reference)) = root.translate(u32::from(function.requester_id())) else {
        return Behind::Nothing;
    };
    match table.node_at(reference) {
        None => Behind::Unresolved,
        Some(next) => match next.smmu_v3() {
            Some(smmu) => behind(units, smmu.base_address, stream),
            None if next.kind == iort::NODE_SMMU_V1_V2 => Behind::Unresolved,
            None => Behind::Nothing,
        },
    }
}

/// Where the device tree puts `function`: its host's `iommu-map`.
///
/// The host is the one naming the function's segment in `linux,pci-domain`,
/// or the only host when it names none; with several hosts and no domains the
/// kernel numbered the segments itself, and this does not guess which is
/// which.
fn place_tree(tree: &Fdt<'_>, units: &[Unit], function: Address) -> Behind {
    // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
    let hosts: Vec<EcamHost> = tree.ecam_hosts().collect();
    let named = hosts
        .iter()
        .find(|host| host.segment == Some(function.segment()));
    let host = match (named, hosts.as_slice()) {
        (Some(host), _) => host,
        (None, [only]) if only.segment.is_none() => only,
        (None, _) => return Behind::Unresolved,
    };
    let Some(map) = tree.ecam_iommu_map(host) else {
        return Behind::Nothing;
    };
    let Some((phandle, stream)) = map.translate(u32::from(function.requester_id())) else {
        return Behind::Nothing;
    };
    match tree.smmu_v3s().find(|smmu| smmu.phandle == Some(phandle)) {
        Some(smmu) => behind(units, smmu.region.address, stream),
        None => Behind::Unresolved,
    }
}

/// What discovery found.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Report {
    /// VT-d units.
    pub(crate) vtd: usize,
    /// `SMMUv3`s.
    pub(crate) smmu_v3: usize,
    /// PCI functions whose DMA a unit translates.
    pub(crate) behind: usize,
    /// PCI functions firmware puts behind no unit.
    pub(crate) bypassing: usize,
    /// PCI functions firmware describes in a way this cannot follow.
    pub(crate) unresolved: usize,
}

/// Find every unit, and place every PCI function among `nodes`.
fn discover(view: &BootView<'_>, nodes: &[Arc<DeviceNode>]) -> (Report, Vec<Unit>, Vec<Placement>) {
    let units = units(view);
    let mut report = Report {
        vtd: units.iter().filter(|unit| unit.kind == Kind::VtD).count(),
        smmu_v3: units
            .iter()
            .filter(|unit| unit.kind == Kind::SmmuV3)
            .count(),
        ..Report::default()
    };
    let functions: Vec<Address> = nodes
        .iter()
        .filter_map(|node| match node.location() {
            Location::Pci(address) => Some(address),
            Location::VirtioMmio(_) | Location::Tree(_) => None,
        })
        // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
        .collect();

    let mut placements = Vec::new();
    let mut record = |function: Address, found: Behind| match found {
        Behind::Unit { unit, stream } => {
            report.behind += 1;
            // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
            placements.push(Placement {
                function,
                unit,
                stream,
            });
        }
        Behind::Nothing => report.bypassing += 1,
        Behind::Unresolved => report.unresolved += 1,
    };

    let machine = description::of(view);
    if let Description::Acpi(firmware) = machine {
        let tables = firmware.acpi();
        let dmar = tables.dmar().ok();
        let iort = tables.iort().ok();
        let bridges = BRIDGES.lock();
        for &function in &functions {
            let found = match (&dmar, &iort) {
                (Some(table), _) => place_dmar(table, &units, &bridges, function),
                (None, Some(table)) => place_iort(table, &units, function),
                (None, None) => Behind::Nothing,
            };
            record(function, found);
        }
    } else if let Description::Tree(tree) = machine {
        for &function in &functions {
            record(function, place_tree(&tree, &units, function));
        }
    } else {
        for &function in &functions {
            record(function, Behind::Nothing);
        }
    }
    (report, units, placements)
}

/// Stage 10: find every IOMMU, and which one each PCI function's DMA arrives
/// at, and say so on the console.
///
/// Nothing here can fail the boot: firmware that describes no IOMMU, or one
/// this cannot follow, is reported and the boot goes on. `xtask test-boot`
/// requires the placements on the machines it configures.
pub(crate) fn report(view: &BootView<'_>, nodes: &[Arc<DeviceNode>]) {
    let (report, units, placements) = discover(view, nodes);
    println!(
        "  iommu    {} VT-d units, {} SMMUv3s; {} PCI functions behind one, {} bypassing, \
         {} unresolved",
        report.vtd, report.smmu_v3, report.behind, report.bypassing, report.unresolved,
    );
    for placement in placements {
        if let Some(unit) = units.get(placement.unit) {
            println!(
                "  iommu    pci {} behind the {:?} unit at {:#x} as stream {:#x}",
                placement.function, unit.kind, unit.phys, placement.stream,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Domains
// ---------------------------------------------------------------------------

/// The next domain's number, so a pin can be checked against the domain that
/// took it.
static NEXT_DOMAIN: AtomicU64 = AtomicU64::new(1);

/// Whether degraded trusted mode has been announced.
static DEGRADED: AtomicBool = AtomicBool::new(false);

/// Bits of I/O address a translated domain's tables reach. A translated domain
/// maps each page at its physical address, so a frame at or above this cannot
/// be pinned into one.
const TRANSLATED_BITS: u32 = 39;

/// A DMA fault a unit recorded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Fault {
    /// The device's stream: its source ID on VT-d, its stream ID on an
    /// `SMMUv3`.
    pub(crate) stream: u32,
    /// The page it addressed.
    pub(crate) page: u64,
    /// Whether the access was a write.
    pub(crate) write: bool,
    /// What the unit recorded it as.
    pub(crate) cause: Cause,
}

/// What a unit recorded a fault as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Cause {
    /// A device's access refused at an address its domain does not map or
    /// does not allow: every VT-d fault record, and an `SMMUv3` translation,
    /// address size, access flag or permission fault. The only kind a check
    /// provokes.
    Access,
    /// An `SMMUv3` event of any other type, by its number: a stream past the
    /// table, a stream table entry the unit would not use, a table walk that
    /// aborted, a transaction the unit does not support. It names a stream,
    /// and no check provokes one: each is a device's DMA stopped for a reason
    /// other than its address, or tables the kernel wrote that the unit
    /// refused, and either is worse than a refused address.
    Event(u8),
    /// An `SMMUv3` event queue that overflowed: the unit had events to record
    /// and no room, and dropped them. It names no stream, and it counts as
    /// stray, as an event does: what was lost is unknown, so none of it can be
    /// shown to be what a check provoked, and the probe provokes one fault
    /// where a full queue is 128 unread.
    Lost,
    /// A VT-d unit's primary fault overflow: a fault found the unit's record
    /// full and was dropped. The fault's `stream` and `page` are the full
    /// record's, the one taken last before the overflow was seen, and the
    /// lost fault is unknown. [`provoked`] says when it counts as stray.
    Overflow,
}

impl Fault {
    /// A refused write by `stream` to `page`: what a check that provokes one
    /// requires its unit to record.
    pub(crate) fn write_to(stream: u32, page: u64) -> Self {
        Fault {
            stream,
            page,
            write: true,
            cause: Cause::Access,
        }
    }
}

impl core::fmt::Display for Fault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.cause {
            Cause::Access => write!(
                f,
                "stream {:#x}, page {:#x}, {}",
                self.stream,
                self.page,
                if self.write { "a write" } else { "a read" }
            ),
            Cause::Event(kind) => write!(
                f,
                "stream {:#x}, SMMUv3 event {kind:#x} ({})",
                self.stream,
                smmuv3::event_name(kind)
            ),
            Cause::Lost => write!(f, "an SMMUv3 event queue overflowed: events were lost"),
            Cause::Overflow => write!(
                f,
                "a VT-d fault was lost to a full record, which held stream {:#x}, page {:#x}",
                self.stream, self.page
            ),
        }
    }
}

/// Why a domain refused to pin or unpin.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DomainError {
    /// There was nothing to pin.
    Empty,
    /// A frame's address does not fit in what the domain can address.
    OutOfRange,
    /// The pin was taken by another domain.
    Foreign,
    /// A page is already pinned into this domain.
    AlreadyPinned,
    /// The domain's tables could not be changed: no frame for a table.
    Tables,
    /// The unit never finished a command.
    Unit(&'static str),
}

/// The memory one device's DMA may reach, and the addresses it reaches it by.
///
/// A translated domain maps each pinned page at its own physical address.
/// What makes it a domain is what it leaves out: nothing is mapped but what was
/// pinned. That saves an I/O address allocator, at the cost of refusing frames
/// above [`TRANSLATED_BITS`].
#[derive(Debug)]
pub(crate) struct Domain {
    /// This domain's number.
    id: u64,
    /// Pages pinned and not yet unpinned.
    pinned: AtomicU64,
    /// Who translates it.
    translation: Translation,
}

/// Who translates a domain.
#[derive(Debug)]
enum Translation {
    /// Nobody: a device address is a physical address.
    None,
    /// A VT-d unit.
    VtD {
        /// The unit.
        unit: &'static vtd::Unit,
        /// The domain on it.
        attached: vtd::Attached,
        /// Held across a pin's or an unpin's changes to the tables and the
        /// unit's wait for them.
        changing: Gate,
    },
    /// An `SMMUv3`.
    SmmuV3 {
        /// The unit.
        unit: &'static smmuv3::Unit,
        /// The domain on it.
        attached: smmuv3::Attached,
        /// Held across a pin's or an unpin's changes to the tables and the
        /// unit's wait for them.
        changing: Gate,
    },
}

impl Translation {
    /// The lock a translated domain holds across a pin's or an unpin's
    /// changes, or `None` for an untranslated one.
    fn changing(&self) -> Option<&Gate> {
        match self {
            Translation::None => None,
            Translation::VtD { changing, .. } | Translation::SmmuV3 { changing, .. } => {
                Some(changing)
            }
        }
    }

    /// Map the page at `phys` at `iova`.
    fn map(&self, iova: u64, phys: u64, flags: MapFlags) -> Result<(), MapError> {
        match self {
            Translation::None => Ok(()),
            Translation::VtD { unit, attached, .. } => unit.map(attached, iova, phys, flags),
            Translation::SmmuV3 { unit, attached, .. } => unit.map(attached, iova, phys, flags),
        }
    }

    /// Take the page at `iova` out of the tables, holding every table that
    /// empties in `tables` until the unit's flush has completed.
    fn unmap(&self, iova: u64, tables: &mut mm::UnlinkedTables) -> Result<(), MapError> {
        match self {
            Translation::None => Ok(()),
            Translation::VtD { unit, attached, .. } => unit.unmap(attached, iova, tables),
            Translation::SmmuV3 { unit, attached, .. } => unit.unmap(attached, iova, tables),
        }
    }

    /// Make the unit forget what it cached, after an unmap or a map.
    fn flush(&self, after_map: bool) -> Result<(), &'static str> {
        match self {
            Translation::None => Ok(()),
            Translation::VtD { unit, attached, .. } => unit.flush(attached, after_map),
            Translation::SmmuV3 { unit, attached, .. } => unit.flush(attached, after_map),
        }
    }

    /// Where an access to `iova` lands.
    fn resolve(&self, iova: u64) -> Option<u64> {
        match self {
            Translation::None => Some(iova),
            Translation::VtD { unit, attached, .. } => unit.resolve(attached, iova),
            Translation::SmmuV3 { unit, attached, .. } => unit.resolve(attached, iova),
        }
    }

    /// The stream the unit sees the domain's device as.
    fn stream(&self) -> Option<u32> {
        match self {
            Translation::None => None,
            Translation::VtD { attached, .. } => Some(attached.stream()),
            Translation::SmmuV3 { attached, .. } => Some(attached.stream()),
        }
    }

    /// The next fault the domain's unit recorded, for any device.
    fn take_fault(&self) -> Option<Fault> {
        match self {
            Translation::None => None,
            Translation::VtD { unit, .. } => recorded(unit.take_fault()),
            Translation::SmmuV3 { unit, .. } => recorded(unit.take_fault()),
        }
    }

    /// Detach the domain from its unit and give back its tables.
    fn detach(&self) -> Result<(), &'static str> {
        match self {
            Translation::None => Ok(()),
            Translation::VtD { unit, attached, .. } => unit.detach(*attached),
            Translation::SmmuV3 { unit, attached, .. } => unit.detach(*attached),
        }
    }
}

/// Pages pinned into one domain, and each one's device address.
///
/// Goes back through [`Domain::unpin`] before its frames are freed. A pin that
/// cannot be given back is forgotten with its frames rather than dropped: the
/// device may still reach them.
#[derive(Debug)]
#[must_use = "unpin it, or leak its frames with it"]
pub(crate) struct Pinned {
    /// The domain that took it.
    domain: u64,
    /// Each page's device address, in the order its frame was given.
    addresses: Vec<u64>,
}

impl Pinned {
    /// Each page's device address, in the order its frame was given. Not
    /// necessarily contiguous.
    pub(crate) fn addresses(&self) -> &[u64] {
        &self.addresses
    }

    /// Never give the pin back: its pages stay reachable by the device for
    /// good, and whoever holds their frames must keep them too.
    pub(crate) fn leak(self) {
        let _ = core::mem::ManuallyDrop::new(self);
    }
}

impl Domain {
    /// A domain no unit translates.
    pub(crate) fn untranslated() -> Self {
        Domain::with(Translation::None)
    }

    /// A domain translated by `translation`.
    fn with(translation: Translation) -> Self {
        Domain {
            id: NEXT_DOMAIN.fetch_add(1, Ordering::Relaxed),
            pinned: AtomicU64::new(0),
            translation,
        }
    }

    /// Whether a unit translates this domain's DMA, so a device can reach only
    /// what is pinned into it.
    pub(crate) fn translated(&self) -> bool {
        !matches!(self.translation, Translation::None)
    }

    /// Where an access by the device to `address` lands, or `None` when the
    /// unit would fault it. An untranslated domain lands every access where it
    /// points.
    pub(crate) fn resolve(&self, address: u64) -> Option<u64> {
        self.translation.resolve(address)
    }

    /// The stream the unit sees this domain's device as, if a unit translates
    /// the domain.
    pub(crate) fn stream(&self) -> Option<u32> {
        self.translation.stream()
    }

    /// The next DMA fault the domain's unit recorded, for any device on it, or
    /// `None`. Taking it clears it, so the unit can record the next.
    ///
    /// A fault no check provoked is counted on the way, so that one taken and
    /// then passed over still fails the boot in [`audit_faults`].
    pub(crate) fn take_fault(&self) -> Option<Fault> {
        let fault = self.translation.take_fault()?;
        let _ = count_if_stray(fault);
        Some(fault)
    }

    /// Clear the unit's faults, and say that this domain's device is about to
    /// be made to write `page`, which the domain does not map, so the fault the
    /// unit records for it is known for the check's own and not counted stray.
    pub(crate) fn provoke(&self, page: u64) {
        self.clear_faults();
        if let Some(stream) = self.stream() {
            record_provoked(stream, page);
        }
    }

    /// Clear every fault the domain's unit holds, so the next one read is new.
    /// Bounded: a device faulting without end cannot keep the caller here.
    /// Nothing provoked what is cleared, so [`Domain::take_fault`] counts each.
    fn clear_faults(&self) {
        for _ in 0..256 {
            if self.take_fault().is_none() {
                return;
            }
        }
    }

    /// Pages pinned and not yet unpinned.
    pub(crate) fn pinned_pages(&self) -> u64 {
        self.pinned.load(Ordering::Relaxed)
    }

    /// Make `frames` reachable by the device, writable when `flags` says so,
    /// and say at which device addresses.
    ///
    /// # Errors
    ///
    /// [`DomainError::Empty`] for no frames, [`DomainError::OutOfRange`] for a
    /// frame the domain cannot address, [`DomainError::AlreadyPinned`],
    /// [`DomainError::Tables`] and [`DomainError::Unit`]. Whatever was mapped
    /// before a failure is unmapped again, except after a unit that never
    /// finished, whose pages stay mapped for good.
    pub(crate) fn pin(&self, frames: &[u64], flags: MapFlags) -> Result<Pinned, DomainError> {
        if frames.is_empty() {
            return Err(DomainError::Empty);
        }
        let mut addresses =
            crate::fallible::try_with_capacity(frames.len()).map_err(|_| DomainError::Tables)?;
        for frame in frames {
            let phys = frame
                .checked_mul(PAGE_SIZE)
                .ok_or(DomainError::OutOfRange)?;
            let _ = crate::fallible::push_within(&mut addresses, phys);
        }
        if let Some(changing) = self.translation.changing() {
            if addresses.iter().any(|&phys| phys >> TRANSLATED_BITS != 0) {
                return Err(DomainError::OutOfRange);
            }
            let _held = changing.enter().map_err(DomainError::Unit)?;
            map_all(&self.translation, &addresses, flags)?;
        } else if !DEGRADED.swap(true, Ordering::Relaxed) {
            println!(
                "  iommu    degraded trusted mode: no IOMMU domain is programmed, so device \
                 DMA reaches all of memory"
            );
        }
        let _ = self
            .pinned
            .fetch_add(addresses.len() as u64, Ordering::Relaxed);
        Ok(Pinned {
            domain: self.id,
            addresses,
        })
    }

    /// Take `pinned`'s pages back out of the domain.
    ///
    /// On a translated domain the device can no longer reach them once this
    /// returns, and their frames may be freed. On an untranslated one it still
    /// can: nothing stands between the device and physical memory, so the
    /// frames may be freed only once the device is known to be quiet — reset,
    /// or never given the addresses — and are otherwise held for good.
    ///
    /// # Errors
    ///
    /// [`DomainError::Foreign`], handing the pin back, when another domain took
    /// it.
    pub(crate) fn unpin(&self, pinned: Pinned) -> Result<(), (DomainError, Pinned)> {
        if pinned.domain != self.id {
            return Err((DomainError::Foreign, pinned));
        }
        if let Some(changing) = self.translation.changing() {
            let _held = match changing.enter() {
                Ok(entered) => entered,
                Err(why) => return Err((DomainError::Unit(why), pinned)),
            };
            // A failure part-way leaves the pages before it unmapped but not
            // yet flushed, and the pages after it still mapped: the domain
            // then holds a partial pin for good, and the caller keeps every
            // frame of it -- and the tables it emptied are kept too, as
            // `tables` is dropped unreleased.
            let mut tables = mm::UnlinkedTables::of(mm::TableOwner::Device);
            if pinned
                .addresses
                .iter()
                .any(|&iova| self.translation.unmap(iova, &mut tables).is_err())
            {
                return Err((DomainError::Tables, pinned));
            }
            // Only once the unit has forgotten the pages may their frames go,
            // or the tables that led to them (finding F-36).
            if let Err(why) = self.translation.flush(false) {
                return Err((DomainError::Unit(why), pinned));
            }
            tables.release();
        }
        let _ = self
            .pinned
            .fetch_sub(pinned.addresses.len() as u64, Ordering::Relaxed);
        Ok(())
    }
}

/// Map every page in `addresses` at its own address in `translation`'s
/// tables, or none of them.
fn map_all(
    translation: &Translation,
    addresses: &[u64],
    flags: MapFlags,
) -> Result<(), DomainError> {
    for (done, &phys) in addresses.iter().enumerate() {
        let Err(error) = translation.map(phys, phys, flags) else {
            continue;
        };
        let mut tables = mm::UnlinkedTables::of(mm::TableOwner::Device);
        for &mapped in addresses.iter().take(done) {
            let _ = translation.unmap(mapped, &mut tables);
        }
        if translation.flush(false).is_ok() {
            tables.release();
        }
        return Err(match error {
            MapError::AlreadyMapped(_) => DomainError::AlreadyPinned,
            _ => DomainError::Tables,
        });
    }
    translation.flush(true).map_err(DomainError::Unit)
}

impl Drop for Domain {
    /// Detach a translated domain from its unit, unless pages are still pinned
    /// into it: the device may still be using those, so its tables and context
    /// entry stay.
    fn drop(&mut self) {
        if self.translated() && self.pinned.load(Ordering::Relaxed) == 0 {
            let _ = self.translation.detach();
        }
    }
}

// ---------------------------------------------------------------------------
// Units the kernel programs
// ---------------------------------------------------------------------------

/// Every unit translation was turned on for, and the functions behind each.
#[derive(Debug, Default)]
struct Programmed {
    /// VT-d units translating.
    vtd: Vec<vtd::Unit>,
    /// The functions the DMAR's single-hop endpoint scopes name, with their
    /// unit's index.
    behind: Vec<(Address, usize)>,
    /// The DMAR's other PCI scopes -- longer paths, and sub-hierarchies --
    /// with their unit's index: followed through the bridges enumeration
    /// finds, once it has ([`learn_bridge`]).
    scopes: Vec<(Scope, usize)>,
    /// `SMMUv3`s translating.
    smmu: Vec<smmuv3::Unit>,
    /// The requester IDs IORT root complexes send to them.
    streams: Vec<StreamMap>,
}

/// A DMAR device scope kept past bring-up, whose path is followed only once
/// enumeration has found the bridges it runs through.
#[derive(Debug)]
struct Scope {
    /// The segment of its unit.
    segment: u16,
    /// Whether it names a bridge and everything below it.
    hierarchy: bool,
    /// The bus its path starts on.
    start_bus: u8,
    /// Its `(device, function)` hops.
    path: Vec<(u8, u8)>,
}

impl Scope {
    /// Whether it puts `function` behind its unit as `function`'s own
    /// requester ID.
    fn places(&self, function: Address, bridges: &[Bridge]) -> bool {
        let Some(named) = topology::follow_path(
            self.segment,
            self.start_bus,
            self.path.iter().copied(),
            bridges,
        ) else {
            return false;
        };
        named == function
            || (self.hierarchy
                && topology::behind(function, named, bridges) == topology::Behind::Own)
    }
}

/// A range of requester IDs on one segment that an IORT root complex sends to
/// a programmed `SMMUv3`, and the stream IDs they arrive as.
#[derive(Clone, Copy, Debug)]
struct StreamMap {
    /// The segment.
    segment: u16,
    /// The first requester ID.
    first: u32,
    /// How many.
    count: u64,
    /// The stream ID the first arrives as.
    stream: u32,
    /// The unit, as an index into `smmu`.
    unit: usize,
}

/// What [`bring_up`] turned on.
static PROGRAMMED: Once<Programmed> = Once::new();

/// What bringing the units up did.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BringUp {
    /// VT-d units now translating.
    pub(crate) vtd: usize,
    /// `SMMUv3`s now translating.
    pub(crate) smmu_v3: usize,
    /// Units left alone.
    pub(crate) refused: usize,
    /// Why the last of them was.
    pub(crate) why: Option<&'static str>,
}

/// Turn translation on for every VT-d unit the DMAR describes, before any
/// device is given DMA. From here a function behind one reaches only what a
/// domain maps for it, and one with no domain reaches nothing.
///
/// A unit that cannot be brought up is left alone, and the functions behind it
/// get untranslated domains. Only the first call does anything.
pub(crate) fn bring_up(view: &BootView<'_>) -> BringUp {
    let mut report = BringUp::default();
    let mut programmed = Programmed::default();
    if let Description::Acpi(firmware) = description::of(view) {
        let tables = firmware.acpi();
        if let Ok(table) = tables.dmar() {
            bring_up_vtd(&table, &mut programmed, &mut report);
        }
        if let Ok(table) = tables.iort() {
            bring_up_smmu(&table, &mut programmed, &mut report);
        }
    }
    let _ = PROGRAMMED.call_once(|| programmed);
    report
}

/// Program every VT-d unit the DMAR describes, and record the functions its
/// endpoint scopes name.
fn bring_up_vtd(table: &dmar::Dmar<'_>, programmed: &mut Programmed, report: &mut BringUp) {
    for structure in table.structures() {
        let Structure::Drhd(unit) = structure else {
            continue;
        };
        let opened =
            vtd::Unit::open(unit.register_base).and_then(|opened| opened.enable().map(|()| opened));
        match opened {
            Ok(opened) => {
                let index = programmed.vtd.len();
                programmed
                    .behind
                    // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
                    .extend(endpoints(&unit).map(|address| (address, index)));
                for scope in unit.device_scopes() {
                    let hierarchy = scope.kind == SCOPE_PCI_SUB_HIERARCHY;
                    // Single-hop endpoints are in `behind` already.
                    let longer_path =
                        scope.kind == dmar::SCOPE_PCI_ENDPOINT && scope.endpoint().is_none();
                    if !hierarchy && !longer_path {
                        continue;
                    }
                    let kept = Scope {
                        segment: unit.segment,
                        hierarchy,
                        start_bus: scope.start_bus,
                        // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
                        path: scope.path().collect(),
                    };
                    // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
                    programmed.scopes.push((kept, index));
                }
                // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
                programmed.vtd.push(opened);
                report.vtd += 1;
            }
            Err(why) => {
                report.refused += 1;
                report.why = Some(why);
            }
        }
    }
}

/// Program every `SMMUv3` the IORT describes, and record which requester IDs
/// each root complex sends to one, and as which streams.
///
/// Only under ACPI. On ARMv7-A the device tree's SMMU is left alone: U-Boot
/// keeps its virtio devices from offering the platform's DMA translation, so
/// they would bypass it anyway, which the stage 10 exit criterion states as
/// degraded trusted mode.
fn bring_up_smmu(table: &iort::Iort<'_>, programmed: &mut Programmed, report: &mut BringUp) {
    let doorbell = arch::msi_doorbell();
    let mut offsets: Vec<(u32, usize)> = Vec::new();
    for node in table.nodes() {
        let Some(smmu) = node.smmu_v3() else {
            continue;
        };
        let opened = smmuv3::Unit::open(smmu.base_address, doorbell)
            .and_then(|opened| opened.enable().map(|()| opened));
        match opened {
            Ok(opened) => {
                // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
                offsets.push((node.offset, programmed.smmu.len()));
                // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
                programmed.smmu.push(opened);
                report.smmu_v3 += 1;
            }
            Err(why) => {
                report.refused += 1;
                report.why = Some(why);
            }
        }
    }
    for node in table.nodes() {
        let Some(complex) = node.root_complex() else {
            continue;
        };
        let Ok(segment) = u16::try_from(complex.segment) else {
            continue;
        };
        // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
        programmed.streams.extend(
            node.id_mappings()
                .filter(|mapping| !mapping.is_single())
                .filter_map(|mapping| {
                    let &(_, unit) = offsets
                        .iter()
                        .find(|(offset, _)| *offset == mapping.output_reference)?;
                    Some(StreamMap {
                        segment,
                        first: mapping.input_base,
                        count: mapping.count,
                        stream: mapping.output_base,
                        unit,
                    })
                }),
        );
    }
}

/// The functions a DRHD's single-hop endpoint scopes name.
fn endpoints<'a>(unit: &dmar::Drhd<'a>) -> impl Iterator<Item = Address> + 'a {
    let segment = unit.segment;
    unit.device_scopes()
        .filter(|scope| scope.kind == dmar::SCOPE_PCI_ENDPOINT)
        .filter_map(|scope| scope.endpoint())
        .filter_map(move |(bus, device, function)| Address::new(segment, bus, device, function))
}

/// The domain `function`'s DMA goes through: one on the VT-d unit the DMAR puts
/// it behind, or on the `SMMUv3` an IORT root complex sends it to, when that
/// unit is translating, and an untranslated one otherwise.
pub(crate) fn domain_for(function: Address) -> Domain {
    let Some(programmed) = PROGRAMMED.get() else {
        return Domain::untranslated();
    };
    let translation = if let Some(unit) = vtd_unit_for(programmed, function) {
        unit.attach(function).map(|attached| Translation::VtD {
            unit,
            attached,
            changing: Gate::new(),
        })
    } else if let Some((unit, stream)) = smmu_stream_for(programmed, function) {
        unit.attach(stream).map(|attached| Translation::SmmuV3 {
            unit,
            attached,
            changing: Gate::new(),
        })
    } else {
        return Domain::untranslated();
    };
    match translation {
        Ok(translation) => Domain::with(translation),
        Err(why) => {
            println!("  iommu    pci {function} gets no translated domain: {why}");
            Domain::untranslated()
        }
    }
}

/// The VT-d unit the DMAR puts `function` behind, if it is translating.
///
/// A single-hop endpoint scope names it directly; any other scope is
/// followed through the bridges enumeration found, which a function below a
/// root port is examined after (`discovery::pci`).
fn vtd_unit_for(programmed: &'static Programmed, function: Address) -> Option<&'static vtd::Unit> {
    let named = programmed
        .behind
        .iter()
        .find(|(address, _)| *address == function)
        .map(|&(_, index)| index);
    let index = named.or_else(|| {
        let bridges = BRIDGES.lock();
        programmed
            .scopes
            .iter()
            .find(|(scope, _)| scope.places(function, &bridges))
            .map(|&(_, index)| index)
    })?;
    programmed.vtd.get(index)
}

/// The translating `SMMUv3` an IORT root complex sends `function` to, and the
/// stream it arrives as.
fn smmu_stream_for(
    programmed: &'static Programmed,
    function: Address,
) -> Option<(&'static smmuv3::Unit, u32)> {
    let requester = u32::from(function.requester_id());
    programmed.streams.iter().find_map(|map| {
        let offset = requester.checked_sub(map.first)?;
        if map.segment != function.segment() || u64::from(offset) >= map.count {
            return None;
        }
        Some((
            programmed.smmu.get(map.unit)?,
            map.stream.checked_add(offset)?,
        ))
    })
}

// ---------------------------------------------------------------------------
// Faults
// ---------------------------------------------------------------------------

/// The accesses a check made fault on purpose, by stream and page: the
/// out-of-domain probe's. A fault for one of these is the check working; any
/// other is DMA a device attempted outside its domain, which is a driver
/// handing its device an address it never pinned, or a device left running
/// across a reset, and [`audit_faults`] fails the boot on it.
static PROVOKED: SpinLock<Vec<(u32, u64)>> = SpinLock::new(Vec::new());

/// Faults taken from a unit that nothing provoked: cleared before a check
/// began, or read by one and not recognised. Counted here so that no fault is
/// taken and then dropped unseen.
static STRAY: AtomicU64 = AtomicU64::new(0);

/// Of [`STRAY`], the ones that were not a refused access: an `SMMUv3` event
/// of another type ([`Cause::Event`]), an overflow of its queue
/// ([`Cause::Lost`]), or a VT-d fault lost to a full record
/// ([`Cause::Overflow`]).
static STRAY_EVENTS: AtomicU64 = AtomicU64::new(0);

/// Record that `stream`'s device is about to be made to write `page`, which its
/// domain does not map, so the fault the unit records for it is not stray.
///
/// Said on the console too, before the write: `xtask` holds every fault the
/// emulated unit traces over a whole run, well past the boot, to these lines.
fn record_provoked(stream: u32, page: u64) {
    // FATAL-ALLOC: boot only: a boot check provokes the fault this records.
    PROVOKED.lock().push((stream, page));
    println!(
        "  iommu    stream {stream:#x} is made to write page {page:#x} outside its domain, on purpose"
    );
}

/// Whether a check provoked `fault`: a refused access, by the stream and to
/// the page it registered.
///
/// Or a VT-d overflow while the full record held such an access, because
/// then the fault lost may have been the same device's next: the probe's
/// device writes its 64 bytes four at a time, sixteen faults in an instant,
/// and a unit that does not collapse a device's repeated faults into its
/// pending record -- the VT-d specification recommends collapsing, it does
/// not require it -- overflows on the probe's second. Any other overflow is
/// stray. QEMU 9.2.4's unit collapses (`vtd_try_collapse_fault` in
/// `vtd_report_frcd_fault`), with one record, so there an overflow is only
/// ever a second source's fault: the probe's own sixteen never set it, and
/// the one overflow this passes over under QEMU is another device faulting
/// while the probe's record was pending. That is the price of never failing
/// a boot on the probe's own faults on a unit that records each; `xtask`
/// still fails such a run, since QEMU traces `vtd_dmar_fault` for every
/// fault before it decides whether to record it (`tools/common/xtask/src/dma_faults.rs`).
fn provoked(fault: Fault) -> bool {
    matches!(fault.cause, Cause::Access | Cause::Overflow)
        && PROVOKED
            .lock()
            .iter()
            .any(|&(stream, page)| fault.stream == stream && fault.page == page)
}

/// Count `fault` as stray unless a check provoked it, however late it
/// arrives, and say whether it was.
fn count_if_stray(fault: Fault) -> bool {
    let stray = !provoked(fault);
    if stray {
        let _ = STRAY.fetch_add(1, Ordering::Relaxed);
        if fault.cause != Cause::Access {
            let _ = STRAY_EVENTS.fetch_add(1, Ordering::Relaxed);
        }
    }
    stray
}

/// What [`audit_faults`] found.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FaultAudit {
    /// Units translating, whose records were read.
    pub(crate) units: usize,
    /// Faults a check provoked that were still recorded: the out-of-domain
    /// probe's, arriving after it had stopped looking.
    pub(crate) provoked: u64,
    /// Faults nothing provoked, since translation was turned on.
    pub(crate) stray: u64,
    /// Of those, `SMMUv3` events other than a refused access
    /// ([`Cause::Event`]).
    pub(crate) stray_events: u64,
    /// The first stray fault the audit itself read, if it read one.
    pub(crate) first: Option<Fault>,
}

/// Empty every translating unit's fault records, and count every fault since
/// translation went on that no check provoked.
///
/// A unit that faults a device's DMA stops it, and records why; if nothing
/// reads the record, the isolation holds but the bug that attempted the DMA
/// goes unseen. VT-d's fault event interrupt is left masked, so this is where
/// the record is read: last in boot, after every driver the boot starts has
/// run. Bounded per unit, as [`Domain::clear_faults`] is.
///
/// An `SMMUv3` event of any type counts, not only a translation fault: no
/// check provokes the others, and each one is a device's DMA stopped or a
/// stream table the unit refused, which the boot must not pass over.
pub(crate) fn audit_faults() -> FaultAudit {
    let mut audit = FaultAudit::default();
    if let Some(programmed) = PROGRAMMED.get() {
        for unit in &programmed.vtd {
            drain(|| recorded(unit.take_fault()), &mut audit);
        }
        for unit in &programmed.smmu {
            drain(|| recorded(unit.take_fault()), &mut audit);
        }
    }
    audit.stray = STRAY.load(Ordering::Relaxed);
    audit.stray_events = STRAY_EVENTS.load(Ordering::Relaxed);
    audit
}

/// A fault a unit reported, recorded in the audit record as it is read
/// (`audit::DMA_FAULT`): every read of a unit's faults passes here. The
/// kernel is the subject, never the task that happens to be running, which
/// is not the device's.
fn recorded(fault: Option<Fault>) -> Option<Fault> {
    if let Some(fault) = fault {
        crate::audit::record(
            crate::audit::DMA_FAULT,
            crate::audit::Outcome::Refused,
            0,
            crate::audit::Subject::KERNEL,
            crate::audit::Target {
                kind: crate::audit::target::DEVICE,
                id: u64::from(fault.stream),
            },
            [
                (fault.page >> 12) as u32,
                (fault.page >> 44) as u32,
                u32::from(fault.write),
            ],
        );
    }
    fault
}

/// Read one unit's faults into `audit`, counting it.
fn drain(mut take: impl FnMut() -> Option<Fault>, audit: &mut FaultAudit) {
    audit.units += 1;
    for _ in 0..256 {
        let Some(fault) = take() else {
            return;
        };
        if count_if_stray(fault) {
            audit.first = audit.first.or(Some(fault));
        } else {
            audit.provoked += 1;
        }
    }
}
