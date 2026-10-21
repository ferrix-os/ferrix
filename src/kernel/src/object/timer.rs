//! Timers: a deadline the kernel keeps, signalled to whoever waits on it.
//!
//! `docs/MICROKERNEL.md` §3, row K10, and its design, `docs/microkernel/K10.md`.
//! A timer holds at most one armed **one-shot** deadline on a counter clock,
//! with a **slack**: it may fire anywhere in `[deadline, deadline + slack]`,
//! never before. When it fires it asserts [`Signals::READABLE`] until it is
//! set or cancelled again, and everything waiting on that level learns it the
//! way it learns any object's: a port through an `object_wait_async`
//! registration ([`Observer`], one packet, its room promised as it was made),
//! a thread through `object_wait_one`. Periodic timers, the real-time clock
//! and overrun counts are the Linux personality's, built on this (§2.6).
//!
//! # One sleeper set, fired from the timer interrupt
//!
//! An armed timer is filed on its processor's sleeper set, beside the tasks
//! sleeping there ([`Due::Fire`]), keyed by the latest instant of its window,
//! in a slot it made when it was created: one set, one hardware one-shot,
//! armed by the run queue for its first key. The set's pass takes a due
//! arm into the run queue's fired buffer, and only the timer interrupt
//! fires what is there ([`crate::sched`]'s `fire_timers`), with the run
//! queue's lock let go: a decision holds that lock across its switch, and a
//! fire wakes waiters, which takes run queue locks.
//!
//! # Generations, and whoever holds the slot files it
//!
//! Every set and every cancel moves the timer to a new generation, and an
//! arm carries the generation it was made in. An arm that leaves the
//! sleeper set has left with the timer's slot; a set or cancel that finds
//! it gone (taken into the fired buffer, or being fired) cannot have the
//! slot back, and records its own arm without filing it. The fire of the old
//! arm then finds a newer generation: it signals nothing, puts the slot
//! home, and files the newer arm if one is waiting. So a timer fires at most
//! once a set, nothing a set or cancel superseded ever signals, and an arm is
//! filed exactly once, by whichever of the two holds the slot
//! (`L.object.219`).
//!
//! # Lock order
//!
//! A timer's lock, then a run queue's lock: a set files and unfiles under
//! both. The fire path lets the run queue's lock go before it takes a
//! timer's. A port's lock is taken only once the timer's is let go: a fire
//! delivers its registrations after letting go, and a registration refused
//! for room is dropped after. Nothing takes a timer's lock under a run
//! queue's or a port's.
//!
//! # What the interrupt may free
//!
//! A fire may hold the last reference to a timer whose handles all closed
//! while its arm was in flight. Its drop then runs in the interrupt and frees
//! heap only: the closing ([`Timer`]'s drop) has already emptied its
//! registrations and given back the ports it kept, and the charges went with
//! the [`Timer`]. A port a fire delivered to is never let go in the
//! interrupt: it is kept on the timer ([`State::spent`]) and let go by the
//! timer's next set, cancel, registration or close, all in task context
//! (`docs/certification/MEMORY-AND-TIMING.md` §2.2t).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use ferrix_native_abi::signals::Signals;
use ferrix_sync::IrqSpinLock;

use crate::arch;
use crate::fallible::{self, AllocError};
use crate::object::port::{self, Observer, Observers, Port, PortError};
use crate::object::quota::{Charge, Resource};
use crate::sched::{self, SleepSlot, WaitQueue};

/// The most `object_wait_async` registrations one timer holds at once. The
/// room for them is made with the timer, so registering allocates nothing
/// under its interrupt-safe lock.
pub(crate) const TIMER_OBSERVERS: usize = 4;

/// The timeline id bit a timer's arm carries, and a task's never does: task
/// ids are counted up from one by one counter (`sched::next_id`), which
/// holds them below this.
pub(crate) const TIMER_ID_BIT: u64 = 1 << 63;

/// The next timer's serial.
static NEXT_SERIAL: AtomicU64 = AtomicU64::new(1);

/// Which counter a timer's deadlines are on. Both are the free-running
/// counter while nothing suspends (`docs/microkernel/K10.md` §2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Clock {
    /// `TIMER_CLOCK_MONOTONIC`.
    Monotonic,
    /// `TIMER_CLOCK_BOOT`.
    Boot,
}

/// One arm of a timer, as its processor's sleeper set holds it.
#[derive(Debug)]
pub(crate) struct Armed {
    /// The timer.
    pub(crate) timer: Arc<TimerCore>,
    /// The generation it was armed in.
    pub(crate) generation: u64,
    /// The earliest instant of its window: its deadline.
    pub(crate) earliest: u64,
}

/// What a timer's lock covers.
#[derive(Debug)]
struct State {
    /// Moved on by every set and cancel.
    generation: u64,
    /// The current arm's window, `(earliest, latest)`, or `None`.
    armed: Option<(u64, u64)>,
    /// Its slot, while no sleeper set, fired buffer or fire holds it.
    slot: Option<SleepSlot>,
    /// `READABLE`: its deadline has passed since it was last set.
    signalled: bool,
    /// Its `object_wait_async` registrations, with room made for
    /// [`TIMER_OBSERVERS`].
    observers: Observers,
    /// Ports fires delivered to, kept so that the interrupt never lets go
    /// of one: room for [`TIMER_OBSERVERS`], made with the timer.
    spent: Vec<Arc<Port>>,
    /// A fire is delivering its registrations, with this lock let go.
    delivering: bool,
    /// When it last fired, on the counter, for the checks.
    fired_at: u64,
    /// The pass of the timer interrupt that fired it last, or zero for a
    /// set that fired it, for the checks.
    fired_in: u64,
    /// Times it has fired, for the checks.
    fires: u64,
}

/// A timer, as its processor's sleeper set and its handles share it.
#[derive(Debug)]
pub(crate) struct TimerCore {
    /// The id its arms are filed under: a serial with [`TIMER_ID_BIT`].
    id: u64,
    /// Its clock.
    clock: Clock,
    /// The key its arm is filed under, or zero while it is not filed:
    /// written only under the lock of the run queue it is filed on, so a set
    /// holding that lock reads the key it was filed under.
    filed_key: AtomicU64,
    /// The processor its arm was last filed on. Written under the timer's
    /// lock and that processor's run queue's.
    filed_cpu: AtomicUsize,
    /// Everything else.
    state: IrqSpinLock<State, arch::Irq>,
    /// Woken when it fires, for `object_wait_one`.
    waiters: WaitQueue,
}

/// A timer, as a handle names it: the timer and what its job is charged.
#[derive(Debug)]
pub(crate) struct Timer {
    /// The timer.
    core: Arc<TimerCore>,
    /// One kernel object, charged to the job that made it (F-35).
    #[expect(
        dead_code,
        reason = "AUDIT: held for its drop, which uncharges the job"
    )]
    object: Charge,
    /// Its heap, charged to the same job as kernel memory (F-37's rule).
    #[expect(
        dead_code,
        reason = "AUDIT: held for its drop, which uncharges the job"
    )]
    heap: ferrix_kmem::Charge,
}

/// Why a timer call was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimerError {
    /// No memory, or its job's object or memory limit.
    NoMemory,
}

impl From<AllocError> for TimerError {
    fn from(_: AllocError) -> TimerError {
        TimerError::NoMemory
    }
}

/// The kernel heap one timer holds: what its job is charged as kernel
/// memory (`L.object.213`).
pub(crate) fn heap_bytes() -> usize {
    ferrix_kmem::arc_footprint::<TimerCore>()
        .saturating_add(ferrix_kmem::arc_footprint::<Timer>())
        .saturating_add(ferrix_kmem::boxed_footprint::<ferrix_sched::Node<sched::Due>>())
        .saturating_add(ferrix_kmem::buffer_footprint::<Observer>(TIMER_OBSERVERS))
        .saturating_add(ferrix_kmem::buffer_footprint::<(Observer, Signals)>(
            TIMER_OBSERVERS,
        ))
        .saturating_add(ferrix_kmem::buffer_footprint::<Arc<Port>>(TIMER_OBSERVERS))
}

impl Timer {
    /// A disarmed timer on `clock`, charged to the running task's job: one
    /// object, and its heap as kernel memory, both before anything is made
    /// (`L.object.213`).
    ///
    /// # Errors
    ///
    /// [`TimerError::NoMemory`] at either limit or with no memory, with
    /// nothing made and nothing charged.
    pub(crate) fn new(clock: Clock) -> Result<Arc<Timer>, TimerError> {
        let object = Charge::running(Resource::Objects, 1).map_err(|_| TimerError::NoMemory)?;
        let heap = ferrix_kmem::Charge::bytes(heap_bytes()).map_err(|_| TimerError::NoMemory)?;
        let slot = SleepSlot::new()?;
        let observers = Observers::with_room(TIMER_OBSERVERS)?;
        let mut spent = Vec::new();
        fallible::try_reserve(&mut spent, TIMER_OBSERVERS)?;
        let serial = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed);
        let core = fallible::try_arc(TimerCore {
            id: TIMER_ID_BIT | serial,
            clock,
            filed_key: AtomicU64::new(0),
            filed_cpu: AtomicUsize::new(0),
            state: IrqSpinLock::new(State {
                generation: 0,
                armed: None,
                slot: Some(slot),
                signalled: false,
                observers,
                spent,
                delivering: false,
                fired_at: 0,
                fired_in: 0,
                fires: 0,
            }),
            waiters: WaitQueue::new(),
        })?;
        Ok(fallible::try_arc(Timer { core, object, heap })?)
    }

    /// The timer itself.
    pub(crate) fn core(&self) -> &Arc<TimerCore> {
        &self.core
    }
}

impl Drop for Timer {
    /// The last handle is gone: cancel, so nothing signals from here on, and
    /// let go of every registration and every port kept, here in task
    /// context (`L.object.220`).
    fn drop(&mut self) {
        let (listed, spent) = self.core.close();
        drop(listed);
        drop(spent);
    }
}

impl TimerCore {
    /// Its clock's reading now.
    fn now(&self) -> u64 {
        match self.clock {
            Clock::Monotonic | Clock::Boot => crate::timer::now_nanos(),
        }
    }

    /// Set it: a new generation, `READABLE` cleared, the previous arm
    /// superseded wherever it is, and a one-shot at `deadline` (on its
    /// clock, or from now with `relative`) with `slack`. A deadline already
    /// past fires here (`L.object.214`).
    pub(crate) fn set(self: &Arc<Self>, deadline: u64, slack: u64, relative: bool) {
        let mut state = self.state.lock();
        // Read once, under the lock: a relative deadline is from here.
        let now = self.now();
        state.signalled = false;
        self.supersede(&mut state);
        let earliest = if relative {
            now.saturating_add(deadline)
        } else {
            deadline
        };
        let past = earliest <= now;
        if past {
            state.armed = None;
            self.signal(&mut state, now, 0);
            state.delivering = true;
        } else {
            state.armed = Some((earliest, earliest.saturating_add(slack)));
            self.file_if_home(&mut state, now);
        }
        let spent = self.spend(state);
        drop(spent);
        if past {
            // With preemption off, as a fire in the interrupt is: a close
            // waits out `delivering` by spinning, and may do so on this
            // processor with preemption off itself (a drop under a spin
            // lock, a dying process's table), which a delivery preempted
            // half-way would never let finish. Bounded by TIMER_OBSERVERS
            // packets and the wakes (K10 code review, ledger 2773, (1)).
            sched::preempt_disable();
            self.deliver();
            sched::preempt_enable();
        }
    }

    /// Cancel it: a new generation, `READABLE` cleared, and nothing the
    /// cancelled arm filed or had in flight signals (`L.object.215`).
    pub(crate) fn cancel(self: &Arc<Self>) {
        let mut state = self.state.lock();
        self.disarm(&mut state);
        let spent = self.spend(state);
        drop(spent);
    }

    /// No arm, `READABLE` cleared, and a new generation with its filed arm
    /// taken back: what a cancel does.
    fn disarm(&self, state: &mut State) {
        state.armed = None;
        state.signalled = false;
        self.supersede(state);
    }

    /// Hand back the ports kept, with the lock let go, for the caller to
    /// drop in task context.
    fn spend(&self, mut state: ferrix_sync::IrqSpinLockGuard<'_, State, arch::Irq>) -> Spent {
        let mut kept = Spent::default();
        for (place, port) in kept.0.iter_mut().zip(state.spent.drain(..)) {
            *place = Some(port);
        }
        drop(state);
        kept
    }

    /// Move it to a new generation, so that no arm made before signals, and
    /// take its filed arm back if there is one.
    fn supersede(&self, state: &mut State) {
        state.generation = state.generation.wrapping_add(1);
        self.take_back(state);
    }

    /// If its arm is filed, take it out of its sleeper set and put the slot
    /// home: by its key, under that run queue's lock, taken under this
    /// timer's. An arm already taken into a fired buffer or being fired is
    /// left to its fire, which finds the generation moved on.
    fn take_back(&self, state: &mut State) {
        if state.slot.is_some() {
            return;
        }
        let cpu = self.filed_cpu.load(Ordering::Relaxed);
        if let Some((armed, slot)) = sched::unfile_timer(cpu, self) {
            state.slot = Some(slot);
            // Not its last reference: the caller holds the timer.
            drop(armed);
        }
    }

    /// File its current arm on this processor if the slot is home.
    fn file_if_home(self: &Arc<Self>, state: &mut State, now: u64) {
        let Some((earliest, latest)) = state.armed else {
            return;
        };
        let Some(slot) = state.slot.take() else {
            return;
        };
        let armed = Armed {
            timer: Arc::clone(self),
            generation: state.generation,
            earliest,
        };
        match sched::file_timer_here(latest, self.id, armed, slot, now) {
            Ok(cpu) => self.filed_cpu.store(cpu, Ordering::Relaxed),
            // No run queue here: before the scheduler, where nothing sets a
            // timer. The arm stays recorded and unfiled, the slot home, and
            // a debug build has already stopped (`sched::file_timer_here`).
            Err((_, slot)) => state.slot = Some(slot),
        }
    }

    /// Fired: `READABLE`, its registrations moved to firing, the time kept.
    fn signal(&self, state: &mut State, now: u64, pass: u64) {
        state.signalled = true;
        state.fired_at = now;
        state.fired_in = pass;
        state.fires = state.fires.wrapping_add(1);
        let _ = port::trigger(&mut state.observers, Signals::READABLE);
    }

    /// Deliver the registrations a fire moved to firing, one at a time with
    /// the lock let go, keeping each port on the timer; then wake its
    /// waiters.
    fn deliver(&self) {
        let mut kept: Option<Arc<Port>> = None;
        loop {
            let next = {
                let mut state = self.state.lock();
                if let Some(port) = kept.take() {
                    // NOALLOC: room for every registration was made with the
                    // timer, and each is delivered once.
                    if let Err(port) = fallible::push_within(&mut state.spent, port) {
                        kept = Some(port);
                    }
                }
                let next = state.observers.next_fired();
                if next.is_none() {
                    state.delivering = false;
                }
                next
            };
            let Some((observer, asserted)) = next else {
                break;
            };
            kept = observer.fire_keeping(asserted);
        }
        self.waiters.wake_all();
        // Room was made for every port a fire can deliver to -- at most
        // `TIMER_OBSERVERS` between two sets, each of which empties `spent`
        // -- so nothing is left here.
        debug_assert!(kept.is_none(), "a timer had no room to keep a port");
        if let Some(port) = kept {
            #[expect(
                clippy::mem_forget,
                reason = "AUDIT: a port reference a fire found no room to keep, never let go \
                          rather than possibly dropped last in an interrupt; unreachable, see \
                          `TimerCore::deliver`"
            )]
            core::mem::forget(port);
        }
    }

    /// Register `observer`: at once if `READABLE` already is, else listed in
    /// the room made with the timer, allocating nothing.
    pub(crate) fn observe(&self, observer: Observer) -> Result<(), PortError> {
        let mut state = self.state.lock();
        if state.signalled && observer.wants(Signals::READABLE) {
            let spent = self.spend(state);
            observer.fire(Signals::READABLE);
            drop(spent);
            return Ok(());
        }
        let refused = port::register_within(&mut state.observers, observer).err();
        let spent = self.spend(state);
        drop(spent);
        match refused {
            None => Ok(()),
            Some((why, observer)) => {
                drop(observer);
                Err(why)
            }
        }
    }

    /// The last handle closed: cancel, wait out a delivery in flight (a few
    /// packets, bounded by [`TIMER_OBSERVERS`]), and hand back every
    /// registration and kept port to drop in task context.
    ///
    /// The spin cannot wait on itself: every delivery runs with preemption
    /// off, in the timer interrupt or in a set that fired in the call, so
    /// one that set `delivering` on this processor has finished before this
    /// runs, and one on another processor finishes without this one's help.
    fn close(self: &Arc<Self>) -> (Vec<Observer>, Spent) {
        loop {
            let mut state = self.state.lock();
            if state.delivering {
                drop(state);
                core::hint::spin_loop();
                continue;
            }
            state.armed = None;
            self.supersede(&mut state);
            state.signalled = false;
            let listed = state.observers.take_listed();
            let spent = self.spend(state);
            return (listed, spent);
        }
    }

    /// `READABLE` while its deadline has passed since it was last set.
    pub(crate) fn signals(&self) -> Signals {
        if self.state.lock().signalled {
            Signals::READABLE
        } else {
            Signals::NONE
        }
    }

    /// The queue woken when it fires.
    pub(crate) fn waiters(&self) -> &WaitQueue {
        &self.waiters
    }

    /// Its timeline id.
    pub(crate) fn timeline_id(&self) -> u64 {
        self.id
    }

    /// The key its arm is filed under, or zero: under the lock of the run
    /// queue it is filed on.
    pub(crate) fn filed_key(&self) -> u64 {
        self.filed_key.load(Ordering::Relaxed)
    }

    /// Its arm left its sleeper set: under that run queue's lock.
    pub(crate) fn unfiled(&self) {
        self.filed_key.store(0, Ordering::Relaxed);
    }

    /// Its arm is filed under `key`: under that run queue's lock.
    pub(crate) fn refiled(&self, key: u64) {
        self.filed_key.store(key, Ordering::Relaxed);
    }

    /// For the checks: when it last fired, the interrupt pass that fired it
    /// (zero for a set), how many times it has fired, and its current
    /// window.
    pub(crate) fn report(&self) -> Report {
        let state = self.state.lock();
        Report {
            fired_at: state.fired_at,
            fired_in: state.fired_in,
            fires: state.fires,
            armed: state.armed,
            home: state.slot.is_some(),
        }
    }
}

impl Drop for TimerCore {
    fn drop(&mut self) {
        // What a fire in the interrupt may drop holds no object: the close
        // emptied both (`docs/microkernel/K10.md` §9, condition 1).
        let state = self.state.get_mut();
        debug_assert!(
            state.observers.is_empty(),
            "a timer dropped with registrations"
        );
        debug_assert!(state.spent.is_empty(), "a timer dropped keeping a port");
    }
}

/// The timer checks' hook, compiled only into a kernel with its self-checks
/// and seen only by `object` (the K10 delta review, ledger 2919, (1)):
/// the product never takes an arm off its sleeper set without its lock.
#[cfg(ferrix_checks = "on")]
pub(in crate::object) mod sabotage {
    use super::{Armed, Ordering, SleepSlot, TimerCore, sched};

    /// Take `timer`'s arm off its sleeper set as a pass would, without its
    /// lock, leaving the slot out: an arm in flight.
    pub(in crate::object) fn take_arm(timer: &TimerCore) -> Option<(Armed, SleepSlot)> {
        sched::unfile_timer(timer.filed_cpu.load(Ordering::Relaxed), timer)
    }
}

/// Ports a timer kept, handed back to be dropped with no lock held.
#[derive(Default)]
struct Spent([Option<Arc<Port>>; TIMER_OBSERVERS]);

/// What [`TimerCore::report`] tells the checks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Report {
    /// When it last fired.
    pub(crate) fired_at: u64,
    /// The interrupt pass that fired it last, or zero for a set.
    pub(crate) fired_in: u64,
    /// Times it has fired.
    pub(crate) fires: u64,
    /// Its current window.
    pub(crate) armed: Option<(u64, u64)>,
    /// Whether its slot is home.
    pub(crate) home: bool,
}

/// Fire `armed`, which a run queue's pass took off its sleeper set with
/// `slot`: from the timer interrupt, with no run queue's lock held, during
/// pass `pass` (`L.object.216`, `L.object.219`).
///
/// The slot goes home first. An arm a set or cancel superseded signals
/// nothing, and files the newer arm on this processor if one is waiting. A
/// current one asserts `READABLE`, delivers its registrations and wakes its
/// waiters. Allocates nothing.
pub(crate) fn fire(armed: Armed, slot: SleepSlot, pass: u64) {
    let Armed {
        timer, generation, ..
    } = armed;
    {
        let mut state = timer.state.lock();
        state.slot = Some(slot);
        if generation != state.generation {
            let now = timer.now();
            timer.file_if_home(&mut state, now);
            return;
        }
        let now = timer.now();
        state.armed = None;
        timer.signal(&mut state, now, pass);
        state.delivering = true;
    }
    timer.deliver();
    // The fire's reference: not the last while a handle is open, since the
    // handle's close takes the arm back or makes it stale first; the last
    // only for a timer closed while this arm was in flight, whose drop then
    // frees heap alone (the module documentation).
    drop(timer);
}
