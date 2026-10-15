//! Where one trip through the block ring spends its time: stamps along the
//! path a read takes, and counts of what the path did (`docs/OPAQUE-KERNEL.md`,
//! S0).
//!
//! The seam check (`interfaces::block_ring::hop_check`) times a read from its
//! call to its answer, which says how long a trip takes and not where the time
//! goes. While that check's depth-1 run is going, and only then, [`TRACING`]
//! is up, and each point the read passes writes the processor's counter and
//! its own number into its slot:
//!
//! | Stamp | Where |
//! | --- | --- |
//! | [`Stamp::Issued`] | the check calls the read |
//! | [`Stamp::Queued`] | the read is on the disk's queue, before the ring's task is nudged |
//! | [`Stamp::RingRunning`] | the ring's task is running after its sleep |
//! | [`Stamp::OnRing`] | the command is on the ring |
//! | [`Stamp::Bell`] | the ring rings the driver |
//! | [`Stamp::DriverBell`] | the driver's `port_wait` takes the bell |
//! | [`Stamp::Irq`] | the device's interrupt queues its packet on the driver's port |
//! | [`Stamp::DriverIrq`] | the driver's `port_wait` takes that packet |
//! | [`Stamp::Posted`] | the driver rings the ring's port with the completion |
//! | [`Stamp::RingAgain`] | the ring's task is running again |
//! | [`Stamp::Answered`] | the ring has copied the answer out for the reader |
//! | [`Stamp::Reader`] | the reader is running again |
//! | [`Stamp::Done`] | the read has returned to the check |
//!
//! Everywhere else a stamp point costs one relaxed load of [`TRACING`], and
//! so does each count. Nothing here allocates or takes a lock: the slots are
//! atomics, and the check copies them out after each read.
//!
//! # Order, and hops a trip did not take
//!
//! A point stamps only when the trip has reached it: [`NEXT`] is the slot the
//! trip expects next, and each point names the earliest slot it may follow.
//! Most must follow directly. A few hops happen only when something was
//! asleep: a ring task already running is not woken, and a driver already
//! awake is not rung. So [`Stamp::OnRing`] may follow [`Stamp::Queued`], the
//! interrupt may follow the ring's bell or the command itself, and so on. A
//! slot passed over takes the time and processor of the stamp before it, so
//! its hop reads as zero and the hop after it carries the time, and the trip
//! records it in its skipped mask. A trip whose [`Stamp::Done`] finds an
//! earlier slot unreached is incomplete, and the check leaves it out.
//!
//! Which disk and which ports are the traced ones is recorded as the trip
//! goes: [`Stamp::Queued`] names the disk by the address of its reader's wait
//! queue, and [`Stamp::OnRing`] the ring's two ports by the addresses of
//! theirs. A point on another disk or another port does not match, and does
//! not stamp.
//!
//! The processor's number is read with interrupts masked, with the counter,
//! so the two belong to the same processor.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use ferrix_sync::IrqControl;

use crate::arch;

use super::WaitQueue;

/// A point on the trip, in the order a trip passes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stamp {
    /// The check calls the read.
    Issued,
    /// The read is queued, before the ring's task is nudged.
    Queued,
    /// The ring's task is running, after the nudge woke it.
    RingRunning,
    /// The command is on the ring.
    OnRing,
    /// The ring rings the driver.
    Bell,
    /// The driver takes the bell.
    DriverBell,
    /// The device's interrupt is queued for the driver.
    Irq,
    /// The driver takes the interrupt's packet.
    DriverIrq,
    /// The driver rings the ring's port with the completion.
    Posted,
    /// The ring's task is running again.
    RingAgain,
    /// The answer is copied out for the reader.
    Answered,
    /// The reader is running again.
    Reader,
    /// The read has returned.
    Done,
}

/// How many stamps a trip has.
pub const STAMPS: usize = 13;

/// Each stamp's name, for the check's line.
pub const NAMES: [&str; STAMPS] = [
    "issue",
    "queued",
    "ring",
    "on-ring",
    "bell",
    "drv-bell",
    "irq",
    "drv-irq",
    "posted",
    "ring-again",
    "answered",
    "reader",
    "done",
];

/// What the trace counts beside the stamps, while it is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Count {
    /// A user root written: a program's address space installed.
    RootInstall,
    /// A user root taken off: back to the kernel's alone.
    RootUninstall,
    /// An interprocessor interrupt sent: one command, however many
    /// processors a broadcast reaches.
    Ipi,
    /// A device interrupt delivered to a driver's line.
    DeviceInterrupt,
}

/// How many kinds [`Count`] has.
const COUNTS: usize = 4;

/// The traced wait queues: the reader's, the ring's port's and the driver's
/// port's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Traced {
    /// The disk's reader queue.
    Reader,
    /// The ring's completion port.
    RingPort,
    /// The driver's port.
    DriverPort,
}

/// How many traced queues there are.
const QUEUES: usize = 3;

/// Whether the seam check's depth-1 run is being traced.
static TRACING: AtomicBool = AtomicBool::new(false);
/// The slot the trip expects next; [`STAMPS`] once it is done.
static NEXT: AtomicUsize = AtomicUsize::new(STAMPS);
/// Each slot's counter value.
static AT: [AtomicU64; STAMPS] = [const { AtomicU64::new(0) }; STAMPS];
/// Each slot's processor.
static CPU: [AtomicU32; STAMPS] = [const { AtomicU32::new(0) }; STAMPS];
/// The slots this trip passed over, as a mask.
static SKIPPED: AtomicU32 = AtomicU32::new(0);
/// The traced queues' addresses, in [`Traced`]'s order: zero, which no
/// queue lives at, until a trip names them.
static QUEUE_AT: [AtomicUsize; QUEUES] = [const { AtomicUsize::new(0) }; QUEUES];
/// [`Count`]s since the trace was armed.
static COUNTED: [AtomicU64; COUNTS] = [const { AtomicU64::new(0) }; COUNTS];
/// Sleeps on each traced queue since the trace was armed that a waker
/// ended, then those the recheck timer ended.
static SLEPT: [[AtomicU64; 2]; QUEUES] = [const { [const { AtomicU64::new(0) }; 2] }; QUEUES];

/// One traced trip: each stamp's counter value and processor, and the slots
/// it passed over.
#[derive(Debug, Clone, Copy)]
pub struct Trip {
    /// Counter values, in the architecture's counter's ticks.
    pub at: [u64; STAMPS],
    /// Processors, by logical number.
    pub cpu: [u32; STAMPS],
    /// Bit `n` set: slot `n` was passed over, and holds the stamp before it.
    pub skipped: u32,
}

/// What the trace counted since it was armed.
#[derive(Debug, Clone, Copy, Default)]
pub struct Counted {
    /// By [`Count`], in its order.
    pub counts: [u64; COUNTS],
    /// By [`Traced`], in its order: sleeps a wake ended, and sleeps the
    /// recheck ended.
    pub slept: [[u64; 2]; QUEUES],
}

/// Raise the trace, and zero its counts. The queues named by earlier trips
/// stay named, so a check can run one trip to name them and then count from
/// a clean start.
pub fn arm() {
    for count in &COUNTED {
        count.store(0, Ordering::Relaxed);
    }
    for queue in &SLEPT {
        for slept in queue {
            slept.store(0, Ordering::Relaxed);
        }
    }
    NEXT.store(STAMPS, Ordering::Relaxed);
    TRACING.store(true, Ordering::Release);
}

/// Lower the trace and forget the queues it named.
pub fn disarm() {
    TRACING.store(false, Ordering::Release);
    for queue in &QUEUE_AT {
        queue.store(0, Ordering::Relaxed);
    }
}

/// What the trace has counted since [`arm`].
pub fn counted() -> Counted {
    let mut counted = Counted::default();
    for (to, from) in counted.counts.iter_mut().zip(&COUNTED) {
        *to = from.load(Ordering::Relaxed);
    }
    for (to, from) in counted.slept.iter_mut().zip(&SLEPT) {
        for (to, from) in to.iter_mut().zip(from) {
            *to = from.load(Ordering::Relaxed);
        }
    }
    counted
}

/// Count one `what`, while the trace is up.
pub(crate) fn count(what: Count) {
    if !TRACING.load(Ordering::Relaxed) {
        return;
    }
    if let Some(count) = COUNTED.get(what as usize) {
        let _ = count.fetch_add(1, Ordering::Relaxed);
    }
}

/// A sleep on `queue` ended, `woken` by a waker or else by the recheck timer:
/// counted if `queue` is a traced one.
pub(crate) fn slept(queue: &WaitQueue, woken: bool) {
    if !TRACING.load(Ordering::Relaxed) {
        return;
    }
    let Some(which) = traced(address(queue)) else {
        return;
    };
    let slot = SLEPT
        .get(which)
        .and_then(|counts| counts.get(usize::from(!woken)));
    if let Some(slot) = slot {
        let _ = slot.fetch_add(1, Ordering::Relaxed);
    }
}

/// The check calls a read: a new trip starts.
pub fn issued() {
    if !TRACING.load(Ordering::Relaxed) {
        return;
    }
    let (now, cpu) = now_here();
    SKIPPED.store(0, Ordering::Relaxed);
    store(Stamp::Issued as usize, now, cpu);
    NEXT.store(Stamp::Queued as usize, Ordering::Release);
}

/// The read is queued on the disk whose readers wait on `reader`: that is
/// the traced disk.
pub fn queued(reader: &WaitQueue) {
    if !TRACING.load(Ordering::Relaxed) {
        return;
    }
    if NEXT.load(Ordering::Acquire) == Stamp::Queued as usize {
        name(Traced::Reader, reader);
        let _ = stamp(Stamp::Queued, Stamp::Queued);
    }
}

/// The ring task serving the disk of `reader` is running after a sleep: the
/// first wake of a trip, or the one after the driver's completion.
pub fn ring_running(reader: &WaitQueue) {
    if !TRACING.load(Ordering::Relaxed) || !is(Traced::Reader, reader) {
        return;
    }
    let _ = stamp(Stamp::RingRunning, Stamp::RingRunning);
    let _ = stamp(Stamp::RingAgain, Stamp::RingAgain);
}

/// The ring serving the disk of `reader` put a command on the ring; its
/// completion port's queue is `ring_port`, which becomes the traced ring
/// port.
pub fn on_ring(reader: &WaitQueue, ring_port: &WaitQueue) {
    if !TRACING.load(Ordering::Relaxed) || !is(Traced::Reader, reader) {
        return;
    }
    if stamp(Stamp::OnRing, Stamp::RingRunning) {
        name(Traced::RingPort, ring_port);
    }
}

/// The ring serving the disk of `reader` rings its driver, whose port's
/// queue is `driver_port`, which becomes the traced driver port. A driver
/// already awake is not rung, and its port stays unnamed until one is: the
/// check's first, untimed read finds it asleep.
pub fn bell(reader: &WaitQueue, driver_port: &WaitQueue) {
    if !TRACING.load(Ordering::Relaxed) || !is(Traced::Reader, reader) {
        return;
    }
    if stamp(Stamp::Bell, Stamp::Bell) {
        name(Traced::DriverPort, driver_port);
    }
}

/// A `port_wait` took a packet from the port whose queue is `port`: on the
/// traced driver's port, the bell or, for an `interrupt` packet, the
/// device's completion.
pub fn port_taken(port: &WaitQueue, interrupt: bool) {
    if !TRACING.load(Ordering::Relaxed) || !is(Traced::DriverPort, port) {
        return;
    }
    if interrupt {
        let _ = stamp(Stamp::DriverIrq, Stamp::DriverIrq);
    } else {
        let _ = stamp(Stamp::DriverBell, Stamp::DriverBell);
    }
}

/// A bound interrupt queued its packet on the port whose queue is `port`.
/// From the interrupt handler: atomics only.
pub(crate) fn interrupt_queued(port: &WaitQueue) {
    if !TRACING.load(Ordering::Relaxed) || !is(Traced::DriverPort, port) {
        return;
    }
    let _ = stamp(Stamp::Irq, Stamp::Bell);
}

/// A program queued a packet on the port whose queue is `port`: on the
/// traced ring's port, the driver posting its completion.
pub fn port_rung(port: &WaitQueue) {
    if !TRACING.load(Ordering::Relaxed) || !is(Traced::RingPort, port) {
        return;
    }
    let _ = stamp(Stamp::Posted, Stamp::Bell);
}

/// The ring serving the disk of `reader` copied an answer out.
pub fn answered(reader: &WaitQueue) {
    if !TRACING.load(Ordering::Relaxed) || !is(Traced::Reader, reader) {
        return;
    }
    let _ = stamp(Stamp::Answered, Stamp::Bell);
}

/// A reader of the disk of `reader` is running with its answer.
pub fn reader_running(reader: &WaitQueue) {
    if !TRACING.load(Ordering::Relaxed) || !is(Traced::Reader, reader) {
        return;
    }
    let _ = stamp(Stamp::Reader, Stamp::Reader);
}

/// The read returned to the check: the trip, if it reached every slot, or
/// else the slot it was still waiting for ([`STAMPS`] when not tracing).
pub fn done() -> Result<Trip, usize> {
    if !TRACING.load(Ordering::Relaxed) {
        return Err(STAMPS);
    }
    let waiting = NEXT.load(Ordering::Acquire);
    if !stamp(Stamp::Done, Stamp::Done) {
        NEXT.store(STAMPS, Ordering::Relaxed);
        return Err(waiting);
    }
    let mut trip = Trip {
        at: [0; STAMPS],
        cpu: [0; STAMPS],
        skipped: SKIPPED.load(Ordering::Relaxed),
    };
    for (to, from) in trip.at.iter_mut().zip(&AT) {
        *to = from.load(Ordering::Relaxed);
    }
    for (to, from) in trip.cpu.iter_mut().zip(&CPU) {
        *to = from.load(Ordering::Relaxed);
    }
    Ok(trip)
}

/// Stamp `at` if the trip is between `earliest` and it, filling any slot
/// passed over with the stamp before it. Whether it stamped.
fn stamp(at: Stamp, earliest: Stamp) -> bool {
    let slot = at as usize;
    let next = NEXT.load(Ordering::Acquire);
    if next < earliest as usize || next > slot || next == 0 {
        return false;
    }
    let (now, cpu) = now_here();
    if NEXT
        .compare_exchange(next, slot + 1, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return false;
    }
    let before = next - 1;
    let was_at = AT.get(before).map_or(now, |at| at.load(Ordering::Relaxed));
    let was_cpu = CPU.get(before).map_or(cpu, |on| on.load(Ordering::Relaxed));
    for passed in next..slot {
        store(passed, was_at, was_cpu);
        let _ = SKIPPED.fetch_or(1 << passed, Ordering::Relaxed);
    }
    store(slot, now, cpu);
    true
}

/// Write slot `slot`.
fn store(slot: usize, now: u64, cpu: u32) {
    if let (Some(at), Some(on)) = (AT.get(slot), CPU.get(slot)) {
        at.store(now, Ordering::Relaxed);
        on.store(cpu, Ordering::Relaxed);
    }
}

/// The counter and the processor reading it, together.
fn now_here() -> (u64, u32) {
    let saved = <arch::Irq as IrqControl>::disable();
    let now = arch::counter_now();
    let cpu = crate::smp::this_cpu().map_or(0, |cpu| cpu.logical);
    <arch::Irq as IrqControl>::restore(saved);
    (now, u32::try_from(cpu).unwrap_or(u32::MAX))
}

/// A queue's address, which names it while it lives.
fn address(queue: &WaitQueue) -> usize {
    core::ptr::from_ref(queue).addr()
}

/// Name `queue` as the traced `which`.
fn name(which: Traced, queue: &WaitQueue) {
    if let Some(at) = QUEUE_AT.get(which as usize) {
        at.store(address(queue), Ordering::Relaxed);
    }
}

/// Whether `queue` is the traced `which`.
fn is(which: Traced, queue: &WaitQueue) -> bool {
    QUEUE_AT
        .get(which as usize)
        .is_some_and(|at| at.load(Ordering::Relaxed) == address(queue))
}

/// Which traced queue lives at `address`, if any.
fn traced(address: usize) -> Option<usize> {
    QUEUE_AT
        .iter()
        .position(|queue| queue.load(Ordering::Relaxed) == address)
}
