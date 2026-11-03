//! The PPTT: which processors are threads of one core.
//!
//! The Processor Properties Topology Table (ACPI 6.5 §5.2.30) is a forest of
//! *processor hierarchy nodes* -- package, cluster, core, thread -- each
//! naming its parent by its offset in the table. A leaf whose flags say "is
//! a thread" is a hardware thread, and its parent is its core. This reads
//! no more than that: how many enabled threads each core has, for `smt=off`'s
//! check on AArch64 (`docs/BUILD-FLAGS.md` §3.1 DF4 (b)).
//!
//! Every offset and length is checked against the table before it is read,
//! and the walks are bounded by the table: a node is at least
//! [`NODE_MIN_LEN`] bytes, so the table holds at most its length over that
//! many nodes, and a parent chain longer than that is a cycle. A malformed
//! table is an error, never "no topology" (the consultant's ledger 2563,
//! condition (3)). Nothing here allocates.

use crate::{AcpiError, Table, u8_at, u32_at};

/// The PPTT's signature.
pub const PPTT_SIGNATURE: [u8; 4] = *b"PPTT";

/// Where the first subtable starts: right after the system description
/// header.
const FIRST_NODE: usize = crate::SDT_HEADER_LEN;
/// Subtable type: a processor hierarchy node.
pub const NODE_PROCESSOR: u8 = 0;
/// Bytes of a processor hierarchy node before its private resources.
pub const NODE_MIN_LEN: usize = 20;
/// Processor node flag: the ACPI processor ID is valid.
pub const FLAG_ID_VALID: u32 = 1 << 1;
/// Processor node flag: the node is a thread.
pub const FLAG_THREAD: u32 = 1 << 2;
/// Processor node flag: the node is a leaf.
pub const FLAG_LEAF: u32 = 1 << 3;

/// The processor topology table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pptt<'a> {
    /// The table everything is read out of.
    table: Table<'a>,
}

/// One processor hierarchy node.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Processor {
    /// Its offset in the table, which is how a child names it.
    pub offset: usize,
    /// Its flags.
    pub flags: u32,
    /// Its parent's offset, zero for a root.
    pub parent: u32,
    /// The ACPI processor ID, meaningful where [`FLAG_ID_VALID`] is set.
    pub acpi_id: u32,
}

impl Processor {
    /// Whether this node is a hardware thread: a leaf flagged as one.
    #[must_use]
    pub const fn is_thread(&self) -> bool {
        self.flags & FLAG_THREAD != 0
    }
}

/// What [`Pptt::threads`] found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Threads {
    /// Enabled hardware threads (leaves flagged as threads).
    pub threads: usize,
    /// Cores that hold at least one of them.
    pub cores: usize,
    /// The most enabled threads any one core holds: one where no core runs
    /// two, zero where the table names no thread.
    pub most_a_core: usize,
}

impl<'a> Pptt<'a> {
    /// Interpret `table` as a PPTT.
    ///
    /// # Errors
    ///
    /// [`AcpiError::BadSignature`] if it is not one.
    pub fn parse(table: Table<'a>) -> Result<Self, AcpiError> {
        table.expect_signature(PPTT_SIGNATURE)?;
        Ok(Pptt { table })
    }

    /// The node at `offset`, which must be a whole processor hierarchy node.
    ///
    /// # Errors
    ///
    /// [`AcpiError::BadParent`] where `offset` names no such node.
    pub fn processor_at(&self, offset: usize) -> Result<Processor, AcpiError> {
        let bytes = self.table.bytes();
        let bad = AcpiError::BadParent { offset };
        if offset < FIRST_NODE {
            return Err(bad);
        }
        let kind = u8_at(bytes, offset).ok_or(bad)?;
        let len = usize::from(u8_at(bytes, offset + 1).ok_or(bad)?);
        if kind != NODE_PROCESSOR || len < NODE_MIN_LEN || offset + len > bytes.len() {
            return Err(bad);
        }
        Ok(Processor {
            offset,
            flags: u32_at(bytes, offset + 4).ok_or(bad)?,
            parent: u32_at(bytes, offset + 8).ok_or(bad)?,
            acpi_id: u32_at(bytes, offset + 12).ok_or(bad)?,
        })
    }

    /// Every processor hierarchy node, in table order; the other subtables
    /// (caches, IDs) are stepped over.
    ///
    /// # Errors
    ///
    /// A subtable shorter than its own header or running past the table.
    pub fn for_each_processor(
        &self,
        mut each: impl FnMut(Processor) -> Result<(), AcpiError>,
    ) -> Result<(), AcpiError> {
        let bytes = self.table.bytes();
        let mut offset = FIRST_NODE;
        while offset < bytes.len() {
            let kind = u8_at(bytes, offset).ok_or(AcpiError::EntryOutOfBounds { offset })?;
            let len = usize::from(
                u8_at(bytes, offset + 1).ok_or(AcpiError::EntryOutOfBounds { offset })?,
            );
            if len < 2 {
                return Err(AcpiError::ZeroLengthEntry { offset });
            }
            if offset + len > bytes.len() {
                return Err(AcpiError::EntryOutOfBounds { offset });
            }
            if kind == NODE_PROCESSOR {
                each(self.processor_at(offset)?)?;
            }
            offset += len;
        }
        Ok(())
    }

    /// The most nodes the table can hold, which bounds every parent chain.
    fn node_bound(&self) -> usize {
        self.table.bytes().len() / NODE_MIN_LEN
    }

    /// Check `node`'s parent chain: every parent a processor node of the
    /// table, and the chain no longer than the table could hold.
    ///
    /// # Errors
    ///
    /// [`AcpiError::BadParent`] for a parent that is no processor node, or a
    /// chain that loops.
    fn check_chain(&self, node: Processor) -> Result<(), AcpiError> {
        let mut at = node;
        for _ in 0..=self.node_bound() {
            if at.parent == 0 {
                return Ok(());
            }
            let parent = usize::try_from(at.parent)
                .map_err(|_| AcpiError::BadParent { offset: at.offset })?;
            at = self
                .processor_at(parent)
                .map_err(|_| AcpiError::BadParent { offset: at.offset })?;
        }
        Err(AcpiError::BadParent {
            offset: node.offset,
        })
    }

    /// How many counted threads share `node`'s parent, and whether `node`
    /// is the first of them in table order.
    ///
    /// # Errors
    ///
    /// As [`Pptt::for_each_processor`].
    fn siblings(
        &self,
        node: Processor,
        counted: &impl Fn(&Processor) -> bool,
    ) -> Result<(usize, bool), AcpiError> {
        let mut siblings = 0;
        let mut first = true;
        self.for_each_processor(|other| {
            if other.parent == node.parent && counted(&other) {
                siblings += 1;
                first &= other.offset >= node.offset;
            }
            Ok(())
        })?;
        Ok((siblings, first))
    }

    /// How many threads each core has, of the leaves `enabled` says are
    /// enabled (by ACPI processor ID, as the MADT's GIC CPU interfaces name
    /// them).
    ///
    /// # Errors
    ///
    /// A malformed table: a subtable out of bounds, a parent that is no
    /// processor node, a parent chain that loops, or a thread with no
    /// parent (a thread of no core).
    pub fn threads(&self, enabled: impl Fn(u32) -> bool) -> Result<Threads, AcpiError> {
        let mut found = Threads {
            threads: 0,
            cores: 0,
            most_a_core: 0,
        };
        let counted = |node: &Processor| {
            node.is_thread() && node.flags & FLAG_ID_VALID != 0 && enabled(node.acpi_id)
        };
        self.for_each_processor(|node| {
            self.check_chain(node)?;
            if !node.is_thread() {
                return Ok(());
            }
            if node.parent == 0 {
                return Err(AcpiError::BadParent {
                    offset: node.offset,
                });
            }
            if !counted(&node) {
                return Ok(());
            }
            found.threads += 1;
            // The threads of this one's core, counted once, at the core's
            // first thread in table order.
            let (siblings, first) = self.siblings(node, &counted)?;
            if first {
                found.cores += 1;
            }
            found.most_a_core = found.most_a_core.max(siblings);
            Ok(())
        })?;
        Ok(found)
    }
}
