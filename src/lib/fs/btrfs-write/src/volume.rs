//! A volume open for writing: its trees, its nodes, and copy-on-write.
//!
//! [`WriteVolume`] owns the device and everything the running transaction
//! holds in memory. Its methods are split by concern across the crate's
//! modules — tree edits in `tree`, the commit in `commit`, the mount in
//! `open` — and this module holds the state and the one operation all of
//! them share: getting a node, and getting a node this transaction may edit.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use ferrix_btrfs::BtrfsError;
use ferrix_btrfs::items::CHUNK_TREE_OBJECTID;
use ferrix_btrfs::tree::{BtrfsKey, KeyPtr, Node};
use ferrix_btrfs::volume::ReadKind;

use crate::chunks::Chunks;
use crate::extent::Backref;
use crate::node::{Body, TreeNode};
use crate::refs::DelayedRefs;
use crate::space::{Kind, Space};
use crate::{Error, Result, WriteDevice, fallible};

/// A tree's id: its object id in the root tree, or 1 and 3 for the root and
/// chunk trees, whose roots the superblock names.
pub type TreeId = u64;

/// Where a tree's root is, and what its top node must say about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Root {
    pub(crate) bytenr: u64,
    pub(crate) level: u8,
    pub(crate) generation: u64,
}

/// The volume's fixed geometry and identity.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Geometry {
    pub(crate) nodesize: u32,
    pub(crate) sectorsize: u32,
    pub(crate) fsid: [u8; 16],
    pub(crate) chunk_tree_uuid: [u8; 16],
    pub(crate) devid: u64,
    pub(crate) dev_uuid: [u8; 16],
    pub(crate) device_size: u64,
}

/// How many clean nodes the cache keeps before it starts again. Clean nodes
/// are only a cache of what is on disk, so dropping them costs reads, never
/// correctness.
const CLEAN_NODES: usize = 4096;

/// Nodes of metadata kept free before every edit. One edit copies at most a
/// root-to-leaf path and splits along it — sixteen nodes on the deepest tree
/// btrfs allows — and the commit's own bookkeeping runs as many small edits,
/// each topping the reserve up again.
const EDIT_RESERVE: u64 = 64;

/// Nodes of metadata only an operation that frees space may use: Linux's
/// global block reserve. Without it a volume whose trees filled could not
/// even delete a file to make room, since a deletion copies nodes too.
const GLOBAL_RESERVE: u64 = 4 * EDIT_RESERVE;

/// What an operation must find room for in the trees before its first edit;
/// see [`WriteVolume::admit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Need {
    /// The most new tree nodes the operation's edits can take.
    pub(crate) nodes: u64,
    /// Whether it frees space, and so may use [`GLOBAL_RESERVE`].
    pub(crate) freeing: bool,
}

impl Need {
    /// One edit, or an operation of a few: a name made or moved, an inode
    /// item written.
    pub(crate) const EDIT: Need = Need {
        nodes: EDIT_RESERVE,
        freeing: false,
    };
    /// An operation that frees space: an unlink, an eviction, a truncation.
    pub(crate) const FREEING: Need = Need {
        nodes: EDIT_RESERVE,
        freeing: true,
    };

    /// Writing `len` bytes of file data: an extent item and its checksums
    /// for each mebibyte, which a node per mebibyte covers many times over.
    pub(crate) const fn data(len: u64) -> Need {
        Need {
            nodes: EDIT_RESERVE.saturating_add(len.div_ceil(1024 * 1024)),
            freeing: false,
        }
    }
}

/// A btrfs volume opened for writing.
///
/// Edits go into an open transaction held in memory; [`WriteVolume::commit`]
/// makes them durable, and until then the volume on disk is exactly what the
/// last commit left.
#[derive(Debug)]
pub struct WriteVolume<D> {
    pub(crate) device: D,
    pub(crate) geometry: Geometry,
    pub(crate) chunks: Chunks,
    /// Device space no chunk holds, in whole mebibytes; see
    /// [`WriteVolume::measure_unallocated`].
    pub(crate) unallocated: u64,
    /// The primary superblock as last committed.
    pub(crate) superblock: Vec<u8>,
    /// Generation of the last commit.
    pub(crate) committed: u64,
    /// The running transaction's id: one more than `committed`.
    pub(crate) transid: u64,
    /// Nodes this transaction wrote or copied, by logical address. Their
    /// generation is `transid`, and they are edited in place.
    pub(crate) dirty: BTreeMap<u64, TreeNode>,
    /// Nodes read from disk and not changed.
    pub(crate) clean: BTreeMap<u64, TreeNode>,
    pub(crate) roots: BTreeMap<TreeId, Root>,
    /// Trees whose root moved since their `ROOT_ITEM` was written.
    pub(crate) stale_roots: BTreeSet<TreeId>,
    pub(crate) refs: DelayedRefs,
    pub(crate) space: Space,
    /// Whether the chunk tree or the system chunk array changed.
    pub(crate) chunks_changed: bool,
    /// Set while a chunk is being recorded, so its edits do not try to make
    /// another.
    pub(crate) growing: bool,
    /// Set by a failed edit; see [`crate::Error::Aborted`].
    pub(crate) aborted: bool,
    /// The error that set `aborted`, for whoever has to say why.
    pub(crate) abort_cause: Option<Error>,
    /// Set by [`WriteVolume::reload_read_only`]: reads go on, edits do not.
    pub(crate) read_only: bool,
    /// Edits that have succeeded, so an operation can tell whether it failed
    /// before changing anything.
    pub(crate) edits: u64,
    /// How deep in operations and edits the running one is: only the
    /// outermost is admitted, see [`WriteVolume::admit`].
    pub(crate) depth: u32,
    /// Whether operations are admitted against the room in the trees: not
    /// while the volume is being opened, whose log replay and orphan cleanup
    /// must run on a volume however full.
    pub(crate) admitting: bool,
}

impl<D: WriteDevice> WriteVolume<D> {
    /// Size of every tree node.
    #[must_use]
    pub const fn nodesize(&self) -> u32 {
        self.geometry.nodesize
    }

    /// Bytes of tree nodes the running transaction holds in memory: what
    /// its commit writes.
    #[must_use]
    pub fn dirty_bytes(&self) -> u64 {
        (self.dirty.len() as u64).saturating_mul(u64::from(self.geometry.nodesize))
    }

    /// Size of a data sector.
    #[must_use]
    pub const fn sectorsize(&self) -> u32 {
        self.geometry.sectorsize
    }

    /// Generation of the last commit.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.committed
    }

    /// Id of the transaction edits are going into now.
    #[must_use]
    pub const fn transid(&self) -> u64 {
        self.transid
    }

    /// Whether the transaction holds anything to commit.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        !self.dirty.is_empty() || !self.refs.is_empty()
    }

    /// The device, for a caller that must reach it directly.
    pub fn device_mut(&mut self) -> &mut D {
        &mut self.device
    }

    /// Give the device back, dropping everything uncommitted.
    pub fn into_device(self) -> D {
        self.device
    }

    /// Refuse to go on after a failure that left the transaction half-done.
    pub(crate) const fn check_open(&self) -> Result<()> {
        if self.aborted {
            Err(Error::Aborted)
        } else {
            Ok(())
        }
    }

    /// Refuse an edit: after a failure that left the transaction half-done,
    /// and on a volume reloaded read-only after one.
    pub(crate) const fn check_writable(&self) -> Result<()> {
        if self.aborted {
            Err(Error::Aborted)
        } else if self.read_only {
            Err(Error::ReadOnly)
        } else {
            Ok(())
        }
    }

    /// Mark the transaction aborted by `cause`, keeping the first cause.
    pub(crate) fn abort_with(&mut self, cause: Error) {
        if !self.aborted {
            self.aborted = true;
            self.abort_cause = Some(cause);
        }
    }

    /// Whether a failed edit has aborted the running transaction, and with
    /// what error: everything but [`WriteVolume::reload_read_only`] and
    /// dropping the volume answers [`Error::Aborted`] from then on.
    #[must_use]
    pub const fn aborted(&self) -> Option<Error> {
        if self.aborted { self.abort_cause } else { None }
    }

    /// Whether the volume was reloaded read-only after an aborted
    /// transaction.
    #[must_use]
    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Run an edit, marking the transaction aborted if it fails part-way.
    ///
    /// Every edit starts here, between complete edits, which is the one
    /// place a chunk can safely be made; so this is where metadata space is
    /// topped up. A volume without room for that answers [`Error::NoSpace`]
    /// before the edit has changed anything, which aborts nothing.
    pub(crate) fn guarded<T>(&mut self, edit: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.guarded_with(Some(Need::EDIT), edit)
    }

    /// [`WriteVolume::guarded`], admitted against `need` when it is the
    /// outermost; `None` for the commit, which the room every admission
    /// left for it pays for.
    pub(crate) fn guarded_with<T>(
        &mut self,
        need: Option<Need>,
        edit: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        self.check_writable()?;
        if let Some(need) = need {
            self.admit(need)?;
        }
        // A chunk the reserve cannot have is not this edit's failure: the
        // admission measured the room, and a group too fragmented to grow
        // still has what it measured.
        match self.ensure_space(EDIT_RESERVE) {
            Ok(()) | Err(Error::NoSpace) => {}
            Err(error) => return Err(error),
        }
        self.depth = self.depth.saturating_add(1);
        let result = edit(self);
        self.depth = self.depth.saturating_sub(1);
        match result {
            Ok(_) => self.edits = self.edits.wrapping_add(1),
            // An answer about the key, given before anything changed: the
            // path was copied, which keeps the tree whole, and nothing else.
            Err(Error::Exists | Error::NotFound) => {}
            Err(error) => self.abort_with(error),
        }
        result
    }

    /// Run an operation made of several edits, marking the transaction
    /// aborted if it fails after any of them succeeded: a half-made name, or
    /// half-written file, is not something to commit.
    pub(crate) fn operation<T>(&mut self, op: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.operation_with(Need::EDIT, op)
    }

    /// [`WriteVolume::operation`], admitted against `need` when it is the
    /// outermost.
    pub(crate) fn operation_with<T>(
        &mut self,
        need: Need,
        op: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        self.check_writable()?;
        self.admit(need)?;
        let before = self.edits;
        self.depth = self.depth.saturating_add(1);
        let result = op(self);
        self.depth = self.depth.saturating_sub(1);
        if let Err(error) = result
            && self.edits != before
        {
            self.abort_with(error);
        }
        result
    }

    /// Refuse an operation the trees have no room for, before it changes
    /// anything: [`Error::NoSpace`], which aborts nothing.
    ///
    /// The room is what the metadata groups have free and what new metadata
    /// chunks could still be made of. An operation needs its own worst case,
    /// the commit's (see [`WriteVolume::commit_reserve`]) and, unless it
    /// frees space, [`GLOBAL_RESERVE`] besides. Running out half-way through
    /// an edit is what used to abort a transaction on full trees; Linux
    /// reserves metadata for each transaction handle the same way. Only the
    /// outermost operation is measured: what it calls is in its own need.
    pub(crate) fn admit(&self, need: Need) -> Result<()> {
        if !self.admitting || self.depth > 0 {
            return Ok(());
        }
        let mut nodes = need.nodes.saturating_add(self.commit_reserve());
        if !need.freeing {
            nodes = nodes.saturating_add(GLOBAL_RESERVE);
        }
        let want = nodes.saturating_mul(u64::from(self.geometry.nodesize));
        if self.meta_room() < want {
            return Err(Error::NoSpace);
        }
        Ok(())
    }

    /// The most new nodes committing the running transaction can take: the
    /// extent and free-space items of every node it changed, which a node
    /// holds hundreds of, so one for each sixteen, and the paths and roots
    /// besides.
    pub(crate) fn commit_reserve(&self) -> u64 {
        (self.dirty.len() as u64 / 16).saturating_add(2 * EDIT_RESERVE)
    }

    /// Make sure metadata and system block groups have room for `nodes` new
    /// nodes, making a chunk if not. Does nothing while a chunk is being
    /// recorded, whose own edits the reserve was taken for.
    pub(crate) fn ensure_space(&mut self, nodes: u64) -> Result<()> {
        if self.growing {
            return Ok(());
        }
        let want = nodes.saturating_mul(u64::from(self.geometry.nodesize));
        for kind in [Kind::System, Kind::Metadata] {
            if self.space.free_bytes(kind) < want {
                self.grow(kind)?;
            }
        }
        Ok(())
    }

    /// Make a chunk of `kind`. [`Error::NoSpace`] when the device has no
    /// room for one, and [`Error::OutOfMemory`] when what it needs in memory
    /// could not be made, both with nothing changed; a failure after the
    /// chunk was placed leaves it half-recorded, and aborts the transaction.
    pub(crate) fn grow(&mut self, kind: Kind) -> Result<()> {
        let placed = self.place_chunk(kind)?;
        let (group, copy) = self.chunk_parts(&placed)?;
        self.growing = true;
        let made = self.add_chunk(&placed, group, copy);
        self.growing = false;
        if let Err(error) = made {
            self.abort_with(error);
        }
        made
    }

    /// Allocate a data extent of up to `want` bytes and at least `min`,
    /// making a data chunk if no group has room. Returns `(start, len)`.
    ///
    /// [`Error::NoSpace`] when neither a group nor the device has room, with
    /// nothing changed, so it aborts nothing.
    pub fn alloc_data(&mut self, want: u64, min: u64) -> Result<(u64, u64)> {
        let sector = u64::from(self.geometry.sectorsize);
        self.check_writable()?;
        self.admit(Need::EDIT)?;
        match self.ensure_space(EDIT_RESERVE) {
            Ok(()) | Err(Error::NoSpace) => {}
            Err(error) => return Err(error),
        }
        let found = match self.space.alloc_data(want, min, sector) {
            Err(Error::NoSpace) => {
                self.grow(Kind::Data)?;
                self.space.alloc_data(want, min, sector)
            }
            other => other,
        };
        let extent = match found {
            Ok(extent) => extent,
            // Found no room, having taken none.
            Err(Error::NoSpace) => return Err(Error::NoSpace),
            Err(error) => {
                self.abort_with(error);
                return Err(error);
            }
        };
        if let Err(error) = self.refs.touch(extent.0, extent.1, None) {
            self.abort_with(error);
            return Err(error);
        }
        self.edits = self.edits.wrapping_add(1);
        Ok(extent)
    }

    pub(crate) fn root(&self, tree: TreeId) -> Result<Root> {
        self.roots
            .get(&tree)
            .copied()
            .ok_or(Error::Volume(BtrfsError::MissingRoot(tree)))
    }

    pub(crate) fn set_root(&mut self, tree: TreeId, root: Root) -> Result<()> {
        let _ = fallible::insert(&mut self.roots, tree, root)?;
        let _ = fallible::insert_into_set(&mut self.stale_roots, tree)?;
        Ok(())
    }

    /// Make sure the node at `logical` is in memory, checking what the
    /// pointer that led to it promised: its level and its generation.
    pub(crate) fn load(&mut self, logical: u64, level: u8, generation: u64) -> Result<()> {
        let held = self
            .dirty
            .get(&logical)
            .or_else(|| self.clean.get(&logical));
        if let Some(node) = held {
            return if node.level == level && node.generation == generation {
                Ok(())
            } else {
                Err(Error::Volume(BtrfsError::BadTree { logical }))
            };
        }
        let node = self.read_node(logical)?;
        if node.level != level || node.generation != generation {
            return Err(Error::Volume(BtrfsError::BadTree { logical }));
        }
        if self.clean.len() >= CLEAN_NODES {
            self.clean.clear();
        }
        let _ = fallible::insert(&mut self.clean, logical, node)?;
        Ok(())
    }

    /// Read and check the node at `logical`, from its first copy that reads
    /// back whole: a DUP node whose first copy is damaged is read from its
    /// second.
    pub(crate) fn read_node(&mut self, logical: u64) -> Result<TreeNode> {
        let size = self.geometry.nodesize as usize;
        let mut buf = fallible::zeroed(size)?;
        let mut last = Error::Volume(BtrfsError::NotMapped(logical));
        for physical in self
            .chunks
            .copies(logical, u64::from(self.geometry.nodesize))?
        {
            let checked = self
                .device
                .read_at(physical, &mut buf, ReadKind::Metadata)
                .and_then(|()| Node::parse(&buf, logical));
            match checked {
                Ok(node) if node.header().fsid == self.geometry.fsid => {
                    return TreeNode::from_node(&node);
                }
                Ok(_) => last = Error::Volume(BtrfsError::BadTree { logical }),
                // Not the copy's fault: another copy would meet the same.
                Err(BtrfsError::OutOfMemory) => return Err(Error::OutOfMemory),
                Err(error) => last = Error::Volume(error),
            }
        }
        Err(last)
    }

    /// The node at `logical`, which must have been loaded.
    pub(crate) fn node(&self, logical: u64) -> Result<&TreeNode> {
        self.dirty
            .get(&logical)
            .or_else(|| self.clean.get(&logical))
            .ok_or(Error::Inconsistent("node used before it was loaded"))
    }

    /// The node at `logical`, which this transaction must already own.
    pub(crate) fn node_mut(&mut self, logical: u64) -> Result<&mut TreeNode> {
        self.dirty.get_mut(&logical).ok_or(Error::Inconsistent(
            "edited a node the transaction does not own",
        ))
    }

    /// The child pointer at `slot` of the internal node at `logical`.
    pub(crate) fn child(&self, logical: u64, slot: usize) -> Result<KeyPtr> {
        match &self.node(logical)?.body {
            Body::Internal(ptrs) => ptrs
                .get(slot)
                .copied()
                .ok_or(Error::Inconsistent("child slot out of range")),
            Body::Leaf(_) => Err(Error::Inconsistent("child of a leaf")),
        }
    }

    /// The kind of block group a tree's blocks come from.
    const fn kind_for(tree: TreeId) -> Kind {
        if tree == CHUNK_TREE_OBJECTID {
            Kind::System
        } else {
            Kind::Metadata
        }
    }

    /// Allocate a block for a new node of `tree` at `level` and queue the
    /// owner's reference to it.
    ///
    /// Never makes a chunk: this runs in the middle of an edit, with a path
    /// held and perhaps an overfull node in memory, and recording a chunk is
    /// itself a series of edits that could reach the same nodes. Space is
    /// reserved before each edit instead, by [`Self::ensure_space`].
    pub(crate) fn alloc_node(&mut self, tree: TreeId, level: u8) -> Result<u64> {
        let nodesize = self.geometry.nodesize;
        let at = self
            .space
            .alloc_tree_block(Self::kind_for(tree), nodesize)?;
        self.refs.add(
            at,
            u64::from(nodesize),
            Some(level),
            Backref::Tree { root: tree },
            1,
        )?;
        Ok(at)
    }

    /// A new node of `tree` holding `body`, owned by this transaction.
    pub(crate) fn new_node(&mut self, tree: TreeId, level: u8, body: Body) -> Result<u64> {
        let at = self.alloc_node(tree, level)?;
        let node = TreeNode {
            bytenr: at,
            generation: self.transid,
            owner: tree,
            level,
            body,
        };
        let _ = fallible::insert(&mut self.dirty, at, node)?;
        Ok(at)
    }

    /// Drop a node this transaction owns from `tree`: its reference goes, and
    /// with it the block.
    pub(crate) fn free_node(&mut self, tree: TreeId, logical: u64) -> Result<()> {
        let node = self.dirty.remove(&logical).ok_or(Error::Inconsistent(
            "freed a node the transaction does not own",
        ))?;
        self.refs.add(
            logical,
            u64::from(self.geometry.nodesize),
            Some(node.level),
            Backref::Tree { root: tree },
            -1,
        )
    }

    /// Copy the committed node at `logical` so this transaction can edit it,
    /// returning the copy's address. A node the transaction already owns is
    /// its own copy.
    pub(crate) fn cow(
        &mut self,
        tree: TreeId,
        logical: u64,
        level: u8,
        generation: u64,
    ) -> Result<u64> {
        if self.dirty.contains_key(&logical) {
            return Ok(logical);
        }
        self.load(logical, level, generation)?;
        let mut node = self
            .clean
            .remove(&logical)
            .ok_or(Error::Inconsistent("copied a node that was not loaded"))?;
        if node.owner != tree {
            return Err(Error::Unsupported(crate::Unsupported::SharedBlock));
        }
        let at = self.alloc_node(tree, level)?;
        self.refs.add(
            logical,
            u64::from(self.geometry.nodesize),
            Some(level),
            Backref::Tree { root: tree },
            -1,
        )?;
        node.bytenr = at;
        node.generation = self.transid;
        let _ = fallible::insert(&mut self.dirty, at, node)?;
        Ok(at)
    }

    /// Make `tree`'s root node one this transaction owns.
    pub(crate) fn cow_root(&mut self, tree: TreeId) -> Result<Root> {
        let root = self.root(tree)?;
        let at = self.cow(tree, root.bytenr, root.level, root.generation)?;
        if at != root.bytenr {
            let moved = Root {
                bytenr: at,
                level: root.level,
                generation: self.transid,
            };
            self.set_root(tree, moved)?;
            return Ok(moved);
        }
        Ok(root)
    }

    /// Make the child at `slot` of `parent` — a node the transaction owns —
    /// one it owns too, and point the parent at the copy.
    pub(crate) fn cow_child(
        &mut self,
        tree: TreeId,
        parent: u64,
        slot: usize,
        level: u8,
    ) -> Result<u64> {
        let ptr = self.child(parent, slot)?;
        let at = self.cow(tree, ptr.blockptr, level, ptr.generation)?;
        if at != ptr.blockptr {
            let transid = self.transid;
            if let Body::Internal(ptrs) = &mut self.node_mut(parent)?.body
                && let Some(entry) = ptrs.get_mut(slot)
            {
                entry.blockptr = at;
                entry.generation = transid;
            }
        }
        Ok(at)
    }

    /// The first key of the node at `logical`.
    pub(crate) fn first_key(&self, logical: u64) -> Result<BtrfsKey> {
        self.node(logical)?
            .first_key()
            .ok_or(Error::Inconsistent("empty node below the root"))
    }
}
