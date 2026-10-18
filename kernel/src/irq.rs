//! Device interrupts, once a controller is delivering them.
//!
//! The two architectures number interrupts differently and acknowledge them
//! differently — x86-64 hands the kernel a vector and wants an end-of-interrupt
//! written to the local APIC afterwards; the GIC hands over an identifier that
//! must be given back to the same register it came from — so the acknowledge
//! protocol stays in `arch`, and what arrives here is the number and nothing
//! else.
//!
//! # One CPU
//!
//! The table below is reached without a lock, which is sound today for the
//! same reason `mm`'s globals are: no second CPU has been started, and a
//! handler cannot re-enter this because the gate masks interrupts on entry.
//! Stage 4 makes both of those false and turns this into a read-mostly
//! structure behind the RCU-like grace period the roadmap already asks for.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

/// Interrupt numbers the table can hold.
///
/// Sized for the larger of the two architectures: the GIC's interrupt
/// identifier space runs to 1020, while x86-64 has 224 usable vectors above
/// the CPU's own exceptions.
pub(crate) const SLOTS: usize = 1024;

/// What a device interrupt runs. Takes its own number, so one function can
/// serve several lines without a closure — there is no allocator guarantee at
/// the point some of these are registered.
pub(crate) type Handler = fn(u32);

/// Why an interrupt could not be registered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum IrqError {
    /// The number is outside the table.
    OutOfRange(u32),
    /// Something is already registered on that number. Sharing a line is a
    /// stage 10 problem, and silently replacing the first handler would be a
    /// bug that only shows up as a device going quiet.
    AlreadyTaken(u32),
}

impl core::fmt::Display for IrqError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            IrqError::OutOfRange(irq) => write!(f, "interrupt {irq} is outside the table"),
            IrqError::AlreadyTaken(irq) => write!(f, "interrupt {irq} already has a handler"),
        }
    }
}

/// The handler table.
struct Table(UnsafeCell<[Option<Handler>; SLOTS]>);

// SAFETY: written only by `register` during single-threaded bring-up, and read
// by `dispatch` from an interrupt handler on the same CPU. No second CPU exists
// until stage 4, and the interrupt gate masks interrupts on entry, so a
// handler cannot re-enter `dispatch` and observe a torn write.
unsafe impl Sync for Table {}

static HANDLERS: Table = Table(UnsafeCell::new([None; SLOTS]));

/// Interrupts delivered to a handler.
static DELIVERED: AtomicU64 = AtomicU64::new(0);

/// Interrupts that arrived with nothing registered for them.
///
/// Counted rather than ignored: a line nobody claimed is either a device the
/// kernel forgot to quiesce or a controller programmed wrongly, and both are
/// worth seeing in the boot log rather than discovering as a livelock.
static UNCLAIMED: AtomicU64 = AtomicU64::new(0);

/// Attach `handler` to interrupt `irq`.
///
/// # Errors
///
/// [`IrqError::OutOfRange`] past the end of the table, and
/// [`IrqError::AlreadyTaken`] if something is already there.
pub(crate) fn register(irq: u32, handler: Handler) -> Result<(), IrqError> {
    // SAFETY: single-threaded bring-up, as documented on the `Sync` impl. The
    // reference does not escape this function.
    let table = unsafe { &mut *HANDLERS.0.get() };
    let slot = table
        .get_mut(irq as usize)
        .ok_or(IrqError::OutOfRange(irq))?;
    if slot.is_some() {
        return Err(IrqError::AlreadyTaken(irq));
    }
    *slot = Some(handler);
    Ok(())
}

/// Run whatever is registered for `irq`.
///
/// Called from the architecture's interrupt path, which has already
/// acknowledged the controller as that controller requires.
pub(crate) fn dispatch(irq: u32) {
    // SAFETY: as `register`. This runs with interrupts masked by the gate, so
    // it cannot observe a half-written slot.
    let table = unsafe { &*HANDLERS.0.get() };
    match table.get(irq as usize).copied().flatten() {
        Some(handler) => {
            let _ = DELIVERED.fetch_add(1, Ordering::Relaxed);
            handler(irq);
        }
        None => {
            let _ = UNCLAIMED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// How many interrupts reached a handler.
pub(crate) fn delivered() -> u64 {
    DELIVERED.load(Ordering::Relaxed)
}

/// How many arrived with nothing registered.
pub(crate) fn unclaimed() -> u64 {
    UNCLAIMED.load(Ordering::Relaxed)
}

/// What interrupt and time bring-up found, for the boot log.
///
/// Named parts rather than a formatted line, so the architecture reports what
/// it discovered and generic code decides how a boot log looks.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Report {
    /// What the free-running counter is.
    pub(crate) counter: &'static str,
    /// How fast it counts.
    pub(crate) counter_hz: u64,
    /// What the interrupt controller is.
    pub(crate) controller: &'static str,
    /// What the timer is.
    pub(crate) timer: &'static str,
    /// How fast the timer counts, which is not the counter's frequency: on
    /// x86-64 they are two different pieces of hardware.
    pub(crate) timer_hz: u64,
}
