//! Names, as opposed to the things they name.
//!
//! An inode does not know what it is called. A hard link means it is called
//! several things; a rename means what it is called changes while it stays
//! the same object; and a mount point means a name in one filesystem leads to
//! the root of another. The dentry is where all three are recorded, which is
//! why `..`, `getcwd` and `/proc/self/fd` are answered from dentries and never
//! from inodes.
//!
//! # Negative entries
//!
//! A dentry with no inode records that a name does *not* exist. A compiler
//! probes dozens of include directories for every header and misses in most
//! of them, so the miss is the common case and is worth caching as much as
//! the hit — `docs/ARCHITECTURE.md` §8 names this as a performance
//! requirement. It is also what `open(O_CREAT)` fills in: the walk finds the
//! negative dentry, the filesystem creates the file, and the dentry becomes
//! positive in place.
//!
//! # Ownership
//!
//! A child holds its parent strongly, so `..` always works from anything a
//! program holds. A parent holds its children *weakly*, so the tree does not
//! keep itself alive: a dentry nobody refers to goes away. What keeps recently
//! used ones is the namespace's cache, a bounded queue of strong references,
//! which is the whole of the eviction policy and is replaceable without
//! touching anything here.
//!
//! # The generation, and the race it closes
//!
//! A lookup that misses the cache asks the filesystem with no lock held, then
//! inserts what it learned. A create that completes in between would have its
//! result overwritten by a stale negative entry, and the file would be
//! invisible until the entry was evicted. So every change to a directory's
//! names bumps its generation under the children lock, and a lookup inserts
//! only if the generation it read before asking is still current.
//!
//! A lookup that loses the race asks again. It must not keep its answer as a
//! dentry of its own, even one it only uses once: a name then has two live
//! dentries, and every rule kept by moving a dentry goes wrong for the one
//! nobody moves. A rename leaves its parent where it was, so `..` and
//! `getcwd` answer from the old place, an `rmdir` leaves it looking hashed,
//! and — the reason this is not merely cosmetic — the rename check that a
//! directory is not being moved into its own subtree climbs stale parents and
//! lets it through. Each retry means another operation changed a name in the
//! directory meanwhile, so the system as a whole makes progress; Linux's
//! `d_alloc_parallel` retries on its directory sequence count the same way.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use core::fmt;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use ferrix_kmem::{Charge, arc_footprint, footprint};

use crate::Result;
use crate::SpinLock;
use crate::node::Inode;

/// Source of dentry identifiers, which the mount table keys on.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// [`Dentry::kept_at`] of a dentry not in the kept set.
pub(crate) const NOT_KEPT: usize = usize::MAX;

/// One name in the tree.
pub struct Dentry {
    /// Unique for the life of the system, so the mount table can name a mount
    /// point without holding a pointer's address as a key.
    id: u64,
    /// The mutable part: what it is called, where, and what it names.
    state: SpinLock<State>,
    /// Children looked up so far, weakly.
    children: SpinLock<BTreeMap<Box<[u8]>, Weak<Dentry>>>,
    /// Bumped by every change to this directory's names; see the module
    /// documentation.
    generation: AtomicU64,
    /// How many mounts sit on this dentry. Checked on every step of every
    /// walk, so it is a counter rather than a question for the mount table.
    mounts: AtomicU32,
    /// Its place in the namespace's set of dentries kept for their files'
    /// cached pages, or [`NOT_KEPT`]: read and written only under that set's
    /// lock (`namespace::Paged`), so that taking one out is one look.
    pub(crate) kept_at: AtomicUsize,
    /// The kernel heap it holds, charged to the job whose walk made it, as
    /// Linux charges a dentry (`SLAB_ACCOUNT`): itself, its name, and its
    /// entry in its parent's children (F-37). A cached dentry nobody holds
    /// stays charged until the cache lets it go.
    _charge: Charge,
}

/// The parts of a dentry a rename or an unlink changes.
struct State {
    name: Box<[u8]>,
    parent: Option<Arc<Dentry>>,
    inode: Option<Arc<dyn Inode>>,
    /// Removed from its parent: unlinked, or replaced by a rename. Whoever
    /// still holds it keeps the inode, but no walk will find it again.
    unhashed: bool,
}

impl Dentry {
    /// A root: no parent, and an inode.
    ///
    /// # Errors
    ///
    /// `ENOMEM` past the job's memory limit.
    pub(crate) fn root(inode: Arc<dyn Inode>) -> Result<Arc<Dentry>> {
        Dentry::new(Box::from(&b"/"[..]), None, Some(inode))
    }

    /// A namespace's root, made at boot and never charged: `Namespace::new`
    /// cannot fail.
    pub(crate) fn uncharged_root(inode: Arc<dyn Inode>) -> Arc<Dentry> {
        Arc::new(Dentry {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            state: SpinLock::new(State {
                name: Box::from(&b"/"[..]),
                parent: None,
                inode: Some(inode),
                unhashed: false,
            }),
            children: SpinLock::new(BTreeMap::new()),
            generation: AtomicU64::new(0),
            mounts: AtomicU32::new(0),
            kept_at: AtomicUsize::new(NOT_KEPT),
            _charge: Charge::none(),
        })
    }

    /// A root with a name of its own, for an object in no tree: see
    /// `Location::detached`.
    ///
    /// # Errors
    ///
    /// `ENOMEM` past the job's memory limit.
    pub(crate) fn named_root(name: Box<[u8]>, inode: Arc<dyn Inode>) -> Result<Arc<Dentry>> {
        Dentry::new(name, None, Some(inode))
    }

    /// What a dentry called `name` holds of the heap: itself, its name, and
    /// for one with a parent, the name and link its parent's children keep.
    fn footprint(name: usize, child: bool) -> usize {
        let own = arc_footprint::<Dentry>().saturating_add(footprint(name, 1));
        if child {
            own.saturating_add(footprint(name, 1))
                .saturating_add(size_of::<(Box<[u8]>, Weak<Dentry>)>())
        } else {
            own
        }
    }

    fn new(
        name: Box<[u8]>,
        parent: Option<Arc<Dentry>>,
        inode: Option<Arc<dyn Inode>>,
    ) -> Result<Arc<Dentry>> {
        let charge = crate::charge(Dentry::footprint(name.len(), parent.is_some()))?;
        Ok(Arc::new(Dentry {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            state: SpinLock::new(State {
                name,
                parent,
                inode,
                unhashed: false,
            }),
            children: SpinLock::new(BTreeMap::new()),
            generation: AtomicU64::new(0),
            mounts: AtomicU32::new(0),
            kept_at: AtomicUsize::new(NOT_KEPT),
            _charge: charge,
        }))
    }

    /// Unique identifier.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// What this dentry is called in its parent.
    #[must_use]
    pub fn name(&self) -> Box<[u8]> {
        self.state.lock().name.clone()
    }

    /// The directory holding it, or `None` for a filesystem's root.
    #[must_use]
    pub fn parent(&self) -> Option<Arc<Dentry>> {
        self.state.lock().parent.clone()
    }

    /// What it names, or `None` if the name does not exist.
    #[must_use]
    pub fn inode(&self) -> Option<Arc<dyn Inode>> {
        self.state.lock().inode.clone()
    }

    /// Whether it has been removed from the tree.
    #[must_use]
    pub fn is_unhashed(&self) -> bool {
        self.state.lock().unhashed
    }

    /// Whether anything is mounted on it.
    pub(crate) fn is_mountpoint(&self) -> bool {
        self.mounts.load(Ordering::Relaxed) != 0
    }

    pub(crate) fn add_mount(&self) {
        let _ = self.mounts.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn remove_mount(&self) {
        let _ = self.mounts.fetch_sub(1, Ordering::Relaxed);
    }

    /// The cached child called `name`, if one is alive.
    pub(crate) fn cached_child(&self, name: &[u8]) -> Option<Arc<Dentry>> {
        self.children.lock().get(name).and_then(Weak::upgrade)
    }

    /// The generation a lookup must see unchanged to cache its answer.
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Record what a filesystem lookup found, if nothing changed meanwhile.
    ///
    /// Returns the dentry the caller should use, and whether it is new: one
    /// that raced in ahead of it if there is one, otherwise a new one, cached.
    /// `None` if `generation` is no longer current, when the answer may be
    /// stale and the caller must look again; see the module documentation for
    /// why it gets no dentry at all.
    ///
    /// The caller keeps its own reference to `inode`, so that what is dropped
    /// here under the children lock is never the last one.
    ///
    /// # Errors
    ///
    /// `ENOMEM` past the job's memory limit, with nothing cached.
    pub(crate) fn insert_looked_up(
        self: &Arc<Self>,
        name: &[u8],
        inode: Option<Arc<dyn Inode>>,
        generation: u64,
    ) -> Result<Option<(Arc<Dentry>, bool)>> {
        let mut children = self.children.lock();
        if let Some(existing) = children.get(name).and_then(Weak::upgrade) {
            return Ok(Some((existing, false)));
        }
        if self.generation() != generation {
            return Ok(None);
        }
        let child = Dentry::new(Box::from(name), Some(Arc::clone(self)), inode)?;
        children.retain(|_, weak| weak.strong_count() > 0);
        let _ = children.insert(Box::from(name), Arc::downgrade(&child));
        Ok(Some((child, true)))
    }

    /// A child for what a lookup found, known to this walk alone.
    ///
    /// For a directory that does not cache lookups: the dentry holds its
    /// parent, so `..` and `path_of` work from it, but no later walk will find
    /// it and it goes away with whoever holds it.
    ///
    /// # Errors
    ///
    /// `ENOMEM` past the job's memory limit.
    pub(crate) fn uncached_child(
        self: &Arc<Self>,
        name: &[u8],
        inode: Option<Arc<dyn Inode>>,
    ) -> Result<Arc<Dentry>> {
        Dentry::new(Box::from(name), Some(Arc::clone(self)), inode)
    }

    /// A name was created in this directory: make its dentry positive.
    ///
    /// `walked` is the (negative) dentry the caller's walk found. If a cached
    /// one exists under the name it is updated instead, so every holder sees
    /// the new inode, and that is the one returned: a caller keeping a dentry
    /// for the new name — `open(O_CREAT)` — must keep the one later walks
    /// find, and not `walked`, which a rename may have unhashed meanwhile.
    pub(crate) fn fill(
        self: &Arc<Self>,
        name: &[u8],
        walked: &Arc<Dentry>,
        inode: Arc<dyn Inode>,
    ) -> Arc<Dentry> {
        // Whatever the two dentries held before, dropped after the locks: an
        // inode's last reference may run a filesystem's `Drop`, which these
        // locks, keeping their holder on its processor, must not wait for.
        let (target, replaced) = {
            let mut children = self.children.lock();
            let target = children
                .get(name)
                .and_then(Weak::upgrade)
                .unwrap_or_else(|| Arc::clone(walked));
            let first = {
                let mut state = target.state.lock();
                state.unhashed = false;
                state.inode.replace(Arc::clone(&inode))
            };
            let second = if Arc::ptr_eq(&target, walked) {
                None
            } else {
                walked.state.lock().inode.replace(inode)
            };
            let _ = children.insert(Box::from(name), Arc::downgrade(&target));
            let _ = self.generation.fetch_add(1, Ordering::Relaxed);
            (target, [first, second])
        };
        drop(replaced);
        target
    }

    /// A name was removed from this directory: unhash whatever held it.
    pub(crate) fn remove_name(&self, name: &[u8]) {
        // Dropped after the lock, for the reason `fill` gives: this may be the
        // child's last reference.
        let removed = {
            let mut children = self.children.lock();
            let child = children.remove(name).and_then(|weak| weak.upgrade());
            if let Some(child) = &child {
                child.state.lock().unhashed = true;
            }
            child
        };
        let _ = self.generation.fetch_add(1, Ordering::Relaxed);
        drop(removed);
    }

    /// Move `child` from this directory to `new_parent` under `new_name`.
    ///
    /// The caller holds the namespace's rename lock, so no other move is in
    /// progress, and has already made the filesystem agree.
    pub(crate) fn move_child(
        self: &Arc<Self>,
        old_name: &[u8],
        child: &Arc<Dentry>,
        new_parent: &Arc<Dentry>,
        new_name: &[u8],
    ) {
        self.remove_name(old_name);
        new_parent.remove_name(new_name);
        {
            let mut state = child.state.lock();
            state.name = Box::from(new_name);
            state.parent = Some(Arc::clone(new_parent));
            state.unhashed = false;
        }
        let mut children = new_parent.children.lock();
        let _ = children.insert(Box::from(new_name), Arc::downgrade(child));
        let _ = new_parent.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether `self` is `other` or one of its ancestors, within one
    /// filesystem.
    pub(crate) fn is_ancestor_of(self: &Arc<Self>, other: &Arc<Dentry>) -> bool {
        let mut at = Some(Arc::clone(other));
        while let Some(dentry) = at {
            if Arc::ptr_eq(self, &dentry) {
                return true;
            }
            at = dentry.parent();
        }
        false
    }
}

impl Drop for Dentry {
    /// Let go of its parent, and of each ancestor only it kept, one after
    /// another.
    ///
    /// A child holds its parent, so the last hold on the deepest dentry of a
    /// chain nobody else holds used to drop its parent inside its own drop,
    /// and that one its parent, a frame each: a directory 200 deep in a tmpfs
    /// torn down at `umount` -- or at power-off, left in `/tmp` -- ran off the
    /// end of the kernel's stack and double-faulted. Here each ancestor is
    /// taken out of the chain before it goes, so it has no parent left to
    /// drop, and the stack stays one frame deep however long the chain.
    fn drop(&mut self) {
        let mut next = self.state.get_mut().parent.take();
        while let Some(parent) = next {
            // `None` when someone else still holds it: the chain ends here.
            next =
                Arc::into_inner(parent).and_then(|mut parent| parent.state.get_mut().parent.take());
        }
    }
}

impl fmt::Debug for Dentry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.lock();
        f.debug_struct("Dentry")
            .field("id", &self.id)
            .field("name", &alloc::string::String::from_utf8_lossy(&state.name))
            .field("positive", &state.inode.is_some())
            .field("unhashed", &state.unhashed)
            .finish_non_exhaustive()
    }
}
