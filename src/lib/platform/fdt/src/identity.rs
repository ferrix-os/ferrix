//! The identity record of a generic device-tree node: what the kernel
//! copies out of the tree for a node it publishes without knowing its
//! binding, and what `devmgr` and the node's driver read back
//! (`docs/DRIVER-MODEL.md` §3).
//!
//! One definition for both ends: the kernel writes a record with [`encode`],
//! `devmgr` and `ferrix-driver` read it with [`Record::parse`]. The kernel
//! interprets nothing it copies: the `compatible` strings, the path, each
//! reference's specifier cells and the subtree are the tree's bytes.
//!
//! # Layout
//!
//! Little-endian, at most [`RECORD_BYTES`] long, every section four-byte
//! aligned:
//!
//! ```text
//!  0 magic u32 (RECORD_MAGIC)   4 version u16 (1)   6 flags u16
//!  8 length u32 (the whole record)                 12 node u32 (Node::id)
//! 16 compatible: offset u16, length u16 (NUL-separated, as the tree has it)
//! 20 path: offset u16, length u16 (no NUL)
//! 24 references: offset u16, count u16
//! 28 names: offset u16, length u16 (the node's own `*-names` properties)
//! 32 subtree: offset u16, length u16 (the node's descendants)
//! 36 interrupt parent u32 (Node::id, 0 for none)
//!
//! a reference, 20 bytes and then its name and cells:
//!  0 kind u8   1 flags u8   2 cells u8   3 name length u8
//!  4 provider u32   8 target u32   12 phandle u32   16 index u16   18 0 u16
//! 20 name, padded to four   then cells x u32
//!
//! a node of the names and subtree sections:
//!  0 depth u8 (1 for a child)   1 name length u8   2 properties u16
//!  4 name, padded to four
//!  then each property: 0 name length u8   1 0 u8   2 value length u16
//!                      4 name, padded   then the value, padded
//! ```
//!
//! The names section is one such node, of depth 0, holding only properties.

use core::ops::ControlFlow;

use crate::generic::{Reference, ReferenceKind};
use crate::{Fdt, Node};

/// The most bytes a record takes: what one page of a node's identity holds,
/// and the largest offset `device_config_read` reads.
pub const RECORD_BYTES: usize = 4096;

/// A record's first word: `FXDT` in memory order.
pub const RECORD_MAGIC: u32 = u32::from_le_bytes(*b"FXDT");

/// The record version this crate writes and reads.
pub const RECORD_VERSION: u16 = 1;

/// Bytes before the first section.
pub const HEADER_BYTES: usize = 40;

/// The most bytes of `compatible` copied.
pub const MAX_COMPATIBLE_BYTES: usize = 512;

/// The most bytes of the path copied.
pub const MAX_PATH_BYTES: usize = 256;

/// Record flag: the kernel depends on the node (D4): a kernel-owned node
/// references it, directly or through other nodes, as a clock, reset, power
/// domain, pin configuration, supply or interconnect.
pub const FLAG_KERNEL_DEPENDS: u16 = 1 << 0;
/// Record flag: something did not fit, the `compatible` list, the path, a
/// reference or the subtree, and was cut short.
pub const FLAG_CUT: u16 = 1 << 1;
/// Record flag: a reference names no node, or a disabled one.
pub const FLAG_MISSING_REFERENCE: u16 = 1 << 2;
/// Record flag: a `reg` range could not become an aperture -- untranslatable,
/// overlapping memory the kernel uses, or sharing a page with another node --
/// and the node was published without it.
pub const FLAG_REGISTERS_WITHHELD: u16 = 1 << 3;
/// Record flag: an interrupt was not minted: not on the primary controller,
/// not a shared peripheral interrupt, or already another node's.
pub const FLAG_INTERRUPTS_WITHHELD: u16 = 1 << 4;

/// Reference flag: the phandle names no node, or a provider that does not
/// say how wide its specifiers are.
pub const REFERENCE_MISSING: u8 = 1 << 0;
/// Reference flag: the node it names is disabled.
pub const REFERENCE_DISABLED: u8 = 1 << 1;
/// Reference flag: cut short; its cells are not all here.
pub const REFERENCE_CUT: u8 = 1 << 2;

/// What [`encode`] wrote.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Encoded {
    /// Bytes of the record.
    pub length: usize,
    /// Its flags, as written at offset 6.
    pub flags: u16,
    /// References written.
    pub references: usize,
}

/// A bounded writer over the record's buffer.
struct Out<'b> {
    bytes: &'b mut [u8],
    at: usize,
    cut: bool,
}

impl Out<'_> {
    fn room(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    fn put(&mut self, data: &[u8]) -> bool {
        let end = self.at.saturating_add(data.len());
        match self.bytes.get_mut(self.at..end) {
            Some(slot) => {
                slot.copy_from_slice(data);
                self.at = end;
                true
            }
            None => {
                self.cut = true;
                false
            }
        }
    }

    fn pad(&mut self) -> bool {
        while !self.at.is_multiple_of(4) {
            if !self.put(&[0]) {
                return false;
            }
        }
        true
    }

    fn set16(&mut self, at: usize, value: usize) {
        let value = u16::try_from(value).unwrap_or(u16::MAX);
        if let Some(slot) = self.bytes.get_mut(at..at + 2) {
            slot.copy_from_slice(&value.to_le_bytes());
        }
    }

    fn set32(&mut self, at: usize, value: u32) {
        if let Some(slot) = self.bytes.get_mut(at..at + 4) {
            slot.copy_from_slice(&value.to_le_bytes());
        }
    }
}

/// Write `node`'s identity record into `out`, with `ancestors` from the root
/// to its parent and `flags` the kernel's own ([`FLAG_KERNEL_DEPENDS`],
/// [`FLAG_REGISTERS_WITHHELD`], [`FLAG_INTERRUPTS_WITHHELD`]).
///
/// What does not fit in `out`, or past [`RECORD_BYTES`], is cut and flagged
/// [`FLAG_CUT`]; the header always fits in a buffer of [`HEADER_BYTES`].
/// `None` only for a buffer shorter than the header.
pub fn encode<'a>(
    fdt: &Fdt<'a>,
    node: &Node<'a>,
    ancestors: &[Node<'a>],
    flags: u16,
    out: &mut [u8],
) -> Option<Encoded> {
    let limit = out.len().min(RECORD_BYTES);
    let bytes = out.get_mut(..limit)?;
    if bytes.len() < HEADER_BYTES {
        return None;
    }
    bytes.fill(0);
    let mut out = Out {
        bytes,
        at: HEADER_BYTES,
        cut: false,
    };
    let mut flags = flags;

    // The compatible list, whole strings only.
    let compatible = node.property("compatible").map_or(&[][..], |p| p.value);
    let mut take = compatible.len().min(MAX_COMPATIBLE_BYTES);
    if take < compatible.len() {
        take = compatible
            .get(..take)
            .and_then(|head| head.iter().rposition(|&b| b == 0))
            .map_or(0, |nul| nul + 1);
        flags |= FLAG_CUT;
    }
    let start = out.at;
    let _ = out.put(compatible.get(..take).unwrap_or(&[]));
    let length = out.at - start;
    out.set16(16, start);
    out.set16(18, length);
    let _ = out.pad();

    // The path, from the root's children down.
    let start = out.at;
    if !put_path(&mut out, node, ancestors) {
        flags |= FLAG_CUT;
    }
    out.set16(20, start);
    out.set16(22, out.at - start);
    let _ = out.pad();

    // The references.
    let start = out.at;
    let mut count = 0usize;
    fdt.for_each_reference(node, |reference| {
        if reference.missing || reference.disabled {
            flags |= FLAG_MISSING_REFERENCE;
        }
        if reference.cut {
            flags |= FLAG_CUT;
        }
        if put_reference(&mut out, &reference) {
            count += 1;
            ControlFlow::Continue(())
        } else {
            flags |= FLAG_CUT;
            ControlFlow::Break(())
        }
    });
    out.set16(24, start);
    out.set16(26, count);

    // The node's own `*-names`.
    let start = out.at;
    if !put_names(&mut out, node) {
        flags |= FLAG_CUT;
    }
    out.set16(28, start);
    out.set16(30, out.at - start);

    // The subtree: every descendant, until the record is full.
    let start = out.at;
    if !put_subtree(&mut out, fdt, node) {
        flags |= FLAG_CUT;
    }
    out.set16(32, start);
    out.set16(34, out.at - start);

    let parent = fdt
        .interrupt_parent(node, ancestors)
        .map_or(0, |parent| parent.id());
    if out.cut {
        flags |= FLAG_CUT;
    }
    let length = out.at;
    out.set32(0, RECORD_MAGIC);
    out.set16(4, usize::from(RECORD_VERSION));
    out.set16(6, usize::from(flags));
    out.set32(8, length as u32);
    out.set32(12, node.id());
    out.set32(36, parent);
    Some(Encoded {
        length,
        flags,
        references: count,
    })
}

/// The node's path, whole components only, at most [`MAX_PATH_BYTES`].
fn put_path(out: &mut Out<'_>, node: &Node<'_>, ancestors: &[Node<'_>]) -> bool {
    if node.depth == 0 {
        return out.put(b"/");
    }
    let mut written = 0usize;
    for component in ancestors.iter().skip(1).chain(core::iter::once(node)) {
        let name = component.name.as_bytes();
        if written + 1 + name.len() > MAX_PATH_BYTES || !out.put(b"/") || !out.put(name) {
            return false;
        }
        written += 1 + name.len();
    }
    true
}

/// One reference, whole or not at all.
fn put_reference(out: &mut Out<'_>, reference: &Reference<'_>) -> bool {
    let name = reference.property.as_bytes();
    let Ok(name_length) = u8::try_from(name.len()) else {
        return false;
    };
    let cells = reference.cells.len() / 4;
    let need = 20 + name.len().next_multiple_of(4) + cells * 4;
    if out.room() < need {
        out.cut = true;
        return false;
    }
    let mut flags = 0;
    if reference.missing {
        flags |= REFERENCE_MISSING;
    }
    if reference.disabled {
        flags |= REFERENCE_DISABLED;
    }
    if reference.cut {
        flags |= REFERENCE_CUT;
    }
    let provider = reference.provider.map_or(0, |node| node.id());
    let target = reference.target.map_or(0, |node| node.id());
    let mut ok = out.put(&[reference.kind as u8, flags, cells as u8, name_length])
        && out.put(&provider.to_le_bytes())
        && out.put(&target.to_le_bytes())
        && out.put(&reference.phandle.to_le_bytes())
        && out.put(&reference.index.to_le_bytes())
        && out.put(&[0, 0])
        && out.put(name)
        && out.pad();
    for cell in reference.cells.chunks_exact(4) {
        let word = cell.try_into().map_or(0, u32::from_be_bytes);
        ok = ok && out.put(&word.to_le_bytes());
    }
    ok
}

/// One node of the names or subtree sections: `depth`, `name` and the
/// properties `keep` keeps, whole or not at all.
fn put_node(
    out: &mut Out<'_>,
    depth: u8,
    node: &Node<'_>,
    name: &[u8],
    keep: impl Fn(&str) -> bool,
) -> bool {
    let mut need = 4 + name.len().next_multiple_of(4);
    let mut properties = 0usize;
    for property in node.properties().filter(|p| keep(p.name)) {
        if property.name.len() > usize::from(u8::MAX)
            || property.value.len() > usize::from(u16::MAX)
        {
            return false;
        }
        need +=
            4 + property.name.len().next_multiple_of(4) + property.value.len().next_multiple_of(4);
        properties += 1;
    }
    let (Ok(name_length), Ok(count)) = (u8::try_from(name.len()), u16::try_from(properties)) else {
        return false;
    };
    if out.room() < need {
        out.cut = true;
        return false;
    }
    let mut ok = out.put(&[depth, name_length])
        && out.put(&count.to_le_bytes())
        && out.put(name)
        && out.pad();
    for property in node.properties().filter(|p| keep(p.name)) {
        ok = ok
            && out.put(&[property.name.len() as u8, 0])
            && out.put(&(property.value.len() as u16).to_le_bytes())
            && out.put(property.name.as_bytes())
            && out.pad()
            && out.put(property.value)
            && out.pad();
    }
    ok
}

/// The node's `*-names` properties, as one node of depth 0.
fn put_names(out: &mut Out<'_>, node: &Node<'_>) -> bool {
    if !node.properties().any(|p| p.name.ends_with("-names")) {
        return true;
    }
    put_node(out, 0, node, b"", |name| name.ends_with("-names"))
}

/// Every descendant of `node`, in tree order, until the first that does not
/// fit.
fn put_subtree<'a>(out: &mut Out<'_>, fdt: &Fdt<'a>, node: &Node<'a>) -> bool {
    let id = node.id();
    let mut inside = false;
    let mut whole = true;
    for candidate in fdt.nodes() {
        if !inside {
            inside = candidate.id() == id;
            continue;
        }
        if candidate.depth <= node.depth {
            break;
        }
        let Ok(depth) = u8::try_from(candidate.depth - node.depth) else {
            whole = false;
            break;
        };
        if !put_node(out, depth, &candidate, candidate.name.as_bytes(), |_| true) {
            whole = false;
            break;
        }
    }
    whole
}

/// Why bytes are not a record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Malformed {
    /// Shorter than its header, or than the length it says.
    Short,
    /// Not [`RECORD_MAGIC`] or not [`RECORD_VERSION`].
    NotARecord,
    /// A section lies outside the record.
    Section,
}

/// An identity record, read back.
#[derive(Clone, Copy, Debug)]
pub struct Record<'r> {
    bytes: &'r [u8],
}

fn le16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn le32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

impl<'r> Record<'r> {
    /// Read the record at the start of `bytes`.
    ///
    /// # Errors
    ///
    /// [`Malformed`].
    pub fn parse(bytes: &'r [u8]) -> Result<Self, Malformed> {
        if bytes.len() < HEADER_BYTES {
            return Err(Malformed::Short);
        }
        if le32(bytes, 0) != Some(RECORD_MAGIC) || le16(bytes, 4) != Some(RECORD_VERSION) {
            return Err(Malformed::NotARecord);
        }
        let length = le32(bytes, 8).ok_or(Malformed::Short)? as usize;
        if !(HEADER_BYTES..=RECORD_BYTES).contains(&length) {
            return Err(Malformed::Short);
        }
        let bytes = bytes.get(..length).ok_or(Malformed::Short)?;
        let record = Record { bytes };
        for at in [16, 20, 28, 32] {
            let _ = record.section(at).ok_or(Malformed::Section)?;
        }
        let start = le16(bytes, 24).ok_or(Malformed::Section)? as usize;
        if start > length {
            return Err(Malformed::Section);
        }
        Ok(record)
    }

    /// The section whose offset and length are at `at`.
    fn section(&self, at: usize) -> Option<&'r [u8]> {
        let start = usize::from(le16(self.bytes, at)?);
        let length = usize::from(le16(self.bytes, at + 2)?);
        self.bytes.get(start..start.checked_add(length)?)
    }

    /// Its flags.
    #[must_use]
    pub fn flags(&self) -> u16 {
        le16(self.bytes, 6).unwrap_or(0)
    }

    /// Whether the kernel depends on the node ([`FLAG_KERNEL_DEPENDS`]).
    #[must_use]
    pub fn kernel_depends(&self) -> bool {
        self.flags() & FLAG_KERNEL_DEPENDS != 0
    }

    /// The node's id ([`Node::id`]).
    #[must_use]
    pub fn node(&self) -> u32 {
        le32(self.bytes, 12).unwrap_or(0)
    }

    /// The node its interrupts go to, by id; 0 for none.
    #[must_use]
    pub fn interrupt_parent(&self) -> u32 {
        le32(self.bytes, 36).unwrap_or(0)
    }

    /// The record's bytes.
    #[must_use]
    pub const fn bytes(&self) -> &'r [u8] {
        self.bytes
    }

    /// The `compatible` strings, most specific first.
    pub fn compatible(&self) -> impl Iterator<Item = &'r str> + use<'r> {
        strings(self.section(16).unwrap_or(&[]))
    }

    /// Whether the node is compatible with `binding`.
    #[must_use]
    pub fn is_compatible(&self, binding: &str) -> bool {
        self.compatible().any(|entry| entry == binding)
    }

    /// The node's path: `/pl031@9010000`.
    #[must_use]
    pub fn path(&self) -> &'r str {
        core::str::from_utf8(self.section(20).unwrap_or(&[])).unwrap_or("")
    }

    /// The node's references, in the tree's order.
    #[must_use]
    pub fn references(&self) -> References<'r> {
        let at = le16(self.bytes, 24).map_or(self.bytes.len(), usize::from);
        References {
            bytes: self.bytes,
            at,
            left: le16(self.bytes, 26).unwrap_or(0),
        }
    }

    /// The strings of the node's own `<what>-names` property (`clock-names`,
    /// `reset-names`, `pinctrl-names`), empty when it has none.
    pub fn names(&self, property: &str) -> impl Iterator<Item = &'r str> + use<'r> {
        let mut value: &'r [u8] = &[];
        if let Some(node) = SectionNodes::new(self.section(28).unwrap_or(&[])).next()
            && let Some((_, found)) = node.properties().find(|(name, _)| *name == property)
        {
            value = found;
        }
        strings(value)
    }

    /// The subtree's nodes, in tree order.
    #[must_use]
    pub fn subtree(&self) -> SectionNodes<'r> {
        SectionNodes::new(self.section(32).unwrap_or(&[]))
    }
}

/// The strings of a NUL-separated list.
fn strings(bytes: &[u8]) -> impl Iterator<Item = &str> {
    bytes
        .split(|&b| b == 0)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| core::str::from_utf8(entry).ok())
}

/// One reference of a record.
#[derive(Clone, Copy, Debug)]
pub struct RecordReference<'r> {
    /// What it names, when the kind is one this crate knows.
    pub kind: Option<ReferenceKind>,
    /// [`REFERENCE_MISSING`], [`REFERENCE_DISABLED`], [`REFERENCE_CUT`].
    pub flags: u8,
    /// The provider's node id, 0 when missing.
    pub provider: u32,
    /// The node the phandle names, 0 when missing.
    pub target: u32,
    /// The phandle as the tree wrote it.
    pub phandle: u32,
    /// Its place in its property.
    pub index: u16,
    /// The property's name.
    pub property: &'r str,
    cells: &'r [u8],
}

impl<'r> RecordReference<'r> {
    /// The specifier's cells, uninterpreted.
    pub fn cells(&self) -> impl Iterator<Item = u32> + use<'r> {
        self.cells
            .chunks_exact(4)
            .map(|word| word.try_into().map_or(0, u32::from_le_bytes))
    }

    /// Whether it names an enabled node, whole.
    #[must_use]
    pub const fn resolved(&self) -> bool {
        self.flags & (REFERENCE_MISSING | REFERENCE_DISABLED | REFERENCE_CUT) == 0
    }
}

/// The references of a record.
#[derive(Clone, Copy, Debug)]
pub struct References<'r> {
    bytes: &'r [u8],
    at: usize,
    left: u16,
}

impl<'r> Iterator for References<'r> {
    type Item = RecordReference<'r>;

    fn next(&mut self) -> Option<RecordReference<'r>> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let at = self.at;
        let header = self.bytes.get(at..at + 20)?;
        let [kind, flags, cells, name_length] = *header.first_chunk::<4>()?;
        let name_length = usize::from(name_length);
        let cells = usize::from(cells);
        let name_at = at + 20;
        let cells_at = name_at + name_length.next_multiple_of(4);
        let end = cells_at + cells * 4;
        let property =
            core::str::from_utf8(self.bytes.get(name_at..name_at + name_length)?).ok()?;
        let cell_bytes = self.bytes.get(cells_at..end)?;
        self.at = end;
        Some(RecordReference {
            kind: ReferenceKind::from_u8(kind),
            flags,
            provider: le32(header, 4)?,
            target: le32(header, 8)?,
            phandle: le32(header, 12)?,
            index: le16(header, 16)?,
            property,
            cells: cell_bytes,
        })
    }
}

/// One node of a record's names or subtree section.
#[derive(Clone, Copy, Debug)]
pub struct SectionNode<'r> {
    /// Its depth under the record's node: 1 for a child.
    pub depth: u8,
    /// Its name, unit address included.
    pub name: &'r str,
    count: u16,
    properties: &'r [u8],
}

impl<'r> SectionNode<'r> {
    /// Its properties, as `(name, value)`.
    pub fn properties(&self) -> impl Iterator<Item = (&'r str, &'r [u8])> + use<'r> {
        let mut at = 0usize;
        let bytes = self.properties;
        (0..self.count).map_while(move |_| {
            let name_length = usize::from(*bytes.get(at)?);
            let value_length = usize::from(le16(bytes, at + 2)?);
            let name_at = at + 4;
            let value_at = name_at + name_length.next_multiple_of(4);
            let name = core::str::from_utf8(bytes.get(name_at..name_at + name_length)?).ok()?;
            let value = bytes.get(value_at..value_at + value_length)?;
            at = value_at + value_length.next_multiple_of(4);
            Some((name, value))
        })
    }
}

/// The nodes of a section.
#[derive(Clone, Copy, Debug)]
pub struct SectionNodes<'r> {
    bytes: &'r [u8],
    at: usize,
}

impl<'r> SectionNodes<'r> {
    const fn new(bytes: &'r [u8]) -> Self {
        SectionNodes { bytes, at: 0 }
    }
}

impl<'r> Iterator for SectionNodes<'r> {
    type Item = SectionNode<'r>;

    fn next(&mut self) -> Option<SectionNode<'r>> {
        let bytes = self.bytes;
        let at = self.at;
        let depth = *bytes.get(at)?;
        let name_length = usize::from(*bytes.get(at + 1)?);
        let count = le16(bytes, at + 2)?;
        let name_at = at + 4;
        let name = core::str::from_utf8(bytes.get(name_at..name_at + name_length)?).ok()?;
        let mut end = name_at + name_length.next_multiple_of(4);
        let start = end;
        for _ in 0..count {
            let property_name = usize::from(*bytes.get(end)?);
            let value = usize::from(le16(bytes, end + 2)?);
            end += 4 + property_name.next_multiple_of(4) + value.next_multiple_of(4);
        }
        let properties = bytes.get(start..end)?;
        self.at = end;
        Some(SectionNode {
            depth,
            name,
            count,
            properties,
        })
    }
}
