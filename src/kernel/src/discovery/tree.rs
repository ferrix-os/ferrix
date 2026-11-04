//! The device tree's `virtio,mmio` nodes, as a [`Finder`]; and generic
//! publication of every other node the kernel does not use
//! (`docs/DRIVER-MODEL.md` §3), with the kernel-owned set it is sealed
//! against.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use core::fmt;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_bootinfo::{BootView, PAGE_SIZE};
use ferrix_fdt::{Fdt, GicInterrupt, Node, VIRTIO_MMIO_COMPATIBLE, identity};

use crate::device::{self, DeviceNode, Location, Reserved};
use crate::discovery::description::{self, Description};
use crate::discovery::finder::{Context, Failed, Finder, OutOfMemory};
use crate::fallible::{self, AllocError};
use crate::sync::SpinLock;

/// Every `virtio,mmio` node in the device tree, on a machine without ACPI.
///
/// Its vectors are decoded only when the tree's interrupt controller takes
/// three-cell GIC specifiers, which is every tree Ferrix boots with; a node's
/// `interrupt-parent` is not followed, so a machine with a second interrupt
/// controller would need that first. A vector is taken only if it is a shared
/// peripheral interrupt that no kernel handler and no earlier node holds. The
/// first that fails ends the node's list, as a bad specifier does, because a
/// driver asks for its interrupts by position.
#[derive(Debug)]
pub struct VirtioMmio {
    /// The tree, on a machine it describes.
    tree: Option<Fdt<'static>>,
    /// Whether `find` ran out of memory adding a node.
    out_of_memory: bool,
}

impl VirtioMmio {
    /// The finder for `view`'s machine: none to find without a device tree
    /// it is read by.
    pub fn new(view: &BootView<'_>) -> Self {
        let tree = match description::of(view) {
            Description::Tree(tree) => Some(tree),
            _ => None,
        };
        VirtioMmio {
            tree,
            out_of_memory: false,
        }
    }
}

impl Finder for VirtioMmio {
    fn name(&self) -> &'static str {
        "tree"
    }

    fn find(&mut self, cx: &mut Context<'_>, nodes: &mut Vec<DeviceNode>) -> Result<(), Failed> {
        let Some(tree) = &self.tree else {
            return Ok(());
        };
        let gic = tree
            .interrupt_controller()
            .is_some_and(|controller| controller.interrupt_cells() == Some(3));

        for found in tree.compatible_nodes(VIRTIO_MMIO_COMPATIBLE) {
            let Some(region) = found.reg().next() else {
                continue;
            };
            let mut node = DeviceNode::empty(Location::VirtioMmio(region.address));
            node.mint(region.address, region.size, false, cx.reserved);
            if gic {
                add_lines(&mut node, found.gic_interrupts(), &mut cx.held);
            }
            fallible::try_push(nodes, node).map_err(|_| {
                self.out_of_memory = true;
                Failed
            })?;
        }
        Ok(())
    }

    fn failure(&self) -> Option<&dyn fmt::Display> {
        self.out_of_memory
            .then_some(&OutOfMemory as &dyn fmt::Display)
    }
}

/// Give `node` the tree's `interrupts` in order, each only if it is a shared
/// line no kernel handler and no earlier node holds. The first that is not
/// ends the list, and it and every one after it are withheld: a driver asks
/// for its interrupts by position.
fn add_lines(
    node: &mut DeviceNode,
    mut interrupts: impl Iterator<Item = GicInterrupt>,
    held: &mut BTreeSet<u32>,
) {
    for interrupt in interrupts.by_ref() {
        if !device::claim_line(interrupt.id, held) {
            node.withhold_lines(1);
            break;
        }
        node.add_line(&interrupt);
    }
    node.withhold_lines(interrupts.count());
}

// ---------------------------------------------------------------------------
// Generic device-tree nodes (`docs/DRIVER-MODEL.md` §3, step 1)
// ---------------------------------------------------------------------------

/// Something the kernel uses of the machine a device tree describes: a node,
/// by its [`Node::id`], or a physical range, `start..end`. A node the kernel
/// uses, or one whose registers overlap a range it uses, is kernel-owned and
/// never published.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum KernelUse {
    /// A node, by its id in the tree the loader handed over.
    Node(u32),
    /// Physical memory, `start..end`.
    Range(u64, u64),
}

/// Why [`claim_kernel_use`] refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ClaimRefused {
    /// The kernel-owned set is sealed: publication has begun.
    Sealed,
    /// No memory to record it.
    NoMemory,
    /// After the seal: a window no claim made before the seal covers.
    NotClaimed,
}

/// Every kernel use registered so far, in order.
static CLAIMS: SpinLock<Vec<KernelUse>> = SpinLock::new(Vec::new());

/// Whether the kernel-owned set is sealed (`L.discovery.8`).
static SEALED: AtomicBool = AtomicBool::new(false);

/// Register a use of the machine by a kernel subsystem: the one way into
/// the kernel-owned set (`docs/DRIVER-MODEL.md` §3, D3; `L.discovery.8`).
///
/// Every claim is made before the seal, which stage 10 sets once every
/// finder has run, so the set is fixed before anything is published. A
/// claim after it is refused, and the boot checks that it is.
///
/// # Errors
///
/// [`ClaimRefused::Sealed`] once sealed; [`ClaimRefused::NoMemory`].
#[cfg_attr(
    not(ferrix_personality = "user"),
    allow(
        dead_code,
        reason = "publication is personality=user's; a hybrid's check alone reaches this"
    )
)]
#[cfg_attr(not(ferrix_personality = "user"), allow(dead_code))]
pub(crate) fn claim_kernel_use(what: KernelUse) -> Result<(), ClaimRefused> {
    claim_in(&mut CLAIMS.lock(), SEALED.load(Ordering::Acquire), what)
}

/// [`claim_kernel_use`] on a set of claims: what the boot check runs on one
/// of its own in a kernel that publishes nothing.
pub(crate) fn claim_in(
    claims: &mut Vec<KernelUse>,
    sealed: bool,
    what: KernelUse,
) -> Result<(), ClaimRefused> {
    if sealed {
        return Err(ClaimRefused::Sealed);
    }
    fallible::try_push(claims, what).map_err(|_| ClaimRefused::NoMemory)
}

/// Whether a claim made before the seal covers physical `start..end`
/// whole: one range claim holding all of it, both rounded out to pages.
fn covered_by(claims: &[KernelUse], start: u64, end: u64) -> bool {
    let start = start - start % PAGE_SIZE;
    let end = end.checked_next_multiple_of(PAGE_SIZE).unwrap_or(u64::MAX);
    claims.iter().any(|&what| match what {
        KernelUse::Range(low, high) => {
            low - low % PAGE_SIZE <= start
                && end <= high.checked_next_multiple_of(PAGE_SIZE).unwrap_or(u64::MAX)
        }
        KernelUse::Node(_) => false,
    })
}

/// Whether a claim made before the seal covers physical `start..end`.
#[cfg_attr(
    not(ferrix_personality = "user"),
    allow(
        dead_code,
        reason = "publication is personality=user's; a hybrid's check alone reaches this"
    )
)]
#[cfg_attr(not(ferrix_personality = "user"), allow(dead_code))]
pub(crate) fn covered(start: u64, end: u64) -> bool {
    covered_by(&CLAIMS.lock(), start, end)
}

/// Use a kernel register window of physical `start..end`: the one way board
/// support maps a window (D3, `L.discovery.8`). Before the seal it is
/// claimed, so no generic node is published over it; after the seal it is
/// allowed only inside a range claimed before it, so a window the kernel
/// first maps later -- a pixel clock, a reboot's boot mode -- can never be a
/// page a generic node's driver also holds.
///
/// # Errors
///
/// [`ClaimRefused::NotClaimed`] after the seal for a window outside every
/// claim; [`ClaimRefused::NoMemory`].
#[allow(
    dead_code,
    reason = "a board's glue is its caller: B1-B3, not in this landing"
)]
pub(crate) fn use_window(start: u64, end: u64) -> Result<(), ClaimRefused> {
    use_window_in(
        &mut CLAIMS.lock(),
        SEALED.load(Ordering::Acquire),
        start,
        end,
    )
}

/// [`use_window`] on a set of claims, as [`claim_in`].
pub(crate) fn use_window_in(
    claims: &mut Vec<KernelUse>,
    sealed: bool,
    start: u64,
    end: u64,
) -> Result<(), ClaimRefused> {
    if sealed {
        return if covered_by(claims, start, end) {
            Ok(())
        } else {
            Err(ClaimRefused::NotClaimed)
        };
    }
    fallible::try_push(claims, KernelUse::Range(start, end)).map_err(|_| ClaimRefused::NoMemory)
}

/// Claim every device window early boot mapped -- the console, whichever
/// the command line or the tree named and the architecture brought up --
/// before the seal, on every machine (`L.discovery.10`).
///
/// # Errors
///
/// [`ClaimRefused`].
#[cfg(ferrix_personality = "user")]
pub(crate) fn claim_early_windows() -> Result<(), ClaimRefused> {
    crate::early::device_windows()
        .try_for_each(|(start, end)| claim_kernel_use(KernelUse::Range(start, end)))
}

/// Seal the kernel-owned set: from now on every [`claim_kernel_use`] is
/// refused. Idempotent.
#[cfg_attr(
    not(ferrix_personality = "user"),
    allow(
        dead_code,
        reason = "publication is personality=user's; a hybrid's check alone reaches this"
    )
)]
#[cfg_attr(not(ferrix_personality = "user"), allow(dead_code))]
pub(crate) fn seal() {
    let _claims = CLAIMS.lock();
    SEALED.store(true, Ordering::Release);
}

/// Whether the kernel-owned set is sealed.
#[cfg_attr(
    not(ferrix_personality = "user"),
    allow(
        dead_code,
        reason = "publication is personality=user's; a hybrid's check alone reaches this"
    )
)]
#[cfg_attr(not(ferrix_personality = "user"), allow(dead_code))]
pub(crate) fn sealed() -> bool {
    SEALED.load(Ordering::Acquire)
}

/// A copy of every registered use.
///
/// # Errors
///
/// [`AllocError`].
#[cfg_attr(
    not(ferrix_personality = "user"),
    allow(
        dead_code,
        reason = "publication is personality=user's; a hybrid's check alone reaches this"
    )
)]
#[cfg_attr(not(ferrix_personality = "user"), allow(dead_code))]
pub(crate) fn claims() -> Result<Vec<KernelUse>, AllocError> {
    fallible::try_to_vec(&CLAIMS.lock())
}

/// Register what the kernel itself reads `tree` for, and the ranges it
/// reserves, through [`claim_kernel_use`] (`L.discovery.10`). The nodes are
/// the ones the kernel's own subsystems find with the accessors they call:
/// the console `stdout-path` names, the interrupt controller, the
/// architected timer, PSCI, the `SMMUv3`s, `/cpus` (and each processor's
/// spin-table release address), `/memory`, `/reserved-memory` and
/// `/chosen`. The ranges are every reserved range that is not the memory
/// map's -- the device windows mapped when stage 10 began, the IOMMUs, the
/// ECAM windows, the console, the boot framebuffer -- and every device
/// window mapped now: a watchdog the kernel pets is one.
///
/// # Errors
///
/// [`ClaimRefused`], from the first claim that was refused.
#[cfg_attr(
    not(ferrix_personality = "user"),
    allow(
        dead_code,
        reason = "publication is personality=user's; a hybrid's check alone reaches this"
    )
)]
#[cfg_attr(not(ferrix_personality = "user"), allow(dead_code))]
pub(crate) fn claim_tree_uses(tree: &Fdt<'_>, reserved: &Reserved) -> Result<(), ClaimRefused> {
    claim_accessor_nodes(tree)?;
    claim_described_nodes(tree)?;
    for (start, end) in reserved.kernel_ranges() {
        claim_kernel_use(KernelUse::Range(start, end))?;
    }
    for (start, end) in crate::vmap::device_windows() {
        claim_kernel_use(KernelUse::Range(start, end))?;
    }
    Ok(())
}

/// Claim a node.
#[cfg_attr(
    not(ferrix_personality = "user"),
    allow(
        dead_code,
        reason = "publication is personality=user's; a hybrid's check alone reaches this"
    )
)]
#[cfg_attr(not(ferrix_personality = "user"), allow(dead_code))]
fn claim_node(node: Node<'_>) -> Result<(), ClaimRefused> {
    claim_kernel_use(KernelUse::Node(node.id()))
}

/// The nodes the kernel's accessors find: the console, the interrupt
/// controller, the architected timer, PSCI and the `SMMUv3`s.
#[cfg_attr(
    not(ferrix_personality = "user"),
    allow(
        dead_code,
        reason = "publication is personality=user's; a hybrid's check alone reaches this"
    )
)]
#[cfg_attr(not(ferrix_personality = "user"), allow(dead_code))]
fn claim_accessor_nodes(tree: &Fdt<'_>) -> Result<(), ClaimRefused> {
    let found = [
        tree.console(),
        tree.interrupt_controller()
            .map(|controller| controller.node),
        tree.timer(),
    ];
    found.into_iter().flatten().try_for_each(claim_node)?;
    tree.nodes()
        .filter(|candidate| {
            ferrix_fdt::PSCI_COMPATIBLES
                .iter()
                .any(|binding| candidate.is_compatible(binding))
        })
        .try_for_each(claim_node)?;
    tree.compatible_nodes(ferrix_fdt::SMMU_V3_COMPATIBLE)
        .try_for_each(claim_node)
}

/// The nodes that describe what the kernel runs on rather than a device:
/// `/cpus`, `/memory` (and every `device_type = "memory"`),
/// `/reserved-memory` and `/chosen`, and each processor's spin-table
/// release address.
#[cfg_attr(
    not(ferrix_personality = "user"),
    allow(
        dead_code,
        reason = "publication is personality=user's; a hybrid's check alone reaches this"
    )
)]
#[cfg_attr(not(ferrix_personality = "user"), allow(dead_code))]
fn claim_described_nodes(tree: &Fdt<'_>) -> Result<(), ClaimRefused> {
    for found in tree.nodes() {
        let top = found.depth == 1
            && matches!(
                found.base_name(),
                "cpus" | "memory" | "reserved-memory" | "chosen"
            );
        if top || found.device_type() == Some("memory") {
            claim_node(found)?;
        }
        let release = found
            .property("cpu-release-addr")
            .and_then(|address| address.as_u64());
        if let Some(release) = release {
            claim_kernel_use(KernelUse::Range(release, release.saturating_add(8)))?;
        }
    }
    Ok(())
}

/// Which nodes of a tree are kernel-owned, and which the kernel depends on,
/// derived from the registered uses alone (D3, D4; `L.discovery.10`,
/// `L.discovery.11`).
#[derive(Debug, Default)]
pub(crate) struct Ownership {
    /// Kernel-owned nodes, by id: each node used, each node under one, and
    /// each node whose registers' pages overlap a range used or a used
    /// node's registers.
    owned: BTreeSet<u32>,
    /// Nodes a kernel-owned node references, directly or through other
    /// nodes, as a clock, reset, power domain, pin configuration, supply or
    /// interconnect: `KERNEL_DEPENDS`.
    depends: BTreeSet<u32>,
    /// Every page of every kernel-owned node's registers, used, under a used
    /// node or overlapping one: no generic node is published over any (D7).
    pages: Vec<(u64, u64)>,
    /// A kernel-owned or depended-on node whose references could not all be
    /// read -- past the record's bound, or a provider that does not say how
    /// wide its specifiers are -- by its id: `KERNEL_DEPENDS` cannot be
    /// closed, so nothing of the tree is published (D4 fails closed).
    incomplete: Option<u32>,
}

impl Ownership {
    /// Derive it for `tree` from `uses`.
    ///
    /// # Errors
    ///
    /// [`AllocError`].
    pub(crate) fn derive(tree: &Fdt<'_>, uses: &[KernelUse]) -> Result<Self, AllocError> {
        let (used, mut ranges) = split(uses)?;
        // The used nodes' own registers are used too.
        let mut failed = Ok(());
        tree.for_each_lineage(|node, ancestors| {
            if !used.contains(&node.id()) {
                return ControlFlow::Continue(());
            }
            failed = fallible::try_extend(&mut ranges, pages_of(tree, &node, ancestors));
            stop_on(&failed)
        });
        failed?;
        let mut ownership = Ownership::default();
        tree.for_each_lineage(|node, ancestors| {
            let under = ancestors
                .iter()
                .any(|ancestor| ownership.owned.contains(&ancestor.id()));
            let overlaps = pages_of(tree, &node, ancestors)
                .any(|(start, end)| ranges.iter().any(|&(low, high)| start < high && low < end));
            if used.contains(&node.id()) || under || overlaps {
                failed = fallible::insert_into_set(&mut ownership.owned, node.id()).map(|_| ());
            }
            stop_on(&failed)
        });
        failed?;
        // Every page of every kernel-owned node, however it came to be one.
        tree.for_each_lineage(|node, ancestors| {
            if ownership.owned.contains(&node.id()) {
                failed =
                    fallible::try_extend(&mut ownership.pages, pages_of(tree, &node, ancestors));
            }
            stop_on(&failed)
        });
        failed?;
        ownership.close(tree)?;
        Ok(ownership)
    }

    /// Set `KERNEL_DEPENDS` on every node a kernel-owned node depends on,
    /// following references through each node found: each node is looked
    /// at once, so the walk ends within the tree's node count.
    fn close(&mut self, tree: &Fdt<'_>) -> Result<(), AllocError> {
        let mut queue: Vec<u32> = fallible::try_collect(self.owned.iter().copied())?;
        let mut looked: BTreeSet<u32> = BTreeSet::new();
        while let Some(id) = queue.pop() {
            if !fallible::insert_into_set(&mut looked, id)? {
                continue;
            }
            let Some(node) = tree.node_by_id(id) else {
                continue;
            };
            let mut failed = None;
            tree.for_each_reference(&node, |reference| {
                // A reference the walk could not read whole leaves the
                // closure short: fail closed for the whole tree.
                let unread = reference.cut
                    || (reference.missing
                        && (reference.target.is_some() || reference.unread_after));
                if unread && self.incomplete.is_none() {
                    self.incomplete = Some(id);
                }
                if !reference.kind.kernel_dependency() {
                    return ControlFlow::Continue(());
                }
                let step = [reference.provider, reference.target]
                    .into_iter()
                    .flatten()
                    .try_for_each(|named| self.depend(named.id(), &mut queue));
                match step {
                    Ok(()) => ControlFlow::Continue(()),
                    Err(error) => {
                        failed = Some(error);
                        ControlFlow::Break(())
                    }
                }
            });
            if let Some(error) = failed {
                return Err(error);
            }
        }
        Ok(())
    }

    /// Flag `id` `KERNEL_DEPENDS`, and look at its own references next.
    fn depend(&mut self, id: u32, queue: &mut Vec<u32>) -> Result<(), AllocError> {
        let _ = fallible::insert_into_set(&mut self.depends, id)?;
        fallible::try_push(queue, id)
    }

    /// Whether the node is kernel-owned.
    pub(crate) fn owns(&self, id: u32) -> bool {
        self.owned.contains(&id)
    }

    /// Whether the kernel depends on the node (`KERNEL_DEPENDS`).
    pub(crate) fn depends_on(&self, id: u32) -> bool {
        self.depends.contains(&id)
    }

    /// Whether physical `start..end` shares a page with a kernel-owned
    /// node's registers.
    pub(crate) fn owns_pages(&self, start: u64, end: u64) -> bool {
        self.pages
            .iter()
            .any(|&(low, high)| start < high && low < end)
    }

    /// The kernel-owned or depended-on node whose references could not all
    /// be read, if one could not.
    pub(crate) const fn incomplete(&self) -> Option<u32> {
        self.incomplete
    }

    /// How many nodes are kernel-owned.
    #[cfg_attr(
        not(ferrix_personality = "user"),
        allow(
            dead_code,
            reason = "publication is personality=user's; a hybrid's check alone reaches this"
        )
    )]
    #[cfg_attr(not(ferrix_personality = "user"), allow(dead_code))]
    pub(crate) fn owned(&self) -> usize {
        self.owned.len()
    }
}

/// The nodes and the `start..end` ranges a set of uses names.
type Used = (BTreeSet<u32>, Vec<(u64, u64)>);

/// The uses' nodes and ranges, apart.
fn split(uses: &[KernelUse]) -> Result<Used, AllocError> {
    let mut used = BTreeSet::new();
    let mut ranges = Vec::new();
    for &what in uses {
        match what {
            KernelUse::Node(id) => {
                let _ = fallible::insert_into_set(&mut used, id)?;
            }
            KernelUse::Range(start, end) => fallible::try_push(&mut ranges, (start, end))?,
        }
    }
    Ok((used, ranges))
}

/// A walk goes on while nothing failed.
fn stop_on<E>(outcome: &Result<(), E>) -> ControlFlow<()> {
    if outcome.is_ok() {
        ControlFlow::Continue(())
    } else {
        ControlFlow::Break(())
    }
}

/// The whole pages each of `node`'s `reg` ranges lies in, translated to the
/// processor's addresses, as `start..end`; a range that does not translate
/// is left out.
fn pages_of<'a, 'n>(
    tree: &'n Fdt<'a>,
    node: &'n Node<'a>,
    ancestors: &'n [Node<'a>],
) -> impl Iterator<Item = (u64, u64)> + 'n {
    node.reg().filter_map(move |region| {
        let phys = tree.translate(node, ancestors, region.address)?;
        let end = phys.checked_add(region.size.max(1))?;
        let end = end.checked_next_multiple_of(PAGE_SIZE)?;
        Some((phys - phys % PAGE_SIZE, end))
    })
}

/// What the generic walk published and left alone, for its boot line.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct GenericReport {
    /// Nodes published.
    pub(crate) published: usize,
    /// Of those, flagged `KERNEL_DEPENDS`.
    pub(crate) depends: usize,
    /// Enabled nodes with a `compatible` left alone as kernel-owned.
    pub(crate) owned: usize,
    /// Left alone because a `TREE_*` binding or the virtio finder has them
    /// (`L.device.65`).
    pub(crate) bound: usize,
    /// Left alone as a bus controller's children, in its subtree.
    pub(crate) children: usize,
    /// Register ranges not minted.
    pub(crate) registers_withheld: usize,
    /// Interrupts not minted: not shared peripheral interrupts, taken, or not
    /// on the primary controller.
    pub(crate) interrupts_withheld: usize,
    /// Of those, private or software-generated lines (`L.device.62`).
    pub(crate) private_refused: usize,
    /// Left alone because their identity record could not be written.
    pub(crate) unrecorded: usize,
    /// The node whose references could not all be read, when publication
    /// stopped on it (D4 fails closed): nothing was published.
    pub(crate) stopped: Option<u32>,
}

/// The most generic nodes one tree publishes. A tree with more has the
/// rest left alone.
pub(crate) const MAX_GENERIC: usize = 256;

/// The primary interrupt controller, as generic publication mints against
/// it: its id, if its specifiers are the three cells a GIC's are.
fn primary_controller(tree: &Fdt<'_>) -> Option<u32> {
    tree.interrupt_controller()
        .filter(|gic| gic.interrupt_cells() == Some(3))
        .map(|gic| gic.node.id())
}

/// Publish every enabled node of `tree` that has a `compatible`, sits on
/// memory-mapped buses and is not kernel-owned as a generic node
/// (`docs/DRIVER-MODEL.md` §3; `H.DEV.7`), after the nodes `nodes` already
/// holds -- the virtio finder's and the boards' -- none of which it
/// publishes again (`L.device.65`).
///
/// # Errors
///
/// [`AllocError`].
pub(crate) fn find_generic(
    tree: &Fdt<'_>,
    ownership: &Ownership,
    reserved: &Reserved,
    held: &mut BTreeSet<u32>,
    nodes: &mut Vec<DeviceNode>,
    say: bool,
) -> Result<GenericReport, AllocError> {
    let mut report = GenericReport::default();
    if let Some(node) = ownership.incomplete() {
        report.stopped = Some(node);
        return Ok(report);
    }
    let earlier: Vec<(u64, u64)> = fallible::try_collect(nodes.iter().flat_map(|node| {
        node.apertures().iter().map(|aperture| {
            (
                aperture.phys(),
                aperture.phys().saturating_add(aperture.len()),
            )
        })
    }))?;
    let walk = Walk {
        tree,
        ownership,
        reserved,
        primary: primary_controller(tree),
        say,
    };
    let mut failed = Ok(());
    tree.for_each_lineage(|node, ancestors| {
        if !walk.publishes(&node, ancestors, &earlier, &mut report) {
            return ControlFlow::Continue(());
        }
        if report.published >= MAX_GENERIC {
            return ControlFlow::Break(());
        }
        failed = match walk.node(&node, ancestors, held, &mut report) {
            Ok(Some(made)) => fallible::try_push(nodes, made).map(|()| report.published += 1),
            Ok(None) => {
                report.unrecorded += 1;
                Ok(())
            }
            Err(error) => Err(error),
        };
        stop_on(&failed)
    });
    failed.map(|()| report)
}

/// A node's path, from its ancestors, for a boot line.
struct PathOf<'p, 'a> {
    node: &'p Node<'a>,
    ancestors: &'p [Node<'a>],
}

impl fmt::Display for PathOf<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for component in self
            .ancestors
            .iter()
            .skip(1)
            .chain(core::iter::once(self.node))
        {
            write!(f, "/{}", component.name)?;
        }
        Ok(())
    }
}

/// Say that generic publication stopped on node `id` of `tree`, by its path.
pub(crate) fn say_stopped(tree: &Fdt<'_>, id: u32) {
    tree.for_each_lineage(|node, ancestors| {
        if node.id() != id {
            return ControlFlow::Continue(());
        }
        crate::console::println!(
            "  generic  nothing published: the references of {}, kernel-owned or depended on, \
             could not all be read, so KERNEL_DEPENDS could not be closed",
            PathOf {
                node: &node,
                ancestors
            }
        );
        ControlFlow::Break(())
    });
}

/// What every node of the generic walk is made against.
struct Walk<'w, 'a> {
    tree: &'w Fdt<'a>,
    ownership: &'w Ownership,
    reserved: &'w Reserved,
    /// The primary interrupt controller's id ([`primary_controller`]).
    primary: Option<u32>,
    /// Whether each node's verdict is said on its boot line: the machine's
    /// tree, not a check's made-up one.
    say: bool,
}

impl<'a> Walk<'_, 'a> {
    /// Say `node`'s verdict on its boot line, on the machine's tree.
    fn say_verdict(&self, verdict: &str, node: &Node<'a>, ancestors: &[Node<'a>]) {
        if !self.say {
            return;
        }
        let depends = if self.ownership.depends_on(node.id()) {
            " KERNEL_DEPENDS"
        } else {
            ""
        };
        crate::console::println!(
            "  generic  {verdict} {}{depends}",
            PathOf { node, ancestors }
        );
    }

    /// Whether `node` is one generic publication publishes: enabled, with
    /// its ancestors, and with a `compatible`, on memory-mapped buses, not
    /// kernel-owned, and holding no register an earlier finder's node in
    /// `earlier` holds. What it left alone, and why, goes in `report`.
    fn publishes(
        &self,
        node: &Node<'a>,
        ancestors: &[Node<'a>],
        earlier: &[(u64, u64)],
        report: &mut GenericReport,
    ) -> bool {
        if node.depth == 0
            || node.property("compatible").is_none()
            || !node.is_enabled()
            || !ancestors.iter().all(Node::is_enabled)
        {
            return false;
        }
        if !Fdt::memory_mapped(ancestors) {
            report.children += 1;
            return false;
        }
        let owned_page = pages_of(self.tree, node, ancestors)
            .any(|(start, end)| self.ownership.owns_pages(start, end));
        if self.ownership.owns(node.id()) || owned_page {
            report.owned += 1;
            self.say_verdict("kernel-owned", node, ancestors);
            return false;
        }
        // A node whose registers an earlier finder's node holds is that
        // binding's, or the virtio finder's (D10, `L.device.65`).
        let held_earlier = pages_of(self.tree, node, ancestors)
            .any(|(start, end)| earlier.iter().any(|&(low, high)| start < high && low < end));
        let bound = node.is_compatible(VIRTIO_MMIO_COMPATIBLE) || held_earlier;
        report.bound += usize::from(bound);
        if !bound {
            self.say_verdict("published", node, ancestors);
        }
        !bound
    }

    /// One generic node: its register ranges minted by whole pages, its
    /// interrupts on the primary controller minted while each is a shared
    /// line no one holds, and its identity record.
    fn node(
        &self,
        node: &Node<'a>,
        ancestors: &[Node<'a>],
        held: &mut BTreeSet<u32>,
        report: &mut GenericReport,
    ) -> Result<Option<DeviceNode>, AllocError> {
        let tree = self.tree;
        let mut flags = 0;
        let first = node
            .reg()
            .next()
            .and_then(|region| tree.translate(node, ancestors, region.address));
        let mut made = DeviceNode::generic(first.unwrap_or(u64::from(node.id())));
        for region in node.reg() {
            let minted = tree
                .translate(node, ancestors, region.address)
                .is_some_and(|phys| made.mint_generic(phys, region.size, self.reserved));
            if !minted {
                report.registers_withheld += 1;
                flags |= identity::FLAG_REGISTERS_WITHHELD;
            }
        }
        let named = node.interrupts().map_or(0, |cells| cells.count() / 3);
        let on_primary = self.primary.is_some()
            && tree
                .interrupt_parent(node, ancestors)
                .map(|parent| parent.id())
                == self.primary;
        let mut minted = 0;
        if on_primary {
            for interrupt in node.gic_interrupts() {
                // Only a shared peripheral interrupt no kernel handler and
                // no other node holds (D8, `L.device.62`): `claim_line` is
                // the one rule every tree node's lines are minted by. The
                // first refused ends the list, since a driver asks for its
                // interrupts by position.
                if !device::claim_line(interrupt.id, held) {
                    report.private_refused += usize::from(interrupt.id < FIRST_SHARED_LINE);
                    break;
                }
                made.add_line(&interrupt);
                minted += 1;
            }
        }
        if minted != named {
            made.withhold_lines(named - minted);
            report.interrupts_withheld += named - minted;
            flags |= identity::FLAG_INTERRUPTS_WITHHELD;
        }
        if self.ownership.depends_on(node.id()) {
            flags |= identity::FLAG_KERNEL_DEPENDS;
            report.depends += 1;
        }
        let mut record = fallible::try_filled(0u8, identity::RECORD_BYTES)?;
        // A node with no record is not a generic node: withheld, never
        // published as one with an empty identity. With a buffer of
        // RECORD_BYTES encode always writes at least its header.
        let Some(encoded) = identity::encode(tree, node, ancestors, flags, &mut record) else {
            return Ok(None);
        };
        record.truncate(encoded.length);
        made.set_identity(record);
        Ok(Some(made))
    }
}

/// The first GIC identifier of a shared peripheral interrupt: below it are
/// the software-generated and private lines, which `claim_line` never mints
/// (D8); the walk counts the ones it refused.
const FIRST_SHARED_LINE: u32 = 32;
