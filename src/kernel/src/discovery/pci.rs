//! Stage 10: finding PCI functions.
//!
//! The kernel enumerates buses and does not drive devices
//! (`docs/ARCHITECTURE.md` §7). This is the enumeration: where firmware said
//! configuration space is, a [`ConfigSpace`] over it, and the walk `src/lib/platform/pci`
//! already proves on the host, run over real devices.
//!
//! # Where configuration space is
//!
//! On a machine with ACPI tables — x86-64, and AArch64 under EDK2 — the MCFG
//! says, and it is authoritative even if a device tree came too. Otherwise the
//! device tree's `pci-host-ecam-generic` nodes say, and its
//! `pci-host-cam-generic` ones, which crosvm writes: the same idea with 256
//! bytes a function and 64 KiB a bus. The two disagree about
//! what their address means — bus zero's for the MCFG, the first bus's for a
//! device tree — and both parsers hand over the first bus's, so nothing here
//! has to remember which it read.
//!
//! Two descriptions of the same buses would enumerate every function on them
//! twice — two device nodes, two drivers, one device — so a host whose segment
//! and buses overlap one already accepted is refused. A device tree host that
//! does not say which segment it is gets the lowest one no other host names,
//! as Linux does, rather than every such host sharing segment zero.
//!
//! # A bus at a time
//!
//! An ECAM window is a megabyte per bus, and a machine describes 256 buses
//! whether or not it has more than one. Mapping all of it would take 256 MiB
//! of the kernel's address space — more than half of what a 32-bit kernel's
//! arena has — and half a megabyte of page tables, to reach a handful of
//! functions on bus zero. So [`Space`] maps a bus's megabyte the first time
//! the walk reads from it, and gives every window back when it is dropped.
//!
//! # Lines, where there are no messages
//!
//! A device tree that describes neither a GICv3 ITS nor a `GICv2m` frame gives
//! its devices no way to signal a message, which is crosvm's machine. There a
//! function's `INTx` pin is followed through its host's `interrupt-map` to the
//! GIC line it drives, and that line becomes the function's one vector
//! (`DeviceNode::pci`). Only functions on a host's root bus are followed,
//! because one behind a bridge has its pin swizzled on the way, which crosvm
//! never needs; and a line two functions share is given to the first, since
//! a vector belongs to one driver.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::cell::RefCell;
use core::fmt;
use core::ops::RangeInclusive;

use ferrix_bootinfo::BootView;
use ferrix_fdt::{EcamHost, Fdt, GicInterrupt};
use ferrix_pci::bar::{self, Region};
use ferrix_pci::capability::{
    self as pci_capability, Capabilities, Capability, ExtendedCapabilities, ID_MSIX, MsiX,
};
use ferrix_pci::ecam::{Layout, Window};
use ferrix_pci::header::{
    BusNumbers, CLASS_BRIDGE, COMMAND, COMMAND_MEMORY_SPACE, Endpoint, HeaderKind,
    SUBCLASS_HOST_BRIDGE,
};
use ferrix_pci::virtio::{self as virtio_pci, SharedMemory, TYPE_ENTROPY, TYPE_GPU, Transport};
use ferrix_pci::walk::{Function, Walk};
use ferrix_pci::{Address, ConfigSpace, PciError};

mod virtio;

use crate::device::{self, DeviceNode, Reserved, Seen};
use crate::discovery::description::{self, Description};
use crate::discovery::fdt;
use crate::discovery::finder::{Context, Failed, Finder};
use crate::mmio::Mmio;
use crate::vmap;

/// Which description a host came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Source {
    /// The ACPI MCFG table.
    Mcfg,
    /// A `pci-host-ecam-generic` or `pci-host-cam-generic` device tree node.
    DeviceTree,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::Mcfg => "MCFG",
            Source::DeviceTree => "device-tree",
        })
    }
}

/// A configuration window firmware described.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Host {
    /// The functions it reaches.
    pub(crate) window: Window,
    /// The physical address of its first bus.
    pub(crate) phys: u64,
    /// The device tree's node for it, which says where its `INTx` pins go.
    /// `None` for an MCFG allocation.
    pub(crate) tree: Option<EcamHost>,
}

/// One description of a host, before it is given a segment and checked
/// against the others.
struct Described {
    /// The segment, if the description names one.
    segment: Option<u16>,
    /// The first bus.
    start_bus: u8,
    /// The last bus.
    end_bus: u8,
    /// The physical address of the first bus.
    phys: u64,
    /// How functions are laid out in the window.
    layout: Layout,
    /// The device tree's node, if that is where it came from.
    tree: Option<EcamHost>,
}

/// Whether two bus ranges share a bus.
fn buses_overlap(a: &RangeInclusive<u8>, b: &RangeInclusive<u8>) -> bool {
    a.start() <= b.end() && b.start() <= a.end()
}

/// Every ECAM host `machine` describes, and how many descriptions could not
/// be used.
fn hosts(machine: Description) -> (Vec<Host>, usize, Source) {
    let mut refused = 0;
    let mut described = Vec::new();

    let source = if let Description::Acpi(firmware) = machine {
        if let Ok(mcfg) = firmware.acpi().mcfg() {
            for allocation in mcfg.entries() {
                match allocation.window_base() {
                    // FATAL-ALLOC: boot only: PCI enumeration runs once, at stage 10, before any program runs.
                    Some(phys) => described.push(Described {
                        segment: Some(allocation.segment),
                        start_bus: allocation.start_bus,
                        end_bus: allocation.end_bus,
                        phys,
                        layout: Layout::Ecam,
                        tree: None,
                    }),
                    None => refused += 1,
                }
            }
        }
        Source::Mcfg
    } else {
        if let Description::Tree(tree) = machine {
            for host in tree.ecam_hosts() {
                // FATAL-ALLOC: boot only: PCI enumeration runs once, at stage 10, before any program runs.
                described.push(Described {
                    segment: host.segment,
                    start_bus: host.start_bus,
                    end_bus: host.end_bus,
                    phys: host.window.address,
                    layout: if host.cam { Layout::Cam } else { Layout::Ecam },
                    tree: Some(host),
                });
            }
        }
        Source::DeviceTree
    };

    // FATAL-ALLOC: boot only: PCI enumeration runs once, at stage 10, before any program runs.
    let named: BTreeSet<u16> = described.iter().filter_map(|host| host.segment).collect();
    let mut hosts: Vec<Host> = Vec::new();
    for host in described {
        let segment = host.segment.or_else(|| {
            (0..=u16::MAX).find(|candidate| {
                !named.contains(candidate)
                    && !hosts
                        .iter()
                        .any(|taken| taken.window.segment() == *candidate)
            })
        });
        let Some(window) = segment.and_then(|segment| {
            Window::with_layout(host.layout, segment, host.start_bus, host.end_bus)
        }) else {
            refused += 1;
            continue;
        };
        let overlaps = hosts.iter().any(|taken| {
            taken.window.segment() == window.segment()
                && buses_overlap(&taken.window.buses(), &window.buses())
        });
        if overlaps {
            refused += 1;
            continue;
        }
        // FATAL-ALLOC: boot only: PCI enumeration runs once, at stage 10, before any program runs.
        hosts.push(Host {
            window,
            phys: host.phys,
            tree: host.tree,
        });
    }
    (hosts, refused, source)
}

/// Where `INTx` lines go on a machine whose device tree describes no MSI
/// controller, and the lines already given out.
struct Lines {
    /// The tree the hosts came from.
    tree: Fdt<'static>,
    /// GIC identifiers already some function's vector.
    taken: BTreeSet<u32>,
}

impl Lines {
    /// The lines of `view`'s machine, if its functions are to be given lines
    /// at all: a device tree machine with no ITS and no `GICv2m` frame.
    fn of(view: &BootView<'_>, source: Source) -> Option<Self> {
        if source != Source::DeviceTree {
            return None;
        }
        let tree = fdt::open(view).ok()?;
        let messages = tree.gicv3_its().is_some() || tree.gicv2m_frames().next().is_some();
        (!messages).then(|| Lines {
            tree,
            taken: BTreeSet::new(),
        })
    }

    /// The line `function`'s `INTx` `pin` drives, if it is on `host`'s root
    /// bus, the map names one, and no function holds it yet.
    fn line(&mut self, host: &Host, function: Address, pin: u8) -> Option<GicInterrupt> {
        let described = host.tree?;
        if function.bus() != described.start_bus {
            return None;
        }
        let line = self.tree.ecam_intx(
            &described,
            function.bus(),
            function.device(),
            function.function(),
            pin,
        )?;
        device::claim_line(line.id, &mut self.taken).then_some(line)
    }
}

/// One host's configuration space, mapped a bus at a time.
#[derive(Debug)]
struct Space {
    /// The window.
    host: Host,
    /// Every bus touched so far: where its megabyte is mapped, or `None` if
    /// mapping it failed, so a failure is not retried on every read.
    buses: RefCell<BTreeMap<u8, Option<u64>>>,
}

impl Space {
    /// Configuration space for `host`, with nothing mapped yet.
    const fn new(host: Host) -> Self {
        Space {
            host,
            buses: RefCell::new(BTreeMap::new()),
        }
    }

    /// The registers of `bus`, mapping them on first use.
    fn bus(&self, bus: u8) -> Option<Mmio> {
        let mut buses = self.buses.borrow_mut();
        if let Some(mapped) = buses.get(&bus) {
            return mapped.map(Mmio::at);
        }
        let index = bus.checked_sub(*self.host.window.buses().start())?;
        let per_bus = self.host.window.layout().bytes_per_bus();
        let mapped = u64::from(index)
            .checked_mul(per_bus)
            .and_then(|offset| self.host.phys.checked_add(offset))
            .and_then(|phys| vmap::map_device(phys, per_bus).ok());
        // FATAL-ALLOC: boot only: PCI enumeration runs once, at stage 10, before any program runs.
        let _ = buses.insert(bus, mapped);
        mapped.map(Mmio::at)
    }

    /// Whether any bus the walk reached could not be mapped.
    fn unmapped(&self) -> Option<u8> {
        self.buses
            .borrow()
            .iter()
            .find_map(|(bus, mapped)| mapped.is_none().then_some(*bus))
    }

    /// The window and offset of `width` bytes at `offset` in `function`'s
    /// space, or `None` if the access is outside the window, not aligned to
    /// its width, or on a bus that could not be mapped.
    ///
    /// The alignment is refused rather than served because a volatile read of
    /// a misaligned `u16` or `u32` is undefined behaviour; `src/lib/platform/pci` never
    /// asks for one, so a refusal reads as all ones and nothing else changes.
    fn register(&self, function: Address, offset: u16, width: u16) -> Option<(Mmio, u64)> {
        if !offset.is_multiple_of(width) {
            return None;
        }
        let at = self.host.window.offset(function, offset, width)?;
        let registers = self.bus(function.bus())?;
        Some((registers, at % self.host.window.layout().bytes_per_bus()))
    }
}

impl ConfigSpace for Space {
    fn read8(&self, function: Address, offset: u16) -> u8 {
        self.register(function, offset, 1)
            .map_or(u8::MAX, |(registers, at)| registers.read8(at))
    }

    fn read16(&self, function: Address, offset: u16) -> u16 {
        self.register(function, offset, 2)
            .map_or(u16::MAX, |(registers, at)| registers.read16(at))
    }

    fn read32(&self, function: Address, offset: u16) -> u32 {
        self.register(function, offset, 4)
            .map_or(u32::MAX, |(registers, at)| registers.read32(at))
    }

    fn write16(&mut self, function: Address, offset: u16, value: u16) {
        if let Some((registers, at)) = self.register(function, offset, 2) {
            registers.write16(at, value);
        }
    }

    fn write32(&mut self, function: Address, offset: u16, value: u32) {
        if let Some((registers, at)) = self.register(function, offset, 4) {
            registers.write32(at, value);
        }
    }
}

impl Drop for Space {
    fn drop(&mut self) {
        for mapped in self.buses.get_mut().values().flatten() {
            let _ = vmap::unmap_device(*mapped);
        }
    }
}

/// What enumeration found.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Report {
    /// ECAM hosts described and walked.
    pub(crate) hosts: usize,
    /// Descriptions that could not be turned into a window, or that overlap
    /// one already accepted.
    pub(crate) refused: usize,
    /// Where the hosts were described.
    pub(crate) source: Source,
    /// Functions found.
    pub(crate) functions: usize,
    /// Of those, host bridges.
    pub(crate) host_bridges: usize,
    /// Bridges whose buses the walk did not follow.
    pub(crate) unfollowed: usize,
    /// BARs sized.
    pub(crate) bars: usize,
    /// Bytes of aperture those BARs decode.
    pub(crate) aperture_bytes: u64,
    /// Standard and extended capabilities walked.
    pub(crate) capabilities: usize,
    /// Virtio functions with a complete transport inside their memory BARs.
    pub(crate) virtio: usize,
    /// Functions given an `INTx` line for a vector, on a machine with no MSI
    /// controller.
    pub(crate) intx: usize,
    /// Bytes of entropy virtio-rng devices wrote into memory the kernel gave
    /// them.
    pub(crate) entropy_bytes: u32,
    /// Entropy checks skipped because the device refused or stalled.
    pub(crate) entropy_skipped: usize,
    /// Why the last one was skipped.
    pub(crate) entropy_skip: Option<&'static str>,
    /// Entropy requests whose completion arrived by MSI-X.
    pub(crate) entropy_by_interrupt: usize,
    /// Why the last request that completed without MSI-X was polled instead.
    pub(crate) entropy_polled: Option<&'static str>,
    /// Writes outside a device's translated domain that its unit faulted.
    pub(crate) out_of_domain_faulted: usize,
    /// The completion the device reported for the last faulted write anyway,
    /// if it did: a fact about the device model, printed so a boot log shows
    /// it (`virtio::Faulted`).
    pub(crate) out_of_domain_completed: Option<virtio::Completed>,
    /// Why the last out-of-domain write was not shown to fault, if one was not.
    pub(crate) out_of_domain_skip: Option<&'static str>,
}

/// Why enumeration failed.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Failure {
    /// A host's window was described but nothing answered on its root bus.
    NothingAnswered {
        /// The physical address the window was mapped from.
        phys: u64,
    },
    /// A bus the walk reached could not be mapped.
    Unmapped {
        /// The physical address of the host's window.
        phys: u64,
        /// The bus.
        bus: u8,
    },
    /// `src/lib/platform/pci` refused what a function presented.
    Refused(PciError),
    /// A virtio queue said something impossible about a request in flight.
    Queue(ferrix_virtio::QueueError),
    /// The entropy self-check saw a completion that cannot be right.
    Entropy(&'static str),
    /// A function kept ATS on after it was switched off, so its device could
    /// translate for itself past an unpin (`docs/certification/SAFETY-MANUAL.md`,
    /// AoU-12).
    AtsOn {
        /// The function.
        function: Address,
    },
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Failure::NothingAnswered { phys } => {
                write!(f, "no function answered in the ECAM window at {phys:#x}")
            }
            Failure::Unmapped { phys, bus } => {
                write!(
                    f,
                    "bus {bus:#04x} of the ECAM window at {phys:#x} could not be mapped"
                )
            }
            Failure::Refused(error) => write!(f, "{error}"),
            Failure::Queue(error) => write!(f, "virtio queue: {error:?}"),
            Failure::Entropy(what) => write!(f, "virtio-rng: {what}"),
            Failure::AtsOn { function } => write!(
                f,
                "{function} kept address translation services on after it was switched off"
            ),
        }
    }
}

impl From<PciError> for Failure {
    fn from(error: PciError) -> Self {
        Failure::Refused(error)
    }
}

impl From<ferrix_virtio::QueueError> for Failure {
    fn from(error: ferrix_virtio::QueueError) -> Self {
        Failure::Queue(error)
    }
}

/// PCI enumeration, as a [`Finder`]: find every function behind the ECAM
/// hosts firmware describes, size its BARs, walk its capabilities, and build
/// a device node for each from what its BARs decode.
///
/// A failure halts the boot, as it did before there were finders: the walk
/// is where a broken configuration space or DMA path shows first.
pub(crate) struct Enumeration {
    /// The hosts still to walk.
    hosts: Vec<Host>,
    /// Their ECAM windows, which no aperture may overlap.
    ecam: Vec<(u64, u64)>,
    /// What the walk found.
    report: Report,
    /// Where `INTx` lines go, on a machine that needs them.
    lines: Option<Lines>,
    /// Why the walk failed, once it has.
    failure: Option<Failure>,
}

impl Enumeration {
    /// The hosts `view`'s machine describes, ready to walk.
    pub(crate) fn new(view: &BootView<'_>) -> Self {
        let (hosts, refused, source) = hosts(description::of(view));
        let ecam: Vec<(u64, u64)> = hosts
            .iter()
            .map(|host| (host.phys, host.phys.saturating_add(host.window.len())))
            // FATAL-ALLOC: boot only: PCI enumeration runs once, at stage 10, before any program runs.
            .collect();
        let report = Report {
            hosts: hosts.len(),
            refused,
            source,
            functions: 0,
            host_bridges: 0,
            unfollowed: 0,
            bars: 0,
            aperture_bytes: 0,
            capabilities: 0,
            virtio: 0,
            intx: 0,
            entropy_bytes: 0,
            entropy_skipped: 0,
            entropy_skip: None,
            entropy_by_interrupt: 0,
            entropy_polled: None,
            out_of_domain_faulted: 0,
            out_of_domain_completed: None,
            out_of_domain_skip: None,
        };
        let lines = Lines::of(view, source);
        Enumeration {
            hosts,
            ecam,
            report,
            lines,
            failure: None,
        }
    }
}

impl fmt::Debug for Enumeration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Enumeration")
            .field("hosts", &self.hosts.len())
            .field("ecam", &self.ecam)
            .finish_non_exhaustive()
    }
}

impl Finder for Enumeration {
    fn name(&self) -> &'static str {
        "pci"
    }

    fn reads(&self) -> &[(u64, u64)] {
        &self.ecam
    }

    fn find(&mut self, cx: &mut Context<'_>, nodes: &mut Vec<DeviceNode>) -> Result<(), Failed> {
        for host in core::mem::take(&mut self.hosts) {
            if let Err(failure) = check_host(
                host,
                cx.reserved,
                &mut self.report,
                nodes,
                self.lines.as_mut(),
            ) {
                self.failure = Some(failure);
                return Err(Failed);
            }
        }
        Ok(())
    }

    fn failure(&self) -> Option<&dyn fmt::Display> {
        self.failure
            .as_ref()
            .map(|failure| failure as &dyn fmt::Display)
    }

    fn report(&self) {
        let report = &self.report;
        if report.hosts == 0 {
            crate::println!(
                "  pci      no ECAM host described, {} descriptions refused",
                report.refused
            );
            return;
        }
        crate::println!(
            "  pci      {} functions from {} {} hosts ({} descriptions refused), {} host bridges, \
             {} unfollowed bridges; {} BARs sized ({} KiB), {} capabilities, {} virtio transports, \
             {} entropy bytes read by DMA, {} completions by MSI-X, {} out-of-domain writes faulted",
            report.functions,
            report.hosts,
            report.source,
            report.refused,
            report.host_bridges,
            report.unfollowed,
            report.bars,
            report.aperture_bytes / 1024,
            report.capabilities,
            report.virtio,
            report.entropy_bytes,
            report.entropy_by_interrupt,
            report.out_of_domain_faulted,
        );
        if report.intx > 0 {
            crate::println!(
                "  pci      {} functions interrupt by INTx lines: the machine has no MSI controller",
                report.intx
            );
        }
        if let Some(why) = &report.out_of_domain_skip {
            crate::println!("  pci      an out-of-domain write was not shown to fault: {why}");
        }
        if let Some(completed) = &report.out_of_domain_completed {
            crate::println!(
                "  pci      the device completed the faulted write anyway: {} bytes that never \
                 reached the page, seen {} the fault, {} further faults recorded for it",
                completed.written,
                if completed.before_fault {
                    "before"
                } else {
                    "after"
                },
                completed.further_faults,
            );
        }
        if let Some(why) = &report.entropy_polled {
            crate::println!("  pci      an entropy request was polled, not interrupted: {why}");
        }
        if let Some(why) = &report.entropy_skip {
            crate::println!(
                "  pci      {} entropy checks skipped: {why}",
                report.entropy_skipped
            );
        }
    }
}

/// Walk one host and examine everything it reaches.
fn check_host(
    host: Host,
    reserved: &Reserved,
    report: &mut Report,
    nodes: &mut Vec<DeviceNode>,
    mut lines: Option<&mut Lines>,
) -> Result<(), Failure> {
    let mut space = Space::new(host);

    // Collected first: the walk borrows the space, and sizing writes to it.
    let mut found: Vec<Function> = Vec::new();
    for item in Walk::new(&space, host.window.segment(), host.window.buses()) {
        match item {
            // FATAL-ALLOC: boot only: PCI enumeration runs once, at stage 10, before any program runs.
            Ok(function) => found.push(function),
            Err(_) => report.unfollowed += 1,
        }
    }
    if let Some(bus) = space.unmapped() {
        return Err(Failure::Unmapped {
            phys: host.phys,
            bus,
        });
    }
    if found.is_empty() {
        return Err(Failure::NothingAnswered { phys: host.phys });
    }

    for function in found {
        let (regions, msix, decoding, transport, host_visible) =
            check_function(&mut space, function, reserved, report)?;
        // Where the function's configuration space is, which minting one of
        // its MSI-X vectors writes to turn MSI-X on.
        let config_phys = host
            .window
            .offset(function.address, 0, 1)
            .and_then(|offset| host.phys.checked_add(offset));
        // What sysfs shows beside the identity: the board's ids for an
        // endpoint, the bus behind a bridge.
        let endpoint = Endpoint::read(&space, function.address).ok();
        let subsystem = endpoint.as_ref().map_or((0, 0), |endpoint| {
            (endpoint.subsystem_vendor, endpoint.subsystem)
        });
        let intx = endpoint.as_ref().and_then(|endpoint| {
            lines
                .as_deref_mut()?
                .line(&host, function.address, endpoint.interrupt_pin)
        });
        report.intx += usize::from(intx.is_some());
        let secondary_bus = BusNumbers::read(&space, function.address)
            .ok()
            .map(|numbers| numbers.secondary);
        // FATAL-ALLOC: boot only: PCI enumeration runs once, at stage 10, before any program runs.
        nodes.push(DeviceNode::pci(
            function.address,
            &Seen {
                config_phys,
                identity: &function.identity,
                transport: transport.as_ref(),
                subsystem,
                secondary_bus,
                host_visible: host_visible.as_ref(),
                intx,
            },
            &regions,
            msix.as_ref(),
            decoding,
            reserved,
        ));
    }
    Ok(())
}

/// What examining one function yields: its sized BARs, its MSI-X capability
/// if it has one, whether firmware left its memory decoding on, its virtio
/// transport, and a virtio GPU's host-visible window.
type Examined = (
    Vec<Region>,
    Option<(Capability, MsiX)>,
    bool,
    Option<Transport>,
    Option<SharedMemory>,
);

/// Walk a function's extended capability list, counting each, and leave its
/// address translation services off if it has them ([`keep_ats_off`]).
///
/// # Errors
///
/// What the walk refused, and [`Failure::AtsOn`].
fn walk_extended(space: &mut Space, function: Address, report: &mut Report) -> Result<(), Failure> {
    let mut ats = None;
    for capability in ExtendedCapabilities::new(&*space, function) {
        let capability = capability?;
        report.capabilities += 1;
        if capability.id == pci_capability::EXTENDED_ID_ATS {
            ats = Some(capability.offset);
        }
    }
    match ats {
        Some(at) => keep_ats_off(space, function, at),
        None => Ok(()),
    }
}

/// Leave a function's address translation services off, as the kernel
/// assumes (`docs/certification/SAFETY-MANUAL.md`, AoU-12): a device that
/// cached translations of its own could reach a page after its unpin, which
/// invalidates the IOMMU's translations and not a device's. Firmware may have
/// switched it on; it is switched off here, before any driver runs.
///
/// # Errors
///
/// [`Failure::AtsOn`] for a function that keeps it on.
fn keep_ats_off(space: &mut Space, function: Address, at: u16) -> Result<(), Failure> {
    let control = at + pci_capability::ATS_CONTROL;
    let value = space.read16(function, control);
    if value & pci_capability::ATS_CONTROL_ENABLE == 0 {
        return Ok(());
    }
    space.write16(
        function,
        control,
        value & !pci_capability::ATS_CONTROL_ENABLE,
    );
    if space.read16(function, control) & pci_capability::ATS_CONTROL_ENABLE != 0 {
        return Err(Failure::AtsOn { function });
    }
    crate::println!("  pci      {function}: address translation services switched off");
    Ok(())
}

/// Size every BAR of one function and walk both its capability lists,
/// returning the BARs it decodes, its MSI-X capability if it has one, and
/// whether firmware left its memory decoding on.
fn check_function(
    space: &mut Space,
    function: Function,
    reserved: &Reserved,
    report: &mut Report,
) -> Result<Examined, Failure> {
    let Function { address, identity } = function;
    report.functions += 1;
    if identity.class.base == CLASS_BRIDGE && identity.class.sub == SUBCLASS_HOST_BRIDGE {
        report.host_bridges += 1;
    }
    // Read before sizing, which switches decoding off and back to this.
    let decoding = space.read16(address, COMMAND) & COMMAND_MEMORY_SPACE != 0;

    for capability in Capabilities::new(&*space, address) {
        let _ = capability?;
        report.capabilities += 1;
    }
    walk_extended(space, address, report)?;

    let msix = match pci_capability::find(&*space, address, ID_MSIX)? {
        Some(capability) => Some((capability, MsiX::read(&*space, capability)?)),
        None => None,
    };

    let mut regions = Vec::new();
    for index in 0..identity.kind.bar_slots() {
        match bar::size(space, address, identity.kind, index) {
            Ok(Some(region)) => {
                report.bars += 1;
                report.aperture_bytes = report.aperture_bytes.saturating_add(region.size);
                // FATAL-ALLOC: boot only: PCI enumeration runs once, at stage 10, before any program runs.
                regions.push(region);
            }
            // An unimplemented slot, or the upper half of a 64-bit BAR.
            Ok(None) | Err(PciError::NoSuchBar { .. }) => {}
            Err(error) => return Err(error.into()),
        }
    }

    // Only a virtio device's vendor capabilities are virtio's: an Intel
    // bridge carries one of its own, shorter than virtio's format, and
    // reading it as virtio's would refuse a perfectly ordinary chipset.
    let subsystem = if identity.kind == HeaderKind::Endpoint {
        Endpoint::read(&*space, address)?.subsystem
    } else {
        0
    };
    let mut found_transport = None;
    let mut host_visible = None;
    if let Some(kind) = virtio_pci::device_type(&identity, subsystem)
        && let Some(transport) = Transport::find(&*space, address)?
    {
        transport.verify(&regions)?;
        report.virtio += 1;
        found_transport = Some(transport);
        // The window a virtio GPU's host maps blob resources into
        // (`docs/GPU.md` §6.1). Taken only whole inside a BAR: the render
        // core maps its pages into programs, and a window that ran past its
        // BAR would hand them whatever lies beyond.
        if kind == TYPE_GPU {
            host_visible =
                SharedMemory::find(&*space, address, ferrix_virtio::gpu::SHM_ID_HOST_VISIBLE)?
                    .filter(|window| regions.iter().any(|region| window.fits(region)));
        }
        if kind == TYPE_ENTROPY {
            match virtio::entropy(
                space,
                address,
                &transport,
                &regions,
                msix.as_ref(),
                reserved,
            )? {
                virtio::Entropy::Read {
                    bytes,
                    polled,
                    out_of_domain,
                } => {
                    report.entropy_bytes = report.entropy_bytes.saturating_add(bytes);
                    match polled {
                        None => report.entropy_by_interrupt += 1,
                        Some(why) => report.entropy_polled = Some(why),
                    }
                    match out_of_domain {
                        Some(Ok(faulted)) => {
                            report.out_of_domain_faulted += 1;
                            report.out_of_domain_completed =
                                faulted.completed.or(report.out_of_domain_completed);
                        }
                        Some(Err(why)) => report.out_of_domain_skip = Some(why),
                        None => {}
                    }
                }
                virtio::Entropy::Skipped(why) => {
                    report.entropy_skipped += 1;
                    report.entropy_skip = Some(why);
                }
            }
        }
    }
    if identity.vendor == NVIDIA_VENDOR {
        describe_nvidia(space, address, &identity, &regions, msix.as_ref(), decoding)?;
    }
    Ok((regions, msix, decoding, found_transport, host_visible))
}

/// NVIDIA's PCI vendor ID.
const NVIDIA_VENDOR: u16 = 0x10DE;

/// The extended capability ID of Resizable BAR (PCIe base 6.0 §7.8.6).
const EXTENDED_ID_RESIZABLE_BAR: u16 = 0x0015;

/// The NVIDIA feasibility probe (`docs/NVIDIA.md` §2.3): say what a driver
/// for an NVIDIA function would be handed -- its identity, every BAR with its
/// width, size and placement, its capabilities, and whether Resizable BAR
/// lets its aperture into video memory grow. Reads only; writes nothing.
fn describe_nvidia(
    space: &Space,
    address: Address,
    identity: &ferrix_pci::header::Identity,
    regions: &[Region],
    msix: Option<&(Capability, MsiX)>,
    decoding: bool,
) -> Result<(), Failure> {
    use core::fmt::Write as _;
    crate::println!(
        "  nvidia   {address}: {:04x}:{:04x} rev {:02x} class {:02x}{:02x}{:02x}, memory decoding {}",
        identity.vendor,
        identity.device,
        identity.revision,
        identity.class.base,
        identity.class.sub,
        identity.class.interface,
        if decoding { "on" } else { "off" },
    );
    for region in regions {
        match region.bar {
            bar::Bar::Memory {
                address: at,
                wide,
                prefetchable,
            } => crate::println!(
                "  nvidia   {address}: BAR{} memory {}-bit{} {} KiB at {at:#x}",
                region.index,
                if wide { 64 } else { 32 },
                if prefetchable { " prefetchable" } else { "" },
                region.size / 1024,
            ),
            bar::Bar::Io { port } => crate::println!(
                "  nvidia   {address}: BAR{} I/O {} bytes at port {port:#x}",
                region.index,
                region.size,
            ),
        }
    }
    let mut standard = alloc::string::String::new();
    for capability in Capabilities::new(space, address) {
        let capability = capability?;
        let _ = write!(standard, " {:02x}@{:#x}", capability.id, capability.offset);
    }
    let mut extended = alloc::string::String::new();
    let mut resizable = None;
    for capability in ExtendedCapabilities::new(space, address) {
        let capability = capability?;
        if capability.id == EXTENDED_ID_RESIZABLE_BAR {
            resizable = Some(capability.offset);
        }
        let _ = write!(extended, " {:04x}@{:#x}", capability.id, capability.offset);
    }
    crate::println!("  nvidia   {address}: capabilities{standard}; extended{extended}");
    if let Some((_, table)) = msix {
        crate::println!(
            "  nvidia   {address}: MSI-X {} vectors, table in BAR{} at {:#x}",
            table.table_size,
            table.table.bar,
            table.table.offset,
        );
    }
    // The CPU's path to the registers: NV_PMC_BOOT_0, at BAR0's start, names
    // the architecture and implementation, and reading it changes nothing.
    // NV_PMC_BOOT_42 at 0xA00 does the same for chips that outgrew BOOT_0.
    if decoding
        && let Some(region) = regions.iter().find(|region| region.index == 0)
        && let Ok(virt) = vmap::map_device(region.bar.address(), 0x1000)
    {
        let registers = Mmio::at(virt);
        crate::println!(
            "  nvidia   {address}: BAR0 read: NV_PMC_BOOT_0 {:#010x}, NV_PMC_BOOT_42 {:#010x}",
            registers.read32(0),
            registers.read32(0xA00),
        );
        let _ = vmap::unmap_device(virt);
    }
    if let Some(at) = resizable {
        // Each entry: a capability register of supported sizes (bit n + 4 is
        // 2^n MiB) and a control register (BAR index in bits 2:0, the
        // number of entries in bits 7:5 of the first, the size in 13:8).
        let first = space.read32(address, at + 8);
        let entries = ((first >> 5) & 0x7).max(1);
        for entry in 0..entries {
            let base = at + 4 + 8 * entry as u16;
            let supported = space.read32(address, base);
            let control = space.read32(address, base + 4);
            crate::println!(
                "  nvidia   {address}: resizable BAR{}: now {} MiB, sizes {:#x} (bit n+4 = 2^n MiB), more {:#x}",
                control & 0x7,
                1u64 << ((control >> 8) & 0x3F),
                supported,
                control >> 16,
            );
        }
    }
    Ok(())
}
