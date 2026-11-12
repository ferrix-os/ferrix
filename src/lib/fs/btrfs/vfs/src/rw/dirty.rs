//! A file's dirty set and the guard every change of it goes through
//! (`docs/RSS-COUNTS.md` §9.4): its fields are this module's alone, so the
//! store-then-tell of [`DirtyGuard`]'s drop is the only way to change it.

use alloc::collections::BTreeSet;
use alloc::sync::{Arc, Weak};
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_vfs::tmpfs::Pages;

use crate::SpinLock;

/// A file's pages written and not yet turned into extents, and the length
/// the page cache reads of it (`docs/RSS-COUNTS.md` §9.4).
///
/// The set is reached only through [`DirtySet::lock`], whose guard, as it
/// is dropped, stores a changed length in [`DirtySet::count`] while the set
/// is still held -- so that a later holder's store can never be overwritten
/// by an earlier one's -- and then, the set let go, tells the store, which
/// reads the count again under its own lock. So every change the six places
/// that change the set make (a write's marks, a truncation's cut, a
/// writeback slice's take and its failed return, a full writeback's take and
/// its failed return) is told, and none of them says so itself.
pub(crate) struct DirtySet {
    /// The pages.
    set: SpinLock<BTreeSet<u64>>,
    /// How many there are, as the last holder left them.
    count: AtomicU64,
    /// The file's page cache, told of a change: held weakly, as its source
    /// holds this set.
    store: SpinLock<Option<Weak<dyn Pages>>>,
}

impl DirtySet {
    /// Empty, telling nobody.
    pub(super) fn new() -> DirtySet {
        DirtySet {
            set: SpinLock::new(BTreeSet::new()),
            count: AtomicU64::new(0),
            store: SpinLock::new(None),
        }
    }

    /// The set, held.
    pub(super) fn lock(&self) -> DirtyGuard<'_> {
        let set = self.set.lock();
        let before = set.len();
        DirtyGuard {
            owner: self,
            set: Some(set),
            before,
            spare: BTreeSet::new(),
        }
    }

    /// How many pages are in it, as its last holder left it.
    pub(super) fn len(&self) -> u64 {
        self.count.load(Ordering::Acquire)
    }

    /// Tell `store` of every change from now on.
    pub(super) fn tell(&self, store: &Arc<dyn Pages>) {
        *self.store.lock() = Some(Arc::downgrade(store));
    }
}

impl fmt::Debug for DirtySet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirtySet")
            .field("count", &self.len())
            .finish_non_exhaustive()
    }
}

/// [`DirtySet::lock`]'s guard.
pub(super) struct DirtyGuard<'a> {
    /// Whose set.
    owner: &'a DirtySet,
    /// The set's own guard, until the drop takes it.
    set: Option<ferrix_sync::PreemptSpinLockGuard<'a, BTreeSet<u64>, ferrix_sync::HookedPreempt>>,
    /// Its length when taken.
    before: usize,
    /// What the guard derefs to once the drop has taken the set: never
    /// seen, as nothing derefs a guard being dropped. Empty, so it holds no
    /// heap.
    spare: BTreeSet<u64>,
}

impl core::ops::Deref for DirtyGuard<'_> {
    type Target = BTreeSet<u64>;

    fn deref(&self) -> &BTreeSet<u64> {
        self.set.as_deref().unwrap_or(&self.spare)
    }
}

impl core::ops::DerefMut for DirtyGuard<'_> {
    fn deref_mut(&mut self) -> &mut BTreeSet<u64> {
        match self.set.as_deref_mut() {
            Some(set) => set,
            None => &mut self.spare,
        }
    }
}

impl Drop for DirtyGuard<'_> {
    /// Store a changed length while the set is held, let it go, then tell
    /// the store with no lock of this file's held.
    fn drop(&mut self) {
        let Some(set) = self.set.take() else {
            return;
        };
        let now = set.len();
        let changed = now != self.before;
        if changed {
            self.owner.count.store(now as u64, Ordering::Release);
        }
        drop(set);
        if changed {
            let store = self.owner.store.lock().as_ref().and_then(Weak::upgrade);
            if let Some(store) = store {
                store.dirty_changed();
            }
        }
    }
}
