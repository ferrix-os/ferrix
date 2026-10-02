//! Block groups, and the space in them: the allocator.
//!
//! A block group is a chunk seen from the allocator's side: a logical range
//! of one kind — data, metadata or system — with a count of bytes in use.
//! For each one this module keeps three sets of ranges, and the whole of
//! crash safety rests on keeping them apart:
//!
//! * `free` — what may be handed out now;
//! * `pinned` — what this transaction freed but the last commit still uses.
//!   Handing it out would let a new node overwrite a node of the committed
//!   tree before the new superblock is down. It returns to `free` only after
//!   the commit ([`Space::unpin`]);
//! * `on_disk` — what the free-space tree says is free. The commit makes the
//!   tree say `free ∪ pinned ∪ reserved`, because once the new superblock is
//!   down the pinned space is free on disk, and writes the difference;
//! * `reserved` — the free part of the group's superblock stripes. Linux
//!   lists them free in the free-space tree and keeps them out of its
//!   in-memory free space (`exclude_super_stripes`, `add_new_free_space`),
//!   so they are never in `free`, but are written back as free.
//!
//! Space allocated in the current transaction is also remembered, because
//! freeing it again before the commit is different: nothing committed ever
//! referred to it, so it goes straight back to `free`.

use alloc::collections::BTreeMap;

use ferrix_btrfs::chunk::{
    BLOCK_GROUP_DATA, BLOCK_GROUP_METADATA, BLOCK_GROUP_SYSTEM, BLOCK_GROUP_TYPE_MASK,
};

use crate::chunks::STRIPE_LEN;
use crate::ranges::RangeSet;
use crate::{Error, Result, fallible};

/// What a block group holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// File data.
    Data,
    /// Tree blocks of every tree but the chunk tree.
    Metadata,
    /// Tree blocks of the chunk tree, which must be readable from the
    /// superblock's system chunk array alone.
    System,
}

impl Kind {
    /// The block group type bit for this kind.
    #[must_use]
    pub const fn bits(self) -> u64 {
        match self {
            Kind::Data => BLOCK_GROUP_DATA,
            Kind::Metadata => BLOCK_GROUP_METADATA,
            Kind::System => BLOCK_GROUP_SYSTEM,
        }
    }

    /// The kind a block group's type bits name, if exactly one.
    #[must_use]
    pub const fn from_bits(bits: u64) -> Option<Kind> {
        match bits & BLOCK_GROUP_TYPE_MASK {
            BLOCK_GROUP_DATA => Some(Kind::Data),
            BLOCK_GROUP_METADATA => Some(Kind::Metadata),
            BLOCK_GROUP_SYSTEM => Some(Kind::System),
            _ => None,
        }
    }

    /// Index into per-kind arrays.
    const fn index(self) -> usize {
        match self {
            Kind::Data => 0,
            Kind::Metadata => 1,
            Kind::System => 2,
        }
    }
}

/// One block group.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(Clone))]
pub struct BlockGroup {
    /// First logical address: the chunk's.
    pub start: u64,
    /// Length: the chunk's.
    pub length: u64,
    /// Type and profile bits, as the item records them.
    pub flags: u64,
    /// Bytes allocated, as the item must record them after the commit.
    pub used: u64,
    /// Whether `used` changed since the item was written.
    pub item_dirty: bool,
    /// Allocatable now.
    pub free: RangeSet,
    /// Freed in this transaction, still referenced by the last commit.
    pub pinned: RangeSet,
    /// What the free-space tree records.
    pub on_disk: RangeSet,
    /// Where superblock copies lie in the group: never allocatable.
    pub excluded: RangeSet,
    /// The part of `excluded` nothing uses, which the free-space tree lists
    /// as free.
    pub reserved: RangeSet,
    /// Whether the free-space tree holds this group as bitmaps.
    pub bitmaps: bool,
}

impl BlockGroup {
    /// Logical end, exclusive.
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.start.saturating_add(self.length)
    }

    /// The kind of space in the group.
    #[must_use]
    pub const fn kind(&self) -> Option<Kind> {
        Kind::from_bits(self.flags)
    }

    /// What the free-space tree must say once this transaction commits.
    pub fn committed_free(&self) -> Result<RangeSet> {
        let mut all = self.free.try_clone()?;
        let _ = all.try_absorb(&self.pinned)?;
        let _ = all.try_absorb(&self.reserved)?;
        Ok(all)
    }

    /// A group whose free-space tree lists `on_disk` as free, held as
    /// extents: all of it allocatable but the superblock stripes in
    /// `excluded`.
    pub fn new(
        start: u64,
        length: u64,
        flags: u64,
        used: u64,
        on_disk: RangeSet,
        excluded: RangeSet,
    ) -> Result<Self> {
        let mut free = on_disk.try_clone()?;
        let reserved = free.try_extract(&excluded)?;
        Ok(BlockGroup {
            start,
            length,
            flags,
            used,
            item_dirty: false,
            free,
            pinned: RangeSet::new(),
            on_disk,
            excluded,
            reserved,
            bitmaps: false,
        })
    }

    /// Make `ranges`, which nothing uses any more, free again, except that
    /// the part in a superblock stripe goes back to `reserved`.
    fn give_back(&mut self, mut ranges: RangeSet) -> Result<bool> {
        let reserved = ranges.try_extract(&self.excluded)?;
        Ok(self.reserved.try_absorb(&reserved)? & self.free.try_absorb(&ranges)?)
    }
}

/// Every block group, and the transaction's allocations.
#[derive(Debug, Default)]
#[cfg_attr(test, derive(Clone))]
pub struct Space {
    groups: BTreeMap<u64, BlockGroup>,
    /// Allocated since the last commit.
    allocated: RangeSet,
    /// Where the last allocation of each kind ended, so the next starts
    /// there: writes land near each other, and freed space is not reused
    /// the moment it is unpinned.
    cursor: [u64; 3],
}

impl Space {
    /// Add a block group found at mount or made by a chunk allocation.
    pub fn insert(&mut self, group: BlockGroup) -> Result<()> {
        if self.groups.contains_key(&group.start) {
            return Err(Error::Inconsistent("block group listed twice"));
        }
        let _ = fallible::insert(&mut self.groups, group.start, group)?;
        Ok(())
    }

    /// Every block group, by start.
    pub fn groups(&self) -> impl Iterator<Item = &BlockGroup> {
        self.groups.values()
    }

    /// Every block group, mutably.
    pub fn groups_mut(&mut self) -> impl Iterator<Item = &mut BlockGroup> {
        self.groups.values_mut()
    }

    /// The group holding `logical`.
    #[must_use]
    pub fn group_of(&self, logical: u64) -> Option<&BlockGroup> {
        let (_, group) = self.groups.range(..=logical).next_back()?;
        (logical < group.end()).then_some(group)
    }

    fn group_of_mut(&mut self, logical: u64) -> Result<&mut BlockGroup> {
        let (_, group) = self
            .groups
            .range_mut(..=logical)
            .next_back()
            .ok_or(Error::Inconsistent("extent outside every block group"))?;
        if logical < group.end() {
            Ok(group)
        } else {
            Err(Error::Inconsistent("extent outside every block group"))
        }
    }

    /// Allocate one tree block of `nodesize` bytes from a group of `kind`.
    ///
    /// Aligned to its size, and never across a 64 KiB stripe boundary, which
    /// Linux's tree-checker refuses for nodes smaller than a stripe.
    pub fn alloc_tree_block(&mut self, kind: Kind, nodesize: u32) -> Result<u64> {
        let size = u64::from(nodesize);
        let boundary = if size < STRIPE_LEN { STRIPE_LEN } else { 0 };
        let from = self.cursor.get(kind.index()).copied().unwrap_or(0);
        let found = self.find(kind, from, |group| {
            group.free.first_fit(size, size, boundary, from)
        });
        let at = found.ok_or(Error::NoSpace)?;
        self.take(kind, at, size)?;
        Ok(at)
    }

    /// Allocate up to `want` bytes of data, at least `min`, aligned to
    /// `align`. Returns `(start, len)`: a fragmented volume gives a shorter
    /// extent, and the caller allocates again for the rest.
    pub fn alloc_data(&mut self, want: u64, min: u64, align: u64) -> Result<(u64, u64)> {
        let from = self.cursor.first().copied().unwrap_or(0);
        let found = self.find(Kind::Data, from, |group| {
            group.free.first_prefix(want, min, align, from)
        });
        let (at, len) = found.ok_or(Error::NoSpace)?;
        self.take(Kind::Data, at, len)?;
        Ok((at, len))
    }

    /// The first answer `fit` gives over the groups of `kind`, starting with
    /// the group holding `from` and wrapping round.
    fn find<T>(&self, kind: Kind, from: u64, fit: impl Fn(&BlockGroup) -> Option<T>) -> Option<T> {
        let matching = |group: &&BlockGroup| group.kind() == Some(kind);
        let after = self
            .groups
            .values()
            .filter(matching)
            .filter(|g| g.end() > from);
        let before = self
            .groups
            .values()
            .filter(matching)
            .filter(|g| g.end() <= from);
        after.chain(before).find_map(fit)
    }

    /// Mark `[at, at + len)` allocated in this transaction.
    fn take(&mut self, kind: Kind, at: u64, len: u64) -> Result<()> {
        let group = self.group_of_mut(at)?;
        if !group.free.try_remove(at, len)? {
            return Err(Error::Inconsistent("allocated space that was not free"));
        }
        if !self.allocated.try_insert(at, len)? {
            return Err(Error::Inconsistent("allocated the same space twice"));
        }
        if let Some(cursor) = self.cursor.get_mut(kind.index()) {
            *cursor = at.saturating_add(len);
        }
        Ok(())
    }

    /// Whether `[at, at + len)` was allocated in this transaction.
    #[must_use]
    pub fn allocated_now(&self, at: u64, len: u64) -> bool {
        self.allocated.contains(at, len)
    }

    /// Take exactly `[at, at + len)`, which must be free and in a group of
    /// `kind`: what a log replay does for the extents the log names, since
    /// the committed trees never learned they were used.
    pub fn reserve(&mut self, at: u64, len: u64, kind: Kind) -> Result<()> {
        if self.group_of(at).and_then(BlockGroup::kind) != Some(kind) {
            return Err(Error::Inconsistent(
                "logged extent is not in a group of its kind",
            ));
        }
        // A log written before superblock stripes were kept out of
        // allocation may name an extent in one. It is in use all the same.
        let group = self.group_of_mut(at)?;
        let mut wanted = RangeSet::new();
        let _ = wanted.try_insert(at, len)?;
        let stripes = group.reserved.try_extract(&wanted)?;
        if !group.free.try_absorb(&stripes)? {
            return Err(Error::Inconsistent("superblock stripe was also free"));
        }
        self.take(kind, at, len)
    }

    /// Give back an extent nothing refers to any more: at once if this
    /// transaction allocated it, after the commit if the last commit uses it.
    pub fn release(&mut self, at: u64, len: u64) -> Result<()> {
        if self.allocated.try_remove(at, len)? {
            let group = self.group_of_mut(at)?;
            let mut released = RangeSet::new();
            let _ = released.try_insert(at, len)?;
            if group.give_back(released)? {
                return Ok(());
            }
            return Err(Error::Inconsistent("released space that was already free"));
        }
        let group = self.group_of_mut(at)?;
        if group.free.overlaps(at, len)
            || group.reserved.overlaps(at, len)
            || !group.pinned.try_insert(at, len)?
        {
            return Err(Error::Inconsistent("freed space that was already free"));
        }
        Ok(())
    }

    /// Count an extent's item in or out of its group's `used`.
    pub fn account(&mut self, at: u64, len: u64, added: bool) -> Result<()> {
        let group = self.group_of_mut(at)?;
        group.used = if added {
            group.used.checked_add(len)
        } else {
            group.used.checked_sub(len)
        }
        .ok_or(Error::Inconsistent("block group usage out of range"))?;
        group.item_dirty = true;
        Ok(())
    }

    /// After a commit: pinned space is free now, and nothing is allocated in
    /// the next transaction yet.
    pub fn unpin(&mut self) -> Result<()> {
        for group in self.groups.values_mut() {
            let pinned = core::mem::take(&mut group.pinned);
            if !group.give_back(pinned)? {
                return Err(Error::Inconsistent("pinned space was also free"));
            }
        }
        self.allocated.clear();
        Ok(())
    }

    /// Bytes in use across every group: the superblock's `bytes_used`.
    #[must_use]
    pub fn used(&self) -> u64 {
        self.groups
            .values()
            .fold(0u64, |sum, group| sum.saturating_add(group.used))
    }

    /// Bytes still allocatable in groups of `kind`.
    #[must_use]
    pub fn free_bytes(&self, kind: Kind) -> u64 {
        self.groups
            .values()
            .filter(|group| group.kind() == Some(kind))
            .fold(0u64, |sum, group| sum.saturating_add(group.free.total()))
    }
}
