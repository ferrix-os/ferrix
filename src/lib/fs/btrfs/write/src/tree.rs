//! Editing a tree: search, insert, update, delete, and the fix-up after each.
//!
//! An edit searches from the root with copy-on-write, so that every node on
//! the path to the leaf is one this transaction owns, and then changes the
//! leaf as a list. That can leave the leaf too full, empty, or starting with
//! a different key than its parent says. One routine, [`WriteVolume::fixup`],
//! repairs all three from the leaf upward along the same path:
//!
//! * a node that overflows is cut into pieces of about equal size, and the
//!   new pieces' pointers go into the parent after its own — which may
//!   overflow the parent in turn, up to a new root;
//! * a node that emptied is dropped from its parent, which may empty it;
//! * every node's pointer in its parent is given the node's first key,
//!   because Linux checks that the two are equal on every read;
//! * and a root left with one child is replaced by it.
//!
//! The rest of the tree is untouched, so the path's slots stay valid while
//! the fix-up climbs: a change at one level only ever moves pointers *after*
//! the one the path went through.

use alloc::vec::Vec;

use ferrix_btrfs::tree::{BtrfsKey, KeyPtr, MAX_LEVEL};

use crate::node::{Body, LeafItem};
use crate::volume::{Root, TreeId};
use crate::{Error, Result, WriteDevice, WriteVolume, fallible};

/// The route a search took: at each level, the node and the slot in it.
/// Index 0 is the leaf.
#[derive(Debug)]
pub(crate) struct Path {
    levels: Vec<(u64, usize)>,
}

impl Path {
    fn at(&self, level: usize) -> Result<(u64, usize)> {
        self.levels
            .get(level)
            .copied()
            .ok_or(Error::Inconsistent("path shorter than the tree"))
    }

    fn top(&self) -> usize {
        self.levels.len().saturating_sub(1)
    }
}

/// What damage to report for a tree that is not shaped like one.
const MALFORMED: Error = Error::Inconsistent("tree node of the wrong kind");

impl<D: WriteDevice> WriteVolume<D> {
    /// Descend `tree` to the leaf holding `key` or the place it would go.
    ///
    /// With `cow`, every node on the way is made one this transaction owns.
    /// Returns the path and whether the key is present; the leaf slot is the
    /// key's, or its insertion point.
    pub(crate) fn search(
        &mut self,
        tree: TreeId,
        key: &BtrfsKey,
        cow: bool,
    ) -> Result<(Path, bool)> {
        let root = if cow {
            self.cow_root(tree)?
        } else {
            self.root(tree)?
        };
        if root.level > MAX_LEVEL {
            return Err(Error::Inconsistent("tree deeper than btrfs allows"));
        }
        let mut levels = fallible::filled((0u64, 0usize), usize::from(root.level) + 1)?;
        let (mut logical, mut level, mut generation) = (root.bytenr, root.level, root.generation);
        loop {
            self.load(logical, level, generation)?;
            let (slot, found) = match &self.node(logical)?.body {
                Body::Leaf(items) => match items.binary_search_by(|item| item.key.cmp(key)) {
                    Ok(slot) => (slot, true),
                    Err(slot) => (slot, false),
                },
                Body::Internal(ptrs) => match ptrs.binary_search_by(|ptr| ptr.key.cmp(key)) {
                    Ok(slot) => (slot, false),
                    Err(slot) => (slot.saturating_sub(1), false),
                },
            };
            if let Some(entry) = levels.get_mut(usize::from(level)) {
                *entry = (logical, slot);
            }
            if level == 0 {
                return Ok((Path { levels }, found));
            }
            let child_level = level - 1;
            if cow {
                logical = self.cow_child(tree, logical, slot, child_level)?;
                generation = self.transid;
            } else {
                let ptr = self.child(logical, slot)?;
                logical = ptr.blockptr;
                generation = ptr.generation;
            }
            level = child_level;
        }
    }

    /// The payload of the item at `key` in `tree`.
    pub fn get(&mut self, tree: TreeId, key: &BtrfsKey) -> Result<Option<Vec<u8>>> {
        self.check_open()?;
        let (path, found) = self.search(tree, key, false)?;
        if !found {
            return Ok(None);
        }
        let (leaf, slot) = path.at(0)?;
        self.leaf_item(leaf, slot)?
            .map(|item| fallible::copy(&item.data))
            .transpose()
    }

    /// Insert an item. [`Error::Exists`] if the key is taken.
    pub fn insert(&mut self, tree: TreeId, key: BtrfsKey, data: Vec<u8>) -> Result<()> {
        self.guarded(|volume| volume.insert_item(tree, key, data, false))
    }

    /// Insert an item, or replace the payload of the one with its key.
    pub fn put(&mut self, tree: TreeId, key: BtrfsKey, data: Vec<u8>) -> Result<()> {
        self.guarded(|volume| volume.insert_item(tree, key, data, true))
    }

    /// Replace the payload of an existing item. [`Error::NotFound`] if there
    /// is none.
    pub fn update(&mut self, tree: TreeId, key: BtrfsKey, data: Vec<u8>) -> Result<()> {
        self.guarded(|volume| {
            let (path, found) = volume.search(tree, &key, true)?;
            if !found {
                return Err(Error::NotFound);
            }
            volume.edit_leaf(&path, |items, slot| {
                let item = items.get_mut(slot).ok_or(MALFORMED)?;
                item.data = data;
                Ok(())
            })?;
            volume.fixup(tree, &path)
        })
    }

    /// Delete an item. [`Error::NotFound`] if there is none.
    pub fn delete(&mut self, tree: TreeId, key: &BtrfsKey) -> Result<()> {
        self.guarded(|volume| {
            let (path, found) = volume.search(tree, key, true)?;
            if !found {
                return Err(Error::NotFound);
            }
            volume.edit_leaf(&path, |items, slot| {
                if slot < items.len() {
                    let _ = items.remove(slot);
                    Ok(())
                } else {
                    Err(MALFORMED)
                }
            })?;
            volume.fixup(tree, &path)
        })
    }

    fn insert_item(
        &mut self,
        tree: TreeId,
        key: BtrfsKey,
        data: Vec<u8>,
        replace: bool,
    ) -> Result<()> {
        let largest = (self.nodesize() as usize)
            .saturating_sub(ferrix_btrfs::tree::HEADER_SIZE)
            .saturating_sub(ferrix_btrfs::tree::ITEM_SIZE);
        if data.len() > largest {
            return Err(Error::ItemTooLarge);
        }
        let (path, found) = self.search(tree, &key, true)?;
        if found && !replace {
            return Err(Error::Exists);
        }
        self.edit_leaf(&path, |items, slot| {
            if found {
                let item = items.get_mut(slot).ok_or(MALFORMED)?;
                item.data = data;
            } else if slot <= items.len() {
                fallible::insert_at(items, slot, LeafItem { key, data })?;
            } else {
                return Err(MALFORMED);
            }
            Ok(())
        })?;
        self.fixup(tree, &path)
    }

    /// Change the leaf at the end of `path` through `edit`.
    fn edit_leaf(
        &mut self,
        path: &Path,
        edit: impl FnOnce(&mut Vec<LeafItem>, usize) -> Result<()>,
    ) -> Result<()> {
        let (leaf, slot) = path.at(0)?;
        match &mut self.node_mut(leaf)?.body {
            Body::Leaf(items) => edit(items, slot),
            Body::Internal(_) => Err(MALFORMED),
        }
    }

    fn leaf_item(&self, leaf: u64, slot: usize) -> Result<Option<&LeafItem>> {
        match &self.node(leaf)?.body {
            Body::Leaf(items) => Ok(items.get(slot)),
            Body::Internal(_) => Err(MALFORMED),
        }
    }

    /// Repair the nodes along `path` after its leaf changed; see the module
    /// documentation.
    fn fixup(&mut self, tree: TreeId, path: &Path) -> Result<()> {
        let top = path.top();
        for level in 0..=top {
            let (logical, _) = path.at(level)?;
            let node = self.node(logical)?;
            let parent = if level < top {
                Some(path.at(level + 1)?)
            } else {
                None
            };
            if node.overflows(self.nodesize()) {
                self.split(tree, logical, level, parent)?;
            } else if node.is_empty() {
                if let Some((parent, slot)) = parent {
                    self.remove_ptr(parent, slot)?;
                    self.free_node(tree, logical)?;
                }
            } else if let Some((parent, slot)) = parent {
                let first = self.first_key(logical)?;
                self.set_ptr_key(parent, slot, logical, first)?;
            }
        }
        self.shrink_root(tree)
    }

    /// Cut an overflowing node into pieces and hang them from its parent, or
    /// from a new root if it was the root.
    fn split(
        &mut self,
        tree: TreeId,
        logical: u64,
        level: usize,
        parent: Option<(u64, usize)>,
    ) -> Result<()> {
        let nodesize = self.nodesize();
        let points = self
            .node(logical)?
            .split_points(nodesize)?
            .ok_or(Error::ItemTooLarge)?;
        // FALLIBLE: the node's own `split_off`, which reports running out of memory.
        let bodies = self.node_mut(logical)?.split_off(&points)?;
        let level8 = u8::try_from(level).map_err(|_| MALFORMED)?;
        let mut ptrs = fallible::with_capacity(bodies.len().saturating_add(1))?;
        fallible::push(
            &mut ptrs,
            KeyPtr {
                key: self.first_key(logical)?,
                blockptr: logical,
                generation: self.transid,
            },
        )?;
        for body in bodies {
            let key = match &body {
                Body::Leaf(items) => items.first().map(|item| item.key),
                Body::Internal(children) => children.first().map(|ptr| ptr.key),
            }
            .ok_or(MALFORMED)?;
            let at = self.new_node(tree, level8, body)?;
            fallible::push(
                &mut ptrs,
                KeyPtr {
                    key,
                    blockptr: at,
                    generation: self.transid,
                },
            )?;
        }
        match parent {
            Some((parent, slot)) => {
                let transid = self.transid;
                match &mut self.node_mut(parent)?.body {
                    Body::Internal(children) => {
                        let mut rest = ptrs.into_iter();
                        let first = rest.next().ok_or(MALFORMED)?;
                        let entry = children.get_mut(slot).ok_or(MALFORMED)?;
                        if entry.blockptr != logical {
                            return Err(MALFORMED);
                        }
                        entry.key = first.key;
                        entry.generation = transid;
                        let after = slot.checked_add(1).ok_or(MALFORMED)?;
                        for (offset, ptr) in rest.enumerate() {
                            fallible::insert_at(children, after.saturating_add(offset), ptr)?;
                        }
                        Ok(())
                    }
                    Body::Leaf(_) => Err(MALFORMED),
                }
            }
            None => {
                let up = level8
                    .checked_add(1)
                    .filter(|&l| l <= MAX_LEVEL)
                    .ok_or(Error::NoSpace)?;
                let at = self.new_node(tree, up, Body::Internal(ptrs))?;
                self.set_root(
                    tree,
                    Root {
                        bytenr: at,
                        level: up,
                        generation: self.transid,
                    },
                )?;
                // A root split into more pieces than a node holds pointers
                // would need another level; the pieces are few, but check.
                if self.node(at)?.overflows(nodesize) {
                    return Err(Error::ItemTooLarge);
                }
                Ok(())
            }
        }
    }

    fn remove_ptr(&mut self, parent: u64, slot: usize) -> Result<()> {
        match &mut self.node_mut(parent)?.body {
            Body::Internal(ptrs) if slot < ptrs.len() => {
                let _ = ptrs.remove(slot);
                Ok(())
            }
            _ => Err(MALFORMED),
        }
    }

    fn set_ptr_key(&mut self, parent: u64, slot: usize, child: u64, key: BtrfsKey) -> Result<()> {
        match &mut self.node_mut(parent)?.body {
            Body::Internal(ptrs) => {
                let entry = ptrs.get_mut(slot).ok_or(MALFORMED)?;
                if entry.blockptr != child {
                    return Err(MALFORMED);
                }
                entry.key = key;
                Ok(())
            }
            Body::Leaf(_) => Err(MALFORMED),
        }
    }

    /// Replace a root that has one child with the child, and an internal root
    /// with none by an empty leaf, until the root is a leaf or branches.
    fn shrink_root(&mut self, tree: TreeId) -> Result<()> {
        loop {
            let root = self.root(tree)?;
            if root.level == 0 {
                return Ok(());
            }
            let (count, first) = match &self.node(root.bytenr)?.body {
                Body::Internal(ptrs) => (ptrs.len(), ptrs.first().copied()),
                Body::Leaf(_) => return Err(MALFORMED),
            };
            match (count, first) {
                (0, _) => {
                    self.free_node(tree, root.bytenr)?;
                    let at = self.new_node(tree, 0, Body::Leaf(Vec::new()))?;
                    self.set_root(
                        tree,
                        Root {
                            bytenr: at,
                            level: 0,
                            generation: self.transid,
                        },
                    )?;
                    return Ok(());
                }
                (1, Some(only)) => {
                    self.free_node(tree, root.bytenr)?;
                    self.set_root(
                        tree,
                        Root {
                            bytenr: only.blockptr,
                            level: root.level - 1,
                            generation: only.generation,
                        },
                    )?;
                    // A root's generation is the transaction that last wrote
                    // it, which for the new root must be this one.
                    let _ = self.cow_root(tree)?;
                }
                _ => return Ok(()),
            }
        }
    }

    /// The first item of `tree` at or after `key`.
    pub fn next_item(
        &mut self,
        tree: TreeId,
        key: &BtrfsKey,
    ) -> Result<Option<(BtrfsKey, Vec<u8>)>> {
        self.check_open()?;
        let (path, _) = self.search(tree, key, false)?;
        let (leaf, slot) = path.at(0)?;
        if let Some(item) = self.leaf_item(leaf, slot)? {
            return Ok(Some((item.key, fallible::copy(&item.data)?)));
        }
        for level in 1..=path.top() {
            let (logical, slot) = path.at(level)?;
            let Some(next) = slot.checked_add(1) else {
                continue;
            };
            let Ok(ptr) = self.child(logical, next) else {
                continue;
            };
            let leaf = self.descend(ptr, level - 1, true)?;
            return self
                .leaf_item(leaf, 0)?
                .map(|item| Ok((item.key, fallible::copy(&item.data)?)))
                .transpose();
        }
        Ok(None)
    }

    /// The last item of `tree` at or before `key`.
    pub fn prev_item(
        &mut self,
        tree: TreeId,
        key: &BtrfsKey,
    ) -> Result<Option<(BtrfsKey, Vec<u8>)>> {
        self.check_open()?;
        let (path, found) = self.search(tree, key, false)?;
        let (leaf, slot) = path.at(0)?;
        let within = if found {
            Some(slot)
        } else {
            slot.checked_sub(1)
        };
        if let Some(slot) = within {
            return self
                .leaf_item(leaf, slot)?
                .map(|item| Ok((item.key, fallible::copy(&item.data)?)))
                .transpose();
        }
        for level in 1..=path.top() {
            let (logical, slot) = path.at(level)?;
            let Some(before) = slot.checked_sub(1) else {
                continue;
            };
            let ptr = self.child(logical, before)?;
            let leaf = self.descend(ptr, level - 1, false)?;
            let count = self.node(leaf)?.nritems();
            let last = count.checked_sub(1).ok_or(MALFORMED)?;
            return self
                .leaf_item(leaf, last)?
                .map(|item| Ok((item.key, fallible::copy(&item.data)?)))
                .transpose();
        }
        Ok(None)
    }

    /// Follow `ptr`, at `level`, down its leftmost (or rightmost) edge to a
    /// leaf, returning the leaf's address.
    fn descend(&mut self, ptr: KeyPtr, level: usize, leftmost: bool) -> Result<u64> {
        let mut at = ptr;
        let mut level = u8::try_from(level).map_err(|_| MALFORMED)?;
        loop {
            self.load(at.blockptr, level, at.generation)?;
            if level == 0 {
                return Ok(at.blockptr);
            }
            let count = self.node(at.blockptr)?.nritems();
            let slot = if leftmost {
                0
            } else {
                count.checked_sub(1).ok_or(MALFORMED)?
            };
            at = self.child(at.blockptr, slot)?;
            level -= 1;
        }
    }

    /// Every item of `tree` from `from` to `to`, both inclusive.
    pub fn range(
        &mut self,
        tree: TreeId,
        from: &BtrfsKey,
        to: &BtrfsKey,
    ) -> Result<Vec<(BtrfsKey, Vec<u8>)>> {
        let mut out = Vec::new();
        let mut key = *from;
        while let Some((found, data)) = self.next_item(tree, &key)? {
            if found > *to {
                break;
            }
            fallible::push(&mut out, (found, data))?;
            match successor(&found) {
                Some(next) => key = next,
                None => break,
            }
        }
        Ok(out)
    }
}

/// The smallest key strictly greater than `key`.
pub(crate) fn successor(key: &BtrfsKey) -> Option<BtrfsKey> {
    if let Some(offset) = key.offset.checked_add(1) {
        return Some(BtrfsKey::new(key.objectid, key.item_type, offset));
    }
    if let Some(item_type) = key.item_type.checked_add(1) {
        return Some(BtrfsKey::new(key.objectid, item_type, 0));
    }
    Some(BtrfsKey::new(key.objectid.checked_add(1)?, 0, 0))
}
