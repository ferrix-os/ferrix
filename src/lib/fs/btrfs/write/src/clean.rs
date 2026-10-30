//! The clean nodes: tree nodes read from the disk and not changed.
//!
//! They are only a cache of what is on the disk, so dropping one costs a
//! read, never correctness -- but only between operations. A walk down a
//! tree loads one node per level and keeps their addresses in its path, and
//! goes back to them afterwards: `next_item` climbs the path to find the
//! next leaf. A node dropped under the walk is one it can no longer read
//! (finding F-99: `load` cleared the whole cache when it was full, between
//! a path's levels, and range scans ended early). So nothing here is dropped
//! by a load; the cache is brought back to its size between operations
//! ([`WriteVolume::trim_clean`]), and given up to a reclaim the same way
//! ([`WriteVolume::drop_clean`]), by whoever holds the volume between them.
//!
//! The nodes are kept in the order they were loaded, as the read-only
//! mount's `NodeCache` keeps its reads: a map by address and a queue of
//! addresses, so the oldest is the front of the queue. A node can leave the
//! map from the middle -- copied by a transaction, or a log's block dropped
//! -- and leaves its place in the queue behind; each place carries the stamp
//! its node was loaded with, so a place whose node left, or was loaded again
//! since, is passed over. The queue is closed up when it holds more stale
//! places than live ones.
//!
//! [`WriteVolume::trim_clean`]: crate::WriteVolume::trim_clean
//! [`WriteVolume::drop_clean`]: crate::WriteVolume::drop_clean

use alloc::collections::{BTreeMap, VecDeque};

use crate::node::TreeNode;
use crate::{Result, fallible};

/// One clean node, with the stamp of its load and what it holds of the heap.
#[derive(Debug)]
struct Held {
    stamp: u64,
    bytes: u64,
    node: TreeNode,
}

/// Clean nodes by logical address, oldest first; see the module.
#[derive(Debug)]
pub(crate) struct CleanNodes {
    nodes: BTreeMap<u64, Held>,
    /// Each load's stamp and address, in the order of the loads.
    order: VecDeque<(u64, u64)>,
    /// The stamp the next load gets.
    next: u64,
    /// What the nodes hold of the heap, kept as they come and go: the sum
    /// of [`TreeNode::heap_bytes`] over `nodes`, exactly.
    bytes: u64,
}

impl CleanNodes {
    /// No nodes.
    pub(crate) const fn new() -> CleanNodes {
        CleanNodes {
            nodes: BTreeMap::new(),
            order: VecDeque::new(),
            next: 0,
            bytes: 0,
        }
    }

    /// The node at `logical`, if it is held.
    pub(crate) fn get(&self, logical: &u64) -> Option<&TreeNode> {
        self.nodes.get(logical).map(|held| &held.node)
    }

    /// How many nodes are held.
    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    /// What the nodes hold of the heap.
    pub(crate) const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Keep `node`, read at `logical`, as the newest. On running out of
    /// memory nothing changes.
    pub(crate) fn insert(&mut self, logical: u64, node: TreeNode) -> Result<()> {
        fallible::reserve_deque(&mut self.order, 1)?;
        let stamp = self.next;
        let bytes = node.heap_bytes();
        let held = Held { stamp, bytes, node };
        if let Some(old) = fallible::insert(&mut self.nodes, logical, held)? {
            self.bytes = self.bytes.saturating_sub(old.bytes);
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.next = stamp.wrapping_add(1);
        // NOALLOC: the room was reserved above.
        self.order.push_back((stamp, logical));
        self.close_up();
        Ok(())
    }

    /// Take the node at `logical` out, if it is held.
    pub(crate) fn remove(&mut self, logical: &u64) -> Option<TreeNode> {
        let held = self.nodes.remove(logical)?;
        self.bytes = self.bytes.saturating_sub(held.bytes);
        self.close_up();
        Some(held.node)
    }

    /// Drop the oldest node, answering what it held of the heap; `None`
    /// when there is none.
    pub(crate) fn drop_oldest(&mut self) -> Option<u64> {
        while let Some((stamp, logical)) = self.order.pop_front() {
            if self
                .nodes
                .get(&logical)
                .is_some_and(|held| held.stamp == stamp)
            {
                let held = self.nodes.remove(&logical)?;
                self.bytes = self.bytes.saturating_sub(held.bytes);
                return Some(held.bytes);
            }
        }
        None
    }

    /// The sum the count keeps, made again from the nodes, for the tests.
    #[cfg(test)]
    pub(crate) fn recount(&self) -> u64 {
        self.nodes.values().map(|held| held.node.heap_bytes()).sum()
    }

    /// Drop the places in the queue whose nodes left, once they outnumber
    /// the live ones; allocates nothing.
    fn close_up(&mut self) {
        if self.order.len() > self.nodes.len().saturating_mul(2).saturating_add(16) {
            let nodes = &self.nodes;
            self.order.retain(|&(stamp, logical)| {
                nodes.get(&logical).is_some_and(|held| held.stamp == stamp)
            });
        }
    }
}
