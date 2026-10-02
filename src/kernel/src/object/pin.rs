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
//! frames once they are unpinned, since nothing stands between it and memory,
//! so the frames stay held until the device is reset. Nothing resets a device
//! yet, so today they stay held for good, and the console says so the first
//! time. A pin its domain refuses to give back keeps its frames the same way.
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
//! again at once is given them.
//!
//! So such a pin goes to a quarantine instead of being given back: its pages
//! stay mapped in the domain and its frames held, charged to nobody, since
//! the job they were charged to is gone. What the device writes late lands in
//! the dead driver's own pages, reached as the domain still allows, and
//! neither faults nor reaches memory anything else holds. The pin is given
//! back, out of the domain first and then to the allocator, once the device's
//! core accepts a new driver's HELLO for it ([`quarantine_release`]). A driver resets its device in its bring-up,
//! before HELLO, and virtio-snd's also releases what the device held. That is
//! an event the device's own protocol orders, not a time. A device no driver
//! takes up again keeps its quarantine for good, as an untranslated domain
//! keeps its pins. That is bounded by what devmgr starts: it starts no
//! driver again after one that died before publishing, which is after its
//! HELLO was accepted, or after its restart budget is spent (`docs/DEVMGR.md`
//! §4). So a device's quarantine holds at most the pins of the last driver
//! that published and of the one after it that died before publishing, and
//! one more driver's for each explicit rebind an administrator asks of a
//! device whose drivers keep dying so (SAFETY-MANUAL AoU-12). The kernel does
//! not rely on that: a domain whose quarantine holds
//! [`QUARANTINE_CAP_PAGES`] refuses the next pin for its device
//! ([`PinError::QuarantineFull`]) until a release, so the device stops
//! working rather than the memory grow.
//!
//! Until the release, a quarantined page stays reachable by its device, and
//! so by that device's next driver, which is the dead one's successor in the
//! same trust domain; nothing else can reach it -- with one exception, for a
//! short while. A driver that served fault windows (`user::window`) lent some
//! of the pinned pages to its clients, and its death revokes them on the
//! window death task, after its handles close. Until that revoke has run, a
//! client may still write a lent page through its own mapping, so such a
//! write can show in the quarantine's sums as if the device had written late.
//! It lands only in the dead driver's own pages, as a late device write does. Once released, a frame is
//! zeroed before a new owner can read it, as every frame handed to a VMO is
//! (O.SCRUB). A pin closed by a live driver is not quarantined: a live
//! driver resets its device before it unpins, as the ring specifications say.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_frame::Frame;
use ferrix_paging::{MapFlags, PAGE_SIZE};

use crate::device::DeviceNode;
use crate::fallible::{self, AllocError};
use crate::iommu::{Domain, DomainError, Pinned};
use crate::mm;
use crate::object::process::Exit;
use crate::println;
use crate::sync::SpinLock;
use crate::user::vmo::Held;

pub(crate) mod check;

/// Whether a kept pin has been announced.
static KEPT: AtomicBool = AtomicBool::new(false);

/// Whether a quarantined pin has been announced.
static QUARANTINED: AtomicBool = AtomicBool::new(false);

/// Every quarantined pin's frames, newest first.
static QUARANTINE: SpinLock<Option<Box<Quarantined>>> = SpinLock::new(None);

/// Pins refused because their device's quarantine was full, since boot.
static REFUSED: AtomicU64 = AtomicU64::new(0);

/// The pages a device's quarantine may hold before a new pin for the device
/// is refused ([`PinError::QuarantineFull`]): memory the kernel holds for no
/// job, so the kernel bounds it itself rather than trust devmgr to stop
/// restarting.
///
/// Chosen as two drivers' worst case, since devmgr starts no driver again
/// after one that died before its HELLO was accepted: the largest pin set a
/// Ferrix driver makes is the GPU's windows onto the display card, 256 MiB
/// (`display::CARD_BYTES`, 65536 pages, which the display core asserts fits),
/// and every driver's rings, areas and scratch are under 1024 pages more. Seen
/// in the gates: 1580 pages for a `gpu` at 1024x768, 20 for `snd`. A
/// quarantine can pass the cap by what drivers already pinned when it was
/// reached, never by a pin made after.
pub(crate) const QUARANTINE_CAP_PAGES: usize = 2 * (LARGEST_DRIVER_PIN_PAGES + 1024);

/// The most pages one driver pins: the display card, 256 MiB.
pub(crate) const LARGEST_DRIVER_PIN_PAGES: usize = 256 * 1024 * 1024 / PAGE_SIZE as usize;

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

/// Pages of a VMO pinned into a device's domain.
pub(crate) struct Pin {
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
    /// allocate (finding F-23): `None` on an untranslated domain, which keeps
    /// its frames for good instead. Taken on drop.
    spare: Option<Box<Quarantined>>,
    /// Whether it is a boot check's, which says nothing on the console.
    quiet: bool,
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
    /// Pin `held`'s pages into `domain`, writable when `flags` says so, for
    /// the process whose end is `owner`.
    ///
    /// # Errors
    ///
    /// What the domain refused, and [`PinError::NoMemory`] for the
    /// quarantine's record. The hold is then released: nothing was mapped.
    pub(crate) fn new(
        domain: Arc<Domain>,
        held: Held,
        flags: MapFlags,
        owner: Arc<Exit>,
    ) -> Result<Pin, PinError> {
        Pin::with_cap(domain, held, flags, owner, QUARANTINE_CAP_PAGES, false)
    }

    /// [`Pin::new`], refused when `domain`'s quarantine holds `cap` pages or
    /// more; `quiet` for a boot check's, which neither announces nor counts.
    fn with_cap(
        domain: Arc<Domain>,
        held: Held,
        flags: MapFlags,
        owner: Arc<Exit>,
        cap: usize,
        quiet: bool,
    ) -> Result<Pin, PinError> {
        if domain.translated() && quarantined_pages(&domain) >= cap {
            if !quiet && REFUSED.fetch_add(1, Ordering::Relaxed) == 0 {
                println!(
                    "  iommu    a device's quarantine is full: its next pins are refused \
                     until a driver of it is accepted"
                );
            }
            return Err(PinError::QuarantineFull);
        }
        let spare = if domain.translated() {
            let frames = fallible::try_boxed_slice(held.frames())?;
            let sums = fallible::try_boxed_filled(0, frames.len())?;
            Some(fallible::try_box(Quarantined {
                domain: Arc::clone(&domain),
                pinned: None,
                frames,
                sums,
                next: None,
            })?)
        } else {
            None
        };
        let pinned = domain.pin(held.frames(), flags)?;
        Ok(Pin {
            domain,
            pinned: Some(pinned),
            held: Some(held),
            owner,
            spare,
            quiet,
        })
    }

    /// Each page's device address, in page order.
    pub(crate) fn addresses(&self) -> &[u64] {
        self.pinned.as_ref().map_or(&[], Pinned::addresses)
    }
}

impl Drop for Pin {
    fn drop(&mut self) {
        let (Some(pinned), Some(held)) = (self.pinned.take(), self.held.take()) else {
            return;
        };
        if let Some(spare) = self.spare.take()
            && self.owner.is_terminated()
        {
            quarantine(spare, pinned, held, self.quiet);
            return;
        }
        let freeable = match self.domain.unpin(pinned) {
            Ok(()) => self.domain.translated(),
            Err((_, back)) => {
                back.leak();
                false
            }
        };
        if freeable {
            drop(held);
            return;
        }
        let _ = core::mem::ManuallyDrop::new(held);
        if !KEPT.swap(true, Ordering::Relaxed) {
            println!(
                "  iommu    a pin was closed while its device may still reach its pages: \
                 the frames are kept until the device is reset"
            );
        }
    }
}

/// Why a pin was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PinError {
    /// What the domain refused.
    Domain(DomainError),
    /// No memory for the quarantine's record.
    NoMemory,
    /// The device's quarantine holds [`QUARANTINE_CAP_PAGES`] already.
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
/// good instead, mapped and held, as an untranslated domain keeps its pins:
/// giving it back would free what the device may still write (finding F-38).
fn quarantine(mut spare: Box<Quarantined>, pinned: Pinned, held: Held, quiet: bool) {
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
        let mut head = QUARANTINE.lock();
        spare.next = head.take();
        *head = Some(spare);
    }
    if !quiet && !QUARANTINED.swap(true, Ordering::Relaxed) {
        println!(
            "  iommu    a dead driver's pins are kept, mapped, until its device's next driver \
             has reset it"
        );
    }
}

/// The pages `domain`'s quarantine holds.
fn quarantined_pages(domain: &Arc<Domain>) -> usize {
    let head = QUARANTINE.lock();
    let mut pages = 0usize;
    let mut next = head.as_deref();
    while let Some(entry) = next {
        if Arc::ptr_eq(&entry.domain, domain) {
            pages = pages.saturating_add(entry.frames.len());
        }
        next = entry.next.as_deref();
    }
    pages
}

/// What a release gave back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Released {
    /// Pages given back to the allocator.
    pages: usize,
    /// Of those, the ones written after their driver died.
    written: usize,
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
    let released = release(&domain);
    if released.pages != 0 {
        println!(
            "  iommu    {} pages a dead driver's device could still write went back once its \
             next driver had reset it, {} of them written after it died; {} pins refused \
             while a quarantine was full",
            released.pages,
            released.written,
            REFUSED.load(Ordering::Relaxed),
        );
    }
}

/// [`quarantine_release`]'s work, for `domain`.
fn release(domain: &Arc<Domain>) -> Released {
    let mut freed: Option<Box<Quarantined>> = None;
    {
        let mut head = QUARANTINE.lock();
        let mut kept: Option<Box<Quarantined>> = None;
        let mut next = head.take();
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
        *head = kept;
    }
    // Outside the lock: an unpin enters the unit's gate, and a frame's
    // release takes the allocator's lock. Out of the domain first, the
    // invalidation completed, and only then to the allocator, as any pin.
    let mut released = Released::default();
    let mut next = freed;
    while let Some(mut entry) = next {
        next = entry.next.take();
        let Some(pinned) = entry.pinned.take() else {
            continue;
        };
        match domain.unpin(pinned) {
            Ok(()) => {
                for (&frame, &sum) in entry.frames.iter().zip(entry.sums.iter()) {
                    released.written += usize::from(fold(frame) != sum);
                    let _ = mm::release_frame(frame);
                }
                released.pages += entry.frames.len();
            }
            // Refused: the device may still reach them, so they stay.
            Err((_, back)) => back.leak(),
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
