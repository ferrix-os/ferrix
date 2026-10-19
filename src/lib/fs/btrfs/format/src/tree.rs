//! The btrfs B-tree: keys, node headers, leaf items and internal key pointers.
//!
//! Every tree in a btrfs filesystem — the chunk tree, the root tree, each
//! subvolume, the checksum tree — is the same shape, so one parser serves all
//! of them. A node is a single block of `nodesize` bytes beginning with a
//! 101-byte header. `level == 0` makes it a leaf holding item payloads;
//! anything else makes it an internal node holding pointers to children one
//! level down.
//!
//! # A leaf grows from both ends
//!
//! This is the layout that surprises everyone reading btrfs for the first time.
//! After the header, a leaf holds an array of fixed-size item descriptors
//! growing *forwards*, while the variable-size payloads those descriptors point
//! at are packed against the *end* of the node growing *backwards*. Free space
//! is the gap in the middle, which is why an item can be inserted without
//! moving every payload after it.
//!
//! ```text
//! 0        101                                                    nodesize
//! +--------+---------+---------+-----   ------+--------+---------+
//! | header | item[0] | item[1] | ...    free  | data 1 | data 0  |
//! +--------+---------+---------+-----   ------+--------+---------+
//!                                              ^         ^
//!                    item[1].offset -----------+         |
//!                    item[0].offset ---------------------+
//! ```
//!
//! Two consequences matter. An item's `offset` is relative to the *end of the
//! header*, not to the start of the node, so the absolute position is
//! `HEADER_SIZE + offset`. And item offsets *descend* as the index rises, even
//! though the keys ascend, so "the last item" is at the lowest address.

use core::cmp::Ordering;

use crate::{
    BtrfsError, CSUM_SIZE, array_at, is_valid_block_size, slice_at, truncated, u8_at, u32_at,
    u64_at, verify_crc32c,
};

/// Bytes in a node header, `0x65`.
pub const HEADER_SIZE: usize = 101;

/// Bytes in an on-disk key: an 8-byte object id, a 1-byte type and an 8-byte
/// offset, with no padding.
pub const KEY_SIZE: usize = 17;

/// Bytes in a leaf's item descriptor: a key, then a `u32` offset and a `u32`
/// size.
pub const ITEM_SIZE: usize = 25;

/// Bytes in an internal node's key pointer: a key, then a `u64` block pointer
/// and a `u64` generation.
pub const KEY_PTR_SIZE: usize = 33;

/// Smallest node size btrfs will format, and the smallest this crate accepts.
pub const MIN_NODE_SIZE: u32 = 4096;

/// Largest node size btrfs will format. Larger values are a corrupt or hostile
/// superblock rather than a future format.
pub const MAX_NODE_SIZE: u32 = 65536;

/// The highest level a tree node may have. btrfs trees have at most eight
/// levels, numbered from zero at the leaves — Linux's `BTRFS_MAX_LEVEL` is the
/// count, eight, and every level it accepts is below it.
pub const MAX_LEVEL: u8 = 7;

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// A btrfs tree key.
///
/// The meaning of `offset` depends entirely on `item_type`: for an
/// `EXTENT_DATA` it is a file offset, for a `DIR_ITEM` it is a hash of the
/// entry name, for a `CHUNK_ITEM` it is a logical address. Only the ordering is
/// universal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct BtrfsKey {
    /// What the item is about — an inode number, a subvolume id, a chunk tree
    /// marker.
    pub objectid: u64,
    /// Which kind of item this is; see the `*_KEY` constants in
    /// [`crate::items`].
    pub item_type: u8,
    /// Type-dependent discriminator, and the reason two items about the same
    /// object can coexist.
    pub offset: u64,
}

impl BtrfsKey {
    /// A key that sorts before every other key, useful as a search lower bound.
    pub const MIN: BtrfsKey = BtrfsKey {
        objectid: 0,
        item_type: 0,
        offset: 0,
    };

    /// A key that sorts after every other key, useful as a search upper bound.
    pub const MAX: BtrfsKey = BtrfsKey {
        objectid: u64::MAX,
        item_type: u8::MAX,
        offset: u64::MAX,
    };

    /// Build a key from its three components.
    #[must_use]
    pub const fn new(objectid: u64, item_type: u8, offset: u64) -> Self {
        BtrfsKey {
            objectid,
            item_type,
            offset,
        }
    }

    /// Read a key at `at` within `bytes`, or `None` past the end.
    #[must_use]
    pub fn parse(bytes: &[u8], at: usize) -> Option<Self> {
        Some(BtrfsKey {
            objectid: u64_at(bytes, at)?,
            item_type: u8_at(bytes, at.checked_add(8)?)?,
            offset: u64_at(bytes, at.checked_add(9)?)?,
        })
    }
}

impl Ord for BtrfsKey {
    /// Object id first, then type, then offset — in that order and no other.
    ///
    /// Written out rather than derived because a derive silently ties this to
    /// the declaration order of the struct's fields, and the whole tree is
    /// sorted by it: comparing offset before type would not fail loudly, it
    /// would make every binary search return a neighbouring item, so lookups
    /// would find the wrong inode instead of no inode.
    fn cmp(&self, other: &Self) -> Ordering {
        self.objectid
            .cmp(&other.objectid)
            .then(self.item_type.cmp(&other.item_type))
            .then(self.offset.cmp(&other.offset))
    }
}

impl PartialOrd for BtrfsKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// ---------------------------------------------------------------------------
// Node header
// ---------------------------------------------------------------------------

/// The 101-byte header every tree node begins with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeHeader {
    /// Checksum of bytes 32 onwards; for `csum_type == 0` only the first four
    /// bytes are used.
    pub csum: [u8; CSUM_SIZE],
    /// The filesystem this node belongs to, matching the superblock's `fsid`.
    pub fsid: [u8; 16],
    /// The *logical* address this node believes it lives at. Comparing it with
    /// the address actually read is the cheapest detector of a bad chunk
    /// mapping there is.
    pub bytenr: u64,
    /// Node flags; bit 0 marks a node written by a kernel that owns the
    /// extent-tree backref for it.
    pub flags: u64,
    /// The chunk tree's UUID, repeated here so a node can be attributed even
    /// when found loose.
    pub chunk_tree_uuid: [u8; 16],
    /// Transaction that last wrote this node; must not exceed the superblock's
    /// generation on a consistent filesystem.
    pub generation: u64,
    /// Object id of the tree that owns this node.
    pub owner: u64,
    /// Number of items (leaf) or key pointers (internal node) that follow.
    pub nritems: u32,
    /// Height above the leaves. Zero is a leaf.
    pub level: u8,
}

impl NodeHeader {
    /// Parse a node header out of the front of `bytes`.
    ///
    /// Does not verify the checksum and does not validate the body; use
    /// [`Node::parse`] for that. This exists so a caller can look at `level`
    /// and `nritems` before deciding whether the block is worth checksumming.
    pub fn parse(bytes: &[u8]) -> Result<Self, BtrfsError> {
        let get = || {
            Some(NodeHeader {
                csum: array_at::<CSUM_SIZE>(bytes, 0)?,
                fsid: array_at::<16>(bytes, 32)?,
                bytenr: u64_at(bytes, 48)?,
                flags: u64_at(bytes, 56)?,
                chunk_tree_uuid: array_at::<16>(bytes, 64)?,
                generation: u64_at(bytes, 80)?,
                owner: u64_at(bytes, 88)?,
                nritems: u32_at(bytes, 96)?,
                level: u8_at(bytes, 100)?,
            })
        };
        get().ok_or_else(|| truncated(HEADER_SIZE, bytes.len()))
    }

    /// Whether this node holds item payloads rather than child pointers.
    #[must_use]
    pub const fn is_leaf(&self) -> bool {
        self.level == 0
    }
}

// ---------------------------------------------------------------------------
// Leaf items and key pointers
// ---------------------------------------------------------------------------

/// One entry of a leaf, descriptor and payload together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item<'a> {
    /// The item's key, which is what the tree is sorted by.
    pub key: BtrfsKey,
    /// Payload position, relative to the end of the node header rather than to
    /// the start of the node.
    pub offset: u32,
    /// Payload length in bytes.
    pub size: u32,
    /// The payload itself, already bounds-checked against the node.
    pub data: &'a [u8],
}

/// One entry of an internal node: a key and the child that begins with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyPtr {
    /// The smallest key present anywhere in the subtree below `blockptr`.
    pub key: BtrfsKey,
    /// *Logical* address of the child node. It must be translated through a
    /// [`crate::chunk::ChunkMap`] before anything can read it.
    pub blockptr: u64,
    /// Transaction that last wrote the child, used to detect a stale pointer.
    pub generation: u64,
}

// ---------------------------------------------------------------------------
// Node
// ---------------------------------------------------------------------------

/// A parsed and validated tree node borrowing the block it was read from.
///
/// Construction is where all the checking happens, so every accessor below is
/// infallible by index and total by iteration.
#[derive(Debug, Clone, Copy)]
pub struct Node<'a> {
    bytes: &'a [u8],
    header: NodeHeader,
}

impl<'a> Node<'a> {
    /// Parse, checksum and fully validate a node read from logical address
    /// `logical`.
    ///
    /// `bytes` must be exactly the node: its length is taken as the node size,
    /// because that is what the item offsets are measured against.
    ///
    /// Beyond the header this checks that the level is one a tree can have,
    /// that `nritems` fits, that leaf payloads are packed back to back from the
    /// end of the node and clear of the descriptor array, and that keys ascend.
    /// A node that fails any of these would still "work" — it would just
    /// return payloads assembled from the wrong bytes.
    pub fn parse(bytes: &'a [u8], logical: u64) -> Result<Self, BtrfsError> {
        let node = Self::parse_unchecked_address(bytes)?;
        if node.header.bytenr != logical {
            return Err(BtrfsError::WrongAddress {
                expected: logical,
                found: node.header.bytenr,
            });
        }
        Ok(node)
    }

    /// The node [`Node::parse`] accepted from these same bytes, rebuilt from
    /// the header it returned without checking anything again.
    ///
    /// For a reader that had to let go of the parsed node to try another
    /// copy of the block, and kept the bytes of the copy that passed: the
    /// borrow checker cannot see that only the failing copies were let go.
    pub(crate) const fn reparsed(bytes: &'a [u8], header: NodeHeader) -> Self {
        Node { bytes, header }
    }

    /// As [`Node::parse`] but without comparing the header's `bytenr` against
    /// the address the block came from.
    ///
    /// Only for a caller that genuinely does not know the address — a fuzzer,
    /// or a repair tool scanning a device for loose metadata. The normal read
    /// path always knows, and skipping the comparison there would throw away
    /// the check that catches a wrong chunk mapping.
    pub fn parse_unchecked_address(bytes: &'a [u8]) -> Result<Self, BtrfsError> {
        let size = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        if !is_valid_block_size(size, MIN_NODE_SIZE, MAX_NODE_SIZE) {
            return Err(BtrfsError::BadNodeSize(size));
        }
        verify_crc32c(bytes)?;
        let header = NodeHeader::parse(bytes)?;
        // A level above the deepest tree btrfs builds is not a node of any
        // tree, whoever points at it. Linux's `btrfs_check_node` refuses it
        // before looking at a key; checking here means a standalone parse
        // refuses it too, not only a walk that knows which level to expect.
        if header.level > MAX_LEVEL {
            return Err(BtrfsError::BadTree {
                logical: header.bytenr,
            });
        }
        let node = Node { bytes, header };
        node.check_capacity()?;
        if header.is_leaf() {
            node.check_leaf()?;
        } else {
            node.check_internal()?;
        }
        Ok(node)
    }

    /// Reject an `nritems` that the node's own size could not hold.
    ///
    /// Done before anything else touches the count, so the later loops cannot
    /// be made to run a few billion times by a one-word edit to a block.
    fn check_capacity(&self) -> Result<(), BtrfsError> {
        let body = self.bytes.len().saturating_sub(HEADER_SIZE);
        let stride = if self.header.is_leaf() {
            ITEM_SIZE
        } else {
            KEY_PTR_SIZE
        };
        let capacity = u32::try_from(body / stride).unwrap_or(u32::MAX);
        if self.header.nritems > capacity {
            return Err(BtrfsError::TooManyItems {
                nritems: self.header.nritems,
                capacity,
            });
        }
        Ok(())
    }

    /// Validate every leaf item's payload range and the key ordering.
    ///
    /// Payloads are packed: item 0's ends exactly at the end of the node, and
    /// each later item's ends exactly where the one before it begins. That is
    /// Linux's `check_leaf` ("unexpected item end"), and it is stronger than
    /// every payload merely lying inside the node — it is what stops two items
    /// from sharing bytes, so that one item's payload cannot be read back,
    /// through another item's key, as something else.
    fn check_leaf(&self) -> Result<(), BtrfsError> {
        // Payloads may not reach back into the descriptor array; this is the
        // low-water mark they must all stay above.
        let descriptors_end = HEADER_SIZE
            .checked_add((self.header.nritems as usize).saturating_mul(ITEM_SIZE))
            .ok_or(BtrfsError::ItemOutOfBounds { slot: 0 })?;
        // Where the next payload must end, measured like an item's offset from
        // the end of the header. For item 0 that is the end of the node.
        let mut expected_end = self.bytes.len().saturating_sub(HEADER_SIZE);
        let mut previous: Option<BtrfsKey> = None;
        for slot in 0..self.header.nritems {
            let item = self
                .raw_item(slot)
                .ok_or(BtrfsError::ItemOutOfBounds { slot })?;
            let (offset, size) = (item.1 as usize, item.2 as usize);
            let start = HEADER_SIZE
                .checked_add(offset)
                .ok_or(BtrfsError::ItemOutOfBounds { slot })?;
            let end = offset
                .checked_add(size)
                .ok_or(BtrfsError::ItemOutOfBounds { slot })?;
            // `end` equal to a bound that starts at the node's end and only
            // descends keeps every payload inside the node as well.
            if start < descriptors_end || end != expected_end {
                return Err(BtrfsError::ItemOutOfBounds { slot });
            }
            expected_end = offset;
            if previous.is_some_and(|prev| prev >= item.0) {
                return Err(BtrfsError::ItemsOutOfOrder { slot });
            }
            previous = Some(item.0);
        }
        Ok(())
    }

    /// Validate an internal node's key ordering. Child addresses cannot be
    /// checked here — that needs the chunk map, which lives a layer up.
    fn check_internal(&self) -> Result<(), BtrfsError> {
        let mut previous: Option<BtrfsKey> = None;
        for slot in 0..self.header.nritems {
            let ptr = self
                .raw_key_ptr(slot)
                .ok_or(BtrfsError::ItemOutOfBounds { slot })?;
            if previous.is_some_and(|prev| prev >= ptr.key) {
                return Err(BtrfsError::ItemsOutOfOrder { slot });
            }
            previous = Some(ptr.key);
        }
        Ok(())
    }

    /// Read a leaf descriptor without bounds-checking its payload.
    ///
    /// Returns key, offset and size. Private because the payload range has not
    /// been validated yet; [`Node::item`] is the checked form.
    fn raw_item(&self, slot: u32) -> Option<(BtrfsKey, u32, u32)> {
        let at = HEADER_SIZE.checked_add((slot as usize).checked_mul(ITEM_SIZE)?)?;
        let key = BtrfsKey::parse(self.bytes, at)?;
        let offset = u32_at(self.bytes, at.checked_add(KEY_SIZE)?)?;
        let size = u32_at(self.bytes, at.checked_add(KEY_SIZE + 4)?)?;
        Some((key, offset, size))
    }

    /// Read an internal node's key pointer at `slot`.
    fn raw_key_ptr(&self, slot: u32) -> Option<KeyPtr> {
        let at = HEADER_SIZE.checked_add((slot as usize).checked_mul(KEY_PTR_SIZE)?)?;
        Some(KeyPtr {
            key: BtrfsKey::parse(self.bytes, at)?,
            blockptr: u64_at(self.bytes, at.checked_add(KEY_SIZE)?)?,
            generation: u64_at(self.bytes, at.checked_add(KEY_SIZE + 8)?)?,
        })
    }

    /// The node's header.
    #[must_use]
    pub const fn header(&self) -> &NodeHeader {
        &self.header
    }

    /// The whole block this node was parsed from.
    #[must_use]
    pub const fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Height above the leaves; zero is a leaf.
    #[must_use]
    pub const fn level(&self) -> u8 {
        self.header.level
    }

    /// Whether this node holds items rather than child pointers.
    #[must_use]
    pub const fn is_leaf(&self) -> bool {
        self.header.level == 0
    }

    /// How many items or key pointers the node holds.
    #[must_use]
    pub const fn nritems(&self) -> u32 {
        self.header.nritems
    }

    /// The item at `slot`, or `None` for a leaf slot that does not exist or on
    /// a node that is not a leaf.
    #[must_use]
    pub fn item(&self, slot: u32) -> Option<Item<'a>> {
        if !self.is_leaf() || slot >= self.header.nritems {
            return None;
        }
        let (key, offset, size) = self.raw_item(slot)?;
        // Validated in `check_leaf`, so this cannot fail; it is written as a
        // checked lookup anyway so the accessor stands on its own.
        let data = slice_at(
            self.bytes,
            HEADER_SIZE.checked_add(offset as usize)?,
            size as usize,
        )?;
        Some(Item {
            key,
            offset,
            size,
            data,
        })
    }

    /// The key pointer at `slot`, or `None` past the end or on a leaf.
    #[must_use]
    pub fn key_ptr(&self, slot: u32) -> Option<KeyPtr> {
        if self.is_leaf() || slot >= self.header.nritems {
            return None;
        }
        self.raw_key_ptr(slot)
    }

    /// The key at `slot`, whichever kind of node this is.
    #[must_use]
    pub fn key(&self, slot: u32) -> Option<BtrfsKey> {
        if slot >= self.header.nritems {
            return None;
        }
        if self.is_leaf() {
            self.raw_item(slot).map(|item| item.0)
        } else {
            self.raw_key_ptr(slot).map(|ptr| ptr.key)
        }
    }

    /// Iterate a leaf's items in key order. Empty on an internal node.
    #[must_use]
    pub const fn items(&self) -> ItemIter<'a> {
        ItemIter {
            node: *self,
            slot: 0,
        }
    }

    /// Iterate an internal node's key pointers in key order. Empty on a leaf.
    #[must_use]
    pub const fn key_ptrs(&self) -> KeyPtrIter<'a> {
        KeyPtrIter {
            node: *self,
            slot: 0,
        }
    }

    /// Binary search the node for `key`.
    ///
    /// `Ok(slot)` is an exact match. `Err(slot)` is the insertion point: the
    /// index `key` would occupy, so `Err(0)` means `key` sorts before
    /// everything in the node and `Err(nritems)` after everything.
    ///
    /// This is the same contract as [`slice::binary_search`], and it is written
    /// against the node's own accessors rather than a slice because the entries
    /// are 25 or 33 unaligned bytes rather than a Rust type.
    pub fn search(&self, key: &BtrfsKey) -> Result<u32, u32> {
        let mut low = 0u32;
        let mut high = self.header.nritems;
        while low < high {
            let mid = low + (high - low) / 2;
            match self.key(mid) {
                // A missing key cannot happen after validation; treating it as
                // "search higher" keeps the loop terminating either way.
                None => return Err(low),
                Some(found) => match found.cmp(key) {
                    Ordering::Less => low = mid + 1,
                    Ordering::Greater => high = mid,
                    Ordering::Equal => return Ok(mid),
                },
            }
        }
        Err(low)
    }

    /// The slot to descend into when looking for `key`.
    ///
    /// In an internal node each key is the *lowest* key in its subtree, so the
    /// child that may contain `key` is the last one whose key is `<= key`.
    /// `None` means `key` sorts before the whole subtree and cannot be present.
    #[must_use]
    pub fn search_slot(&self, key: &BtrfsKey) -> Option<u32> {
        match self.search(key) {
            Ok(slot) => Some(slot),
            Err(0) => None,
            Err(slot) => Some(slot - 1),
        }
    }
}

/// Iterator over a leaf's items, produced by [`Node::items`].
#[derive(Debug, Clone, Copy)]
pub struct ItemIter<'a> {
    node: Node<'a>,
    slot: u32,
}

impl<'a> Iterator for ItemIter<'a> {
    type Item = Item<'a>;

    fn next(&mut self) -> Option<Item<'a>> {
        let item = self.node.item(self.slot)?;
        self.slot = self.slot.checked_add(1)?;
        Some(item)
    }
}

/// Iterator over an internal node's key pointers, produced by
/// [`Node::key_ptrs`].
#[derive(Debug, Clone, Copy)]
pub struct KeyPtrIter<'a> {
    node: Node<'a>,
    slot: u32,
}

impl Iterator for KeyPtrIter<'_> {
    type Item = KeyPtr;

    fn next(&mut self) -> Option<KeyPtr> {
        let ptr = self.node.key_ptr(self.slot)?;
        self.slot = self.slot.checked_add(1)?;
        Some(ptr)
    }
}
