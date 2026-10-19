//! A bounded cache of what a mount reads as metadata: the superblock and tree
//! nodes.
//!
//! Every lookup descends a tree from its root, so the same few nodes are read on
//! every call. On a read-only volume the bytes at a physical address never
//! change, so a node read once can be handed out again without asking the
//! device. Only reads `ferrix-btrfs` marks [`ReadKind::Metadata`] are kept: file
//! data belongs in the page cache, and the two never hold the same bytes.
//!
//! A hit returns exactly the bytes the device would, so nothing downstream
//! trusts it more: `ferrix-btrfs` still checks every node's level, generation,
//! fsid and checksum against what its parent promised.
//!
//! Entries are shared `Arc<[u8]>`s. A hit takes a reference under the lock and
//! copies with the lock released; a miss reads the device with no lock held and
//! keeps the bytes only if no racing reader already did. Eviction is CLOCK over
//! a fixed number of entries. Stage 12, which writes, must invalidate by
//! physical range before a cache like this sits under a writable volume.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use core::fmt;

use ferrix_btrfs::BtrfsError;
use ferrix_btrfs::volume::{Device, ReadKind};

use crate::SpinLock;

/// How many metadata reads a mount keeps: 4 to 16 MiB at 4 to 16 KiB a node.
pub(crate) const ENTRIES: usize = 1024;

/// One cached read.
struct Entry {
    bytes: Arc<[u8]>,
    /// Set by a hit and cleared as the clock hand passes: CLOCK's second
    /// chance.
    referenced: bool,
}

/// The cache's state, behind a spin lock held only to look up or insert.
struct Table {
    /// Cached reads by physical address. A read of another length at the same
    /// address misses, and its bytes replace the entry.
    entries: BTreeMap<u64, Entry>,
    /// The addresses in the order the clock hand reaches them.
    clock: VecDeque<u64>,
    capacity: usize,
    hits: u64,
    misses: u64,
}

impl Table {
    /// Drop one entry that has not been hit since the hand last passed it.
    ///
    /// Two sweeps are always enough: the first clears every second chance.
    fn evict(&mut self) {
        let sweeps = self.clock.len().saturating_mul(2).saturating_add(1);
        for _ in 0..sweeps {
            let Some(address) = self.clock.pop_front() else {
                return;
            };
            match self.entries.get_mut(&address) {
                Some(entry) if entry.referenced => {
                    entry.referenced = false;
                    self.clock.push_back(address);
                }
                Some(_) => {
                    let _evicted = self.entries.remove(&address);
                    return;
                }
                // An address whose entry is already gone: nothing to keep.
                None => {}
            }
        }
    }
}

/// A bounded CLOCK cache of metadata reads, shared by every handle of a mount.
pub(crate) struct NodeCache {
    table: SpinLock<Table>,
}

impl NodeCache {
    /// A cache holding at most `capacity` reads, and at least one.
    pub(crate) fn new(capacity: usize) -> NodeCache {
        NodeCache {
            table: SpinLock::new(Table {
                entries: BTreeMap::new(),
                clock: VecDeque::new(),
                capacity: capacity.max(1),
                hits: 0,
                misses: 0,
            }),
        }
    }

    /// The bytes held for a read of `len` bytes at `physical`, if any.
    pub(crate) fn get(&self, physical: u64, len: usize) -> Option<Arc<[u8]>> {
        let mut table = self.table.lock();
        let found = match table.entries.get_mut(&physical) {
            Some(entry) if entry.bytes.len() == len => {
                entry.referenced = true;
                Some(Arc::clone(&entry.bytes))
            }
            _ => None,
        };
        if found.is_some() {
            table.hits = table.hits.saturating_add(1);
        } else {
            table.misses = table.misses.saturating_add(1);
        }
        found
    }

    /// Keep `bytes`, read at `physical`, unless a racing reader already did.
    /// The copy is made before the lock is taken.
    pub(crate) fn insert(&self, physical: u64, bytes: &[u8]) {
        let copy: Arc<[u8]> = Arc::from(bytes);
        let mut table = self.table.lock();
        if let Some(entry) = table.entries.get_mut(&physical) {
            if entry.bytes.len() != copy.len() {
                entry.bytes = copy;
                entry.referenced = false;
            }
            return;
        }
        if table.entries.len() >= table.capacity {
            table.evict();
        }
        let _previous = table.entries.insert(
            physical,
            Entry {
                bytes: copy,
                referenced: false,
            },
        );
        table.clock.push_back(physical);
    }

    /// Hits, misses and entries held, for tests and diagnostics.
    pub(crate) fn stats(&self) -> (u64, u64, usize) {
        let table = self.table.lock();
        (table.hits, table.misses, table.entries.len())
    }
}

impl fmt::Debug for NodeCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (hits, misses, entries) = self.stats();
        f.debug_struct("NodeCache")
            .field("entries", &entries)
            .field("hits", &hits)
            .field("misses", &misses)
            .finish()
    }
}

/// A mount's device handle with the mount's metadata cache in front of it.
pub(crate) struct Cached<D> {
    device: D,
    cache: Arc<NodeCache>,
}

impl<D> Cached<D> {
    /// Put `cache` in front of `device`.
    pub(crate) const fn new(device: D, cache: Arc<NodeCache>) -> Cached<D> {
        Cached { device, cache }
    }
}

impl<D: Clone> Clone for Cached<D> {
    fn clone(&self) -> Self {
        Cached {
            device: self.device.clone(),
            cache: Arc::clone(&self.cache),
        }
    }
}

impl<D> fmt::Debug for Cached<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cached")
            .field("cache", &self.cache)
            .finish_non_exhaustive()
    }
}

impl<D: Device> Device for Cached<D> {
    fn read_at(&mut self, physical: u64, buf: &mut [u8], kind: ReadKind) -> Result<(), BtrfsError> {
        if kind != ReadKind::Metadata {
            return self.device.read_at(physical, buf, kind);
        }
        if let Some(bytes) = self.cache.get(physical, buf.len()) {
            buf.copy_from_slice(&bytes);
            return Ok(());
        }
        self.device.read_at(physical, buf, kind)?;
        self.cache.insert(physical, buf);
        Ok(())
    }
}
