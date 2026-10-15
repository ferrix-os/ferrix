//! Pins: pages of a VMO a device may reach, held until the handle is closed.
//!
//! `docs/ARCHITECTURE.md` §7 gives a driver "a `Vmo` for DMA, whose device
//! addresses come from an IOMMU domain scoped to that device". A pin is that
//! grant. `Vmo::hold` keeps the pages where they are, and the device's domain
//! maps them and says at which addresses the device reaches them.
//!
//! # The order a pin is given back in
//!
//! Out of the domain, and forgotten by the unit, first; only then are the
//! holds released and the frames free to go. On a translated domain that is
//! the whole story. On an untranslated one the device can still reach the
//! frames once they are unpinned, since nothing stands between it and
//! memory -- but it can reach every other frame too: such a domain is the
//! degraded trusted mode of VULNERABILITY-ANALYSIS V-03, where the driver is
//! trusted with all of memory. Keeping a live driver's closed pin there
//! protects nothing its device could not reach anyway, so it is given back
//! at once, as on a translated domain (finding F-59; before it, such pins
//! were kept for good, a leak, and since the pin budget a device that stopped
//! after enough attaches). A pin its domain refuses to give back keeps its
//! frames for good, and the console says so the first time.
//!
//! # The quarantine
//!
//! On a translated domain the unit forgets a page before its frame goes
//! ([`Domain::unpin`] waits for the invalidation to complete), which is all a
//! device with no translation cache of its own needs (ATS is never enabled:
//! `docs/certification/SAFETY-MANUAL.md`, AoU-12). An emulator can reach
//! further. QEMU maps a virtqueue buffer's memory when the device takes the
//! buffer and writes its status through that mapping when it hands the buffer
//! back, whatever the domain says by then. virtio-snd holds buffers across its
//! driver's death, and its reset leaves them queued (`docs/AUDIO.md` §3.3).
//! So the frames of a pin closed because its process died could be written
//! after they were freed, into whatever took them next: a driver started
//! again at once is given them. On an untranslated domain nothing forgets
//! anything, and the same late write lands wherever the frame went next.
//!
//! So such a pin, on either kind of domain, goes to a quarantine instead of being given back: its pages
//! stay mapped in the domain and its frames held, charged to nobody, since
//! the job they were charged to is gone. What the device writes late lands in
//! the dead driver's own pages, reached as the domain still allows, and
//! neither faults nor reaches memory anything else holds. An untranslated
//! domain has nothing to unmap: its quarantined pin is only its frames, held
//! (F-59). The pin is given
//! back, out of the domain first and then to the allocator, once the device's
//! core accepts a new driver's HELLO for it ([`quarantine_release`]). A driver resets its device in its bring-up,
//! before HELLO, and virtio-snd's also releases what the device held. That is
//! an event the device's own protocol orders, not a time. A device no driver
//! takes up again keeps its quarantine for good. That is bounded by what devmgr starts: it starts no
//! driver again after one that died before publishing, which is after its
//! HELLO was accepted, or after its restart budget is spent (`docs/DEVMGR.md`
//! §4). So a device's quarantine holds at most the pins of the last driver
//! that published and of the one after it that died before publishing, and
//! one more driver's for each explicit rebind an administrator asks of a
//! device whose drivers keep dying so (SAFETY-MANUAL AoU-12). The kernel does
//! not rely on that: see the budget below.
//!
//! # The budget
//!
//! Each device has a pin budget `B`, in pages ([`PinBudget`], kept on its
//! domain, `docs/NVIDIA.md` §12.2): [`DEFAULT_PIN_BUDGET_PAGES`] until
//! `devmgr`, which alone holds `SET_LIMIT` on the device, sets another
//! ([`set_budget`]). Against it three counts are kept, each read and changed
//! only under [`QUARANTINE`]'s lock: `live`, the pages of the device's pins
//! still open, reserved before the domain is asked to map them ([`reserve`])
//! and given back if it refuses; `quarantined`, the pages its quarantine
//! holds; and `kept`, the pages kept for good because the domain would not
//! give them back, or a dead driver's pin could not be quarantined. A pin of `n`
//! pages is refused, with nothing mapped, when `live + n > B`
//! ([`PinError::LimitReached`]), and when `quarantined + kept + live + n >
//! 2B` ([`PinError::QuarantineFull`]). A death moves pages from `live` to
//! `quarantined` or `kept` and never adds to the sum, and a release takes off
//! `quarantined` only what it gave back, so the memory a device's pins hold
//! never passes `2B`: two drivers' worth, the last that published and one
//! that died before publishing. A budget raised above the default is
//! counted against the kernel's ceiling, a quarter of RAM ([`ceiling`]); the
//! default is not, as it is the per-device bound AoU-12 accepts.
//!
//! Until the release, a quarantined page stays reachable by its device, and
//! so by that device's next driver, which is the dead one's successor in the
//! same trust domain; nothing else can reach it. Once released, a frame is
//! zeroed before a new owner can read it, as every frame handed to a VMO is
//! (O.SCRUB). A pin closed by a live driver is not quarantined: a live
//! driver resets its device before it unpins, as the ring specifications say.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ferrix_frame::Frame;
use ferrix_paging::{MapFlags, PAGE_SIZE};

use crate::device::DeviceNode;
use crate::fallible::{self, AllocError};
use crate::iommu::{Domain, DomainError, Pinned, Wait};
use crate::mm;
use crate::object::process::Exit;
use crate::println;
use crate::sync::SpinLock;
use crate::user::vmo::Held;


/// Whether a kept pin has been announced.
static KEPT: AtomicBool = AtomicBool::new(false);

/// Whether a quarantined pin has been announced.
static QUARANTINED: AtomicBool = AtomicBool::new(false);

/// Every quarantined pin's frames, newest first, and what raised budgets
/// hold of the ceiling. Its lock is also the one every [`PinBudget`]'s
/// counts are read and changed under: a leaf, taken for a few loads and
/// stores, never across an unpin or the allocator.
static QUARANTINE: SpinLock<Quarantine> = SpinLock::new(Quarantine {
    head: None,
    raised: 0,
});

/// What [`QUARANTINE`] guards beside the budgets' counts.
struct Quarantine {
    /// Every quarantined pin, newest first.
    head: Option<Box<Quarantined>>,
    /// Twice every budget raised above [`DEFAULT_PIN_BUDGET_PAGES`], in
    /// pages: what [`ceiling`] bounds.
    raised: usize,
}

/// Pins refused because their device's quarantine was full, since boot.
static REFUSED: AtomicU64 = AtomicU64::new(0);

/// The kernel's ceiling on twice every raised budget, in pages: a quarter of
/// the RAM the allocator manages, counted at stage 10 ([`count_ceiling`]).
/// Zero until then, which refuses every raise.
static CEILING: AtomicUsize = AtomicUsize::new(0);

/// A device's pin budget until `devmgr` sets another: one driver's worst
/// case. Twice it bounds what a device's pins hold, for the two drivers
/// devmgr's restart rule allows to have pinned: it starts no driver again
/// after one that died before its HELLO was accepted.
///
/// The largest pin set a Ferrix driver makes today is the GPU's windows onto
/// the display card, 256 MiB (`display::CARD_BYTES`, 65536 pages, which the
/// display core asserts fits), and every driver's rings, areas and scratch
/// are under 1024 pages more. Seen in the gates: 1580 pages for a `gpu` at
/// 1024x768, 20 for `snd`. So twice the default is the fixed cap the
/// quarantine had before budgets, and no existing driver sees a change.
pub const DEFAULT_PIN_BUDGET_PAGES: usize = LARGEST_DRIVER_PIN_PAGES + 1024;

/// The most pages one driver pins: the display card, 256 MiB.
pub const LARGEST_DRIVER_PIN_PAGES: usize = 256 * 1024 * 1024 / PAGE_SIZE as usize;

/// One pin closed by its process's death, until its device's next driver has
/// reset the device.
struct Quarantined {
    /// The domain its pages are still mapped in, which names the device.
    domain: Arc<Domain>,
    /// The domain's record of them, given back at the release.
    pinned: Option<Pinned>,
    /// Its frames, one reference to each taken as the pin closed.
    frames: Box<[Frame]>,
    /// Each frame's contents folded as the pin closed ([`fold`]), so the
    /// release can say whether the device wrote them after their driver
    /// died: the writes the quarantine exists for.
    sums: Box<[u64]>,
    /// The next one in [`QUARANTINE`].
    next: Option<Box<Quarantined>>,
}

/// A device's pin budget and the pages counted against it, kept on its
/// domain (`docs/NVIDIA.md` §12.2).
///
/// Atomic only so that a shared domain can carry them without `unsafe`:
/// every field but the two announcements is read and changed under
/// [`QUARANTINE`]'s lock alone, so a check and the count it allows are one
/// step.
#[derive(Debug)]
pub(crate) struct PinBudget {
    /// `B`, in pages.
    budget: AtomicUsize,
    /// Pages of pins still open, reserved before the domain maps them.
    live: AtomicUsize,
    /// Pages in the quarantine.
    quarantined: AtomicUsize,
    /// Pages kept for good: a pin the domain would not give back, or one that
    /// could not be quarantined.
    kept: AtomicUsize,
    /// Whether a pin past the budget has been announced for this device.
    said_limit: AtomicBool,
    /// Whether a pin past twice the budget has been announced.
    said_full: AtomicBool,
}

impl PinBudget {
    /// The default budget, and nothing counted.
    pub(crate) const fn new() -> Self {
        PinBudget {
            budget: AtomicUsize::new(DEFAULT_PIN_BUDGET_PAGES),
            live: AtomicUsize::new(0),
            quarantined: AtomicUsize::new(0),
            kept: AtomicUsize::new(0),
            said_limit: AtomicBool::new(false),
            said_full: AtomicBool::new(false),
        }
    }

    /// The four numbers, read under the caller's hold of [`QUARANTINE`].
    fn read(&self, _held: &Quarantine) -> Counts {
        Counts {
            budget: self.budget.load(Ordering::Relaxed),
            live: self.live.load(Ordering::Relaxed),
            quarantined: self.quarantined.load(Ordering::Relaxed),
            kept: self.kept.load(Ordering::Relaxed),
        }
    }

    /// Store `counts`, under the caller's hold of [`QUARANTINE`].
    fn write(&self, _held: &mut Quarantine, counts: Counts) {
        self.budget.store(counts.budget, Ordering::Relaxed);
        self.live.store(counts.live, Ordering::Relaxed);
        self.quarantined
            .store(counts.quarantined, Ordering::Relaxed);
        self.kept.store(counts.kept, Ordering::Relaxed);
    }
}

/// A device's budget and its counts at one moment, in pages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// `B`.
    pub budget: usize,
    /// Pages of live pins.
    pub live: usize,
    /// Pages in the quarantine.
    pub quarantined: usize,
    /// Pages kept for good.
    pub kept: usize,
}

impl fmt::Display for Counts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "budget {} pages, {} live, {} quarantined, {} kept",
            self.budget, self.live, self.quarantined, self.kept
        )
    }
}

/// `domain`'s budget and counts now.
pub fn counts(domain: &Domain) -> Counts {
    let held = QUARANTINE.lock();
    domain.pin_budget().read(&held)
}

/// `node`'s pin budget: its domain's, or the default for a node whose
/// domain was never made.
pub fn budget_of(node: &DeviceNode) -> usize {
    node.domain_made()
        .map_or(DEFAULT_PIN_BUDGET_PAGES, |domain| counts(&domain).budget)
}

/// Count the kernel's ceiling: a quarter of the RAM the allocator manages.
/// Stage 10 calls it once, before any budget can be set.
pub(crate) fn count_ceiling() {
    let quarter = usize::try_from(mm::managed_frames() / 4).unwrap_or(usize::MAX);
    CEILING.store(quarter, Ordering::Relaxed);
}

/// The kernel's ceiling on twice every raised budget, in pages.
pub fn ceiling() -> usize {
    CEILING.load(Ordering::Relaxed)
}

/// What twice `budget` holds of the ceiling: nothing at or below the
/// default (`docs/NVIDIA.md` §12.2, F5).
fn raise_of(budget: usize) -> usize {
    if budget > DEFAULT_PIN_BUDGET_PAGES {
        budget.saturating_mul(2)
    } else {
        0
    }
}

/// What of the ceiling budgets raised on devices other than `domain`'s
/// leave: the most twice `domain`'s budget may be.
pub fn room(domain: &Domain) -> usize {
    let held = QUARANTINE.lock();
    let own = raise_of(domain.pin_budget().read(&held).budget);
    ceiling().saturating_sub(held.raised.saturating_sub(own))
}

/// What of the ceiling raised budgets leave, for a device whose budget was
/// never set and so holds none of it.
pub fn ceiling_room() -> usize {
    let held = QUARANTINE.lock();
    ceiling().saturating_sub(held.raised)
}

/// Why a budget was not set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BudgetError {
    /// The device has live pins.
    LivePins,
    /// Raised budgets would hold more than the ceiling.
    PastCeiling,
}

/// Set `domain`'s pin budget to `pages`: for `device_set_limit`, whose
/// caller holds `SET_LIMIT`. Answers the budget it replaced.
///
/// # Errors
///
/// [`BudgetError::LivePins`] while the device has live pins, tested under
/// the lock a pin's reservation takes, so no pin is counted against one
/// budget and held against another; [`BudgetError::PastCeiling`] when twice
/// every raised budget, this one's new value counted and its old one not,
/// would pass [`ceiling`]. Nothing changes on either.
pub fn set_budget(domain: &Domain, pages: usize) -> Result<usize, BudgetError> {
    let mut held = QUARANTINE.lock();
    let pins = domain.pin_budget();
    let mut now = pins.read(&held);
    if now.live != 0 {
        return Err(BudgetError::LivePins);
    }
    let raised = held
        .raised
        .saturating_sub(raise_of(now.budget))
        .saturating_add(raise_of(pages));
    if raised > ceiling() {
        return Err(BudgetError::PastCeiling);
    }
    held.raised = raised;
    let old = now.budget;
    now.budget = pages;
    pins.write(&mut held, now);
    Ok(old)
}

/// Reserve `pages` of live pins against `domain`'s budget, or `budget` for a
/// boot check's pin, testing both rules and counting the pages in one step
/// under the lock (`docs/NVIDIA.md` §12.2, F1 and F2).
///
/// # Errors
///
/// [`PinError::LimitReached`] when the live pins would pass the budget, and
/// [`PinError::QuarantineFull`] when quarantined, kept and live pages would
/// pass twice it. Nothing is counted then.
fn reserve(domain: &Domain, pages: usize, budget: Option<usize>) -> Result<(), PinError> {
    let mut held = QUARANTINE.lock();
    let pins = domain.pin_budget();
    let mut now = pins.read(&held);
    let budget = budget.unwrap_or(now.budget);
    let live = now.live.saturating_add(pages);
    if live > budget {
        return Err(PinError::LimitReached);
    }
    let all = now
        .quarantined
        .saturating_add(now.kept)
        .saturating_add(live);
    if all > budget.saturating_mul(2) {
        return Err(PinError::QuarantineFull);
    }
    now.live = live;
    pins.write(&mut held, now);
    Ok(())
}

/// Give back `pages` [`reserve`] counted for a pin the domain then refused,
/// or one closed and given back whole.
fn unreserve(domain: &Domain, pages: usize) {
    let mut held = QUARANTINE.lock();
    let pins = domain.pin_budget();
    let mut now = pins.read(&held);
    now.live = now.live.saturating_sub(pages);
    pins.write(&mut held, now);
}

/// Move `pages` of a closed pin from `live` to `kept`: its frames stay held
/// for good.
fn keep(domain: &Domain, pages: usize) {
    let mut held = QUARANTINE.lock();
    let pins = domain.pin_budget();
    let mut now = pins.read(&held);
    now.live = now.live.saturating_sub(pages);
    now.kept = now.kept.saturating_add(pages);
    pins.write(&mut held, now);
}

/// What a release did with one quarantined pin's pages.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Release {
    /// Given back to the allocator: off `quarantined`.
    Freed,
    /// The domain would not unpin them: from `quarantined` to `kept`.
    Kept,
}

/// Count `pages` a release took out of `domain`'s quarantine as `how`.
fn account_release(domain: &Domain, pages: usize, how: Release) {
    let mut held = QUARANTINE.lock();
    let pins = domain.pin_budget();
    let mut now = pins.read(&held);
    now.quarantined = now.quarantined.saturating_sub(pages);
    if how == Release::Kept {
        now.kept = now.kept.saturating_add(pages);
    }
    pins.write(&mut held, now);
}

/// Pages of a VMO pinned into a device's domain.
pub struct Pin {
    /// The domain they are pinned into.
    domain: Arc<Domain>,
    /// The domain's record of them. Taken on drop.
    pinned: Option<Pinned>,
    /// The VMO's hold on them. Taken on drop.
    held: Option<Held>,
    /// How the process that pinned them ends: a pin closed as it dies goes to
    /// the quarantine.
    owner: Arc<Exit>,
    /// The quarantine's record of it, made with the pin, since a drop cannot
    /// allocate (finding F-23), on every domain: an untranslated domain's
    /// dead pins are quarantined too, with nothing to unmap (F-59). Taken on
    /// drop.
    spare: Option<Box<Quarantined>>,
    /// Whether it is a boot check's, which says nothing on the console.
    quiet: bool,
    /// How its unpin's invalidation is waited for: [`Wait::Status`], but
    /// [`Wait::Unwritten`] for check R7's, set by `check.rs` alone, whose
    /// invalidation then fails (`docs/NVIDIA.md` §12.3).
    pub wait: Wait,
    /// The pages it counts in its device's `live`, until it is closed.
    pages: usize,
}

impl fmt::Debug for Pin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pin")
            .field("pages", &self.addresses().len())
            .field("translated", &self.domain.translated())
            .finish_non_exhaustive()
    }
}

impl Pin {
    /// Pin `held`'s pages into `node`'s domain `domain`, writable when
    /// `flags` says so, for the process whose end is `owner`, against the
    /// device's budget.
    ///
    /// # Errors
    ///
    /// [`PinError::LimitReached`] and [`PinError::QuarantineFull`] for the
    /// budget's two rules, the first of each per device announced with its
    /// counts; what the domain refused; [`PinError::NoMemory`] for the
    /// quarantine's record. The hold is then released and nothing is counted:
    /// nothing was mapped.
    pub fn new(
        node: &DeviceNode,
        domain: &Arc<Domain>,
        held: Held,
        flags: MapFlags,
        owner: Arc<Exit>,
    ) -> Result<Pin, PinError> {
        let pages = held.frames().len();
        let made = Pin::with_budget(Arc::clone(domain), held, flags, owner, None, false);
        if let Err(why @ (PinError::LimitReached | PinError::QuarantineFull)) = made {
            announce(node, domain, pages, why);
        }
        made
    }

    /// [`Pin::new`]'s work, against `budget` in place of the device's when a
    /// boot check gives one; `quiet` for a boot check's, which neither
    /// announces nor counts in [`REFUSED`].
    pub fn with_budget(
        domain: Arc<Domain>,
        held: Held,
        flags: MapFlags,
        owner: Arc<Exit>,
        budget: Option<usize>,
        quiet: bool,
    ) -> Result<Pin, PinError> {
        let pages = held.frames().len();
        if let Err(why) = reserve(&domain, pages, budget) {
            if why == PinError::QuarantineFull && !quiet {
                let _ = REFUSED.fetch_add(1, Ordering::Relaxed);
            }
            return Err(why);
        }
        match Pin::map(&domain, &held, flags) {
            Ok((spare, pinned)) => Ok(Pin {
                domain,
                pinned: Some(pinned),
                held: Some(held),
                owner,
                spare,
                quiet,
                wait: Wait::Status,
                pages,
            }),
            Err(why) => {
                unreserve(&domain, pages);
                Err(why)
            }
        }
    }

    /// The quarantine's record of a pin of `held`, and the domain's mapping
    /// of it, for pages already reserved.
    fn map(
        domain: &Arc<Domain>,
        held: &Held,
        flags: MapFlags,
    ) -> Result<(Option<Box<Quarantined>>, Pinned), PinError> {
        let frames = fallible::try_boxed_slice(held.frames())?;
        let sums = fallible::try_boxed_filled(0, frames.len())?;
        let spare = fallible::try_box(Quarantined {
            domain: Arc::clone(domain),
            pinned: None,
            frames,
            sums,
            next: None,
        })?;
        Ok((Some(spare), domain.pin(held.frames(), flags)?))
    }

    /// Each page's device address, in page order.
    pub fn addresses(&self) -> &[u64] {
        self.pinned.as_ref().map_or(&[], Pinned::addresses)
    }
}

/// Say, the first time for `node`, that a pin of `pages` was refused `why`,
/// with the device's counts.
fn announce(node: &DeviceNode, domain: &Domain, pages: usize, why: PinError) {
    let pins = domain.pin_budget();
    let (said, rule) = if why == PinError::LimitReached {
        (&pins.said_limit, "past its pin budget")
    } else {
        (
            &pins.said_full,
            "past twice its pin budget, counting its quarantine",
        )
    };
    if said.swap(true, Ordering::Relaxed) {
        return;
    }
    println!(
        "  iommu    {}: a pin of {pages} pages was refused, {rule}: {}",
        node.location(),
        counts(domain),
    );
}

impl Drop for Pin {
    fn drop(&mut self) {
        let (Some(pinned), Some(held)) = (self.pinned.take(), self.held.take()) else {
            return;
        };
        if let Some(spare) = self.spare.take()
            && self.owner.is_terminated()
        {
            quarantine(spare, pinned, held, self.quiet, self.pages);
            return;
        }
        // Given back on every domain the unpin succeeds on: an untranslated
        // one's device reaches all of memory anyway (F-59, V-03).
        let freeable = match self.domain.unpin_waiting(pinned, self.wait) {
            Ok(()) => true,
            Err((_, back)) => {
                back.leak();
                false
            }
        };
        if freeable {
            drop(held);
            unreserve(&self.domain, self.pages);
            return;
        }
        let _ = core::mem::ManuallyDrop::new(held);
        keep(&self.domain, self.pages);
        if !KEPT.swap(true, Ordering::Relaxed) {
            println!(
                "  iommu    a pin its domain would not give back was closed: its frames are \
                 kept for good"
            );
        }
    }
}

/// Why a pin was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PinError {
    /// What the domain refused.
    Domain(DomainError),
    /// No memory for the quarantine's record.
    NoMemory,
    /// The device's live pins would pass its budget.
    LimitReached,
    /// Its quarantined, kept and live pages would pass twice its budget.
    QuarantineFull,
}

impl From<DomainError> for PinError {
    fn from(why: DomainError) -> Self {
        Self::Domain(why)
    }
}

impl From<AllocError> for PinError {
    fn from(_: AllocError) -> Self {
        Self::NoMemory
    }
}

/// Keep `pinned` mapped and `held`'s frames past `held`, charged to nobody,
/// until [`quarantine_release`] for their device.
///
/// A reference to each frame is taken first, so the VMO may go with `held`
/// and the frames stay. If any cannot be taken -- a frame the allocator does
/// not count, which the VMOs drivers pin never have -- the pin is kept for
/// good instead, mapped and held: giving it back would free what the device may still write (finding F-38).
///
/// `pages` move from the device's `live` to its `quarantined`, or to `kept`.
fn quarantine(mut spare: Box<Quarantined>, pinned: Pinned, held: Held, quiet: bool, pages: usize) {
    let taken = spare
        .frames
        .iter()
        .take_while(|&&frame| mm::share_frame(frame).is_some())
        .count();
    if taken != spare.frames.len() {
        for &frame in spare.frames.iter().take(taken) {
            let _ = mm::release_frame(frame);
        }
        pinned.leak();
        let _ = core::mem::ManuallyDrop::new(held);
        keep(&spare.domain, pages);
        if !quiet && !KEPT.swap(true, Ordering::Relaxed) {
            println!(
                "  iommu    a dead driver's pin could not be quarantined: its frames are kept \
                 for good"
            );
        }
        return;
    }
    for (&frame, sum) in spare.frames.iter().zip(spare.sums.iter_mut()) {
        mm::disown_frame(frame);
        *sum = fold(frame);
    }
    drop(held);
    spare.pinned = Some(pinned);
    {
        let mut queue = QUARANTINE.lock();
        let domain = Arc::clone(&spare.domain);
        let pins = domain.pin_budget();
        let mut now = pins.read(&queue);
        now.live = now.live.saturating_sub(pages);
        now.quarantined = now.quarantined.saturating_add(pages);
        pins.write(&mut queue, now);
        spare.next = queue.head.take();
        queue.head = Some(spare);
    }
    if !quiet && !QUARANTINED.swap(true, Ordering::Relaxed) {
        println!(
            "  iommu    a dead driver's pins are kept, mapped, until its device's next driver \
             has reset it"
        );
    }
}

/// What a release gave back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Released {
    /// Pages given back to the allocator.
    pub pages: usize,
    /// Of those, the ones written after their driver died.
    written: usize,
    /// Pages the domain would not unpin, kept for good.
    pub kept: usize,
}

/// Give back the frames of every pin into `node`'s domain that its process's
/// death sent to the quarantine: for a core that has just accepted a new
/// driver's HELLO for `node`, which that driver sent only after resetting the
/// device. Nothing when `node` has no domain, or none of its pins was
/// quarantined.
pub(crate) fn quarantine_release(node: &DeviceNode) {
    let Some(domain) = node.domain_made() else {
        return;
    };
    let released = release(&domain, 0);
    if released.pages != 0 || released.kept != 0 {
        println!(
            "  iommu    {} pages a dead driver's device could still write went back once its \
             next driver had reset it, {} of them written after it died; {} kept for good; {} \
             pins refused while a quarantine was full; {}: {}",
            released.pages,
            released.written,
            released.kept,
            REFUSED.load(Ordering::Relaxed),
            node.location(),
            counts(&domain),
        );
    }
}

/// [`quarantine_release`]'s work, for `domain`. A boot check names in
/// `refusing` how many of the pins to treat as refused by the domain, as a
/// unit that never answers would refuse them; every other caller passes 0.
pub fn release(domain: &Arc<Domain>, refusing: usize) -> Released {
    let mut freed: Option<Box<Quarantined>> = None;
    {
        let mut held = QUARANTINE.lock();
        let mut kept: Option<Box<Quarantined>> = None;
        let mut next = held.head.take();
        while let Some(mut entry) = next {
            next = entry.next.take();
            let list = if Arc::ptr_eq(&entry.domain, domain) {
                &mut freed
            } else {
                &mut kept
            };
            entry.next = list.take();
            *list = Some(entry);
        }
        held.head = kept;
    }
    // Outside the lock: an unpin enters the unit's gate, and a frame's
    // release takes the allocator's lock. Out of the domain first, the
    // invalidation completed, and only then to the allocator, as any pin.
    let mut released = Released::default();
    let mut refused = 0;
    let mut next = freed;
    while let Some(mut entry) = next {
        next = entry.next.take();
        let Some(pinned) = entry.pinned.take() else {
            continue;
        };
        let pages = entry.frames.len();
        let unpinned = if refused < refusing {
            refused += 1;
            Err((DomainError::Unit("refused by a boot check"), pinned))
        } else {
            domain.unpin(pinned)
        };
        match unpinned {
            Ok(()) => {
                for (&frame, &sum) in entry.frames.iter().zip(entry.sums.iter()) {
                    released.written += usize::from(fold(frame) != sum);
                    let _ = mm::release_frame(frame);
                }
                released.pages += pages;
                account_release(domain, pages, Release::Freed);
            }
            // Refused: the device may still reach them, so they stay, and
            // stay counted (F3).
            Err((_, back)) => {
                back.leak();
                released.kept += pages;
                account_release(domain, pages, Release::Kept);
            }
        }
    }
    released
}

/// `frame`'s contents folded into one word (FNV-1a over its words): enough to
/// tell whether anything wrote the page between two looks.
fn fold(frame: Frame) -> u64 {
    let at = mm::direct_map(frame * PAGE_SIZE) as *const u64;
    let mut sum = 0xcbf2_9ce4_8422_2325_u64;
    for word in 0..(PAGE_SIZE / 8) as usize {
        // SAFETY: (DMA) the quarantine holds a reference on `frame`, so it is an
        // allocated frame the direct map covers, and `word` is within it. A
        // device may write it meanwhile, which a volatile read of a whole
        // aligned word tolerates: the answer is then either word.
        let value = unsafe { core::ptr::read_volatile(at.wrapping_add(word)) };
        sum = (sum ^ value).wrapping_mul(0x0000_0100_0000_01b3);
    }
    sum
}
