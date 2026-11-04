//! Generic device-tree nodes: what the kernel hands a ring-3 driver about a
//! node it publishes without knowing its binding (`docs/DRIVER-MODEL.md` §3).
//!
//! The kernel publishes every enabled node that has a `compatible` and that
//! it does not use itself. It interprets none of the node's cells: what a
//! driver and `devmgr` need -- the `compatible` list, the node's path, its
//! references to other nodes and the subtree under a bus controller -- is
//! copied into an [`identity`](crate::identity) record as data. This module
//! is the parsing half: walking the tree with each node's ancestors,
//! translating a `reg` address up through the buses' `ranges`, and reading a
//! node's references as `(provider node, specifier cells)` pairs.
//!
//! # Bounds (D9, ledger 4475)
//!
//! Every walk here is over a tree [`Fdt::parse`] accepted, so it ends within
//! the token budget [`MAX_TOKENS`](crate::MAX_TOKENS). On top of that:
//!
//! * a node deeper than [`LINEAGE_DEPTH`] is never visited by
//!   [`Fdt::for_each_lineage`], so it is never published;
//! * a node's references are at most [`MAX_REFERENCES`], each with at most
//!   [`MAX_SPECIFIER_CELLS`] cells; past either, the reference is reported
//!   cut short, never read further;
//! * a `ranges` translation is at most [`LINEAGE_DEPTH`] hops, and an
//!   address wider than two cells is not translated at all.
//!
//! A phandle that names no node, or a node that is disabled, is a reference
//! still: one marked [`Reference::missing`] or [`Reference::disabled`], so the
//! consumer is published and `devmgr` can name what it lacks.

use core::ops::ControlFlow;

use crate::{Fdt, Node, Property, read_cells, u32_at};

/// How deep a node may sit and still be visited by [`Fdt::for_each_lineage`]:
/// sixteen levels, the root being zero. Real trees nest four or five deep.
pub const LINEAGE_DEPTH: usize = 16;

/// The most references one node's are read to.
pub const MAX_REFERENCES: usize = 64;

/// The most specifier cells one reference carries. A provider that declares
/// more is reported as cut short.
pub const MAX_SPECIFIER_CELLS: usize = 8;

/// What a reference names: one of the kinds `docs/DRIVER-MODEL.md` §3 lists.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ReferenceKind {
    /// `clocks`, specifier width `#clock-cells`.
    Clock = 1,
    /// `resets`, `#reset-cells`.
    Reset = 2,
    /// `power-domains`, `#power-domain-cells`.
    PowerDomain = 3,
    /// `pinctrl-<n>`: a list of phandles to pin configurations, no cells.
    PinControl = 4,
    /// `<name>-supply`: one phandle to a regulator, no cells.
    Supply = 5,
    /// `interconnects`, `#interconnect-cells`.
    Interconnect = 6,
    /// `interrupts-extended`, `#interrupt-cells`.
    Interrupt = 7,
    /// `gpios` or `<name>-gpios`, `#gpio-cells`.
    Gpio = 8,
    /// `dmas`, `#dma-cells`.
    Dma = 9,
    /// `mboxes`, `#mbox-cells`.
    Mailbox = 10,
    /// `iommus`, `#iommu-cells`.
    Iommu = 11,
}

impl ReferenceKind {
    /// The kind a property of this name is, if it is a reference at all.
    #[must_use]
    pub fn of(name: &str) -> Option<ReferenceKind> {
        Some(match name {
            "clocks" => ReferenceKind::Clock,
            "resets" => ReferenceKind::Reset,
            "power-domains" => ReferenceKind::PowerDomain,
            "interconnects" => ReferenceKind::Interconnect,
            "interrupts-extended" => ReferenceKind::Interrupt,
            "dmas" => ReferenceKind::Dma,
            "mboxes" => ReferenceKind::Mailbox,
            "iommus" => ReferenceKind::Iommu,
            "gpios" => ReferenceKind::Gpio,
            _ if name.ends_with("-gpios") || name.ends_with("-gpio") => ReferenceKind::Gpio,
            _ if name.len() > "-supply".len() && name.ends_with("-supply") => ReferenceKind::Supply,
            _ if name.strip_prefix("pinctrl-").is_some_and(|state| {
                !state.is_empty() && state.bytes().all(|b| b.is_ascii_digit())
            }) =>
            {
                ReferenceKind::PinControl
            }
            _ => return None,
        })
    }

    /// The provider's property that says how many cells a specifier takes;
    /// `None` for the kinds whose entries are bare phandles.
    #[must_use]
    pub const fn cells_property(self) -> Option<&'static str> {
        match self {
            ReferenceKind::Clock => Some("#clock-cells"),
            ReferenceKind::Reset => Some("#reset-cells"),
            ReferenceKind::PowerDomain => Some("#power-domain-cells"),
            ReferenceKind::Interconnect => Some("#interconnect-cells"),
            ReferenceKind::Interrupt => Some("#interrupt-cells"),
            ReferenceKind::Gpio => Some("#gpio-cells"),
            ReferenceKind::Dma => Some("#dma-cells"),
            ReferenceKind::Mailbox => Some("#mbox-cells"),
            ReferenceKind::Iommu => Some("#iommu-cells"),
            ReferenceKind::PinControl | ReferenceKind::Supply => None,
        }
    }

    /// Whether a kernel-owned node's reference of this kind makes the node it
    /// names one the kernel depends on (D4): clocks, resets, power domains,
    /// pin control, supplies and interconnects. A device the kernel drives
    /// stops if any of those is gated under it.
    #[must_use]
    pub const fn kernel_dependency(self) -> bool {
        matches!(
            self,
            ReferenceKind::Clock
                | ReferenceKind::Reset
                | ReferenceKind::PowerDomain
                | ReferenceKind::PinControl
                | ReferenceKind::Supply
                | ReferenceKind::Interconnect
        )
    }

    /// The kind numbered `value` in an identity record.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<ReferenceKind> {
        Some(match value {
            1 => ReferenceKind::Clock,
            2 => ReferenceKind::Reset,
            3 => ReferenceKind::PowerDomain,
            4 => ReferenceKind::PinControl,
            5 => ReferenceKind::Supply,
            6 => ReferenceKind::Interconnect,
            7 => ReferenceKind::Interrupt,
            8 => ReferenceKind::Gpio,
            9 => ReferenceKind::Dma,
            10 => ReferenceKind::Mailbox,
            11 => ReferenceKind::Iommu,
            _ => return None,
        })
    }
}

/// One reference of a node to another: the property it is in, which entry
/// of it, the node it names and the specifier cells, uninterpreted.
#[derive(Clone, Copy, Debug)]
pub struct Reference<'a> {
    /// What it names.
    pub kind: ReferenceKind,
    /// The property, as the tree spells it: `clocks`, `vs-supply`, `pinctrl-0`.
    pub property: &'a str,
    /// Its place in that property, from zero.
    pub index: u16,
    /// The phandle as written.
    pub phandle: u32,
    /// The node the phandle names, when one does.
    pub target: Option<Node<'a>>,
    /// The node that serves it: the target, except for a pin configuration,
    /// whose provider is the pin controller above it.
    pub provider: Option<Node<'a>>,
    /// The specifier's cells, big-endian as the tree holds them; empty for a
    /// bare phandle.
    pub cells: &'a [u8],
    /// Whether the phandle names no node, or a provider that does not say how
    /// wide its specifiers are: the rest of the property is not read.
    pub missing: bool,
    /// Whether the node it names is disabled.
    pub disabled: bool,
    /// Whether the specifier was wider than [`MAX_SPECIFIER_CELLS`], or the
    /// node's references ran past [`MAX_REFERENCES`] here.
    pub cut: bool,
    /// Whether its property holds words after it that were not read: a
    /// missing provider left their widths unknown.
    pub unread_after: bool,
}

impl<'a> Fdt<'a> {
    /// The node whose [`Node::id`] is `id`.
    #[must_use]
    pub fn node_by_id(&self, id: u32) -> Option<Node<'a>> {
        self.nodes().find(|node| node.id() == id)
    }

    /// Visit every node no deeper than [`LINEAGE_DEPTH`], in tree order, with
    /// its ancestors from the root down to its parent.
    ///
    /// A node deeper than the limit is not visited, and neither is anything
    /// under it. `visit` may stop the walk.
    pub fn for_each_lineage(
        &self,
        mut visit: impl FnMut(Node<'a>, &[Node<'a>]) -> ControlFlow<()>,
    ) {
        let mut stack: [Option<Node<'a>>; LINEAGE_DEPTH] = [None; LINEAGE_DEPTH];
        let mut ancestors: [Node<'a>; LINEAGE_DEPTH] = [Node::placeholder(); LINEAGE_DEPTH];
        for node in self.nodes() {
            if node.depth >= LINEAGE_DEPTH {
                continue;
            }
            if let Some(slot) = stack.get_mut(node.depth) {
                *slot = Some(node);
            }
            let mut count = 0;
            for (level, entry) in stack.iter().enumerate().take(node.depth) {
                let Some(ancestor) = entry else { break };
                // A slot is the node's own ancestor only if it sits at its
                // level; the walk is depth first, so it does.
                if ancestor.depth != level {
                    break;
                }
                if let Some(out) = ancestors.get_mut(count) {
                    *out = *ancestor;
                    count += 1;
                }
            }
            if count != node.depth {
                continue;
            }
            if visit(node, ancestors.get(..count).unwrap_or(&[])).is_break() {
                return;
            }
        }
    }

    /// `address` of `node`'s `reg`, as the processor sees it: translated up
    /// through each ancestor's `ranges`, from the parent to the root's child.
    ///
    /// `None` when an ancestor below the root has no `ranges` -- the node is
    /// on a bus that is not memory-mapped, an I2C controller's child, say --
    /// when no entry of a `ranges` covers the address, or when an address or
    /// size is wider than two cells.
    #[must_use]
    pub fn translate(&self, node: &Node<'a>, ancestors: &[Node<'a>], address: u64) -> Option<u64> {
        if node.address_cells > 2 {
            return None;
        }
        let mut address = address;
        // The cells the address is in: the node's parent's declaration.
        let mut child_address_cells = node.address_cells;
        let mut child_size_cells = node.size_cells;
        for ancestor in ancestors.iter().skip(1).rev() {
            let ranges = ancestor.property("ranges")?;
            let parent_address_cells = ancestor.address_cells;
            if ranges.value.is_empty() {
                child_address_cells = ancestor.address_cells;
                child_size_cells = ancestor.size_cells;
                continue;
            }
            if child_address_cells > 2 || parent_address_cells > 2 || child_size_cells > 2 {
                return None;
            }
            address = translate_one(
                ranges.value,
                address,
                (child_address_cells, parent_address_cells, child_size_cells),
            )?;
            child_address_cells = ancestor.address_cells;
            child_size_cells = ancestor.size_cells;
        }
        Some(address)
    }

    /// Whether `node`, with these `ancestors`, sits on memory-mapped buses
    /// all the way up: every ancestor below the root has `ranges`. A node
    /// that does not is part of the nearest such ancestor's subtree, for that
    /// ancestor's driver to read (`docs/DRIVER-MODEL.md` §5).
    #[must_use]
    pub fn memory_mapped(ancestors: &[Node<'a>]) -> bool {
        ancestors
            .iter()
            .skip(1)
            .all(|ancestor| ancestor.property("ranges").is_some())
    }

    /// The node `node`'s interrupts go to: its own `interrupt-parent`, or the
    /// nearest ancestor's. `None` when none names a node.
    #[must_use]
    pub fn interrupt_parent(&self, node: &Node<'a>, ancestors: &[Node<'a>]) -> Option<Node<'a>> {
        let phandle = node
            .property("interrupt-parent")
            .or_else(|| {
                ancestors
                    .iter()
                    .rev()
                    .find_map(|ancestor| ancestor.property("interrupt-parent"))
            })?
            .as_u32()?;
        self.node_by_phandle(phandle)
    }

    /// Visit each of `node`'s references, in property order, at most
    /// [`MAX_REFERENCES`]. A pin configuration's provider is its
    /// controller: the nearest ancestor of the configuration node that has a
    /// `compatible`.
    pub fn for_each_reference(
        &self,
        node: &Node<'a>,
        mut visit: impl FnMut(Reference<'a>) -> ControlFlow<()>,
    ) {
        let mut seen = 0usize;
        for property in node.properties() {
            let Some(kind) = ReferenceKind::of(property.name) else {
                continue;
            };
            if self
                .references_in(kind, &property, &mut seen, &mut visit)
                .is_break()
            {
                return;
            }
        }
    }

    /// The references in one property.
    fn references_in(
        &self,
        kind: ReferenceKind,
        property: &Property<'a>,
        seen: &mut usize,
        visit: &mut impl FnMut(Reference<'a>) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        let value = property.value;
        let mut offset = 0usize;
        let mut index: u16 = 0;
        while offset < value.len() {
            let Some(phandle) = u32_at(value, offset) else {
                return ControlFlow::Continue(());
            };
            offset = offset.saturating_add(4);
            let mut reference = Reference {
                kind,
                property: property.name,
                index,
                phandle,
                target: None,
                provider: None,
                cells: &[],
                missing: false,
                disabled: false,
                cut: false,
                unread_after: false,
            };
            if *seen >= MAX_REFERENCES {
                reference.cut = true;
                return visit(reference);
            }
            *seen += 1;
            let target = self.node_by_phandle(phandle);
            reference.target = target;
            let Some(target) = target else {
                // Its width is the missing node's to say: nothing after it
                // can be read.
                reference.missing = true;
                reference.unread_after = offset < value.len();
                return visit(reference);
            };
            reference.disabled = !target.is_enabled();
            reference.provider = Some(match kind {
                ReferenceKind::PinControl => self.controller_of(&target).unwrap_or(target),
                _ => target,
            });
            let width = match kind.cells_property() {
                None => 0,
                Some(name) => match target.property(name).and_then(|cells| cells.as_u32()) {
                    Some(width) => width as usize,
                    None => {
                        reference.missing = true;
                        reference.unread_after = offset < value.len();
                        return visit(reference);
                    }
                },
            };
            if width > MAX_SPECIFIER_CELLS {
                reference.cut = true;
                return visit(reference);
            }
            let end = offset.saturating_add(width.saturating_mul(4));
            let Some(cells) = value.get(offset..end) else {
                reference.cut = true;
                return visit(reference);
            };
            reference.cells = cells;
            offset = end;
            visit(reference)?;
            index = index.saturating_add(1);
        }
        ControlFlow::Continue(())
    }

    /// The nearest ancestor of `node` that has a `compatible`: a pin
    /// configuration's controller.
    fn controller_of(&self, node: &Node<'a>) -> Option<Node<'a>> {
        let mut found = None;
        let id = node.id();
        self.for_each_lineage(|candidate, ancestors| {
            if candidate.id() == id {
                found = ancestors
                    .iter()
                    .rev()
                    .find(|ancestor| ancestor.property("compatible").is_some())
                    .copied();
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        });
        found
    }
}

/// One step of a `ranges` translation: the entry of `ranges` whose child
/// range holds `address`, and the address in its parent's space. Cell counts
/// are `(child address, parent address, size)`, each at most two.
fn translate_one(ranges: &[u8], address: u64, cells: (u32, u32, u32)) -> Option<u64> {
    let (child, parent, size) = cells;
    let entry = (child + parent + size) as usize * 4;
    if entry == 0 {
        return None;
    }
    let mut offset = 0usize;
    while offset.checked_add(entry)? <= ranges.len() {
        let mut at = offset;
        let child_base = read_cells(ranges, &mut at, child)?;
        let parent_base = read_cells(ranges, &mut at, parent)?;
        let length = read_cells(ranges, &mut at, size)?;
        if address >= child_base && address - child_base < length {
            return parent_base.checked_add(address - child_base);
        }
        offset += entry;
    }
    None
}

impl Node<'_> {
    /// A name for the node unique within its tree: the structure block offset
    /// of its first property. Stable for the tree's life, and what an identity
    /// record names a provider by.
    #[must_use]
    pub const fn id(&self) -> u32 {
        self.properties as u32
    }

    /// A node of no tree, to fill an array before a walk overwrites it.
    pub(crate) const fn placeholder() -> Self {
        Node {
            name: "",
            depth: 0,
            address_cells: 0,
            size_cells: 0,
            structs: &[],
            strings: &[],
            properties: 0,
        }
    }
}
