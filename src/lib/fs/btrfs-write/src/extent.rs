//! Extent items: what the extent tree records about each allocation.
//!
//! Every allocated extent — a tree block or a run of file data — has one item
//! in the extent tree, keyed by its logical address. The item carries a total
//! reference count, the transaction that allocated it, whether it is data or
//! a tree block, and a list of *back-references*: who points at it. A tree
//! block is referenced by the tree that owns it; a data extent by each
//! `(tree, inode, file offset)` that maps it, with a count, because one
//! extent split by a partial overwrite is mapped twice from the same inode.
//!
//! References that fit are stored *inline*, inside the extent item; the rest
//! get items of their own, *keyed* under the same address. Which ones are
//! inline is the writer's choice; their order is not. Linux's tree-checker
//! (`check_extent_item`) refuses an item whose inline references are not in
//! ascending type order, and within a type in *descending* order of a
//! sequence number: the root or parent for most types, and for a data
//! reference the hash [`hash_extent_data_ref`]. [`ExtentRecord::to_items`]
//! produces that order and nothing else.
//!
//! The record is parsed whole, edited as a map, and written whole, for the
//! same reason nodes are (see [`crate::node`]).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use ferrix_btrfs::crc32c::crc32c_update;
use ferrix_btrfs::tree::BtrfsKey;

use crate::bytes::{get_u8, get_u32, get_u64, put_u8, put_u32, put_u64};
use crate::{Error, Result, Unsupported, fallible};

/// An allocated extent; for data, and for tree blocks on volumes without
/// skinny metadata. The key's offset is the length.
pub const EXTENT_ITEM_KEY: u8 = 168;
/// An allocated tree block, skinny form. The key's offset is its level.
pub const METADATA_ITEM_KEY: u8 = 169;
/// An owner reference, written only with simple quotas.
pub const EXTENT_OWNER_REF_KEY: u8 = 172;
/// Back-reference from a tree block to the tree that owns it.
pub const TREE_BLOCK_REF_KEY: u8 = 176;
/// Back-reference from a data extent to a file that maps it.
pub const EXTENT_DATA_REF_KEY: u8 = 178;
/// Back-reference from a tree block to the parent block holding it.
pub const SHARED_BLOCK_REF_KEY: u8 = 182;
/// Back-reference from a data extent to the leaf holding its file item.
pub const SHARED_DATA_REF_KEY: u8 = 184;
/// A block group's usage.
pub const BLOCK_GROUP_ITEM_KEY: u8 = 192;
/// A block group's free-space summary in the free-space tree.
pub const FREE_SPACE_INFO_KEY: u8 = 198;
/// A run of free space.
pub const FREE_SPACE_EXTENT_KEY: u8 = 199;
/// A bitmap of free sectors.
pub const FREE_SPACE_BITMAP_KEY: u8 = 200;
/// A stretch of a device that a chunk stripe occupies.
pub const DEV_EXTENT_KEY: u8 = 204;

/// Extent flag: the extent holds file data.
pub const EXTENT_FLAG_DATA: u64 = 1 << 0;
/// Extent flag: the extent is a tree block.
pub const EXTENT_FLAG_TREE_BLOCK: u64 = 1 << 1;
/// Extent flag: the tree block's references name parents, not owners.
pub const BLOCK_FLAG_FULL_BACKREF: u64 = 1 << 8;

/// Bytes of the fixed part of an extent item: refs, generation, flags.
const EXTENT_ITEM_SIZE: usize = 24;
/// Bytes of the `tree_block_info` a non-skinny tree block item carries.
const TREE_BLOCK_INFO_SIZE: usize = 18;
/// Bytes of an `extent_data_ref`: root, objectid, offset, count.
const DATA_REF_SIZE: usize = 28;

/// One back-reference, without its count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Backref {
    /// The tree block belongs to tree `root`.
    Tree {
        /// The owning tree's id.
        root: u64,
    },
    /// File `objectid` in tree `root` maps the extent with its start at file
    /// offset `offset` (the item's key offset less its offset into the
    /// extent, so every piece of one split extent names the same place).
    Data {
        /// The tree the file is in.
        root: u64,
        /// The file's inode number.
        objectid: u64,
        /// Where the extent's first byte would be in the file.
        offset: u64,
    },
    /// The tree block is referenced from parent block `parent`.
    SharedBlock {
        /// The parent's logical address.
        parent: u64,
    },
    /// A file item in leaf `parent` maps the extent.
    SharedData {
        /// The leaf's logical address.
        parent: u64,
    },
}

impl Backref {
    /// The key type this reference has when it is an item of its own, and
    /// the type byte it has inline.
    #[must_use]
    pub const fn key_type(&self) -> u8 {
        match self {
            Backref::Tree { .. } => TREE_BLOCK_REF_KEY,
            Backref::Data { .. } => EXTENT_DATA_REF_KEY,
            Backref::SharedBlock { .. } => SHARED_BLOCK_REF_KEY,
            Backref::SharedData { .. } => SHARED_DATA_REF_KEY,
        }
    }

    /// The number the tree-checker orders same-typed inline references by,
    /// descending.
    #[must_use]
    pub fn sequence(&self) -> u64 {
        match *self {
            Backref::Tree { root } => root,
            Backref::Data {
                root,
                objectid,
                offset,
            } => hash_extent_data_ref(root, objectid, offset),
            Backref::SharedBlock { parent } | Backref::SharedData { parent } => parent,
        }
    }

    /// Bytes the reference takes inline: its type byte and body.
    const fn inline_size(&self) -> usize {
        match self {
            Backref::Tree { .. } | Backref::SharedBlock { .. } => 9,
            Backref::Data { .. } => 1 + DATA_REF_SIZE,
            Backref::SharedData { .. } => 13,
        }
    }

    /// Whether a reference of this kind carries a count. The tree-block kinds
    /// do not: each exists once or not at all.
    const fn counted(&self) -> bool {
        matches!(self, Backref::Data { .. } | Backref::SharedData { .. })
    }
}

/// Linux's `hash_extent_data_ref`: the key offset a keyed data reference is
/// filed under, and the order inline ones are kept in.
///
/// CRC-32C of the root, and separately of inode and offset, as raw registers
/// seeded with all ones — the kernel's `crc32c`, which does not complement —
/// combined with a 31-bit, not 32-bit, shift.
#[must_use]
pub fn hash_extent_data_ref(root: u64, objectid: u64, offset: u64) -> u64 {
    let high = crc32c_update(!0, &root.to_le_bytes());
    let low = crc32c_update(!0, &objectid.to_le_bytes());
    let low = crc32c_update(low, &offset.to_le_bytes());
    (u64::from(high) << 31) ^ u64::from(low)
}

/// An extent item and every back-reference to the extent, inline or keyed.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(Clone))]
pub struct ExtentRecord {
    /// Transaction that allocated the extent.
    pub generation: u64,
    /// [`EXTENT_FLAG_DATA`] or [`EXTENT_FLAG_TREE_BLOCK`], and for a tree
    /// block perhaps [`BLOCK_FLAG_FULL_BACKREF`].
    pub flags: u64,
    /// Every reference and its count.
    pub refs: BTreeMap<Backref, u64>,
}

impl ExtentRecord {
    /// A record for an extent with no references yet.
    #[must_use]
    pub const fn new(generation: u64, flags: u64) -> Self {
        ExtentRecord {
            generation,
            flags,
            refs: BTreeMap::new(),
        }
    }

    /// The total the item's `refs` field must hold.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.refs
            .values()
            .fold(0u64, |sum, &count| sum.saturating_add(count))
    }

    /// Whether this is a tree block's record.
    #[must_use]
    pub const fn is_tree_block(&self) -> bool {
        self.flags & EXTENT_FLAG_TREE_BLOCK != 0
    }

    /// Parse an `EXTENT_ITEM` or `METADATA_ITEM` and its inline references.
    ///
    /// Checks what `check_extent_item` checks that matters to a writer: one
    /// of the data and tree-block flags, known reference types, no counts of
    /// zero, no padding, and a stored total that matches the references once
    /// the keyed ones are added — which [`ExtentRecord::check_total`] does.
    pub fn parse(key: &BtrfsKey, data: &[u8]) -> Result<(Self, u64)> {
        let bad = Error::Inconsistent("malformed extent item");
        let stored = get_u64(data, 0).ok_or(bad)?;
        let generation = get_u64(data, 8).ok_or(bad)?;
        let flags = get_u64(data, 16).ok_or(bad)?;
        let kinds = flags & (EXTENT_FLAG_DATA | EXTENT_FLAG_TREE_BLOCK);
        if kinds.count_ones() != 1 {
            return Err(bad);
        }
        let mut record = ExtentRecord::new(generation, flags);
        let mut at = EXTENT_ITEM_SIZE;
        if record.is_tree_block() && key.item_type == EXTENT_ITEM_KEY {
            at = at.saturating_add(TREE_BLOCK_INFO_SIZE);
        }
        while at < data.len() {
            let (backref, count, size) = parse_inline(data, at)?;
            record.add(backref, count)?;
            at = at.saturating_add(size);
        }
        if at != data.len() {
            return Err(bad);
        }
        Ok((record, stored))
    }

    /// Add a keyed reference item found under the extent's address.
    pub fn add_keyed(&mut self, key: &BtrfsKey, data: &[u8]) -> Result<()> {
        let bad = Error::Inconsistent("malformed keyed extent reference");
        let (backref, count) = match key.item_type {
            TREE_BLOCK_REF_KEY => (Backref::Tree { root: key.offset }, 1),
            SHARED_BLOCK_REF_KEY => (Backref::SharedBlock { parent: key.offset }, 1),
            EXTENT_DATA_REF_KEY => parse_data_ref(data, 0).ok_or(bad)?,
            SHARED_DATA_REF_KEY => (
                Backref::SharedData { parent: key.offset },
                u64::from(get_u32(data, 0).ok_or(bad)?),
            ),
            EXTENT_OWNER_REF_KEY => return Err(Error::Unsupported(Unsupported::Quotas)),
            _ => return Err(bad),
        };
        self.add(backref, count)
    }

    /// Count a reference in. A reference found twice is damage.
    fn add(&mut self, backref: Backref, count: u64) -> Result<()> {
        if count == 0 || self.refs.contains_key(&backref) {
            return Err(Error::Inconsistent("extent reference repeated or zero"));
        }
        if fallible::insert(&mut self.refs, backref, count)?.is_some() {
            return Err(Error::Inconsistent("extent reference repeated or zero"));
        }
        Ok(())
    }

    /// Refuse a record whose stored total disagrees with its references.
    pub fn check_total(&self, stored: u64) -> Result<()> {
        if stored == self.total() {
            Ok(())
        } else {
            Err(Error::Inconsistent(
                "extent refs disagree with its references",
            ))
        }
    }

    /// Change the count of `backref` by `delta`, dropping it at zero. A count
    /// that would go below zero, or a tree-block reference counted twice, is
    /// a bookkeeping error.
    pub fn apply(&mut self, backref: Backref, delta: i64) -> Result<()> {
        let now = self.refs.get(&backref).copied().unwrap_or(0);
        let next = now
            .checked_add_signed(delta)
            .ok_or(Error::Inconsistent("extent reference count below zero"))?;
        if !backref.counted() && next > 1 {
            return Err(Error::Inconsistent(
                "tree block referenced twice by one owner",
            ));
        }
        if next == 0 {
            let _ = self.refs.remove(&backref);
        } else if let Some(count) = self.refs.get_mut(&backref) {
            *count = next;
        } else {
            let _ = fallible::insert(&mut self.refs, backref, next)?;
        }
        Ok(())
    }

    /// The items that store this record for the extent at `key`: the extent
    /// item itself, with as many references inline as fit in `max_inline`
    /// bytes of item, then one keyed item for each of the rest.
    ///
    /// Inline references come out in the tree-checker's order: ascending type,
    /// then descending sequence. The keyed data references are filed under
    /// their hash; two whose hashes collide take the next free offsets, as
    /// Linux's `insert_extent_data_ref` does.
    pub fn to_items(&self, key: &BtrfsKey, max_inline: usize) -> Result<Vec<(BtrfsKey, Vec<u8>)>> {
        let bad = Error::Inconsistent("extent record does not encode");
        let mut ordered: Vec<(&Backref, u64)> =
            fallible::collect(self.refs.iter().map(|(r, &c)| (r, c)))?;
        // Unstable, which sorts in place, and the same order the map's own
        // gave a stable sort: two references alike in type and sequence --
        // data references whose hashes collide -- stay in the map's order.
        ordered.sort_unstable_by(|(a, _), (b, _)| {
            a.key_type()
                .cmp(&b.key_type())
                .then(b.sequence().cmp(&a.sequence()))
                .then(a.cmp(b))
        });
        let mut item = fallible::zeroed(EXTENT_ITEM_SIZE)?;
        put_u64(&mut item, 0, self.total()).ok_or(bad)?;
        put_u64(&mut item, 8, self.generation).ok_or(bad)?;
        put_u64(&mut item, 16, self.flags).ok_or(bad)?;
        let mut keyed = ordered.as_slice();
        while let Some((&(backref, count), rest)) = keyed.split_first() {
            if item.len().saturating_add(backref.inline_size()) > max_inline {
                break;
            }
            push_inline(&mut item, backref, count)?;
            keyed = rest;
        }
        let mut items = fallible::with_capacity(keyed.len().saturating_add(1))?;
        fallible::push(&mut items, (*key, item))?;
        let mut taken = BTreeSet::new();
        for &(backref, count) in keyed {
            let keyed_item = keyed_item(key.objectid, backref, count, &mut taken)?;
            fallible::push(&mut items, keyed_item)?;
        }
        // Every key differs, so an unstable sort is the stable one.
        items.sort_unstable_by_key(|(k, _)| *k);
        Ok(items)
    }
}

/// Parse the inline reference at `at`: the reference, its count, and how many
/// bytes it took.
fn parse_inline(data: &[u8], at: usize) -> Result<(Backref, u64, usize)> {
    let bad = Error::Inconsistent("malformed inline extent reference");
    let kind = get_u8(data, at).ok_or(bad)?;
    let body = at.checked_add(1).ok_or(bad)?;
    match kind {
        TREE_BLOCK_REF_KEY => Ok((
            Backref::Tree {
                root: get_u64(data, body).ok_or(bad)?,
            },
            1,
            9,
        )),
        SHARED_BLOCK_REF_KEY => Ok((
            Backref::SharedBlock {
                parent: get_u64(data, body).ok_or(bad)?,
            },
            1,
            9,
        )),
        EXTENT_DATA_REF_KEY => {
            let (backref, count) = parse_data_ref(data, body).ok_or(bad)?;
            Ok((backref, count, 1 + DATA_REF_SIZE))
        }
        SHARED_DATA_REF_KEY => {
            let parent = get_u64(data, body).ok_or(bad)?;
            let count = get_u32(data, body.checked_add(8).ok_or(bad)?).ok_or(bad)?;
            Ok((Backref::SharedData { parent }, u64::from(count), 13))
        }
        EXTENT_OWNER_REF_KEY => Err(Error::Unsupported(Unsupported::Quotas)),
        _ => Err(bad),
    }
}

/// Parse an `extent_data_ref` at `at`.
fn parse_data_ref(data: &[u8], at: usize) -> Option<(Backref, u64)> {
    Some((
        Backref::Data {
            root: get_u64(data, at)?,
            objectid: get_u64(data, at.checked_add(8)?)?,
            offset: get_u64(data, at.checked_add(16)?)?,
        },
        u64::from(get_u32(data, at.checked_add(24)?)?),
    ))
}

/// Append one inline reference to an extent item.
fn push_inline(item: &mut Vec<u8>, backref: &Backref, count: u64) -> Result<()> {
    let bad = Error::Inconsistent("extent record does not encode");
    let at = item.len();
    fallible::resize(item, at.checked_add(backref.inline_size()).ok_or(bad)?, 0)?;
    encode_inline(item, at, backref, count).ok_or(bad)
}

/// Lay one inline reference out at `at`, in room already made for it.
fn encode_inline(item: &mut [u8], at: usize, backref: &Backref, count: u64) -> Option<()> {
    put_u8(item, at, backref.key_type())?;
    let body = at.checked_add(1)?;
    match *backref {
        Backref::Tree { root } => put_u64(item, body, root),
        Backref::SharedBlock { parent } => put_u64(item, body, parent),
        Backref::Data {
            root,
            objectid,
            offset,
        } => put_data_ref(item, body, root, objectid, offset, count),
        Backref::SharedData { parent } => {
            put_u64(item, body, parent)?;
            put_u32(item, body.checked_add(8)?, u32::try_from(count).ok()?)
        }
    }
}

/// Store an `extent_data_ref` at `at`.
fn put_data_ref(
    buf: &mut [u8],
    at: usize,
    root: u64,
    objectid: u64,
    offset: u64,
    count: u64,
) -> Option<()> {
    put_u64(buf, at, root)?;
    put_u64(buf, at.checked_add(8)?, objectid)?;
    put_u64(buf, at.checked_add(16)?, offset)?;
    put_u32(buf, at.checked_add(24)?, u32::try_from(count).ok()?)
}

/// The keyed item for one reference of the extent at `bytenr`.
fn keyed_item(
    bytenr: u64,
    backref: &Backref,
    count: u64,
    taken: &mut BTreeSet<u64>,
) -> Result<(BtrfsKey, Vec<u8>)> {
    let bad = Error::Inconsistent("extent record does not encode");
    match *backref {
        Backref::Tree { root } => Ok((BtrfsKey::new(bytenr, TREE_BLOCK_REF_KEY, root), Vec::new())),
        Backref::SharedBlock { parent } => Ok((
            BtrfsKey::new(bytenr, SHARED_BLOCK_REF_KEY, parent),
            Vec::new(),
        )),
        Backref::SharedData { parent } => {
            let mut data = fallible::zeroed(4)?;
            put_u32(&mut data, 0, u32::try_from(count).map_err(|_| bad)?).ok_or(bad)?;
            Ok((BtrfsKey::new(bytenr, SHARED_DATA_REF_KEY, parent), data))
        }
        Backref::Data {
            root,
            objectid,
            offset,
        } => {
            let mut slot = hash_extent_data_ref(root, objectid, offset);
            while taken.contains(&slot) {
                slot = slot.checked_add(1).ok_or(bad)?;
            }
            let _ = fallible::insert_into_set(taken, slot)?;
            let mut data = fallible::zeroed(DATA_REF_SIZE)?;
            put_data_ref(&mut data, 0, root, objectid, offset, count).ok_or(bad)?;
            Ok((BtrfsKey::new(bytenr, EXTENT_DATA_REF_KEY, slot), data))
        }
    }
}

#[cfg(test)]
mod tests;
