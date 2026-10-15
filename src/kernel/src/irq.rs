//! Device interrupts, once a controller is delivering them.
//!
//! The architectures number interrupts differently and acknowledge them
//! differently — x86-64 hands the kernel a vector and wants an end-of-interrupt
//! written to the local APIC afterwards; the GIC hands over an identifier that
//! must be given back to the same register it came from — so the acknowledge
//! protocol stays in `arch`, and what arrives here is the number and nothing
//! else.
//!
//! # Locking
//!
//! The table is behind an interrupt-masking lock, and [`dispatch`] holds it
//! only long enough to copy a function pointer out. The handler runs after the
//! lock is released, so two CPUs taking interrupts at once contend for a load
//! rather than waiting on each other's handlers.
//!
//! What a lock does not give is safe *removal*: a handler unregistered on one
//! CPU may still be running on another that copied it out a moment earlier.
//! Nothing is unregistered before stage 10, and `smp::synchronize` is what
//! will make it safe when something is — a running handler is a read-side
//! section, because it runs with interrupts masked, so taking a handler out of
//! the table and then waiting a grace period leaves nothing still running it.

use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_sync::IrqSpinLock;

/// Interrupt numbers the table can hold.
///
/// Sized for the largest of the architectures: the GIC's interrupt
/// identifier space runs to 1020, while x86-64 has 224 usable vectors above
/// the CPU's own exceptions. Then 256 more for the message-signalled vectors
/// a GICv3's ITS hands out: the GIC numbers them from 8192, and its driver
/// presents them from 1024 so that the table need not span the gap.
pub(crate) const SLOTS: usize = 1024 + 256;

/// What a device interrupt runs. Takes its own number, so one function can
/// serve several lines without a closure — there is no allocator guarantee at
/// the point some of these are registered.
pub(crate) type Handler = fn(u32);

/// Why an interrupt could not be registered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IrqError {
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
///
/// Interrupt-masking because [`dispatch`] takes it from interrupt context: a
/// plain lock held by [`register`] on a CPU that then took an interrupt would
/// be waited on by that same CPU's handler, forever.
static HANDLERS: IrqSpinLock<[Option<Handler>; SLOTS], crate::arch::Irq> =
    IrqSpinLock::new([None; SLOTS]);

/// Interrupts delivered to a handler.
static DELIVERED: AtomicU64 = AtomicU64::new(0);

/// Interrupts that arrived with nothing registered for them.
///
/// Counted rather than ignored: a line nobody claimed is either a device the
/// kernel forgot to quiesce or a controller programmed wrongly, and both are
/// worth seeing in the boot log rather than discovering as a livelock.
static UNCLAIMED: AtomicU64 = AtomicU64::new(0);

/// Where a device writes to raise one allocated interrupt, and the number it
/// then arrives as. What `arch::msi_allocate` hands out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Msi {
    /// What [`register`] and [`dispatch`] call it.
    pub number: u32,
    /// The address the device writes.
    pub address: u64,
    /// The value it writes there.
    pub data: u32,
}

/// Attach `handler` to interrupt `irq`.
///
/// # Errors
///
/// [`IrqError::OutOfRange`] past the end of the table, and
/// [`IrqError::AlreadyTaken`] if something is already there.
pub fn register(irq: u32, handler: Handler) -> Result<(), IrqError> {
    let mut table = HANDLERS.lock();
    let slot = table
        .get_mut(irq as usize)
        .ok_or(IrqError::OutOfRange(irq))?;
    if slot.is_some() {
        return Err(IrqError::AlreadyTaken(irq));
    }
    *slot = Some(handler);
    Ok(())
}

/// Move the handler registered on `from` to `to`, and leave `from` with
/// `left`, in one hold of the table's lock: so an interrupt arriving on
/// either number finds a handler, and none finds the moved one twice.
///
/// For x86-64's conversion of the console's I/O APIC line to a remapped
/// vector of its own at interrupt remapping's bring-up (`docs/NVIDIA.md`
/// §12.3): the handler goes to the new vector before the line is unmasked,
/// and the old vector keeps a handler that counts what still arrives on it.
///
/// # Errors
///
/// [`IrqError::OutOfRange`] for either number past the table, and
/// [`IrqError::AlreadyTaken`] when `to` has a handler or `from` has none;
/// the table is then unchanged.
#[cfg_attr(
    not(target_arch = "x86_64"),
    expect(dead_code, reason = "only x86-64 converts a live interrupt line")
)]
pub(crate) fn move_handler(from: u32, to: u32, left: Handler) -> Result<(), IrqError> {
    let mut table = HANDLERS.lock();
    if table
        .get(to as usize)
        .ok_or(IrqError::OutOfRange(to))?
        .is_some()
    {
        return Err(IrqError::AlreadyTaken(to));
    }
    let slot = table
        .get_mut(from as usize)
        .ok_or(IrqError::OutOfRange(from))?;
    let Some(moved) = *slot else {
        return Err(IrqError::AlreadyTaken(from));
    };
    *slot = Some(left);
    if let Some(slot) = table.get_mut(to as usize) {
        *slot = Some(moved);
    }
    Ok(())
}

/// Undo [`move_handler`]: the handler on `from` goes back to `to`, over
/// what [`move_handler`] left there, and `from` is left empty. Nothing
/// changes unless both have a handler.
#[cfg_attr(
    not(target_arch = "x86_64"),
    expect(dead_code, reason = "only x86-64 converts a live interrupt line")
)]
pub(crate) fn return_handler(from: u32, to: u32) {
    let mut table = HANDLERS.lock();
    let (Some(&Some(moved)), Some(&Some(_))) = (table.get(from as usize), table.get(to as usize))
    else {
        return;
    };
    if let Some(slot) = table.get_mut(to as usize) {
        *slot = Some(moved);
    }
    if let Some(slot) = table.get_mut(from as usize) {
        *slot = None;
    }
}

/// Whether something is registered on `irq`.
///
/// For stage 10's device nodes: a line the kernel handles itself is not one a
/// driver may be given.
pub(crate) fn is_registered(irq: u32) -> bool {
    HANDLERS
        .lock()
        .get(irq as usize)
        .is_some_and(Option::is_some)
}

/// Run whatever is registered for `irq`.
///
/// Called from the architecture's interrupt path, which has already
/// acknowledged the controller as that controller requires.
pub(crate) fn dispatch(irq: u32) {
    // Copied out, and the guard dropped at the end of this statement: the
    // handler runs without the table locked, so a handler that registers
    // another line does not deadlock, and other CPUs are not held up by it.
    let handler = HANDLERS.lock().get(irq as usize).copied().flatten();
    match handler {
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
pub fn delivered() -> u64 {
    DELIVERED.load(Ordering::Relaxed)
}

/// How many arrived with nothing registered.
pub fn unclaimed() -> u64 {
    UNCLAIMED.load(Ordering::Relaxed)
}

/// What interrupt and time bring-up found, for the boot log.
///
/// Named parts rather than a formatted line, so the architecture reports what
/// it discovered and generic code decides how a boot log looks.
#[derive(Clone, Copy, Debug)]
pub struct Report {
    /// What the free-running counter is.
    pub counter: &'static str,
    /// How fast it counts.
    pub counter_hz: u64,
    /// What the interrupt controller is.
    pub controller: &'static str,
    /// What the timer is.
    pub timer: &'static str,
    /// How fast the timer counts, which is not the counter's frequency: on
    /// x86-64 they are two different pieces of hardware.
    pub timer_hz: u64,
}
