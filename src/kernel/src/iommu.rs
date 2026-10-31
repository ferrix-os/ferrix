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
use ferrix_paging::vtd::rmrr::{self, Identity};
use ferrix_paging::{MapError, MapFlags};
use ferrix_pci::Address;
use ferrix_pci::topology::{self, Bridge};
use ferrix_sync::Once;

use crate::device::{DeviceNode, Location};
use crate::discovery::description::{self, Description};
use crate::sync::SpinLock;
use crate::{arch, mm, println};

// CHECKS-ONLY-METHODS: home: self

pub(crate) mod amdvi;
pub mod amdvi_check;
pub mod check;
pub mod gate;
mod ivrs;
pub mod remapping;
mod smmuv3;
pub mod vtd;

pub use remapping::{CHECK_VECTOR, console_vectors, route, stray_deliveries};
pub(crate) use remapping::{
    LiveLine, close_check_window, function_isolated, nmis, open_check_window,
};

pub use check::check_dma_faults;
pub use check::run as check_gate;
pub use vtd::{Invalidations, invalidations};

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
    /// An AMD-Vi unit.
    AmdVi,
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
        // Every IVHD's unit, of every type, kept from drivers: a type not
        // read still names a unit's registers.
        if let Ok(table) = tables.ivrs() {
            for block in table.blocks() {
                if let ferrix_acpi::ivrs::Block::Ivhd(unit) = block {
                    add(
                        &mut units,
                        Kind::AmdVi,
                        unit.base,
                        ferrix_paging::amdvi::unit::KEPT,
                    );
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
///
/// A function no explicit scope reaches is the segment's catch-all unit's
/// (`INCLUDE_PCI_ALL`), as its own requester ID: [`topology::place`]
/// decides, as it does for [`vtd_unit_for`], so the report and the domains
/// cannot disagree.
fn place_dmar(
    table: &dmar::Dmar<'_>,
    units: &[Unit],
    bridges: &[Bridge],
    function: Address,
) -> Behind {
    let explicit = explicit_dmar(table, bridges, function);
    let catch_all = match table.catch_all(function.segment()) {
        dmar::CatchAll::None => topology::CatchAll::None,
        dmar::CatchAll::One(unit) => topology::CatchAll::One(unit.register_base),
        dmar::CatchAll::Several => topology::CatchAll::Several,
    };
    match topology::place(explicit, catch_all, function, bridges) {
        topology::Placed::Unit(base) => behind(units, base, u32::from(function.requester_id())),
        topology::Placed::Nothing => Behind::Nothing,
        topology::Placed::Unresolved => Behind::Unresolved,
    }
}

/// What the DMAR's explicit scopes say of `function` (`place_dmar`): the
/// first unit whose scope reaches it as its own requester, else whether one
/// reaches it under an alias or a scope's path could not be followed.
fn explicit_dmar(
    table: &dmar::Dmar<'_>,
    bridges: &[Bridge],
    function: Address,
) -> topology::Explicit<u64> {
    let mut explicit = topology::Explicit::None;
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
            let named = scope
                .path_is_whole()
                .then(|| {
                    topology::follow_path(
                        function.segment(),
                        scope.start_bus,
                        scope.path(),
                        bridges,
                    )
                })
                .flatten();
            let Some(named) = named else {
                explicit = topology::Explicit::Unfollowed;
                continue;
            };
            let hierarchy = scope.kind == SCOPE_PCI_SUB_HIERARCHY;
            match claims(hierarchy, named, function, bridges) {
                topology::Behind::Own => {
                    explicit = topology::Explicit::Own(unit.register_base);
                    break;
                }
                topology::Behind::Aliased => explicit = topology::Explicit::Unfollowed,
                topology::Behind::No => {}
            }
        }
        if matches!(explicit, topology::Explicit::Own(_)) {
            break;
        }
    }
    explicit
}

/// Whether a scope naming `named` -- a sub-hierarchy if `hierarchy`, an
/// endpoint otherwise -- puts `function` behind its unit as `function`'s own
/// requester ID. [`topology::Behind::Aliased`] for a function the scope
/// reaches whose DMA arrives under a bridge's alias, or on a bus no bridge
/// explains: never given a domain, since a second function behind the same
/// bridge would share it.
///
/// The decision is [`topology::claims`], kept in `ferrix_pci` beside the
/// walks it composes so its host tests (L.iommu.45) run it as this does;
/// every placement asks it here, so none can answer `Own` where
/// [`topology::behind`] says aliased.
fn claims(
    hierarchy: bool,
    named: Address,
    function: Address,
    bridges: &[Bridge],
) -> topology::Behind {
    topology::claims(hierarchy, named, function, bridges)
}

/// The bridges PCI enumeration found, every host's, for following DMAR
/// scopes through them: filled once at stage 10, before any function on a
/// bus behind one is examined, and only read from then on.
static BRIDGES: SpinLock<Vec<Bridge>> = SpinLock::new(Vec::new());

/// Record a bridge PCI enumeration found (`discovery::pci`).
pub fn learn_bridge(bridge: Bridge) {
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

/// D10's answer for `function`, from its node's class and bus mastering as
/// enumeration found them (`iommu/ivrs.rs`).
fn place_ivrs(
    places: &[ivrs::Placing],
    _units: &[Unit],
    _bridges: &[Bridge],
    nodes: &[Arc<DeviceNode>],
    function: Address,
) -> Option<ivrs::Exempt> {
    let node = nodes
        .iter()
        .find(|node| node.location() == Location::Pci(function))?;
    let class = node.pci_function()?.class;
    ivrs::exempt(places, function, class, node.bus_master_on())
}

/// Where the AMD-Vi unit whose own function `function` is, at index `at`
/// of `places`, puts it (D10, `docs/AMD-VI.md` §9.1).
fn exempt_unit(places: &[ivrs::Placing], units: &[Unit], function: Address, at: usize) -> Behind {
    places.get(at).map_or(Behind::Unresolved, |unit| {
        behind(units, unit.base, u32::from(function.requester_id()))
    })
}

/// Where the IVRS's units put `function` (`iommu/ivrs.rs`).
fn ivrs_behind(
    places: &[ivrs::Placing],
    units: &[Unit],
    bridges: &[Bridge],
    function: Address,
) -> Behind {
    match ivrs::placed(places, function, bridges) {
        topology::Placed::Unit(at) => places.get(at).map_or(Behind::Unresolved, |unit| {
            behind(units, unit.base, u32::from(function.requester_id()))
        }),
        topology::Placed::Nothing => Behind::Nothing,
        topology::Placed::Unresolved => Behind::Unresolved,
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
    /// PCI functions firmware places behind a unit.
    pub(crate) behind: usize,
    /// Of them, those whose unit does not translate: left alone, turned
    /// off by `ferrix.vtd=off`, or held to remapping only by an RMRR.
    pub(crate) untranslated: usize,
    /// PCI functions firmware puts behind no unit.
    pub(crate) bypassing: usize,
    /// PCI functions firmware describes in a way this cannot follow.
    pub(crate) unresolved: usize,
    /// AMD-Vi units.
    pub(crate) amdvi: usize,
    /// Root complexes no IVHD entry names, which issue no DMA (D10).
    pub(crate) root_complex: usize,
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
    // Counted from the units read, not every IVHD kept from drivers.
    report.amdvi = units.iter().filter(|unit| unit.kind == Kind::AmdVi).count();
    let functions: Vec<Address> = nodes
        .iter()
        .filter_map(|node| match node.location() {
            Location::Pci(address) => Some(address),
            Location::VirtioMmio(_) | Location::Tree(_) => None,
        })
        // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
        .collect();

    let mut placements = Vec::new();
    let mut tally = ferrix_paging::vtd::unit::Tally::default();
    let mut record = |function: Address, found: Behind| match found {
        Behind::Unit { unit, stream } => {
            tally.add(units.get(unit).is_some_and(translates));
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

    let mut roots = 0;
    let machine = description::of(view);
    if let Description::Acpi(firmware) = machine {
        let tables = firmware.acpi();
        let dmar = tables.dmar().ok();
        let iort = tables.iort().ok();
        let ivrs = tables.ivrs().ok();
        let bridges = BRIDGES.lock();
        let placing = ivrs.as_ref().map(ivrs::placing);
        let mut exempt = Vec::new();
        let mut root_complex = 0;
        for &function in &functions {
            let found = match (&dmar, &iort, &placing) {
                (Some(table), _, _) => place_dmar(table, &units, &bridges, function),
                (None, Some(table), _) => place_iort(table, &units, function),
                (None, None, Some(Some(places))) => {
                    let exemption = place_ivrs(places, &units, &bridges, nodes, function);
                    if exemption.is_some() {
                        // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
                        exempt.push(function);
                    }
                    match exemption {
                        Some(ivrs::Exempt::RootComplex) => {
                            root_complex += 1;
                            continue;
                        }
                        Some(ivrs::Exempt::Unit(at)) => exempt_unit(places, &units, function, at),
                        None => ivrs_behind(places, &units, &bridges, function),
                    }
                }
                (None, None, Some(None)) => Behind::Unresolved,
                (None, None, None) => Behind::Nothing,
            };
            record(function, found);
        }
        ivrs::note_exempt(exempt);
        roots = root_complex;
    } else if let Description::Tree(tree) = machine {
        for &function in &functions {
            record(function, place_tree(&tree, &units, function));
        }
    } else {
        for &function in &functions {
            record(function, Behind::Nothing);
        }
    }
    report.behind = tally.behind;
    report.untranslated = tally.untranslated;
    report.root_complex = roots;
    (report, units, placements)
}

/// Whether the kernel turned translation on for `unit`: an `SMMUv3` it
/// programmed, or a VT-d unit it programmed that translates.
fn translates(unit: &Unit) -> bool {
    PROGRAMMED.get().is_some_and(|programmed| match unit.kind {
        Kind::VtD => programmed
            .vtd
            .iter()
            .any(|programmed| programmed.phys() == unit.phys && programmed.translating()),
        Kind::SmmuV3 => programmed.smmu_phys.contains(&unit.phys),
        Kind::AmdVi => programmed
            .amdvi
            .iter()
            .any(|programmed| programmed.phys() == unit.phys && programmed.translating()),
    })
}

/// Stage 10: find every IOMMU, and which one each PCI function's DMA arrives
/// at, and say so on the console.
///
/// Nothing here can fail the boot: firmware that describes no IOMMU, or one
/// this cannot follow, is reported and the boot goes on. `xtask test-boot`
/// requires the placements on the machines it configures. Last, the pin
/// budgets' ceiling is counted.
pub fn report(view: &BootView<'_>, nodes: &[Arc<DeviceNode>]) {
    let (report, units, placements) = discover(view, nodes);
    remapping::note_bypassing(report.bypassing);
    // The AMD-Vi count and the root complexes only where there are any, so
    // every other machine's line is as it was (ledger 3867, 3923).
    let amdvi = Counted(", {} AMD-Vi units", report.amdvi);
    let roots = Counted(", {} root complex", report.root_complex);
    println!(
        "  iommu    {} VT-d units, {} SMMUv3s{amdvi}; {} PCI functions behind one, {} of them \
         untranslated, {} bypassing{roots}, {} unresolved",
        report.vtd,
        report.smmu_v3,
        report.behind,
        report.untranslated,
        report.bypassing,
        report.unresolved,
    );
    for placement in placements {
        if let Some(unit) = units.get(placement.unit) {
            println!(
                "  iommu    pci {} behind the {:?} unit at {:#x} as stream {:#x}",
                placement.function, unit.kind, unit.phys, placement.stream,
            );
        }
    }
    name_functions_without_vectors(nodes);
    // The ceiling on raised pin budgets, a quarter of RAM, counted here at
    // stage 10, checks or not: before devmgr can set a budget, and before
    // the budget's checks in `domain_check::check_iommu` set one
    // (`object::pin`). Counted in `check_iommu` until 2026-10-15, which ran
    // straight after this (`docs/KERNEL-CRATES.md` §3 (3)).
    crate::object::pin::count_ceiling();
    announce_intel_gpu(nodes);
}

/// A count said only when it is not 0: `", N label"`.
struct Counted(&'static str, usize);

impl core::fmt::Display for Counted {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Counted(said, count) = *self;
        if count == 0 {
            return Ok(());
        }
        let (before, after) = said.split_once("{}").unwrap_or((said, ""));
        write!(f, "{before}{count}{after}")
    }
}

/// Say how the Intel integrated GPU stands (`docs/INTEL-GPU.md` §4.4 (b)):
/// not driven without `ferrix.intelgpu`; driven under it, its domain made
/// now, with the kernel's pin budget, and named as DMA nobody translates
/// while its unit does not: degraded trusted mode, its driver trusted with
/// all of memory (VULNERABILITY-ANALYSIS V-03), and no client may reach it
/// (`L.object.192`).
fn announce_intel_gpu(nodes: &[Arc<DeviceNode>]) {
    for node in nodes.iter().filter(|node| node.intel_gpu()) {
        let location = node.location();
        if !crate::device::intel_gpu_allowed() {
            println!(
                "  iommu    {location}, the Intel integrated GPU, is not driven: no {}",
                crate::device::INTEL_GPU_OPTION
            );
            continue;
        }
        let Ok(domain) = node.domain() else {
            println!("  iommu    {location}, the Intel integrated GPU: no memory for its domain");
            continue;
        };
        let budget = crate::object::pin::counts(&domain).budget;
        if domain.translated() {
            println!(
                "  iommu    {location}, the Intel integrated GPU, driven under {} with its DMA \
                 translated; pin budget {budget} pages",
                crate::device::INTEL_GPU_OPTION
            );
        } else {
            println!(
                "  iommu    DEGRADED: {location}, the Intel integrated GPU, driven under {} with \
                 its DMA untranslated: its driver is trusted with all of memory, and no client \
                 node is published for it; pin budget {budget} pages",
                crate::device::INTEL_GPU_OPTION
            );
        }
    }
}

/// While a unit remaps interrupts, name every PCI function that gets no
/// message vector: one no unit that remaps places as its own requester --
/// behind no unit, behind one that does not remap, or under a bridge's
/// alias. Its messages could not be told from another's, so it gets no
/// MSI at all (`L.device.28`): fail-closed for isolation, an availability
/// loss on a machine whose units do not cover every function
/// (SAFETY-MANUAL AoU-23).
fn name_functions_without_vectors(nodes: &[Arc<DeviceNode>]) {
    if !remapping::any_remapping() {
        return;
    }
    for node in nodes {
        if let Location::Pci(function) = node.location()
            && route(function.requester_id()).is_err()
        {
            println!(
                "  iommu    pci {function} gets no message vector: no unit that remaps \
                 interrupts places it as its own requester"
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
pub struct Fault {
    /// The device's stream: its source ID on VT-d, its stream ID on an
    /// `SMMUv3`.
    pub stream: u32,
    /// The page it addressed.
    pub page: u64,
    /// Whether the access was a write.
    pub write: bool,
    /// What the unit recorded it as.
    pub cause: Cause,
}

/// What a unit recorded a fault as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cause {
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
    /// A VT-d unit whose invalidation queue stopped (`IQE` or `ITE`), and
    /// which was marked failed: it names no stream, nothing provokes it, and
    /// it counts as stray, so the boot fails on it (FX-1007).
    Queue,
    /// An interrupt request a VT-d unit refused to remap, by its fault
    /// reason, 0x20 to 0x26, and the entry index it named: a request in
    /// compatibility format while it is blocked (0x25), one whose source ID
    /// failed its entry's check (0x26), or one naming no present entry
    /// (0x21, 0x22). Its page is not used. Provoked only by checks R1 to R3,
    /// which register the stream and reason ([`record_provoked_interrupt`]).
    Interrupt {
        /// The fault reason.
        reason: u8,
        /// The interrupt entry index the request named.
        index: u16,
    },
    /// An AMD-Vi event other than a refused access or interrupt request,
    /// by its code: a device table entry the unit would not use, a hardware
    /// error, a command it refused, an IOTLB timeout. No check provokes
    /// one, so it counts as stray.
    AmdVi(u8),
    /// An AMD-Vi event log that overflowed: events were lost. Stray, as
    /// [`Cause::Lost`] is.
    AmdViLost,
    /// An AMD-Vi unit marked failed: a completion wait never stored, or its
    /// command buffer stopped. Stray, as [`Cause::Queue`] is.
    AmdViFailed,
}

/// What an interrupt fault reason means, for the log.
fn interrupt_reason_name(reason: u8) -> &'static str {
    match reason {
        0x20 => "reserved field in the request",
        0x21 => "index past the table",
        0x22 => "entry not present",
        0x23 => "table address invalid",
        0x24 => "reserved field in the entry",
        0x25 => "compatibility format blocked",
        0x26 => "source ID check",
        amdvi::INTERRUPT_REFUSED => "an AMD-Vi unit refused the interrupt request",
        _ => "reserved",
    }
}

/// How a VT-d invalidation's wait descriptor completes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wait {
    /// It writes its status word, which the kernel waits for: every
    /// invalidation but check R7's.
    Status,
    /// It writes nothing, so the invalidation fails after a short patience
    /// of its own: only for check R7 (`docs/NVIDIA.md` §12.3), which passes it
    /// through [`Domain::unpin_waiting`] to show that a failed invalidation
    /// releases nothing. Never chosen at run time.
    Unwritten,
}

impl Fault {
    /// A refused write by `stream` to `page`: what a check that provokes one
    /// requires its unit to record.
    pub fn write_to(stream: u32, page: u64) -> Self {
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
            Cause::Queue => write!(
                f,
                "a VT-d unit's invalidation queue stopped, and the unit was marked failed"
            ),
            Cause::Interrupt { reason, index } => write!(
                f,
                "stream {:#x}, interrupt index {index}, reason {reason:#x} ({})",
                self.stream,
                interrupt_reason_name(reason)
            ),
            Cause::AmdVi(code) => write!(f, "stream {:#x}, AMD-Vi event {code:#x}", self.stream),
            Cause::AmdViLost => write!(f, "an AMD-Vi event log overflowed: events were lost"),
            Cause::AmdViFailed => write!(
                f,
                "an AMD-Vi unit's completion wait was never stored, and the unit was marked failed"
            ),
        }
    }
}

/// Why a domain refused to pin or unpin.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DomainError {
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
    /// A page is an RMRR's, which the domain holds for good: never pinned,
    /// and never unpinned (`L.iommu.76`, `L.iommu.77`).
    Reserved,
}

/// The memory one device's DMA may reach, and the addresses it reaches it by.
///
/// A translated domain maps each pinned page at its own physical address.
/// What makes it a domain is what it leaves out: nothing is mapped but what was
/// pinned. That saves an I/O address allocator, at the cost of refusing frames
/// above [`TRANSLATED_BITS`].
#[derive(Debug)]
pub struct Domain {
    /// This domain's number.
    id: u64,
    /// Pages pinned and not yet unpinned.
    pinned: AtomicU64,
    /// Who translates it.
    translation: Translation,
    /// Its device's pin budget and the pages counted against it
    /// (`object::pin`), which belong to the device for as long as its node,
    /// as the domain does.
    budget: crate::object::pin::PinBudget,
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
        /// The RMRR identity domain it is, whose pages it maps for good
        /// ([`IdentityDomain`]); `None` for any other domain.
        identity: Option<&'static IdentityDomain>,
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
    /// An AMD-Vi unit.
    AmdVi {
        /// The unit.
        unit: &'static amdvi::Unit,
        /// The domain on it.
        attached: amdvi::Attached,
        /// Held across a pin's or an unpin's changes to the tables and the
        /// unit's wait for them.
        changing: Gate,
        /// The IVMD identity domain it is; `None` for any other domain.
        identity: Option<&'static IdentityDomain>,
    },
}

impl Translation {
    /// The lock a translated domain holds across a pin's or an unpin's
    /// changes, or `None` for an untranslated one.
    fn changing(&self) -> Option<&Gate> {
        match self {
            Translation::None => None,
            Translation::VtD { changing, .. }
            | Translation::SmmuV3 { changing, .. }
            | Translation::AmdVi { changing, .. } => Some(changing),
        }
    }

    /// Map the page at `phys` at `iova`.
    fn map(&self, iova: u64, phys: u64, flags: MapFlags) -> Result<(), MapError> {
        match self {
            Translation::None => Ok(()),
            Translation::VtD { unit, attached, .. } => unit.map(attached, iova, phys, flags),
            Translation::SmmuV3 { unit, attached, .. } => unit.map(attached, iova, phys, flags),
            Translation::AmdVi { unit, attached, .. } => unit.map(attached, iova, phys, flags),
        }
    }

    /// The VT-d unit and domain behind a translated domain whose pins are
    /// taken down a run at a time, or `None` for every other domain.
    fn vtd(&self) -> Option<(&'static vtd::Unit, &vtd::Attached)> {
        match self {
            Translation::VtD { unit, attached, .. } => Some((unit, attached)),
            _ => None,
        }
    }

    /// Whether a pin's aligned runs are mapped as 2 MiB entries: on a VT-d
    /// unit whose `CAP.SLLPS` offers them (`L.iommu.105`, `L.iommu.106`).
    fn blocks(&self) -> bool {
        self.vtd().is_some_and(|(unit, _)| unit.blocks())
    }

    /// Take the page at `iova` out of the tables, holding every table that
    /// empties in `tables` until the unit's flush has completed.
    fn unmap(&self, iova: u64, tables: &mut mm::UnlinkedTables) -> Result<(), MapError> {
        match self {
            Translation::None => Ok(()),
            Translation::VtD { unit, attached, .. } => unit.unmap(attached, iova, tables),
            Translation::SmmuV3 { unit, attached, .. } => unit.unmap(attached, iova, tables),
            Translation::AmdVi { unit, attached, .. } => unit.unmap(attached, iova, tables),
        }
    }

    /// Make the unit forget what it cached, after an unmap or a map. `wait`
    /// reaches a VT-d unit's queue; an `SMMUv3`'s commands have no such
    /// choice, and check R7 runs only on a VT-d domain.
    fn flush(&self, after_map: bool, wait: Wait) -> Result<(), &'static str> {
        match self {
            Translation::None => Ok(()),
            Translation::VtD { unit, attached, .. } => unit.flush(attached, after_map, wait),
            Translation::SmmuV3 { unit, attached, .. } => unit.flush(attached, after_map),
            // After a map too: a unit may cache an entry not present.
            Translation::AmdVi { unit, attached, .. } => unit.flush(attached, wait),
        }
    }

    /// Where an access to `iova` lands.
    fn resolve(&self, iova: u64) -> Option<u64> {
        match self {
            Translation::None => Some(iova),
            Translation::VtD { unit, attached, .. } => unit.resolve(attached, iova),
            Translation::SmmuV3 { unit, attached, .. } => unit.resolve(attached, iova),
            Translation::AmdVi { unit, attached, .. } => unit.resolve(attached, iova),
        }
    }

    /// The stream the unit sees the domain's device as.
    fn stream(&self) -> Option<u32> {
        match self {
            Translation::None => None,
            Translation::VtD { attached, .. } => Some(attached.stream()),
            Translation::SmmuV3 { attached, .. } => Some(attached.stream()),
            Translation::AmdVi { attached, .. } => Some(attached.stream()),
        }
    }

    /// The next fault the domain's unit recorded, for any device.
    fn take_fault(&self) -> Option<Fault> {
        match self {
            Translation::None => None,
            Translation::VtD { unit, .. } => recorded(counted(unit.take_fault())),
            Translation::SmmuV3 { unit, .. } => recorded(counted(unit.take_fault())),
            Translation::AmdVi { unit, .. } => recorded(counted(unit.take_fault())),
        }
    }

    /// The RMRR pages the domain holds for good: empty but for an RMRR's
    /// identity domain.
    fn held(&self) -> &'static [Identity] {
        self.identity().map_or(&[], IdentityDomain::regions)
    }

    /// The identity domain it is, an RMRR's or an IVMD's, if it is one.
    fn identity(&self) -> Option<&'static IdentityDomain> {
        match self {
            Translation::VtD { identity, .. } | Translation::AmdVi { identity, .. } => *identity,
            Translation::None | Translation::SmmuV3 { .. } => None,
        }
    }

    /// Detach the domain from its unit and give back its tables. An RMRR's
    /// identity domain is never detached (`L.iommu.78`).
    fn detach(&self) -> Result<(), &'static str> {
        if !self.held().is_empty() {
            return Err("an RMRR's identity domain is never detached");
        }
        match self {
            Translation::None => Ok(()),
            Translation::VtD { unit, attached, .. } => unit.detach(*attached),
            Translation::SmmuV3 { unit, attached, .. } => unit.detach(*attached),
            Translation::AmdVi { unit, attached, .. } => unit.detach(*attached),
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
pub struct Pinned {
    /// The domain that took it.
    pub domain: u64,
    /// Each page's device address, in the order its frame was given.
    pub addresses: Vec<u64>,
}

impl Pinned {
    /// Each page's device address, in the order its frame was given. Not
    /// necessarily contiguous.
    pub fn addresses(&self) -> &[u64] {
        &self.addresses
    }

    /// Never give the pin back: its pages stay reachable by the device for
    /// good, and whoever holds their frames must keep them too.
    pub fn leak(self) {
        let _ = core::mem::ManuallyDrop::new(self);
    }
}

impl Domain {
    /// Detach the domain's translation, as the check of an RMRR identity
    /// domain asks it to (`L.iommu.78`): `translation` is private to this
    /// module, and the check is raised above the core (ledger 1795 S3.2a,
    /// 1630 F1).
    ///
    /// # Errors
    ///
    /// Whatever the unit's detach refuses.
    #[doc(hidden)]
    pub fn detach_translation(&self) -> Result<(), &'static str> {
        self.translation.detach()
    }

    /// This domain's number, as a check of its device-table entry reads it
    /// (`L.iommu.77`): `id` is private to this module, and the check is raised
    /// above the core (ledger 1630 F1).
    #[doc(hidden)]
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// A domain no unit translates.
    pub fn untranslated() -> Self {
        Domain::with(Translation::None)
    }

    /// A domain translated by `translation`.
    fn with(translation: Translation) -> Self {
        Domain {
            id: NEXT_DOMAIN.fetch_add(1, Ordering::Relaxed),
            pinned: AtomicU64::new(0),
            translation,
            budget: crate::object::pin::PinBudget::new(),
        }
    }

    /// Its device's pin budget and counts, which only `object::pin` reads
    /// or changes, under its quarantine's lock.
    pub fn pin_budget(&self) -> &crate::object::pin::PinBudget {
        &self.budget
    }

    /// The span of the entry that maps `address` in a VT-d domain's tables,
    /// or `None` where nothing does or the domain is not VT-d's.
    #[doc(hidden)]
    pub fn entry_span(&self, address: u64) -> Option<u64> {
        let (unit, attached) = self.translation.vtd()?;
        unit.entry_span(attached, address)
    }

    /// Leave an empty leaf table in a VT-d domain's 2 MiB slot over
    /// `address`, as no product path does, for stage 10's fallback case.
    /// The unit may hold the page `address` was mapped as for a moment, as
    /// after any unmap before its flush; nothing is pinned there.
    ///
    /// # Errors
    ///
    /// The domain is not VT-d's, or the tables refused.
    #[doc(hidden)]
    pub fn link_empty_table(&self, address: u64) -> Result<(), DomainError> {
        let (unit, attached) = self.translation.vtd().ok_or(DomainError::Tables)?;
        unit.link_empty_table(attached, address)
            .map_err(|_| DomainError::Tables)?;
        self.translation
            .flush(false, Wait::Status)
            .map_err(DomainError::Unit)
    }

    /// For a VT-d domain, whether its pins' runs are mapped as blocks and
    /// whether the unit's `CAP.SLLPS` bit 34, read now, offers them; `None`
    /// for any other domain.
    #[doc(hidden)]
    pub fn blocks_wiring(&self) -> Option<(bool, bool)> {
        let (unit, _) = self.translation.vtd()?;
        Some((self.translation.blocks(), unit.sllps_2m()))
    }

    /// Whether a unit translates this domain's DMA, so a device can reach only
    /// what is pinned into it.
    pub fn translated(&self) -> bool {
        !matches!(self.translation, Translation::None)
    }

    /// Where an access by the device to `address` lands, or `None` when the
    /// unit would fault it. An untranslated domain lands every access where it
    /// points.
    pub fn resolve(&self, address: u64) -> Option<u64> {
        self.translation.resolve(address)
    }

    /// The stream the unit sees this domain's device as, if a unit translates
    /// the domain.
    pub fn stream(&self) -> Option<u32> {
        self.translation.stream()
    }

    /// The next DMA fault the domain's unit recorded, for any device on it, or
    /// `None`. Taking it clears it, so the unit can record the next.
    ///
    /// A fault no check provoked is counted on the way, so that one taken and
    /// then passed over still fails the boot in [`audit_faults`].
    pub fn take_fault(&self) -> Option<Fault> {
        let fault = self.translation.take_fault()?;
        let _ = count_if_stray(fault);
        Some(fault)
    }

    /// Clear the unit's faults, and say that this domain's device is about to
    /// be made to write `page`, which the domain does not map, so the fault the
    /// unit records for it is known for the check's own and not counted stray.
    pub fn provoke(&self, page: u64) {
        self.clear_faults();
        if let Some(stream) = self.stream() {
            record_provoked(stream, page);
        }
    }

    /// Clear the unit's faults, and say that this domain's device is about to
    /// send an interrupt request its unit refuses with `reason`, so the fault
    /// is known for the check's own (checks R1 to R3).
    pub(crate) fn provoke_interrupt(&self, reason: u8) {
        self.clear_faults();
        if let Some(stream) = self.stream() {
            record_provoked_interrupt(stream, reason);
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
    pub fn pinned_pages(&self) -> u64 {
        self.pinned.load(Ordering::Relaxed)
    }

    /// Make `frames` reachable by the device, writable when `flags` says so,
    /// and say at which device addresses.
    ///
    /// # Errors
    ///
    /// [`DomainError::Empty`] for no frames, [`DomainError::OutOfRange`] for a
    /// frame the domain cannot address, [`DomainError::Reserved`] for a page
    /// an RMRR holds in the domain, with nothing mapped (`L.iommu.76`),
    /// [`DomainError::AlreadyPinned`],
    /// [`DomainError::Tables`] and [`DomainError::Unit`]. Whatever was mapped
    /// before a failure is unmapped again, except after a unit that never
    /// finished, whose pages stay mapped for good.
    pub fn pin(&self, frames: &[u64], flags: MapFlags) -> Result<Pinned, DomainError> {
        self.pin_with(frames, flags, self.translation.blocks())
    }

    /// [`Domain::pin`], its aligned runs mapped as 2 MiB entries only where
    /// `blocks` says: what [`Domain::pin`] passes is the unit's `CAP.SLLPS`;
    /// stage 10 passes `false` to see the arm a unit without it takes.
    ///
    /// # Errors
    ///
    /// As [`Domain::pin`].
    #[doc(hidden)]
    pub fn pin_runs(
        &self,
        frames: &[u64],
        flags: MapFlags,
        blocks: bool,
    ) -> Result<Pinned, DomainError> {
        self.pin_with(frames, flags, blocks)
    }

    /// [`Domain::pin`]'s work, its runs mapped as blocks where `blocks` says.
    fn pin_with(
        &self,
        frames: &[u64],
        flags: MapFlags,
        blocks: bool,
    ) -> Result<Pinned, DomainError> {
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
        let held = self.translation.held();
        if addresses.iter().any(|&phys| rmrr::held(held, phys)) {
            return Err(DomainError::Reserved);
        }
        if let Some(changing) = self.translation.changing() {
            if addresses.iter().any(|&phys| phys >> TRANSLATED_BITS != 0) {
                return Err(DomainError::OutOfRange);
            }
            let _held = changing.enter().map_err(DomainError::Unit)?;
            map_all(&self.translation, &addresses, flags, blocks)?;
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
    /// can, as it can every frame: nothing stands between the device and
    /// physical memory, and its driver is trusted with all of it (degraded
    /// trusted mode, VULNERABILITY-ANALYSIS V-03), so `object::pin` gives a
    /// live driver's frames back here too (F-59) and quarantines a dead one's
    /// until its device is reset.
    ///
    /// # Errors
    ///
    /// [`DomainError::Foreign`], handing the pin back, when another domain took
    /// it; [`DomainError::Reserved`], handing it back with nothing unmapped,
    /// when it names a page an RMRR holds in the domain (`L.iommu.77`).
    pub fn unpin(&self, pinned: Pinned) -> Result<(), (DomainError, Pinned)> {
        self.unpin_waiting(pinned, Wait::Status)
    }

    /// Whether a VT-d unit translates this domain: where its invalidations
    /// go through a queue, and [`Wait::Unwritten`] means something.
    pub fn queued(&self) -> bool {
        matches!(
            self.translation,
            Translation::VtD { .. } | Translation::AmdVi { .. }
        )
    }

    /// [`Domain::unpin`], its invalidation waited for as `wait` says:
    /// [`Wait::Status`] but in check R7, whose [`Wait::Unwritten`] makes the
    /// invalidation fail so that the pin comes back (`object::pin`).
    ///
    /// # Errors
    ///
    /// As [`Domain::unpin`].
    pub(crate) fn unpin_waiting(
        &self,
        pinned: Pinned,
        wait: Wait,
    ) -> Result<(), (DomainError, Pinned)> {
        if pinned.domain != self.id {
            return Err((DomainError::Foreign, pinned));
        }
        let held = self.translation.held();
        if pinned.addresses.iter().any(|&iova| rmrr::held(held, iova)) {
            return Err((DomainError::Reserved, pinned));
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
            if take_down(&self.translation, &pinned.addresses, &mut tables).is_err() {
                return Err((DomainError::Tables, pinned));
            }
            // Only once the unit has forgotten the pages may their frames go,
            // or the tables that led to them (finding F-36).
            if let Err(why) = self.translation.flush(false, wait) {
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

/// Pages in a run: the 4 KiB pages one 2 MiB entry maps.
const RUN_PAGES: usize = (mm::IO_RUN / PAGE_SIZE) as usize;

/// Whether `addresses` starts with a run: 512 consecutive pages from a
/// 2 MiB-aligned address, all of them the pin's own (`L.iommu.106`).
fn starts_run(addresses: &[u64]) -> bool {
    let Some((&first, run)) = addresses.get(..RUN_PAGES).and_then(<[u64]>::split_first) else {
        return false;
    };
    first.is_multiple_of(mm::IO_RUN)
        && run
            .iter()
            .zip(1..)
            .all(|(&address, page)| address == first + page * PAGE_SIZE)
}

/// Map every page in `addresses` at its own address in `translation`'s
/// tables, or none of them: where `blocks` says, each run as one 2 MiB entry,
/// and a run whose slot holds a table as its 512 pages instead, so that a pin
/// is refused only where its pages mapped one by one would be (`L.iommu.106`,
/// `L.iommu.107`).
fn map_all(
    translation: &Translation,
    addresses: &[u64],
    flags: MapFlags,
    blocks: bool,
) -> Result<(), DomainError> {
    let mut done = 0;
    while let Some(rest) = addresses.get(done..)
        && let Some(&phys) = rest.first()
    {
        let mut pages = 1;
        if let Some((unit, attached)) = translation.vtd().filter(|_| blocks)
            && starts_run(rest)
        {
            match unit.map_run(attached, phys, phys, flags) {
                Ok(()) => {
                    done += RUN_PAGES;
                    continue;
                }
                // Another pin's block: under identity, the same frames.
                Err(error @ MapError::AlreadyMapped(_))
                    if unit.entry_span(attached, phys) == Some(mm::IO_RUN) =>
                {
                    return Err(unwind(translation, addresses, done, RUN_PAGES, error));
                }
                // A table in the run's slot, empty or not: the run's pages,
                // one by one, and only a page refused there refuses the pin.
                Err(MapError::AlreadyMapped(_)) => pages = RUN_PAGES,
                Err(error) => return Err(unwind(translation, addresses, done, RUN_PAGES, error)),
            }
        }
        for (at, &phys) in addresses.iter().enumerate().skip(done).take(pages) {
            if let Err(error) = translation.map(phys, phys, flags) {
                return Err(unwind(translation, addresses, at, 1, error));
            }
        }
        done += pages;
    }
    translation
        .flush(true, Wait::Status)
        .map_err(DomainError::Unit)
}

/// Take out again what [`map_all`] mapped of `addresses` before the map of
/// `width` pages at `failed` was refused with `error`, and say why the pin is
/// refused.
fn unwind(
    translation: &Translation,
    addresses: &[u64],
    failed: usize,
    width: usize,
    error: MapError,
) -> DomainError {
    let mut tables = mm::UnlinkedTables::of(mm::TableOwner::Device);
    // A map refused for a write not cleaned (finding F-58) has written its
    // page or run into the tables, so the unwind takes that out too.
    let written = failed
        + if error == MapError::NotCleaned {
            width
        } else {
            0
        };
    let mapped = addresses.get(..written).unwrap_or(addresses);
    let _ = take_down(translation, mapped, &mut tables);
    if translation.flush(false, Wait::Status).is_ok() {
        tables.release();
    }
    match error {
        // A page of the run mapped already, or inside another pin's block:
        // under identity, the same frame pinned twice.
        MapError::AlreadyMapped(_) | MapError::BlockInTheWay(_) => DomainError::AlreadyPinned,
        _ => DomainError::Tables,
    }
}

/// Take `addresses` out of `translation`'s tables: on a VT-d domain each run
/// [`starts_run`] finds as one 2 MiB range, whether it was mapped as one
/// entry or as 512 pages, and every other page by itself (`L.iommu.108`).
/// Every table that empties goes to `tables`, for the caller to give back
/// once the unit's flush has completed (F-36).
///
/// # Errors
///
/// The first take-down the tables refused; what came before it is out.
fn take_down(
    translation: &Translation,
    addresses: &[u64],
    tables: &mut mm::UnlinkedTables,
) -> Result<(), MapError> {
    let mut rest = addresses;
    while let Some(&first) = rest.first() {
        let step = if let Some((unit, attached)) = translation.vtd()
            && starts_run(rest)
        {
            unit.unmap_run(attached, first, tables)?;
            RUN_PAGES
        } else {
            translation.unmap(first, tables)?;
            1
        };
        rest = rest.get(step..).unwrap_or_default();
    }
    Ok(())
}

impl Drop for Domain {
    /// Detach a translated domain from its unit, unless pages are still pinned
    /// into it: the device may still be using those, so its tables and context
    /// entry stay. An RMRR's identity domain is never detached, pinned or
    /// not: firmware's device keeps reaching the RMRR. With no page pinned
    /// its claim goes back instead, so the next [`domain_for`] hands out the
    /// same attachment again and attaches nothing; with pages still pinned
    /// it keeps the claim, so no second domain is ever made over tables
    /// holding this one's pins (`L.iommu.78`).
    fn drop(&mut self) {
        if let Some(identity) = self.translation.identity() {
            if self.pinned.load(Ordering::Relaxed) == 0 {
                identity.release();
            }
            return;
        }
        if self.translated() && self.pinned.load(Ordering::Relaxed) == 0 {
            let _ = self.translation.detach();
        }
    }
}

// ---------------------------------------------------------------------------
// Units the kernel programs
// ---------------------------------------------------------------------------

// CHECKS-ONLY: iommu/domain_check.rs
/// Every unit translation was turned on for, and the functions behind each.
#[derive(Debug, Default)]
#[doc(hidden)]
pub struct Programmed {
    /// VT-d units brought up: translating, or held to remapping only by an
    /// RMRR ([`vtd::Unit::translating`]).
    pub vtd: Vec<vtd::Unit>,
    /// The functions the DMAR's single-hop endpoint scopes name, with their
    /// unit's index, or `None` for a unit left alone: a function a refused
    /// unit names is that unit's, never the catch-all's.
    behind: Vec<(Address, Option<usize>)>,
    /// The DMAR's other PCI scopes -- longer paths, and sub-hierarchies --
    /// with their unit's index as `behind` has it: followed through the
    /// bridges enumeration finds, once it has ([`learn_bridge`]).
    scopes: Vec<(Scope, Option<usize>)>,
    /// Each segment's catch-all unit (`INCLUDE_PCI_ALL`), by index as
    /// `behind` has it.
    catch_all: Vec<(u16, topology::CatchAll<Option<usize>>)>,
    /// `SMMUv3`s translating.
    smmu: Vec<smmuv3::Unit>,
    /// Their registers' physical addresses, in the same order.
    smmu_phys: Vec<u64>,
    /// The requester IDs IORT root complexes send to them.
    streams: Vec<StreamMap>,
    /// The RMRRs' identity domains, each with its unit's index in `vtd`.
    identity: Vec<(usize, IdentityDomain)>,
    /// AMD-Vi units brought up: translating, or remapping only.
    pub(crate) amdvi: Vec<amdvi::Unit>,
    /// The IVRS's units as placement reads them, each with its index in
    /// `amdvi` (`iommu/ivrs.rs`).
    amdvi_places: Vec<ivrs::Placing>,
    /// The IVMDs' identity domains, each with its unit's index in `amdvi`.
    amdvi_identity: Vec<(usize, IdentityDomain)>,
}

impl Programmed {
    /// The identity domain an RMRR gave `function`, by the requester
    /// bring-up attached it for, and its unit's index in `vtd`.
    fn identity_of(&'static self, function: Address) -> Option<(usize, &'static IdentityDomain)> {
        self.identity
            .iter()
            .find(|(_, identity)| identity.function == function)
            .map(|(index, identity)| (*index, identity))
    }
}

/// An RMRR's identity domain (`vtd-design.md` §3.9(b), K2 of
/// `docs/INTEL-GPU.md`): the kernel's, attached at bring-up before its unit
/// translates, with exactly the RMRRs' pages mapped at their own addresses,
/// and never detached (`L.iommu.73`, `L.iommu.78`).
///
/// It is handed out by [`IdentityDomain::claim`] as an ordinary [`Domain`]
/// whose pins may never cover its RMRR pages and whose unpins may never take
/// one out, and only while no other such `Domain` lives: a claim made while
/// one does gets nothing, and a dropped one gives its claim back, so the
/// claim is never lost to a failed allocation and never doubled. The device
/// node keeps its domain for its life, so a driver that restarts gets the
/// same one, and nothing is attached again.
#[derive(Debug)]
pub struct IdentityDomain {
    /// The function the RMRRs name.
    function: Address,
    /// Its domain on the unit.
    attached: Attachment,
    /// The RMRR pages it maps, which no two share.
    regions: Vec<Identity>,
    /// Whether a [`Domain`] for it lives, or a dropped one left pins.
    claimed: AtomicBool,
    /// Whether [`domain_for`] has said why it handed out no domain, so the
    /// line is printed once, not at every ask.
    said: AtomicBool,
}

/// An identity domain's attachment: an RMRR's on a VT-d unit, an IVMD's on
/// an AMD-Vi unit.
#[derive(Clone, Copy, Debug)]
enum Attachment {
    /// On a VT-d unit.
    VtD(vtd::Attached),
    /// On an AMD-Vi unit.
    AmdVi(amdvi::Attached),
}

impl IdentityDomain {
    /// The record of `function`'s identity domain `attached`, holding
    /// `regions`.
    pub fn new(function: Address, attached: vtd::Attached, regions: Vec<Identity>) -> Self {
        Self::of(function, Attachment::VtD(attached), regions)
    }

    /// The record of an IVMD's identity domain `attached` on an AMD-Vi
    /// unit, holding `regions` (`docs/AMD-VI.md` §4.1).
    pub(crate) fn new_amdvi(
        function: Address,
        attached: amdvi::Attached,
        regions: Vec<Identity>,
    ) -> Self {
        Self::of(function, Attachment::AmdVi(attached), regions)
    }

    /// The record, unclaimed.
    fn of(function: Address, attached: Attachment, regions: Vec<Identity>) -> Self {
        IdentityDomain {
            function,
            attached,
            regions,
            claimed: AtomicBool::new(false),
            said: AtomicBool::new(false),
        }
    }

    /// The domain, on `unit`, unless one for it lives already.
    pub fn claim(&'static self, unit: &'static vtd::Unit) -> Option<Domain> {
        let Attachment::VtD(attached) = self.attached else {
            return None;
        };
        if self.claimed.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(Domain::with(Translation::VtD {
            unit,
            attached,
            changing: Gate::new(),
            identity: Some(self),
        }))
    }

    /// The domain, on AMD-Vi `unit`, unless one for it lives already.
    pub(crate) fn claim_amdvi(&'static self, unit: &'static amdvi::Unit) -> Option<Domain> {
        let Attachment::AmdVi(attached) = self.attached else {
            return None;
        };
        if self.claimed.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(Domain::with(Translation::AmdVi {
            unit,
            attached,
            changing: Gate::new(),
            identity: Some(self),
        }))
    }

    /// Give the claim back, as its [`Domain`] is dropped: nothing is
    /// detached.
    fn release(&self) {
        self.claimed.store(false, Ordering::Release);
    }

    /// Whether no line has yet said why it was not handed out; from here
    /// on, one has.
    fn first_refusal(&self) -> bool {
        !self.said.swap(true, Ordering::AcqRel)
    }

    /// Whether a [`Domain`] for it lives.
    pub fn claimed(&self) -> bool {
        self.claimed.load(Ordering::Acquire)
    }

    /// The RMRR pages it maps.
    pub(crate) fn regions(&self) -> &[Identity] {
        &self.regions
    }
}

/// Every RMRR page range an identity domain holds, for the frame
/// allocator's check (`L.iommu.75`).
pub fn identity_regions() -> impl Iterator<Item = Identity> {
    PROGRAMMED
        .get()
        .into_iter()
        .flat_map(|programmed| programmed.identity.iter().chain(&programmed.amdvi_identity))
        .flat_map(|(_, identity)| identity.regions().iter().copied())
}

/// What a unit's RMRRs ask of its bring-up ([`plan_rmrrs`]).
#[derive(Debug)]
enum RmrrPlan {
    /// No RMRR reaches it: it translates.
    None,
    /// Every RMRR that reaches it can be honoured: each function's
    /// identity domain is attached, then it translates.
    Honour(Vec<(Address, Vec<Identity>)>),
    /// An RMRR holds it to remapping only.
    Hold(Held),
}

/// An RMRR that holds its unit to remapping only, and why it was not
/// honoured, if it names a function behind the unit.
#[derive(Clone, Copy, Debug)]
struct Held {
    /// The RMRR's scope, as the table places it.
    hold: dmar::RmrrHold,
    /// Why it is not honoured; `None` for one the table cannot place.
    why: Option<Unhonoured>,
}

/// Why an RMRR naming a function behind its unit is not honoured.
#[derive(Clone, Copy, Debug)]
enum Unhonoured {
    /// The region itself, or a frame the allocator holds
    /// (`ferrix_paging::vtd::rmrr::Refusal`).
    Refused(rmrr::Refusal),
    /// Its identity domain could not be made.
    Attach(&'static str),
}

impl core::fmt::Display for Unhonoured {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Unhonoured::Refused(refusal) => refusal.fmt(f),
            Unhonoured::Attach(why) => f.write_str(why),
        }
    }
}

/// Whether `unit`'s RMRRs can be honoured, read from the table and the
/// memory map before the unit is opened (`L.iommu.73`, `L.iommu.74`,
/// `L.iommu.75`).
///
/// Every RMRR scope that reaches the unit is read
/// (`ferrix_acpi::dmar::Dmar::rmrr_holds`). One the table cannot place holds
/// it. Each one naming a function behind it must be whole pages, inside a
/// domain's reach, bounded, over no region the memory map calls RAM
/// (`ferrix_paging::vtd::rmrr::identity`), with every frame outside the
/// frame allocator, and share no page with the function's other RMRRs.
/// The first that is not holds the unit, with its reason.
fn plan_rmrrs(
    table: &dmar::Dmar<'_>,
    unit: &dmar::Drhd<'_>,
    memory: &[ferrix_bootinfo::MemRegion],
) -> RmrrPlan {
    let ram = || {
        memory
            .iter()
            .filter(|region| region.kind.is_ram())
            .map(|region| (region.base, region.len))
    };
    let mut functions: Vec<(Address, Vec<Identity>)> = Vec::new();
    for hold in table.rmrr_holds(*unit) {
        let held = |why| RmrrPlan::Hold(Held { hold, why });
        let dmar::RmrrHold::Named {
            base,
            limit,
            function: (bus, device, function),
        } = hold
        else {
            return held(None);
        };
        let Some(function) = Address::new(unit.segment, bus, device, function) else {
            return held(Some(Unhonoured::Attach(
                "its function is not a PCI address",
            )));
        };
        let region = match rmrr::identity(base, limit, ram()) {
            Ok(region) => region,
            Err(refusal) => return held(Some(Unhonoured::Refused(refusal))),
        };
        if let Err(refusal) = frames_outside_allocator(region) {
            return held(Some(Unhonoured::Refused(refusal)));
        }
        let at = match functions.iter().position(|(named, _)| *named == function) {
            Some(at) => at,
            None => {
                // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
                functions.push((function, Vec::new()));
                functions.len() - 1
            }
        };
        let Some((_, regions)) = functions.get_mut(at) else {
            continue;
        };
        // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
        regions.push(region);
        if let Err(refusal) = rmrr::disjoint(regions) {
            return held(Some(Unhonoured::Refused(refusal)));
        }
    }
    if functions.is_empty() {
        RmrrPlan::None
    } else {
        RmrrPlan::Honour(functions)
    }
}

/// Whether every frame of `region` is outside the frame allocator: past its
/// array, or recorded reserved -- never free, never handed out
/// (`L.iommu.75`).
///
/// # Errors
///
/// [`rmrr::Refusal::AllocatorFrame`] for the first frame the allocator
/// holds.
pub fn frames_outside_allocator(region: Identity) -> Result<(), rmrr::Refusal> {
    for page in region.page_addresses() {
        match mm::frame_state(page / PAGE_SIZE) {
            None | Some(ferrix_frame::State::Reserved) => {}
            Some(_) => return Err(rmrr::Refusal::AllocatorFrame { page }),
        }
    }
    Ok(())
}

/// Honour `plan` on `unit`, after its queue and interrupt remapping are on:
/// attach each RMRR function's identity domain, then turn translation on --
/// or, for a plan that holds the unit or an identity domain that could not
/// be made, leave it off, the plan now saying why (`L.iommu.73`,
/// `L.iommu.74`).
///
/// # Errors
///
/// [`vtd::Unit::translate`]'s.
fn honour_and_translate(
    unit: &vtd::Unit,
    plan: &mut RmrrPlan,
) -> Result<(vtd::Protected, Vec<IdentityDomain>), &'static str> {
    let mut identities = Vec::new();
    if let RmrrPlan::Honour(functions) = plan {
        match attach_identities(unit, functions) {
            Ok(made) => identities = made,
            Err(held) => *plan = RmrrPlan::Hold(held),
        }
    }
    if matches!(plan, RmrrPlan::Hold(_)) {
        return Ok((vtd::Protected::None, identities));
    }
    Ok((unit.translate()?, identities))
}

/// Attach each RMRR function's identity domain on `unit`, before it
/// translates.
///
/// # Errors
///
/// The first that could not be made, holding the unit to remapping only.
/// Those made before it stay attached, and are never handed out: the unit
/// does not translate, so its functions get untranslated domains.
fn attach_identities(
    unit: &vtd::Unit,
    functions: &mut Vec<(Address, Vec<Identity>)>,
) -> Result<Vec<IdentityDomain>, Held> {
    let mut made = Vec::new();
    for (function, regions) in functions.drain(..) {
        let attached = unit.attach_identity(function, &regions).map_err(|why| {
            let first = regions
                .first()
                .copied()
                .unwrap_or(Identity { base: 0, pages: 0 });
            Held {
                hold: dmar::RmrrHold::Named {
                    base: first.base,
                    limit: first.end().saturating_sub(1),
                    function: (function.bus(), function.device(), function.function()),
                },
                why: Some(Unhonoured::Attach(why)),
            }
        })?;
        // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
        made.push(IdentityDomain::new(function, attached, regions));
    }
    Ok(made)
}

/// The line for one identity domain an RMRR gave a function behind `unit`.
fn say_identity(unit: &dmar::Drhd<'_>, identity: &IdentityDomain) {
    for region in identity.regions() {
        println!(
            "  iommu    vt-d unit {:#x}: RMRR {:#x}-{:#x} identity-mapped for pci {}, {} pages, \
             held for the boot and never detached",
            unit.register_base,
            region.base,
            region.end().saturating_sub(1),
            identity.function,
            region.pages,
        );
    }
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
    /// Whether its path was a whole number of hops: one that was not names
    /// nothing, but may have meant a function the catch-all would take.
    whole: bool,
}

impl Scope {
    /// Whether it puts `function` behind its unit as `function`'s own
    /// requester ID, under an alias, or not at all; `None` for a path that
    /// cannot be followed.
    fn places(&self, function: Address, bridges: &[Bridge]) -> Option<topology::Behind> {
        let named = topology::follow_path(
            self.segment,
            self.start_bus,
            self.path.iter().copied(),
            bridges,
        )?;
        Some(claims(self.hierarchy, named, function, bridges))
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

/// Whether `ferrix.vtd=off` was asked, as [`bring_up`] read it.
static VTD_OFF: AtomicBool = AtomicBool::new(false);

/// Why every VT-d unit was left alone under `ferrix.vtd=off`.
const VTD_OFF_WHY: &str = "ferrix.vtd=off asked for every unit to be left alone";

/// Whether the command line says `ferrix.vtd=off`
/// (`ferrix_paging::vtd::unit::switched_off`), for [`bring_up`] and the
/// audit record's configuration.
pub fn vtd_off_asked(view: &BootView<'_>) -> bool {
    // The literal, not `ferrix_paging::vtd::unit::OFF_OPTION`, which names
    // the same word: the build-flags gate reads each option by its literal.
    ferrix_paging::vtd::unit::switched_off(view.option("ferrix.vtd"))
}

/// Whether [`bring_up`] left every VT-d unit alone for `ferrix.vtd=off`.
fn vtd_switched_off() -> bool {
    VTD_OFF.load(Ordering::Acquire)
}

// CHECKS-ONLY: iommu/domain_check.rs
/// What [`bring_up`] turned on.
#[doc(hidden)]
pub static PROGRAMMED: Once<Programmed> = Once::new();

/// What bringing the units up did.
#[derive(Clone, Copy, Debug, Default)]
pub struct BringUp {
    /// VT-d units now translating.
    pub vtd: usize,
    /// VT-d units an RMRR holds to remapping only.
    pub remapping_only: usize,
    /// `SMMUv3`s now translating.
    pub smmu_v3: usize,
    /// AMD-Vi units now translating.
    pub(crate) amdvi: usize,
    /// Units left alone.
    pub refused: usize,
    /// Why the last of them was.
    pub why: Option<&'static str>,
}

/// Turn translation on for every VT-d unit the DMAR describes, before any
/// device is given DMA. From here a function behind one reaches only what a
/// domain maps for it, and one with no domain reaches nothing.
///
/// A unit that cannot be brought up is left alone, and the functions behind it
/// get untranslated domains. Only the first call does anything.
pub fn bring_up(view: &BootView<'_>) -> BringUp {
    let mut report = BringUp::default();
    VTD_OFF.store(vtd_off_asked(view), Ordering::Release);
    let mut programmed = Programmed::default();
    if let Description::Acpi(firmware) = description::of(view) {
        let tables = firmware.acpi();
        if let Ok(table) = tables.dmar() {
            bring_up_vtd(&table, view.regions(), &mut programmed, &mut report);
        }
        if let Ok(table) = tables.iort() {
            bring_up_smmu(&table, &mut programmed, &mut report);
        }
        // A machine has a DMAR or an IVRS; were there both, the DMAR's units
        // would be the ones remapping's state is recorded for.
        if let (Err(_), Ok(table)) = (tables.dmar(), tables.ivrs()) {
            let off = amdvi_off_asked(view);
            ivrs::bring_up(&table, view.regions(), off, &mut programmed, &mut report);
        }
    }
    let _ = PROGRAMMED.call_once(|| programmed);
    report
}

/// Whether the command line says `ferrix.amdvi=off`
/// (`ferrix_paging::amdvi::unit::switched_off`), for the audit record's
/// configuration.
pub fn amdvi_off_asked(view: &BootView<'_>) -> bool {
    ferrix_paging::amdvi::unit::switched_off(view.option("ferrix.amdvi"))
}

/// Program every VT-d unit the DMAR describes, and record the functions its
/// endpoint scopes name, and each segment's catch-all.
///
/// Firmware keeps using an RMRR's region for a function behind a unit --
/// the N150's iGPU scanning out its framebuffer from stolen memory -- and a
/// unit that translated with no map of it would cut it off. So a unit whose
/// RMRRs can all be honoured ([`plan_rmrrs`]) gives each RMRR's function an
/// identity domain of them before it translates (`L.iommu.73`). One an RMRR
/// holds -- an RMRR the table cannot place, or one not honoured
/// (`L.iommu.74`) -- is brought up for interrupt remapping only,
/// translation off; its functions' DMA is untranslated and reaches all of
/// memory, which its line says (`L.iommu.67`).
///
/// With `ferrix.vtd=off` every unit is left alone (`L.iommu.71`).
fn bring_up_vtd(
    table: &dmar::Dmar<'_>,
    memory: &[ferrix_bootinfo::MemRegion],
    programmed: &mut Programmed,
    report: &mut BringUp,
) {
    let intr_remap = table.flags() & dmar::FLAG_INTR_REMAP != 0;
    let switched_off = vtd_switched_off();
    let mut states = Vec::new();
    // Each DRHD's register base and its index in `programmed.vtd`, or `None`
    // for one left alone.
    let mut indices: Vec<(u64, Option<usize>)> = Vec::new();
    for structure in table.structures() {
        let Structure::Drhd(unit) = structure else {
            continue;
        };
        let mut state = ferrix_paging::vtd::remap::UnitState {
            remapping: false,
            compatibility_blocked: false,
        };
        let mut plan = plan_rmrrs(table, &unit, memory);
        let mut protected = vtd::Protected::None;
        let mut identities = Vec::new();
        let opened = if switched_off {
            Err(VTD_OFF_WHY)
        } else {
            vtd::Unit::open(unit.register_base, intr_remap).and_then(|opened| {
                check::check_fault_events_masked(&opened);
                check::check_firmware_left_on(&opened);
                opened.enable()?;
                state = remapping::bring_up(&opened, &unit, table);
                (protected, identities) = honour_and_translate(&opened, &mut plan)?;
                Ok(opened)
            })
        };
        // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
        states.push(state);
        let hold = match &plan {
            RmrrPlan::Hold(held) => Some(*held),
            RmrrPlan::None | RmrrPlan::Honour(_) => None,
        };
        let index = match opened {
            Ok(opened) => {
                say_unit(&unit, &opened, hold, state, protected);
                let index = programmed.vtd.len();
                for identity in identities {
                    say_identity(&unit, &identity);
                    // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
                    programmed.identity.push((index, identity));
                }
                // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
                programmed.vtd.push(opened);
                if hold.is_none() {
                    report.vtd += 1;
                } else {
                    report.remapping_only += 1;
                }
                Some(index)
            }
            Err(why) => {
                report.refused += 1;
                report.why = Some(why);
                None
            }
        };
        // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
        indices.push((unit.register_base, index));
        record_scopes(&unit, index, programmed);
    }
    record_catch_alls(table, &indices, programmed);
    if switched_off {
        crate::println!(
            "  iommu    ferrix.vtd=off: every VT-d unit is left alone, so no DMA is translated \
             and no interrupt remapped; this is a recovery configuration, not an evaluated one"
        );
    }
    remapping::record(states);
}

/// Record the functions `unit`'s scopes name, against its index in
/// `programmed.vtd`, or `None` for a unit left alone.
fn record_scopes(unit: &dmar::Drhd<'_>, index: Option<usize>, programmed: &mut Programmed) {
    programmed
        .behind
        // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
        .extend(endpoints(unit).map(|address| (address, index)));
    for scope in unit.device_scopes() {
        let hierarchy = scope.kind == SCOPE_PCI_SUB_HIERARCHY;
        // Single-hop endpoints are in `behind` already.
        let longer_path = scope.kind == dmar::SCOPE_PCI_ENDPOINT && scope.endpoint().is_none();
        // A path with an odd byte names nothing (`place_dmar`) but stops
        // the catch-all taking what it might name.
        if !hierarchy && !longer_path {
            continue;
        }
        let kept = Scope {
            segment: unit.segment,
            hierarchy,
            start_bus: scope.start_bus,
            whole: scope.path_is_whole(),
            // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
            path: scope.path().collect(),
        };
        // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
        programmed.scopes.push((kept, index));
    }
}

/// A VT-d unit's bring-up line: translating with its walk, or held to
/// remapping only by an RMRR, with what that leaves its functions' DMA.
fn say_unit(
    unit: &dmar::Drhd<'_>,
    opened: &vtd::Unit,
    hold: Option<Held>,
    state: ferrix_paging::vtd::remap::UnitState,
    protected: vtd::Protected,
) {
    let remap_words = if state.remapping {
        "remapping on (256 entries, xAPIC format), CFIS=0 read back"
    } else {
        "remapping off"
    };
    match hold {
        None => println!(
            "  iommu    vt-d unit {:#x}: translating ({}), queue on, {remap_words}, {}",
            unit.register_base,
            opened.walk().name(),
            protected.says(),
        ),
        Some(Held {
            hold:
                dmar::RmrrHold::Named {
                    base,
                    limit,
                    function: (bus, device, function),
                },
            why,
        }) => println!(
            "  iommu    vt-d unit {:#x}: queue on, {remap_words}, translation left off: RMRR \
             {base:#x}-{limit:#x} names {:04x}:{bus:02x}:{device:02x}.{function}, which \
             firmware keeps using, and it is not honoured: {}; the DMA of every function \
             behind this unit is untranslated and reaches all of memory",
            unit.register_base,
            unit.segment,
            why.unwrap_or(Unhonoured::Attach("no reason was kept")),
        ),
        Some(Held {
            hold: dmar::RmrrHold::Unfollowed { base, limit },
            ..
        }) => println!(
            "  iommu    vt-d unit {:#x}: queue on, {remap_words}, translation left off: an RMRR \
             at {base:#x}-{limit:#x} names a function this table cannot place, which holds \
             every unit on segment {}; the DMA of every function behind this unit is \
             untranslated and reaches all of memory",
            unit.register_base, unit.segment,
        ),
    }
}

/// Record each segment's catch-all unit, by its index in `programmed.vtd`
/// as `indices` gives it (`None` for one left alone).
fn record_catch_alls(
    table: &dmar::Dmar<'_>,
    indices: &[(u64, Option<usize>)],
    programmed: &mut Programmed,
) {
    let index_of = |base: u64| {
        indices
            .iter()
            .find(|&&(at, _)| at == base)
            .and_then(|&(_, index)| index)
    };
    for &(base, _) in indices {
        let Some(segment) = table.structures().find_map(|structure| match structure {
            Structure::Drhd(unit) if unit.register_base == base => Some(unit.segment),
            _ => None,
        }) else {
            continue;
        };
        if programmed.catch_all.iter().any(|&(at, _)| at == segment) {
            continue;
        }
        let catch_all = match table.catch_all(segment) {
            dmar::CatchAll::None => topology::CatchAll::None,
            dmar::CatchAll::One(unit) => topology::CatchAll::One(index_of(unit.register_base)),
            dmar::CatchAll::Several => {
                println!(
                    "  iommu    two INCLUDE_PCI_ALL units on segment {segment}: neither is \
                     believed, and every function no scope names is unresolved"
                );
                topology::CatchAll::Several
            }
        };
        // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
        programmed.catch_all.push((segment, catch_all));
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
                // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
                programmed.smmu_phys.push(smmu.base_address);
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
///
/// For a function an RMRR gave an identity domain at bring-up, that domain
/// ([`IdentityDomain::claim`]), found by the requester it was attached for.
/// `None`, no domain at all, while one for it lives, or when enumeration
/// does not place the function behind that unit as its own requester
/// (`ferrix_pci::topology::agrees`): never an untranslated one, whose first
/// pin would say its DMA reaches all of memory while the unit confines it
/// (`L.iommu.78`). Every other function gets `Some`.
pub fn domain_for(function: Address) -> Option<Domain> {
    let Some(programmed) = PROGRAMMED.get() else {
        return Some(Domain::untranslated());
    };
    // An AMD-Vi unit's own function and the root complex (D10): never a
    // domain, so never a pin.
    if ivrs::is_exempt(function) {
        return None;
    }
    if let Some((index, identity)) = programmed
        .amdvi_identity
        .iter()
        .find(|(_, identity)| identity.function == function)
    {
        let unit = programmed.amdvi.get(*index)?;
        if !topology::agrees(amdvi_placed(programmed, function), &Some(*index)) {
            return None;
        }
        return identity.claim_amdvi(unit);
    }
    if let Some((index, identity)) = programmed.identity_of(function) {
        let unit = programmed.vtd.get(index)?;
        if !topology::agrees(vtd_placed(programmed, function), &Some(index)) {
            if !identity.first_refusal() {
                return None;
            }
            println!(
                "  iommu    pci {function} gets no domain: its RMRR identity domain is on the \
                 unit at {:#x}, and enumeration does not place it there as its own requester",
                unit.phys()
            );
            return None;
        }
        let domain = identity.claim(unit);
        if domain.is_none() && identity.first_refusal() {
            println!(
                "  iommu    pci {function} gets no second domain: its RMRR identity domain is \
                 held already"
            );
        }
        return domain;
    }
    Some(domain_placed(programmed, function))
}

/// [`domain_for`] for a function no RMRR gave an identity domain.
fn domain_placed(programmed: &'static Programmed, function: Address) -> Domain {
    let translation = if let Some(unit) = vtd_unit_for(programmed, function) {
        // A unit an RMRR holds to remapping only: its functions' DMA is
        // untranslated, and the first pin announces degraded trusted mode.
        if !unit.translating() {
            return Domain::untranslated();
        }
        unit.attach(function).map(|attached| Translation::VtD {
            unit,
            attached,
            changing: Gate::new(),
            identity: None,
        })
    } else if let Some((unit, stream)) = smmu_stream_for(programmed, function) {
        unit.attach(stream).map(|attached| Translation::SmmuV3 {
            unit,
            attached,
            changing: Gate::new(),
        })
    } else if let Some(unit) = amdvi_unit_for(programmed, function) {
        // A unit an IVMD holds to remapping only: untranslated, as VT-d's.
        if !unit.translating() {
            return Domain::untranslated();
        }
        unit.attach(function).map(|attached| Translation::AmdVi {
            unit,
            attached,
            changing: Gate::new(),
            identity: None,
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

/// How many VT-d units are translating, and how many of them read back a
/// register-based invalidation pending: none ever should, since every
/// invalidation goes through the queue (check R6).
pub fn register_invalidations_pending() -> (usize, usize) {
    PROGRAMMED.get().map_or((0, 0), |programmed| {
        (
            programmed.vtd.len(),
            programmed
                .vtd
                .iter()
                .filter(|unit| unit.register_invalidation_pending())
                .count(),
        )
    })
}

/// The VT-d unit the DMAR puts `function` behind as its own requester, if
/// it is translating.
fn programmed_unit_for(function: Address) -> Option<&'static vtd::Unit> {
    vtd_unit_for(PROGRAMMED.get()?, function)
}

/// The unit that remaps `function`'s interrupts as its own requester: the
/// VT-d unit the DMAR puts it behind, or the AMD-Vi unit the IVRS does.
/// `None` for a function behind neither, under an alias, or exempted by
/// D10.
fn remapper_for(function: Address) -> Option<remapping::Remapper> {
    if let Some(unit) = programmed_unit_for(function) {
        return Some(remapping::Remapper::VtD(unit));
    }
    if ivrs::is_exempt(function) {
        return None;
    }
    amdvi_unit_for(PROGRAMMED.get()?, function).map(remapping::Remapper::AmdVi)
}

/// Whether D10 exempted `function` -- an AMD-Vi unit's own function or the
/// root complex -- so that it is never given a domain, a vector, or bus
/// mastering (`DeviceNode::enable_dma`).
pub(crate) fn dma_exempt(function: Address) -> bool {
    ivrs::is_exempt(function)
}

/// Where the IVRS places `function` among the programmed AMD-Vi units, by
/// index (`None` for a unit left alone), as bring-up's placings say.
fn amdvi_placed(
    programmed: &'static Programmed,
    function: Address,
) -> topology::Placed<Option<usize>> {
    let bridges = BRIDGES.lock();
    match ivrs::placed(&programmed.amdvi_places, function, &bridges) {
        topology::Placed::Unit(at) => {
            topology::Placed::Unit(programmed.amdvi_places.get(at).and_then(|unit| unit.index))
        }
        topology::Placed::Nothing => topology::Placed::Nothing,
        topology::Placed::Unresolved => topology::Placed::Unresolved,
    }
}

/// The AMD-Vi unit the IVRS puts `function` behind as its own requester,
/// if it was brought up.
fn amdvi_unit_for(
    programmed: &'static Programmed,
    function: Address,
) -> Option<&'static amdvi::Unit> {
    match amdvi_placed(programmed, function) {
        topology::Placed::Unit(Some(index)) => programmed.amdvi.get(index),
        topology::Placed::Unit(None) | topology::Placed::Nothing | topology::Placed::Unresolved => {
            None
        }
    }
}

/// The VT-d unit the DMAR puts `function` behind, if it is translating.
///
/// A single-hop endpoint scope names it directly; any other scope is
/// followed through the bridges enumeration found, which a function below a
/// root port is examined after (`discovery::pci`).
fn vtd_unit_for(programmed: &'static Programmed, function: Address) -> Option<&'static vtd::Unit> {
    match vtd_placed(programmed, function) {
        topology::Placed::Unit(Some(index)) => programmed.vtd.get(index),
        topology::Placed::Unit(None) | topology::Placed::Nothing | topology::Placed::Unresolved => {
            None
        }
    }
}

/// Where enumeration places `function` among `programmed`'s VT-d units, by
/// index (`None` for a unit left alone), as [`vtd_unit_for`] decides.
fn vtd_placed(
    programmed: &'static Programmed,
    function: Address,
) -> topology::Placed<Option<usize>> {
    let bridges = BRIDGES.lock();
    let mut explicit = topology::Explicit::None;
    // A single-hop endpoint on a bus behind an aliasing bridge is refused
    // as a longer path through one is: the same `claims` decides.
    for &(address, index) in &programmed.behind {
        if address != function {
            continue;
        }
        if claims(false, function, function, &bridges) == topology::Behind::Own {
            explicit = topology::Explicit::Own(index);
            break;
        }
        explicit = topology::Explicit::Unfollowed;
    }
    if !matches!(explicit, topology::Explicit::Own(_)) {
        for (scope, index) in &programmed.scopes {
            if scope.segment != function.segment() {
                continue;
            }
            match scope
                .whole
                .then(|| scope.places(function, &bridges))
                .flatten()
            {
                Some(topology::Behind::Own) => {
                    explicit = topology::Explicit::Own(*index);
                    break;
                }
                Some(topology::Behind::No) => {}
                Some(topology::Behind::Aliased) | None => {
                    explicit = topology::Explicit::Unfollowed;
                }
            }
        }
    }
    let catch_all = programmed
        .catch_all
        .iter()
        .find(|&&(segment, _)| segment == function.segment())
        .map_or(topology::CatchAll::None, |&(_, catch_all)| catch_all);
    topology::place(explicit, catch_all, function, &bridges)
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

/// The interrupt requests a check made a unit refuse on purpose, by stream
/// and fault reason: checks R1 and R2's compatibility-format messages
/// (0x25) and R3's message naming another function's entry (0x26).
static PROVOKED_INTERRUPTS: SpinLock<Vec<(u32, u8)>> = SpinLock::new(Vec::new());

/// Record that `stream`'s device is about to send an interrupt request its
/// unit refuses with `reason`, so the fault is not stray. Said on the
/// console too, as [`record_provoked`] says a DMA fault.
pub(crate) fn record_provoked_interrupt(stream: u32, reason: u8) {
    // FATAL-ALLOC: boot only: a boot check provokes the fault this records.
    PROVOKED_INTERRUPTS.lock().push((stream, reason));
    println!(
        "  iommu    stream {stream:#x} is made to send an interrupt its unit refuses with reason \
         {reason:#x} ({}), on purpose",
        interrupt_reason_name(reason)
    );
}

/// Faults taken from a unit that nothing provoked: cleared before a check
/// began, or read by one and not recognised. Counted here so that no fault is
/// taken and then dropped unseen.
static STRAY: AtomicU64 = AtomicU64::new(0);

/// Of [`STRAY`], the ones that were not a refused access: an `SMMUv3` event
/// of another type ([`Cause::Event`]), an overflow of its queue
/// ([`Cause::Lost`]), or a VT-d fault lost to a full record
/// ([`Cause::Overflow`]).
static STRAY_EVENTS: AtomicU64 = AtomicU64::new(0);

/// DMA faults read from a unit, provoked or not, counted where each read is
/// made ([`counted`]) and not by [`recorded`], so that a read that skipped the
/// record still counts: what the end-of-boot audit check requires a
/// `DMA_FAULT` record for. A boot whose out-of-domain probe did not run --
/// its entropy request skipped, under load an interrupt that never came --
/// read none, and owes none (FX-0309).
static DMA_FAULTS_READ: AtomicU64 = AtomicU64::new(0);

/// How many DMA faults have been read from a unit so far. See
/// [`DMA_FAULTS_READ`].
fn dma_faults_read() -> u64 {
    DMA_FAULTS_READ.load(Ordering::Relaxed)
}

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
    if let Cause::Interrupt { reason, .. } = fault.cause {
        return PROVOKED_INTERRUPTS
            .lock()
            .iter()
            .any(|&(stream, provoked)| fault.stream == stream && reason == provoked);
    }
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
    /// Units that translate or remap, whose records were read.
    pub(crate) units: usize,
    /// Of the VT-d ones, those that translate.
    pub(crate) translating: usize,
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
            if !ferrix_paging::vtd::unit::audited(unit.translating(), unit.remapping()) {
                continue;
            }
            if unit.translating() {
                audit.translating += 1;
            }
            drain(|| recorded(counted(unit.take_fault())), &mut audit);
        }
        for unit in &programmed.smmu {
            audit.translating += 1;
            drain(|| recorded(counted(unit.take_fault())), &mut audit);
        }
        for unit in &programmed.amdvi {
            if unit.translating() {
                audit.translating += 1;
            }
            drain(|| recorded(counted(unit.take_fault())), &mut audit);
        }
    }
    audit.stray = STRAY.load(Ordering::Relaxed);
    audit.stray_events = STRAY_EVENTS.load(Ordering::Relaxed);
    audit
}

/// The same reading of every unit's faults at the end of a boot that skips
/// its checks (`ferrix.checks=skip`, as the N150 box boots), which nothing
/// else would read: one line, never fatal, interrupt requests' faults
/// included, so a boot on hardware shows what its units refused.
pub fn report_unchecked_faults() {
    let audit = audit_faults();
    println!(
        "  iommu    {} faults recorded in an unchecked boot, {} of them not a refused DMA \
         access (interrupt requests' included), across {} units that translate or remap",
        audit.stray, audit.stray_events, audit.units,
    );
    if let Some(fault) = audit.first {
        println!("  iommu    the first: {fault}");
    }
}

/// Count `fault` in [`DMA_FAULTS_READ`] if it is one [`recorded`] writes as a
/// `DMA_FAULT`: anything but an interrupt request's fault.
fn counted(fault: Option<Fault>) -> Option<Fault> {
    if fault.is_some_and(|fault| !matches!(fault.cause, Cause::Interrupt { .. })) {
        let _ = DMA_FAULTS_READ.fetch_add(1, Ordering::Relaxed);
    }
    fault
}

/// A fault a unit reported, recorded in the audit record as it is read
/// (`audit::DMA_FAULT`): every read of a unit's faults passes here. The
/// kernel is the subject, never the task that happens to be running, which
/// is not the device's.
fn recorded(fault: Option<Fault>) -> Option<Fault> {
    if let Some(fault) = fault {
        // An interrupt fault is its own event, with its reason and index
        // where a DMA fault has its page (`L.iommu.54`).
        let (event, detail) = match fault.cause {
            Cause::Interrupt { reason, index } => (
                crate::audit::INTERRUPT_FAULT,
                [u32::from(reason), u32::from(index), 0],
            ),
            _ => (
                crate::audit::DMA_FAULT,
                [
                    (fault.page >> 12) as u32,
                    (fault.page >> 44) as u32,
                    u32::from(fault.write),
                ],
            ),
        };
        crate::audit::record(
            event,
            crate::audit::Outcome::Refused,
            0,
            crate::audit::Subject::KERNEL,
            crate::audit::Target {
                kind: crate::audit::target::DEVICE,
                id: u64::from(fault.stream),
            },
            detail,
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
