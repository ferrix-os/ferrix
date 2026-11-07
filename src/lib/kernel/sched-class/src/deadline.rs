//! A deadline class's interface: what a reservation asks for, why one is
//! refused, the operations the kernel reaches a deadline tier by, and the
//! cell a task keeps its reservation in (`docs/SCHED-CLASSES.md` §9g).
//!
//! The rule itself -- admission, the hard budget, the refill, the wake rule
//! -- is `ferrix-sched-edf`'s; this is only what the kernel names, so that no
//! kernel file names a class crate (§9b, G2).

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::{CpuSet, SchedClass};

/// The most reservations on one processor: what bounds a pick's scan.
pub const MOST: usize = 32;
/// The most of a processor reservations may ask for, in parts per million.
pub const LIMIT_PPM: u64 = 950_000;
/// The least budget a reservation may have: Linux's 1,024 ns would be shorter than a switch.
pub const LEAST_RUNTIME_NS: u64 = 100_000;
/// The shortest period, which bounds the refill timer's rate.
pub const LEAST_PERIOD_NS: u64 = 1_000_000;
/// The longest period, which keeps the arithmetic in range.
pub const MOST_PERIOD_NS: u64 = 10_000_000_000;

/// What a thread asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reservation {
    /// Its budget in each period.
    pub runtime_ns: u64,
    /// When each period's work is due, from the period's start.
    pub deadline_ns: u64,
    /// The period.
    pub period_ns: u64,
}

/// Why a thread was not admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not `LEAST_RUNTIME_NS <= runtime <= period`, `deadline == period`, and the period
    /// within `LEAST_PERIOD_NS` to `MOST_PERIOD_NS`.
    Malformed,
    /// With it the processor's reservations would pass [`LIMIT_PPM`]: an unschedulable set;
    /// also a stack with no deadline tier.
    Unschedulable,
    /// The processor already holds [`MOST`].
    Full,
    /// It is already admitted.
    Already,
    /// The caller has not the authority to reserve (`THREAD_RESERVE`'s job handle).
    Denied,
}

impl Reservation {
    /// Its share of a processor, in parts per million, rounded up, or why it has none.
    ///
    /// # Errors
    ///
    /// [`Refusal::Malformed`].
    pub const fn share_ppm(self) -> Result<u64, Refusal> {
        if self.runtime_ns < LEAST_RUNTIME_NS
            || self.runtime_ns > self.period_ns
            || self.deadline_ns != self.period_ns
            || self.period_ns < LEAST_PERIOD_NS
            || self.period_ns > MOST_PERIOD_NS
        {
            return Err(Refusal::Malformed);
        }
        // At most 10^10 * 10^6, well inside 64 bits.
        Ok((self.runtime_ns * 1_000_000).div_ceil(self.period_ns))
    }
}

/// A deadline class: a [`SchedClass`] whose entities are admitted to a processor before they
/// are queued there, and whose time is absolute (ledger 7763 DG.4): [`DeadlineClass::note_time`]
/// tells it the time, before a charge, before a pick and before each enqueue onto it, and
/// [`SchedClass::elapse`] does nothing.
pub trait DeadlineClass<T>: SchedClass<T> {
    /// The entity of an admitted task, which goes to its admission's entry.
    fn admitted() -> Self::Entity;
    /// Admit thread `id`.
    ///
    /// # Errors
    ///
    /// [`Refusal`], with nothing changed.
    fn admit(queue: &mut Self::Queue, id: u64, reservation: Reservation) -> Result<(), Refusal>;
    /// Take thread `id`'s reservation away wherever it stands: its share comes back now, and
    /// its entry goes once no queue holds it there. Answers whether it held one.
    fn withdraw(queue: &mut Self::Queue, id: u64) -> bool;
    /// Tell the class the time; it never goes back.
    fn note_time(queue: &mut Self::Queue, now: u64);
    /// Whether thread `id` holds an admission here, given back or not.
    fn is_reserved(queue: &Self::Queue, id: u64) -> bool;
    /// Whether a reservation with budget is waiting to run.
    fn any_ready(queue: &Self::Queue) -> bool;
    /// Whether a reservation is running here.
    fn runs_reserved(queue: &Self::Queue) -> bool;
    /// The sum of the shares, in parts per million, and the throttles and refills so far.
    fn state(queue: &Self::Queue) -> (u64, u64, u64);
    /// The identifier of a waiting entity whose payload `matches`, never the running one: for
    /// a processor taken out of service.
    fn waiting_where(queue: &Self::Queue, matches: impl Fn(&T) -> bool) -> Option<u64>;
}

/// What an upper tier is, for the stack's one generic impl over it (ledger 7763 DG.6): a kind
/// marker, so that the impl names no class crate and two kinds never overlap.
pub trait UpperClass<T>: SchedClass<T> {
    /// [`RealtimeKind`] or [`DeadlineKind`].
    type Kind;
}

/// The kind of a fixed-priority tier ([`crate::RealtimeClass`]).
#[derive(Debug)]
pub struct RealtimeKind;

/// The kind of a deadline tier ([`DeadlineClass`]).
#[derive(Debug)]
pub struct DeadlineKind;

/// None held: a [`ReserveCell`]'s word.
pub const NONE: usize = usize::MAX;

/// Where a task keeps its reservation: the processor that admitted it and the mask its pin
/// replaced. Zero-sized in a stack without a deadline tier ([`NoReserve`]).
pub trait ReserveCell {
    /// An empty cell: what every new task, a child of a reserved one included, starts with
    /// (7088 E.3).
    fn empty() -> Self;
    /// The processor holding its reservation, [`NONE`] for none.
    fn on(&self) -> usize;
    /// Set it, answering what it was.
    fn swap_on(&self, cpu: usize) -> usize;
    /// Keep the mask the pin replaced.
    fn keep_mask(&self, mask: Option<CpuSet>);
    /// The mask the pin replaced, taken.
    fn take_mask(&self) -> Option<CpuSet>;
}

/// The cell of a stack with no deadline tier: never holds anything.
#[derive(Debug)]
pub struct NoReserve;

impl ReserveCell for NoReserve {
    fn empty() -> Self {
        NoReserve
    }
    fn on(&self) -> usize {
        NONE
    }
    fn swap_on(&self, _: usize) -> usize {
        NONE
    }
    fn keep_mask(&self, _: Option<CpuSet>) {}
    fn take_mask(&self) -> Option<CpuSet> {
        None
    }
}

/// The cell of a stack with a deadline tier.
#[derive(Debug)]
pub struct HeldReserve {
    on: AtomicUsize,
    mask_before: ferrix_sync::SpinLock<Option<CpuSet>>,
}

impl ReserveCell for HeldReserve {
    fn empty() -> Self {
        HeldReserve {
            on: AtomicUsize::new(NONE),
            mask_before: ferrix_sync::SpinLock::new(None),
        }
    }
    fn on(&self) -> usize {
        self.on.load(Ordering::Acquire)
    }
    fn swap_on(&self, cpu: usize) -> usize {
        self.on.swap(cpu, Ordering::AcqRel)
    }
    fn keep_mask(&self, mask: Option<CpuSet>) {
        *self.mask_before.lock() = mask;
    }
    fn take_mask(&self) -> Option<CpuSet> {
        self.mask_before.lock().take()
    }
}
