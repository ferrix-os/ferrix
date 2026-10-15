//! Interrupts: a device's vector, delivered to the driver that holds it.
//!
//! `docs/ARCHITECTURE.md` §7 gives a driver "an `Interrupt` object per
//! vector". It is built only from a [`Vector`], which only `crate::device`
//! can make, so a driver cannot claim a line its device does not have.
//!
//! # Masked from delivery to acknowledgement, where it must be
//!
//! When the line fires, the kernel's handler marks the object pending. The
//! driver sees `READABLE`, services the device, and acknowledges, which clears
//! pending. A line the interrupt controller holds -- level-triggered, or one
//! whose trigger firmware left unsaid -- is also masked by every delivery and
//! unmasked by the acknowledgement. A device that keeps such a line asserted
//! therefore cannot keep a processor in the handler, and a driver that has
//! wedged costs one masked line rather than an interrupt storm.
//!
//! An edge-triggered MSI-X vector is not masked per delivery. Its entry lives
//! in the device's table, so masking and unmasking are two writes to device
//! memory per interrupt -- under a hypervisor, two exits to the emulator for
//! every block read. It needs neither: a message is an edge, it does not stay
//! asserted, and a delivery while the line is pending changes nothing, since
//! `pending` coalesces it into the packet already queued. A delivery between
//! the driver's acknowledgement and its drain is not lost either, because the
//! acknowledgement clears pending first and that delivery queues a packet of
//! its own.
//!
//! What such a vector keeps is a stated bound on what a device may cost the
//! processor between two acknowledgements: at most [`STORM_BOUND`] deliveries
//! reach the handler, each a lock, two atomics and no wake once the line is
//! pending, and the time is charged to whichever task the interrupt cut, as
//! every interrupt's is. The delivery past the bound masks the entry, and the
//! acknowledgement that follows unmasks it. A driver that wedges therefore
//! costs [`STORM_BOUND`] handler runs and one masked line, not a storm. The
//! bound is counted per acknowledgement rather than per unit of time because
//! the acknowledgement is what the holder controls: a device raising
//! completions as fast as its driver takes them is a working device, and one
//! raising them while nobody listens is the storm.
//!
//! # What an interrupt handler may do
//!
//! Take interrupt-safe locks, and wake. The handler marks the object pending
//! and queues its port's packet under interrupt-safe locks only, lets go of
//! them, and then wakes whoever waits on the interrupt and on the port.
//! `WaitQueue::wake_all` may be called from a handler: its own lock and every
//! run queue's are taken with interrupts masked, it allocates nothing, and it
//! leaves the reschedule to the way out of the interrupt or to an IPI. What it
//! must not be called with is a lock it needs already held, which is why the
//! binding's lock is released first. A plain `SpinLock` is still never taken
//! here.
//!
//! # Holding the object is holding the line
//!
//! A claim lasts exactly as long as the [`Interrupt`], which only handles, and
//! messages carrying them, keep alive. When the last of those goes the line is
//! free by the time the close returns, because `object::dispose` drops an
//! interrupt on the spot, and a driver restarted after its predecessor died
//! can claim it straight away.
//!
//! A delivery never holds the claim. The handler takes the claimed [`Line`] --
//! the pending mark, the binding and the waiters -- and fires that, so a holder
//! that lets go while a delivery runs on another processor frees the line at
//! once, and the delivery finishes against state nobody can reach any more: at
//! most one packet on the port the old holder had bound it to. The claim used
//! to be the object itself, found through a weak reference the handler
//! upgraded, and a delivery preempted holding that reference made an immediate
//! re-claim of the line fail (`FX-0901`).
//!
//! Finding a line and masking it are one step under [`BOUND`]'s lock, and so
//! are giving a line up and masking it. Otherwise a delivery that found the old
//! holder, or the old holder's own drop, could mask the line after a new holder
//! had claimed and unmasked it, and the new driver would never hear its device
//! again.
//!
//! # One kernel handler per line, for good
//!
//! `irq` has no way to take a handler back out, so the first `Interrupt` on a
//! line registers [`on_interrupt`] and it stays registered for the life of the
//! machine. The handler finds the line through [`BOUND`]; a line nobody holds
//! is masked and left alone.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use ferrix_sync::IrqSpinLock;

use super::port::{Port, Promise};
use crate::arch;
use crate::device::Vector;
use crate::fallible;
use crate::irq;
use crate::sched::WaitQueue;

/// Each claimed line, by vector number: there exactly as long as the
/// [`Interrupt`] that claimed it.
static BOUND: IrqSpinLock<BTreeMap<u32, Arc<Line>>, arch::Irq> = IrqSpinLock::new(BTreeMap::new());

/// The lines [`on_interrupt`] is registered on.
static REGISTERED: IrqSpinLock<BTreeSet<u32>, arch::Irq> = IrqSpinLock::new(BTreeSet::new());

/// How many deliveries an edge-triggered MSI-X vector, which is not masked
/// per delivery, may make between two acknowledgements. The next one masks
/// it until the acknowledgement: see the module documentation.
///
/// Above a block device's queue depth, 32, so that a device completing a full
/// queue while its driver is busy is not taken for a storm and made to pay the
/// two writes the bound exists to save.
pub const STORM_BOUND: u32 = 64;

/// Why an interrupt could not be claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptError {
    /// Another `Interrupt` holds the line, or the kernel uses it.
    Taken,
    /// The interrupt controller cannot mask this line.
    NotMaskable,
    /// The interrupt is already bound to a port someone still holds.
    AlreadyBound,
    /// There was no memory to claim or bind it.
    NoMemory,
}

/// Where a bound interrupt's packets go.
#[derive(Debug)]
struct Binding {
    /// The port, and the room it promised for this line's packet: a line
    /// queues at most one at a time, so one promise lasts the binding.
    /// Weak, so a binding does not keep a port nobody holds alive.
    port: Promise,
    /// The key its packets carry.
    key: u64,
}

/// What a delivery reaches on a claimed line.
///
/// Shared by the [`Interrupt`] that claimed the line and by [`BOUND`], and
/// held for a moment by a [`Delivery`]: see the module documentation.
#[derive(Debug)]
struct Line {
    /// Which line.
    vector: Vector,
    /// Fired and not yet acknowledged. Set only by a delivery, cleared only by
    /// an acknowledgement.
    pending: AtomicBool,
    /// The port it is bound to, if any.
    ///
    /// A delivery takes this lock before it marks the line pending, and
    /// [`Interrupt::bind`] takes it to bind and to look at pending. That
    /// serialises the two: a delivery and a bind racing on two processors
    /// produce exactly one packet between them, never none and never two.
    binding: IrqSpinLock<Option<Binding>, arch::Irq>,
    /// Woken by a delivery each time the line goes from quiet to pending.
    waiters: WaitQueue,
    /// Deliveries since the last acknowledgement, counted only for a vector
    /// not masked per delivery: what [`STORM_BOUND`] bounds.
    unacknowledged: AtomicU32,
    /// A delivery masked the line and the next acknowledgement unmasks it.
    ///
    /// Set only after the mask is written and read only by the
    /// acknowledgement, so an acknowledgement that finds it set always
    /// unmasks a line that was masked. One that runs between a delivery's mask
    /// and this flag misses it, and the next does not: that delivery goes on
    /// to mark the line pending, which the acknowledgement had just cleared,
    /// and so queues a packet the holder answers with another acknowledgement.
    masked: AtomicBool,
    /// Times [`STORM_BOUND`] masked the line, for the checks.
    storms: AtomicU32,
}

impl Line {
    /// The first half of a delivery: hold the line back where it must be.
    ///
    /// A line the controller holds is masked every time. An edge-triggered
    /// MSI-X vector is counted, and masked only by the delivery past
    /// [`STORM_BOUND`]. Takes no lock, so it runs under [`BOUND`]'s.
    fn hold_back(&self) {
        if !self.vector.coalesces() {
            let _ = self.vector.mask();
            self.masked.store(true, Ordering::Release);
            return;
        }
        let seen = self
            .unacknowledged
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        if seen > STORM_BOUND && !self.masked.load(Ordering::Acquire) {
            let _ = self.vector.mask();
            self.masked.store(true, Ordering::Release);
            let _ = self.storms.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Mark it pending, queue its packet if it is bound and was quiet, and
    /// wake whoever waits on it and on its port.
    ///
    /// From its interrupt handler. The marking and the packet happen under
    /// interrupt-safe locks only, and the wakes after those are released: see
    /// the module documentation.
    fn fire(&self) {
        crate::sched::trip::count(crate::sched::trip::Count::DeviceInterrupt);
        let port = {
            let binding = self.binding.lock();
            if self.pending.swap(true, Ordering::AcqRel) {
                return;
            }
            binding.as_ref().and_then(|bound| {
                let port = bound.port.port()?;
                crate::sched::trip::interrupt_queued(port.waiters());
                let _ = port.queue_from_interrupt(bound.key, crate::timer::now_nanos());
                Some(port)
            })
        };
        self.waiters.wake_all();
        if let Some(port) = port {
            port.waiters().wake_all();
        }
    }

    /// Whether it has fired and not been acknowledged.
    fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }
}

/// Register [`on_interrupt`] on line `number`, once for the life of the
/// machine, and record that it is.
///
/// Room to record it is reserved first: `irq` has no way to take a handler
/// back out, so one registered and not recorded would be registered again by
/// the next claim, and refused.
fn register_handler(number: u32) -> Result<(), InterruptError> {
    let mut lines = REGISTERED.lock();
    if lines.contains(&number) {
        return Ok(());
    }
    let held = fallible::reserve().map_err(|_| InterruptError::NoMemory)?;
    irq::register(number, on_interrupt).map_err(|_| InterruptError::Taken)?;
    let _ = fallible::insert_into_set_held(&held, &mut lines, number);
    Ok(())
}

/// A device interrupt a driver holds, and with it the claim on its line.
#[derive(Debug)]
pub struct Interrupt {
    /// The line it claimed.
    line: Arc<Line>,
}

impl Interrupt {
    /// Claim `vector`.
    ///
    /// # Nothing is masked until the line is known to be free
    ///
    /// The line is claimed in [`BOUND`] first, then the kernel's handler is
    /// registered, and only then is the line unmasked. The first version
    /// masked it up front, to find out whether it could be masked, and a
    /// refused second claim, or a claim on a line the kernel itself uses,
    /// would then have masked a line somebody else owns. A delivery that
    /// arrives between registering and unmasking finds the line in the table
    /// already, so it is not lost.
    ///
    /// # Errors
    ///
    /// [`InterruptError::Taken`], [`InterruptError::NotMaskable`], and
    /// [`InterruptError::NoMemory`].
    pub fn new(vector: Vector) -> Result<Arc<Interrupt>, InterruptError> {
        let number = vector.number();
        let line = fallible::try_arc(Line {
            vector,
            pending: AtomicBool::new(false),
            binding: IrqSpinLock::new(None),
            waiters: WaitQueue::new(),
            unacknowledged: AtomicU32::new(0),
            masked: AtomicBool::new(false),
            storms: AtomicU32::new(0),
        })
        .map_err(|_| InterruptError::NoMemory)?;
        let interrupt =
            fallible::try_arc(Interrupt { line }).map_err(|_| InterruptError::NoMemory)?;

        {
            let mut bound = BOUND.lock();
            if bound.contains_key(&number) {
                // `interrupt` is dropped on the way out, and its drop leaves
                // the live holder's line alone: see `Drop`.
                return Err(InterruptError::Taken);
            }
            let _ = fallible::insert(&mut bound, number, Arc::clone(&interrupt.line))
                .map_err(|_| InterruptError::NoMemory)?;
        }

        let registered = register_handler(number);
        if let Err(why) = registered {
            // The kernel handles this line itself, or there was no memory to
            // say that this one does. Unclaimed before the object is
            // dropped, so its drop does not mask the kernel's line.
            let _unclaimed = BOUND.lock().remove(&number);
            return Err(why);
        }

        if vector.unmask().is_err() {
            let _unclaimed = BOUND.lock().remove(&number);
            return Err(InterruptError::NotMaskable);
        }
        Ok(interrupt)
    }

    /// Deliver it to `port` as packets carrying `key`: one each time it goes
    /// from quiet to pending, so a device that fires twice before its driver
    /// acknowledges produces one packet, not a queue of them. If it is already
    /// pending when bound, its packet is queued at once.
    ///
    /// # Errors
    ///
    /// [`InterruptError::AlreadyBound`] if it is bound to a port anyone still
    /// holds; [`InterruptError::NoMemory`] if the port could not promise room
    /// for the line's packet.
    pub fn bind(&self, port: &Arc<Port>, key: u64) -> Result<(), InterruptError> {
        // Promised before the binding's lock is taken, and given back as it
        // drops if the bind is refused.
        let promise = Promise::new(port).map_err(|_| InterruptError::NoMemory)?;
        let pending = {
            let mut binding = self.line.binding.lock();
            if binding.as_ref().is_some_and(|bound| bound.port.is_live()) {
                return Err(InterruptError::AlreadyBound);
            }
            *binding = Some(Binding { port: promise, key });
            self.line.is_pending()
        };
        if pending {
            port.queue_interrupt(key, crate::timer::now_nanos());
        }
        Ok(())
    }

    /// Whether it has fired and not been acknowledged.
    pub(crate) fn is_pending(&self) -> bool {
        self.line.is_pending()
    }

    /// The queue woken when it fires.
    pub(crate) fn waiters(&self) -> &WaitQueue {
        &self.line.waiters
    }

    /// The driver has serviced the device: clear pending and let the line
    /// through again.
    ///
    /// Pending is cleared first, so a delivery from here on queues a packet
    /// of its own rather than being coalesced into the one being answered. A
    /// line the controller holds is unmasked every time, as it is masked
    /// every time. An edge-triggered MSI-X vector is unmasked only if a
    /// delivery past [`STORM_BOUND`] masked it: otherwise it never was, and
    /// the write would be a device access for nothing.
    ///
    /// # Errors
    ///
    /// If the controller refuses to unmask, which a line it masked does not.
    pub fn acknowledge(&self) -> Result<(), &'static str> {
        self.line.pending.store(false, Ordering::Release);
        self.line.unacknowledged.store(0, Ordering::Release);
        let masked = self.line.masked.swap(false, Ordering::AcqRel);
        if masked || !self.line.vector.coalesces() {
            return self.line.vector.unmask();
        }
        Ok(())
    }

    /// Times [`STORM_BOUND`] has masked the line, for the checks.
    pub fn storms(&self) -> u32 {
        self.line.storms.load(Ordering::Relaxed)
    }

    /// Whether the line reads back masked where it is masked, where that can
    /// be read (an MSI-X entry), for the checks.
    pub fn reads_masked(&self) -> Option<bool> {
        self.line.vector.reads_masked()
    }

    /// Whether a delivery leaves the line unmasked: an edge-triggered MSI-X
    /// vector, below [`STORM_BOUND`].
    pub fn coalesces(&self) -> bool {
        self.line.vector.coalesces()
    }
}

impl Drop for Interrupt {
    /// Give the line up and mask it, but only if this object held it.
    ///
    /// A claim refused because the line was taken drops an object that never
    /// held anything; masking there would silence the live holder's device.
    /// The table's entry tells them apart: it is this object's own line or it
    /// is not. Masked before the table's lock goes, so that nobody can claim
    /// the line and unmask it in between and then have it masked from under
    /// them.
    fn drop(&mut self) {
        let number = self.line.vector.number();
        let given_up = {
            let mut bound = BOUND.lock();
            if bound
                .get(&number)
                .is_some_and(|held| Arc::ptr_eq(held, &self.line))
            {
                let _ = self.line.vector.mask();
                bound.remove(&number)
            } else {
                None
            }
        };
        // The table's reference goes after its lock does.
        drop(given_up);
    }
}

/// A delivery found and not yet fired: what the handler holds between finding
/// the line and waking its waiters.
///
/// It holds the line's state, never the claim, so the holder can let go and
/// somebody else can claim the line while one is in flight. Its reference may
/// be the last to a line given up meanwhile, whose drop only frees memory.
#[derive(Debug)]
pub struct Delivery(Arc<Line>);

impl Delivery {
    /// Mark the line pending, queue its packet, and wake its waiters.
    pub fn fire(self) {
        self.0.fire();
    }
}

/// The first half of a delivery on line `number`: find the line its holder
/// claimed and hold it back ([`Line::hold_back`]: mask it, or count it
/// against [`STORM_BOUND`]), in one step under [`BOUND`]'s lock.
///
/// The line is found before it is masked, because only its [`Vector`] knows
/// where masking happens: at the controller for a line, at the device's table
/// entry for an MSI-X vector, which the controller cannot reach. A line nobody
/// holds is masked at the controller and has nothing to deliver; an MSI-X
/// entry nobody holds was masked by its holder's drop.
pub fn take_delivery(number: u32) -> Option<Delivery> {
    let bound = BOUND.lock();
    match bound.get(&number) {
        Some(line) => {
            line.hold_back();
            Some(Delivery(Arc::clone(line)))
        }
        None => {
            let _ = arch::mask_interrupt(number);
            None
        }
    }
}

/// The kernel's handler for every line an `Interrupt` has claimed.
///
/// Mask, mark, queue a bound interrupt's packet, and wake, and nothing else:
/// see the module documentation for what an interrupt handler may not do.
pub fn on_interrupt(number: u32) {
    if let Some(delivery) = take_delivery(number) {
        delivery.fire();
    }
}
