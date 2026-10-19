//! Delayed references: extent reference changes, queued until the commit.
//!
//! Copying a tree block adds a reference to the copy and drops one from the
//! original; mapping file data adds one to its extent. Applying each change
//! to the extent tree as it happens would change the extent tree in the
//! middle of changing some other tree, and every change to the extent tree
//! copies extent-tree blocks, which is more reference changes. Linux breaks
//! the recursion by queueing: a change becomes a *delayed ref* on the
//! extent's *head*, and the heads are run in a batch.
//!
//! This queue is the same idea with less machinery. A head is an extent's
//! address, its size, its level if it is a tree block, and the net change to
//! each of its references. Changes that cancel — a block copied and then
//! freed in one transaction — cancel here and never reach the tree.

use alloc::collections::BTreeMap;

use crate::extent::Backref;
use crate::{Error, Result, fallible};

/// The pending changes to one extent's references.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(Clone))]
pub struct Head {
    /// The extent's length.
    pub num_bytes: u64,
    /// The tree block's level; `None` for file data.
    pub level: Option<u8>,
    /// Net change per reference.
    pub deltas: BTreeMap<Backref, i64>,
}

/// Heads by extent address.
#[derive(Debug, Default)]
#[cfg_attr(test, derive(Clone))]
pub struct DelayedRefs {
    heads: BTreeMap<u64, Head>,
}

impl DelayedRefs {
    /// Make sure `bytenr` has a head, so it is run even with no change: a
    /// freshly allocated extent nothing ends up referring to must still be
    /// given back.
    pub fn touch(&mut self, bytenr: u64, num_bytes: u64, level: Option<u8>) -> Result<()> {
        let head = fallible::entry(&mut self.heads, bytenr, || Head {
            num_bytes,
            level,
            deltas: BTreeMap::new(),
        })?;
        if head.num_bytes != num_bytes || head.level != level {
            return Err(Error::Inconsistent("one extent queued with two sizes"));
        }
        Ok(())
    }

    /// Queue a change of `delta` to `backref` on the extent at `bytenr`.
    pub fn add(
        &mut self,
        bytenr: u64,
        num_bytes: u64,
        level: Option<u8>,
        backref: Backref,
        delta: i64,
    ) -> Result<()> {
        self.touch(bytenr, num_bytes, level)?;
        let head = self
            .heads
            .get_mut(&bytenr)
            .ok_or(Error::Inconsistent("delayed ref head vanished"))?;
        let entry = fallible::entry(&mut head.deltas, backref, || 0)?;
        *entry = entry
            .checked_add(delta)
            .ok_or(Error::Inconsistent("delayed ref overflow"))?;
        if *entry == 0 {
            let _ = head.deltas.remove(&backref);
        }
        Ok(())
    }

    /// Take the head with the lowest address.
    pub fn pop(&mut self) -> Option<(u64, Head)> {
        self.heads.pop_first()
    }

    /// Whether nothing is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.heads.is_empty()
    }

    /// Drop everything queued: the transaction is being thrown away.
    pub fn clear(&mut self) {
        self.heads.clear();
    }
}
