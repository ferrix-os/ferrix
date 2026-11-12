//! The resident counts an address space keeps as pages come and go
//! (`docs/RSS-COUNTS.md`, `L.user.236`, `L.user.237`).
//!
//! A space's `VmRSS` and its three parts are each region's object's
//! committed pages inside the region's range (`L.user.171`). That depends on
//! two things that change apart: which page indexes of an object the space's
//! regions cover, which the space changes under its own lock, and which of
//! them the object holds, which anyone may change under the object's pages
//! lock. So the count is kept where the two meet, in the object, under its
//! pages lock:
//!
//! * every object keeps a [`View`] per space, id and kind that names it: how
//!   many of that id's regions cover each index (its [`Cover`]), and, for
//!   the file of a private file mapping, the indexes the mapping's shadow
//!   holds a copy of, which the region shows instead of the file's page (its
//!   mask);
//! * a page that comes or goes ([`Frames`], the one type an object's page set
//!   changes through) moves each view's count by how often the view covers
//!   it, unless masked;
//! * a region that comes or goes ([`Frames::cover_step`]) moves the cover,
//!   and the count by the committed, unmasked pages in its range, at most
//!   [`COUNT_CHUNK`] of them a hold.
//!
//! Every move is one atomic add, made inside the object's pages lock after
//! the change it accounts for, so each count is exact between changes and
//! never below zero; a reader's three loads may see an operation in progress
//! partly done (`docs/RSS-COUNTS.md` §6).
//!
//! # Lock order
//!
//! A shadow's change to its page set moves its file's mask under the file's
//! pages lock, taken inside the shadow's ([`Vmo::with_frames_nested`]): a
//! shadow's pages lock, then its file's, and never two pages locks otherwise.
//! Check builds assert it at every take of a pages lock.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Deref;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_frame::Frame;
use ferrix_kmem::Charge;
use ferrix_vma::{Cover, CoverError, UNDO_ROOM};

use crate::fallible::{self, AllocError, Reserved};
use crate::user::space::Resident;
use crate::user::vmo::{COUNT_CHUNK, Vmo};

/// A space's resident pages, by kind: what `/proc/<pid>/status` and `stat`
/// read, three loads and no lock.
#[derive(Debug, Default)]
pub(crate) struct Rss {
    /// `RssAnon`: private memory and a private file mapping's copies.
    anon: AtomicU64,
    /// `RssFile`: a disk file's pages.
    file: AtomicU64,
    /// `RssShmem`: shared anonymous memory and a memory filesystem's file.
    shmem: AtomicU64,
}

/// Which of [`Rss`]'s counts a region's pages go to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `RssAnon`.
    Anon,
    /// `RssFile`.
    File,
    /// `RssShmem`.
    Shmem,
}

impl Rss {
    /// Nothing resident.
    pub(crate) const fn new() -> Rss {
        Rss {
            anon: AtomicU64::new(0),
            file: AtomicU64::new(0),
            shmem: AtomicU64::new(0),
        }
    }

    /// The count `kind` names.
    fn slot(&self, kind: Kind) -> &AtomicU64 {
        match kind {
            Kind::Anon => &self.anon,
            Kind::File => &self.file,
            Kind::Shmem => &self.shmem,
        }
    }

    /// The three counts, each as it stands: no lock, no walk.
    pub(crate) fn read(&self) -> Resident {
        Resident {
            anon: self.anon.load(Ordering::Relaxed),
            file: self.file.load(Ordering::Relaxed),
            shmem: self.shmem.load(Ordering::Relaxed),
        }
    }

    /// `pages` more of `kind`.
    fn add(&self, kind: Kind, pages: u64) {
        if pages != 0 {
            let _ = self.slot(kind).fetch_add(pages, Ordering::Relaxed);
        }
    }

    /// `pages` fewer of `kind`. Never more than it holds: check builds stop
    /// on it rather than wrap.
    fn take(&self, kind: Kind, pages: u64) {
        if pages == 0 {
            return;
        }
        let _before = self.slot(kind).fetch_sub(pages, Ordering::Relaxed);
        #[cfg(ferrix_checks = "on")]
        if _before < pages {
            crate::panic::fatal!(
                crate::panic::catalog::RSS_COUNT_BROKEN,
                "kept resident counts: a kept resident count would fall below zero ({_before} less {pages})"
            );
        }
    }

    /// Move `kind` by `pages`, up or down.
    fn shift(&self, kind: Kind, pages: u64, up: bool) {
        if up {
            self.add(kind, pages);
        } else {
            self.take(kind, pages);
        }
    }
}

/// Which view of an object: the space's counts, the id its regions name the
/// object by, and the kind they count its pages as. The `Arc` keeps the
/// counts alive while a view names them, so a freed space's address can never
/// name another's.
#[derive(Debug, Clone)]
pub(crate) struct ViewKey {
    /// The space's counts.
    pub(crate) rss: Arc<Rss>,
    /// The id.
    pub(crate) id: u64,
    /// The kind.
    pub(crate) kind: Kind,
}

impl ViewKey {
    /// Whether `view` is this one's.
    fn names(&self, view: &View) -> bool {
        Arc::ptr_eq(&self.rss, &view.rss) && self.id == view.id && self.kind == view.kind
    }
}

/// One space's view of an object by one id: see the module.
#[derive(Debug)]
pub(crate) struct View {
    /// The space's counts.
    rss: Arc<Rss>,
    /// The id.
    id: u64,
    /// What its pages count as.
    kind: Kind,
    /// How many of the id's regions cover each index.
    cover: Cover,
    /// For a private file mapping's file: the indexes its shadow holds,
    /// counted there as anonymous and not here.
    pub(super) mask: Option<Mask>,
}

impl View {
    /// Room for a whole raise (`raise`) or lower of `from..end`, made in
    /// steps, and for a raise its exact undo.
    fn room(&mut self, from: u64, end: u64, raise: bool) -> Result<(), CoverError> {
        let undo = if raise { UNDO_ROOM } else { 0 };
        let extra = self.cover.room_for(from, end, raise) + undo;
        // FALLIBLE: `Cover::reserve` charges first and refuses with `NoMemory`.
        self.cover.reserve(extra)
    }

    /// Raise (`raise`) or lower the cover over `from..to`.
    fn change(&mut self, from: u64, to: u64, raise: bool) -> Result<(), CoverError> {
        if raise {
            self.cover.raise(from, to)
        } else {
            self.cover.lower(from, to)
        }
    }

    /// How many times index `index` counts in this view: its cover, unless
    /// masked.
    fn weight(&self, index: u64) -> u64 {
        if self.mask.as_ref().is_some_and(|mask| mask.masked(index)) {
            return 0;
        }
        u64::from(self.cover.at(index))
    }
}

/// A shadow's link to its file's view, through which its own page changes
/// move the file view's mask.
#[derive(Debug)]
struct ShadowLink {
    /// The file object.
    file: Arc<Vmo>,
    /// The space's counts.
    rss: Arc<Rss>,
    /// The id the space names both by.
    id: u64,
}

impl ShadowLink {
    /// The shadow now holds `index`: masked in the file's view. Under the
    /// shadow's pages lock, which this nests the file's inside.
    fn mask(&self, index: u64) -> Result<(), AllocError> {
        self.file
            .with_frames_nested(|frames| frames.mask_index(&self.rss, self.id, index))
    }

    /// The shadow no longer holds `index`. Allocates nothing.
    fn unmask(&self, index: u64) {
        self.file
            .with_frames_nested(|frames| frames.unmask_index(&self.rss, self.id, index));
    }
}

/// The kernel heap one mask entry is charged at: a B-tree leaf of `(u64,
/// bool)` holds at least five entries in a node of about 120 bytes, and the
/// internal nodes above add a sixth.
pub(super) const MASK_ENTRY_HEAP: usize = 32;

/// A file view's mask: the indexes its shadow holds, true; an index the
/// shadow held and gave back stays, false, until the view goes, so that an
/// unmask frees no node on the decommit and drop paths. Each entry is
/// charged to the job whose fault made it, for as long as the view lives
/// (ledger 8894 T1).
#[derive(Debug)]
pub(super) struct Mask {
    /// Index to whether it is masked.
    pub(super) entries: BTreeMap<u64, bool>,
    /// What the entries hold of the heap, charged.
    pub(super) room: Charge,
}

impl Mask {
    /// No entry.
    fn new() -> Result<Mask, AllocError> {
        Ok(Mask {
            entries: BTreeMap::new(),
            room: Charge::bytes(0).map_err(|_| AllocError)?,
        })
    }

    /// Whether `index` is masked.
    fn masked(&self, index: u64) -> bool {
        self.entries.get(&index).copied().unwrap_or(false)
    }

    /// Unmask `index`: whether it was masked. Allocates and frees nothing.
    fn clear(&mut self, index: u64) -> bool {
        self.entries
            .get_mut(&index)
            .is_some_and(|entry| core::mem::replace(entry, false))
    }

    /// Mask `index` (`masked`): whether that changed it. A new entry is
    /// charged first.
    fn set(&mut self, index: u64, masked: bool) -> Result<bool, AllocError> {
        match (self.entries.get_mut(&index), masked) {
            (Some(entry), _) => Ok(core::mem::replace(entry, masked) != masked),
            (None, true) => {
                self.room.grow(MASK_ENTRY_HEAP).map_err(|_| AllocError)?;
                let inserted = fallible::insert(&mut self.entries, index, true);
                if inserted.is_err() {
                    self.room.shrink(MASK_ENTRY_HEAP);
                }
                Ok(inserted?.is_none())
            }
            (None, false) => Ok(false),
        }
    }
}

/// One hold's chunk of `map` from `from` to at most `end`: the committed
/// pages not in `mask` among the first [`COUNT_CHUNK`] committed, and where
/// the chunk ends -- past the last of them, or `end` if there were fewer.
fn unmasked_chunk(
    map: &BTreeMap<u64, Frame>,
    mask: Option<&Mask>,
    from: u64,
    end: u64,
) -> (u64, u64) {
    let mut pages = 0u64;
    let mut taken = 0usize;
    let mut last = from;
    for &index in map
        .range(from..end)
        .map(|(index, _)| index)
        .take(COUNT_CHUNK)
    {
        taken += 1;
        last = index;
        if !mask.is_some_and(|mask| mask.masked(index)) {
            pages += 1;
        }
    }
    let to = if taken == COUNT_CHUNK {
        last.saturating_add(1)
    } else {
        end
    };
    (pages, to)
}

/// An object's page set, and every view of it: the one type the set changes
/// through, so that no change can leave a count behind. Read through
/// [`Deref`] as the map it is.
#[derive(Debug, Default)]
pub(crate) struct Frames {
    /// Page index to the frame holding it.
    map: BTreeMap<u64, Frame>,
    /// Every space's view of the object.
    pub(super) views: Vec<View>,
    /// A shadow's link to its file's view.
    link: Option<ShadowLink>,
}

impl Deref for Frames {
    type Target = BTreeMap<u64, Frame>;

    fn deref(&self) -> &BTreeMap<u64, Frame> {
        &self.map
    }
}

impl Frames {
    /// The pages `map` names, viewed by nobody yet: a fork's new object.
    pub(crate) const fn from_map(map: BTreeMap<u64, Frame>) -> Frames {
        Frames {
            map,
            views: Vec::new(),
            link: None,
        }
    }

    /// Whether any space views the object.
    pub(crate) fn viewed(&self) -> bool {
        !self.views.is_empty()
    }

    /// Page `index` came (`up`) or went: each view's count moves by its
    /// weight there.
    fn counted(&self, index: u64, up: bool) {
        for view in &self.views {
            view.rss.shift(view.kind, view.weight(index), up);
        }
    }

    /// `map.insert(index, frame)` inside a section the caller has entered,
    /// counted. Not for a shadow, whose pages come only through
    /// [`Frames::try_insert`], which can refuse.
    pub(crate) fn insert_held(
        &mut self,
        held: &Reserved,
        index: u64,
        frame: Frame,
    ) -> Option<Frame> {
        if self.link.is_some() && !self.map.contains_key(&index) {
            #[cfg(ferrix_checks = "on")]
            crate::panic::fatal!(
                crate::panic::catalog::RSS_COUNT_BROKEN,
                "kept resident counts: a shadow gained page {index} by a path that cannot refuse"
            );
            #[cfg(not(ferrix_checks = "on"))]
            if let Some(link) = &self.link {
                let _ = link.mask(index);
            }
        }
        let displaced = fallible::insert_held(held, &mut self.map, index, frame);
        if displaced.is_none() {
            self.counted(index, true);
        }
        displaced
    }

    /// `map.insert(index, frame)`, counted; for a shadow, masked in its file's
    /// view first.
    ///
    /// # Errors
    ///
    /// [`AllocError`]; nothing changed and `frame` is the caller's.
    pub(crate) fn try_insert(
        &mut self,
        index: u64,
        frame: Frame,
    ) -> Result<Option<Frame>, AllocError> {
        let new = !self.map.contains_key(&index);
        if new && let Some(link) = &self.link {
            link.mask(index)?;
        }
        match fallible::insert(&mut self.map, index, frame) {
            Ok(displaced) => {
                if displaced.is_none() {
                    self.counted(index, true);
                }
                Ok(displaced)
            }
            Err(error) => {
                if new && let Some(link) = &self.link {
                    link.unmask(index);
                }
                Err(error)
            }
        }
    }

    /// Put `frame` in page `index`, which holds one: the old frame back. A
    /// page replaced is counted as it was.
    pub(crate) fn replace(&mut self, index: u64, frame: Frame) -> Option<Frame> {
        self.map
            .get_mut(&index)
            .map(|slot| core::mem::replace(slot, frame))
    }

    /// `map.remove(index)`, counted. Allocates nothing. Inlined into its
    /// callers' frames: it sits on a task's deepest path (`check-stack-depth`).
    #[inline(always)]
    pub(crate) fn remove(&mut self, index: u64) -> Option<Frame> {
        let frame = self.map.remove(&index)?;
        self.counted(index, false);
        if let Some(link) = &self.link {
            link.unmask(index);
        }
        Some(frame)
    }

    /// Every page out, counted, and back to the caller. Allocates nothing.
    pub(crate) fn take_all(&mut self) -> BTreeMap<u64, Frame> {
        let map = core::mem::take(&mut self.map);
        for &index in map.keys() {
            for view in &self.views {
                view.rss.take(view.kind, view.weight(index));
            }
            if let Some(link) = &self.link {
                link.unmask(index);
            }
        }
        map
    }

    /// For the object's drop: no view may be left, and a shadow's pages
    /// leave its file's mask.
    pub(crate) fn dropping(&mut self) {
        #[cfg(ferrix_checks = "on")]
        if !self.views.is_empty() {
            crate::panic::fatal!(
                crate::panic::catalog::RSS_COUNT_BROKEN,
                "kept resident counts: an object was dropped with {} views of it left",
                self.views.len()
            );
        }
        // Under no pages lock: the object is going, and nothing else reaches
        // it. Its file's view is normally gone first, with the space.
        if let Some(link) = &self.link {
            for &index in self.map.keys() {
                link.file
                    .with_frames(|frames| frames.unmask_index(&link.rss, link.id, index));
            }
        }
    }

    /// The file's side of a shadow gaining `index`: the view `(rss, id)`
    /// with a mask, if any, masks it, and its count falls by the weight there
    /// if this object holds it.
    ///
    /// # Errors
    ///
    /// [`AllocError`] with no room for the mask's entry; nothing changed.
    fn mask_index(&mut self, rss: &Arc<Rss>, id: u64, index: u64) -> Result<(), AllocError> {
        let held = self.map.contains_key(&index);
        if let Some((view, weight)) = self.view_masking(rss, id, index)
            && let Some(mask) = view.mask.as_mut()
            && mask.set(index, true)?
            && held
        {
            view.rss.take(view.kind, weight);
        }
        Ok(())
    }

    /// The file's side of a shadow losing `index`: unmasked, and the count
    /// back up by the weight there if this object holds it. Allocates and
    /// frees nothing: it runs on the decommit and drop paths, inside the
    /// shadow's pages lock (`check-stack-depth`'s chains).
    fn unmask_index(&mut self, rss: &Arc<Rss>, id: u64, index: u64) {
        let held = self.map.contains_key(&index);
        if let Some((view, weight)) = self.view_masking(rss, id, index)
            && let Some(mask) = view.mask.as_mut()
            && mask.clear(index)
            && held
        {
            view.rss.add(view.kind, weight);
        }
    }

    /// The view `(rss, id)` with a mask, and its cover at `index`.
    fn view_masking(&mut self, rss: &Arc<Rss>, id: u64, index: u64) -> Option<(&mut View, u64)> {
        let view = self
            .views
            .iter_mut()
            .find(|view| Arc::ptr_eq(&view.rss, rss) && view.id == id && view.mask.is_some())?;
        let weight = u64::from(view.cover.at(index));
        Some((view, weight))
    }

    /// Have the view `key` with a mask of exactly `copies`, a shadow's
    /// pages, making it if it is not there; with it there already, nothing.
    /// Its cover is empty when made, so no count moves.
    ///
    /// # Errors
    ///
    /// [`AllocError`]; nothing changed.
    pub(crate) fn masked_view(&mut self, key: &ViewKey, copies: &[u64]) -> Result<(), AllocError> {
        if let Some(_view) = self.views.iter().find(|view| key.names(view)) {
            // An id's `shared` is its mapping's for life, so the view a
            // private file mapping's key names is always a masked one.
            #[cfg(ferrix_checks = "on")]
            if _view.mask.is_none() {
                crate::panic::fatal!(
                    crate::panic::catalog::RSS_COUNT_BROKEN,
                    "kept resident counts: id {} viewed unmasked, then as a private mapping",
                    _view.id
                );
            }
            return Ok(());
        }
        let mut mask = Mask::new()?;
        for &index in copies {
            let _ = mask.set(index, true)?;
        }
        let view = View {
            rss: Arc::clone(&key.rss),
            id: key.id,
            kind: key.kind,
            cover: Cover::new().map_err(|_| AllocError)?,
            mask: Some(mask),
        };
        fallible::try_push(&mut self.views, view)
    }

    /// Link this object, a shadow, to `file`'s view `(rss, id)`, unless it is
    /// linked already.
    pub(crate) fn link_to(&mut self, file: &Arc<Vmo>, rss: &Arc<Rss>, id: u64) {
        if self.link.is_none() {
            self.link = Some(ShadowLink {
                file: Arc::clone(file),
                rss: Arc::clone(rss),
                id,
            });
        }
    }

    /// Room in view `key` for `changes` lowers, each of one region's range.
    ///
    /// # Errors
    ///
    /// [`AllocError`]; nothing changed.
    pub(crate) fn reserve_lowers(
        &mut self,
        key: &ViewKey,
        changes: usize,
    ) -> Result<(), AllocError> {
        let Some(view) = self.views.iter_mut().find(|view| key.names(view)) else {
            return Ok(());
        };
        // Each lower adds two runs at most once done, and the one under way
        // one more while it is made in steps.
        let extra = changes.saturating_mul(2).saturating_add(1);
        // FALLIBLE: `Cover::reserve` charges first and refuses with `NoMemory`.
        view.cover.reserve(extra).map_err(|_| AllocError)
    }

    /// One hold's step of raising (`raise`) or lowering view `key`'s cover
    /// over `from..end`: at most [`COUNT_CHUNK`] committed pages, the cover
    /// changed over the indexes they span and the count by the unmasked ones,
    /// in this one hold. The first step of a change (`first`) makes the view
    /// for a raise if it is not there, and has the room for the whole change
    /// (and for a raise, its exact undo); a view whose cover a lower empties
    /// goes. Where to go on from, or `None` once `end` is reached.
    ///
    /// # Errors
    ///
    /// [`CoverError`]: for a first step, nothing changed; a later step of a
    /// change that had its room refuses nothing.
    pub(crate) fn cover_step(
        &mut self,
        key: &ViewKey,
        (from, end): (u64, u64),
        raise: bool,
        first: bool,
    ) -> Result<Option<u64>, CoverError> {
        if from >= end {
            return Ok(None);
        }
        let at = self.view_for(key, raise && first)?;
        let Frames { map, views, .. } = self;
        let Some(view) = views.get_mut(at) else {
            return Err(CoverError::Bounds);
        };
        if first && let Err(error) = view.room(from, end, raise) {
            if view.cover.is_empty() && view.mask.is_none() {
                let _ = views.swap_remove(at);
            }
            return Err(error);
        }
        let (pages, to) = unmasked_chunk(map, view.mask.as_ref(), from, end);
        view.change(from, to, raise)?;
        view.rss.shift(view.kind, pages, raise);
        if view.cover.is_empty() {
            let _ = views.swap_remove(at);
        }
        Ok((to < end).then_some(to))
    }

    /// Where view `key` is in the list; with `make`, made if it is not.
    fn view_for(&mut self, key: &ViewKey, make: bool) -> Result<usize, CoverError> {
        if let Some(at) = self.views.iter().position(|view| key.names(view)) {
            return Ok(at);
        }
        if !make {
            return Err(CoverError::Bounds);
        }
        let view = View {
            rss: Arc::clone(&key.rss),
            id: key.id,
            kind: key.kind,
            cover: Cover::new()?,
            mask: None,
        };
        fallible::try_push(&mut self.views, view).map_err(|_| CoverError::NoMemory)?;
        Ok(self.views.len() - 1)
    }

    /// Every view of `rss` gone, or with `id` those of that id alone, as the
    /// space goes or the id leaves its tables: nothing counted for them moves
    /// again. An id leaves once no region names it, so its views cover
    /// nothing by then; check builds stop on one that still does.
    pub(crate) fn forget(&mut self, rss: &Arc<Rss>, id: Option<u64>) {
        self.views.retain(|view| {
            let gone = Arc::ptr_eq(&view.rss, rss) && id.is_none_or(|id| view.id == id);
            #[cfg(ferrix_checks = "on")]
            if gone && id.is_some() && !view.cover.is_empty() {
                crate::panic::fatal!(
                    crate::panic::catalog::RSS_COUNT_BROKEN,
                    "kept resident counts: id {} left its space's tables with its view still covering pages",
                    view.id
                );
            }
            !gone
        });
    }
}
