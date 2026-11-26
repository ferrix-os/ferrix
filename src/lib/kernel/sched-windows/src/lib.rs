//! `schedule=windows`: a static frame of time windows per processor, ARINC 653's shape, with
//! fixed priorities inside each window and no fair class (`docs/microkernel/K9.md` §4; the
//! customer's C3 and C6; ledger 7011 S.1-S.3, 7088 W.1-W.4, 7460 WN.1-WN.14).
//!
//! A pure crate: a function of the numbers it is handed, with no clock of its own, no unsafe
//! and no allocation after construction. [`Windows`] answers every method of the run queue
//! the kernel calls, so one type holds every door, and it holds **no `RunQueue`**: its tasks
//! wait in [`ferrix_sched::Ordered`] sets, one per partition, through [`Fixed`].
//!
//! # Inside a window
//!
//! [`Fixed`] is the one fixed-priority round-robin body (ledger 7395 TR.10): [`LEVELS`] levels,
//! a lower first, equals in turn by a quantum of [`QUANTUM_NS`] of running time, whose
//! remainder a task keeps across a window's end and a sleep. A higher level ready preempts at
//! once; a lower one waits while a higher is runnable. Weights, lag and virtual time are not
//! read: the level is the payload's own ([`Placed::level`]), never a weight (WN.9).
//!
//! # The frame
//!
//! [`frame::set_window`] collects windows into each processor's [`frame::Row`] and
//! [`frame::seal`] refuses a frame that overlaps, overruns or leaves the kernel no window;
//! each processor's class holds its [`frame::Part`]. In a window only that partition's set is
//! read, and with nothing there the processor is idle: the slack is never another
//! partition's. Before a part is installed the class reads every set as one, by level: the
//! boot's fixed-priority machine.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod frame;

use ferrix_sched::{Config, EntityState, EntityView, Left, Ordered, Refused, SchedError, Slot};
use frame::{PARTITIONS, Part};

/// How many priority levels there are: 0 first.
pub const LEVELS: u8 = 40;

/// The quantum equals take in turn, in running time: two of Linux's 1 ms round-robin
/// intervals at HZ=1000, four turns in a 9 ms window.
pub const QUANTUM_NS: u64 = 2_000_000;

/// What the class reads of a task: its level and its partition.
pub trait Placed {
    /// Its priority level, below [`LEVELS`]; higher values are clamped to the last.
    fn level(&self) -> u8;
    /// Its partition, below [`PARTITIONS`].
    fn partition(&self) -> u8;
}

impl<P: Placed + ?Sized> Placed for alloc::sync::Arc<P> {
    fn level(&self) -> u8 {
        (**self).level()
    }
    fn partition(&self) -> u8 {
        (**self).partition()
    }
}

/// Bits of a key below the level: the arrival sequence.
const SEQUENCE_BITS: u32 = 56;
const SEQUENCE_MASK: u64 = (1 << SEQUENCE_BITS) - 1;
/// Where head and tail sequences start: head counts down from here, tail up. At 10^9
/// enqueues a second, 2^55 lasts over a year before the two would meet.
const MIDDLE: u64 = 1 << (SEQUENCE_BITS - 1);

/// A key: the level in the high bits, the sequence below, compared plainly.
const fn key_of(level: u8, sequence: u64) -> u64 {
    let level = if level >= LEVELS { LEVELS - 1 } else { level };
    ((level as u64) << SEQUENCE_BITS) | (sequence & SEQUENCE_MASK)
}

/// The level a key holds.
#[must_use]
pub const fn level_of_key(key: u64) -> u8 {
    (key >> SEQUENCE_BITS) as u8
}

/// The servers' levels, 0 to 9: a server of nice -20 to 19 takes `(nice + 20) / 4`.
pub const SERVER_LEVELS: core::ops::RangeInclusive<u8> = 0..=9;
/// The programs' levels, 10 to 39: a program of nice -20 to 19 takes `10 + (nice + 20) * 3 / 4`,
/// so every program's level is beneath every server's (L.sched.169).
pub const PROGRAM_LEVELS: core::ops::RangeInclusive<u8> = 10..=39;

/// The level a task of `weight`'s nice takes: a server's in [`SERVER_LEVELS`], a program's in
/// [`PROGRAM_LEVELS`] (WN.10, L.sched.169). The nice is the one whose weight is nearest at or
/// above `weight`, since the table is falling; past its end, 19's.
#[must_use]
pub fn level_of(weight: u32, program: bool) -> u8 {
    let nice = (-20..=19_i32)
        .find(|&nice| ferrix_sched::weight_of_nice(nice).is_some_and(|at| at <= weight))
        .unwrap_or(19);
    let step = u8::try_from(nice + 20).unwrap_or(39);
    if program { 10 + step * 3 / 4 } else { step / 4 }
}

/// The fixed-priority round-robin class: one set, by level, then arrival.
#[derive(Debug)]
pub struct Fixed<T> {
    set: Ordered<T>,
    head: u64,
    tail: u64,
}

impl<T> Default for Fixed<T> {
    fn default() -> Fixed<T> {
        Fixed::new()
    }
}

impl<T> Fixed<T> {
    /// An empty class.
    #[must_use]
    pub const fn new() -> Fixed<T> {
        Fixed {
            set: Ordered::new(),
            head: MIDDLE,
            tail: MIDDLE,
        }
    }

    /// Queue `payload` at `level`: behind its equals, or with `at_head` before them (a task
    /// put back with quantum left).
    pub fn push(
        &mut self,
        id: u64,
        level: u8,
        payload: T,
        state: EntityState,
        slot: Slot<T>,
        at_head: bool,
    ) {
        let sequence = if at_head {
            self.head = self.head.wrapping_sub(1);
            self.head
        } else {
            self.tail = self.tail.wrapping_add(1);
            self.tail
        };
        self.set
            .insert(id, key_of(level, sequence), payload, state, slot); // NOALLOC: into the task's own run slot, the `Slot<T>` taken out with it, in ferrix-sched's intrusive tree.
    }

    /// The first: its id, its level and its payload.
    #[must_use]
    pub fn first(&self) -> Option<(u64, u8, &T)> {
        self.set
            .first()
            .map(|(id, key, payload)| (id, level_of_key(key), payload))
    }

    /// The first's key, for comparing classes before the seal.
    #[must_use]
    pub fn first_key(&self) -> Option<u64> {
        self.set.first().map(|(_, key, _)| key)
    }

    /// Take the first out.
    pub fn pop(&mut self) -> Option<Left<T>> {
        self.set.pop_first()
    }

    /// Take `id` out.
    pub fn remove(&mut self, id: u64) -> Option<Left<T>> {
        self.set.remove(id)
    }

    /// Whether `id` waits here.
    #[must_use]
    pub fn holds(&self, id: u64) -> bool {
        self.set.key_of(id).is_some()
    }

    /// How many wait.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.set.len()
    }

    /// Whether none waits.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    /// The id of the last in order whose payload satisfies `wanted`.
    #[must_use]
    pub fn last_where(&self, wanted: impl Fn(&T) -> bool) -> Option<u64> {
        self.set.last_where(wanted)
    }

    /// The underlying set, read only.
    #[must_use]
    pub const fn set(&self) -> &Ordered<T> {
        &self.set
    }
}

/// The task running on a class.
#[derive(Debug)]
struct Running<T> {
    id: u64,
    payload: T,
    state: EntityState,
    slot: Slot<T>,
    partition: u8,
    level: u8,
    /// What is left of its quantum.
    left_ns: u64,
    /// When the window it was picked in ends, absolute; `u64::MAX` with no frame.
    until: u64,
}

/// What the window at the class's latest time is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Now {
    /// No frame: every set, as one.
    Merged,
    /// This partition's window.
    In(u8),
    /// Between windows: nobody's.
    Gap,
}

/// One processor's class.
#[derive(Debug)]
pub struct Windows<T> {
    sets: [Fixed<T>; PARTITIONS],
    running: Option<Running<T>>,
    part: Option<Part>,
    now: u64,
    config: Config,
    /// The window's end at which a task of the ended window still ran, until the next pick.
    ended_at: Option<u64>,
    worst_overrun_ns: u64,
    windows_ended: u64,
    /// The last window's end a task ran past, and the pick that ended it: the class's own stamp
    /// at that interrupt, on the caller's timescale.
    last_overrun: Option<(u64, u64)>,
}

impl<T: Placed> Windows<T> {
    /// An empty class with no frame.
    ///
    /// # Errors
    ///
    /// [`SchedError::ZeroSlice`] for a configured slice of zero, as the run queue's.
    pub fn new(config: Config) -> Result<Windows<T>, SchedError> {
        if config.slice_ns == 0 {
            return Err(SchedError::ZeroSlice);
        }
        Ok(Windows {
            sets: core::array::from_fn(|_| Fixed::new()),
            running: None,
            part: None,
            now: 0,
            config,
            ended_at: None,
            worst_overrun_ns: 0,
            windows_ended: 0,
            last_overrun: None,
        })
    }

    /// Install this processor's part of a sealed frame. `false`, and nothing changed, when one is
    /// already installed.
    pub fn install(&mut self, part: Part) -> bool {
        if self.part.is_some() {
            return false;
        }
        self.part = Some(part);
        true
    }

    /// Take the part off, as the boot check does with its own before the boot's seal; the
    /// kernel refuses this once the boot's frame is sealed. Whether there was one.
    pub fn uninstall(&mut self) -> bool {
        self.part.take().is_some()
    }

    /// The part installed, if any.
    #[must_use]
    pub const fn part(&self) -> Option<Part> {
        self.part
    }

    /// Whether the processor this class serves may come back into service (L.sched.170):
    /// before the seal any may; after it only one whose sealed row was installed here, so a
    /// processor with no row of the sealed frame is refused. The kernel's
    /// `sched::windows::may_return` is this body.
    #[must_use]
    pub const fn may_return(&self, sealed: bool) -> bool {
        !sealed || self.part.is_some()
    }

    /// Tell the class the time.
    pub fn note_time(&mut self, now: u64) {
        self.now = self.now.max(now);
        if let Some(running) = self.running.as_ref()
            && self.ended_at.is_none()
            && self.now >= running.until
        {
            self.ended_at = Some(running.until);
            self.windows_ended += 1;
        }
    }

    /// The window at the latest time.
    #[must_use]
    pub fn window(&self) -> Now {
        self.window_at(self.now)
    }

    /// The window at `now`, or at the class's latest time where that is later; the class is not
    /// changed. A reader outside a pick and a tick passes the instant it decides at (ledger 7639
    /// WT.1): the latest time is the last pick's or tick's, which on a halted processor can lie
    /// in an earlier frame's window.
    #[must_use]
    pub fn window_at(&self, now: u64) -> Now {
        match self.part {
            None => Now::Merged,
            Some(part) => match part.at(self.now.max(now)).0 {
                Some(partition) => Now::In(partition),
                None => Now::Gap,
            },
        }
    }

    /// When the window at the latest time ends, absolute; `None` with no frame.
    #[must_use]
    pub fn window_until(&self) -> Option<u64> {
        self.part.map(|part| part.at(self.now).1)
    }

    fn may_run(&self, partition: u8) -> bool {
        self.may_run_at(partition, self.now)
    }

    fn may_run_at(&self, partition: u8, now: u64) -> bool {
        match self.window_at(now) {
            Now::Merged => true,
            Now::In(current) => current == partition,
            Now::Gap => false,
        }
    }

    /// Whether a task of `partition` made runnable here may be run in the window at `now`: what
    /// decides a wake's kick (L.sched.164), a pull's and a placement's (L.sched.166). The
    /// instant is the caller's, never the class's latest time alone (WT.1).
    #[must_use]
    pub fn admits_at(&self, partition: u8, now: u64) -> bool {
        self.may_run_at(partition, now)
    }

    /// The set the window reads, its index, for the first waiting; `None` in a gap or empty.
    fn best_waiting(&self) -> Option<usize> {
        self.best_waiting_at(self.now)
    }

    fn best_waiting_at(&self, now: u64) -> Option<usize> {
        match self.window_at(now) {
            Now::Merged => self
                .sets
                .iter()
                .enumerate()
                .filter_map(|(at, set)| set.first_key().map(|key| (key, at)))
                .min()
                .map(|(_, at)| at),
            Now::In(partition) => {
                let at = usize::from(partition);
                self.sets
                    .get(at)
                    .is_some_and(|set| !set.is_empty())
                    .then_some(at)
            }
            Now::Gap => None,
        }
    }

    /// Whether the window has a waiting task to run (WN.4): what deciding halt, the tick and
    /// the idle balance reads; [`Windows::is_empty`] counts every partition.
    #[must_use]
    pub fn runnable_now(&self) -> bool {
        self.best_waiting().is_some()
    }

    /// [`Windows::runnable_now`] at `now`, for a reader outside a pick (WT.1).
    #[must_use]
    pub fn runnable_at(&self, now: u64) -> bool {
        self.best_waiting_at(now).is_some()
    }

    fn running_may_stay(&self) -> bool {
        self.running
            .as_ref()
            .is_some_and(|running| self.may_run(running.partition))
    }

    /// As the run queue's: queue a task behind its equals.
    ///
    /// # Errors
    ///
    /// [`SchedError::Uncovered`] naming the partition, for one not below [`PARTITIONS`]: never
    /// another partition's set.
    pub fn enqueue(
        &mut self,
        id: u64,
        payload: T,
        state: EntityState,
        slot: Slot<T>,
    ) -> Result<(), Refused<T>> {
        let partition = payload.partition();
        let level = payload.level();
        let Some(set) = self.sets.get_mut(usize::from(partition)) else {
            return Err(Refused {
                payload,
                slot,
                reason: SchedError::Uncovered(usize::from(partition)),
            });
        };
        // NOALLOC: into the task's own run slot, the `Slot<T>` taken out with it, in ferrix-sched's intrusive tree.
        set.push(id, level, payload, state, slot, false);
        Ok(())
    }

    fn put_back(&mut self) {
        let Some(running) = self.running.take() else {
            return;
        };
        let Running {
            id,
            payload,
            mut state,
            slot,
            partition,
            level,
            left_ns,
            ..
        } = running;
        let at_head = left_ns != 0;
        state.vlag = i64::try_from(left_ns).unwrap_or(0);
        if let Some(set) = self.sets.get_mut(usize::from(partition)) {
            // NOALLOC: into the task's own run slot, the `Slot<T>` taken out with it, in ferrix-sched's intrusive tree.
            set.push(id, level, payload, state, slot, at_head);
        }
    }

    /// As the run queue's: put back what runs (before its equals with quantum left, behind
    /// them without), then the window's first; with none, idle.
    pub fn pick_next(&mut self) -> Option<&T> {
        self.put_back();
        if let Some(ended) = self.ended_at.take() {
            self.worst_overrun_ns = self.worst_overrun_ns.max(self.now.saturating_sub(ended));
            self.last_overrun = Some((ended, self.now));
        }
        let at = self.best_waiting()?;
        let (id, payload, state, slot) = self.sets.get_mut(at)?.pop()?;
        let partition = payload.partition();
        let level = payload.level();
        let kept = u64::try_from(state.vlag).unwrap_or(0);
        let left_ns = if kept == 0 || kept > QUANTUM_NS {
            QUANTUM_NS
        } else {
            kept
        };
        let until = self.window_until().unwrap_or(u64::MAX);
        self.running = Some(Running {
            id,
            payload,
            state,
            slot,
            partition,
            level,
            left_ns,
            until,
        });
        self.current()
    }

    /// As the run queue's, always the long way: queue, take the running one out, pick. The
    /// kernel's `direct::begin` declines across partitions first.
    ///
    /// # Errors
    ///
    /// As [`Windows::enqueue`].
    pub fn hand_over(
        &mut self,
        id: u64,
        payload: T,
        state: EntityState,
        slot: Slot<T>,
        slice_after: u64,
    ) -> Result<Option<Left<T>>, Refused<T>> {
        self.enqueue(id, payload, state, slot)?;
        let _ = self.set_slice_ns(slice_after);
        let left = self.remove_curr();
        let _ = self.pick_next();
        Ok(left)
    }

    /// As the run queue's: charge the running quantum. Whether a decision is due: the quantum
    /// is spent or the running task's window is over.
    pub fn update_curr(&mut self, delta_ns: u64) -> bool {
        let stays = self.running_may_stay();
        let Some(running) = self.running.as_mut() else {
            return false;
        };
        running.state.sum_exec = running.state.sum_exec.saturating_add(delta_ns);
        running.left_ns = running.left_ns.saturating_sub(delta_ns);
        running.left_ns == 0 || !stays
    }

    /// As the run queue's: give up the quantum, behind the equals at the next pick.
    pub fn yield_curr(&mut self) {
        if let Some(running) = self.running.as_mut() {
            running.left_ns = 0;
        }
    }

    /// As the run queue's: the least of the quantum's end while an equal waits, and the
    /// window's end; an idle processor in a frame wakes at its window's end (WN.4).
    #[must_use]
    pub fn decision_in_ns(&self) -> Option<u64> {
        let window = self
            .window_until()
            .map(|until| until.saturating_sub(self.now));
        let quantum = self
            .running
            .as_ref()
            .filter(|_| self.runnable_now())
            .map(|running| running.left_ns);
        match (window, quantum) {
            (Some(window), Some(quantum)) => Some(window.min(quantum)),
            (window, quantum) => window.or(quantum),
        }
    }

    /// [`Windows::decision_in_ns`] as an absolute time, which is what a timer must be armed for:
    /// the class's latest time may be older than the caller's, and a relative answer from it
    /// would arm late, past the window's end. From a stale time this is early, never late.
    #[must_use]
    pub fn decision_at(&self) -> Option<u64> {
        let window = self.window_until();
        let quantum = self
            .running
            .as_ref()
            .filter(|_| self.runnable_now())
            .map(|running| self.now.saturating_add(running.left_ns));
        match (window, quantum) {
            (Some(window), Some(quantum)) => Some(window.min(quantum)),
            (window, quantum) => window.or(quantum),
        }
    }

    /// As the run queue's: a higher level waits in the window, the running task's window is
    /// over, or its quantum is spent with an equal waiting; with nothing running, anything
    /// the window may run.
    #[must_use]
    pub fn should_preempt(&self) -> bool {
        self.should_preempt_at(self.now)
    }

    /// [`Windows::should_preempt`] at `now`, for a wake's decision (WT.1).
    #[must_use]
    pub fn should_preempt_at(&self, now: u64) -> bool {
        let Some(running) = self.running.as_ref() else {
            return self.runnable_at(now);
        };
        if !self.may_run_at(running.partition, now) {
            return true;
        }
        let Some(first) = self
            .best_waiting_at(now)
            .and_then(|at| self.sets.get(at)?.first())
        else {
            return false;
        };
        let level = running.level.min(LEVELS - 1);
        first.1 < level || (running.left_ns == 0 && first.1 == level)
    }

    /// As the run queue's: what is left of the running quantum.
    #[must_use]
    pub fn remaining_ns(&self) -> Option<u64> {
        self.running.as_ref().map(|running| running.left_ns)
    }

    /// As the run queue's: the running task taken out, its quantum's remainder in its state.
    pub fn remove_curr(&mut self) -> Option<Left<T>> {
        let Running {
            id,
            payload,
            mut state,
            slot,
            left_ns,
            ..
        } = self.running.take()?;
        state.vlag = i64::try_from(left_ns).unwrap_or(0);
        Some((id, payload, state, slot))
    }

    /// As the run queue's: `id` taken out, running or waiting.
    pub fn remove(&mut self, id: u64) -> Option<(T, EntityState, Slot<T>)> {
        if self.current_id() == Some(id) {
            return self
                .remove_curr()
                .map(|(_, payload, state, slot)| (payload, state, slot));
        }
        self.sets
            .iter_mut()
            .find_map(|set| set.remove(id))
            .map(|(_, payload, state, slot)| (payload, state, slot))
    }

    /// The running task.
    #[must_use]
    pub fn current(&self) -> Option<&T> {
        self.running.as_ref().map(|running| &running.payload)
    }

    /// The running task's id.
    #[must_use]
    pub fn current_id(&self) -> Option<u64> {
        self.running.as_ref().map(|running| running.id)
    }

    /// Every task here, of every partition, the running one included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.queued() + usize::from(self.running.is_some())
    }

    /// Whether nothing is here at all, of any partition: not "a pick finds something", which
    /// is [`Windows::runnable_now`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every task waiting here, of every partition.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.sets.iter().map(Fixed::len).sum()
    }

    /// The last waiting task of the window's partition whose payload satisfies `movable`: what
    /// steal, balance and push may take. None in a gap.
    #[must_use]
    pub fn latest_where(&self, movable: impl Fn(&T) -> bool) -> Option<u64> {
        self.latest_where_at(self.now, movable)
    }

    /// [`Windows::latest_where`] at `now`: steal, balance and push decide outside the source's
    /// picks (WT.1).
    #[must_use]
    pub fn latest_where_at(&self, now: u64, movable: impl Fn(&T) -> bool) -> Option<u64> {
        match self.window_at(now) {
            Now::Merged => self
                .sets
                .iter()
                .rev()
                .find_map(|set| set.last_where(&movable)),
            Now::In(partition) => self.sets.get(usize::from(partition))?.last_where(movable),
            Now::Gap => None,
        }
    }

    /// The run queue's `set_weight`: the weight is not read. A waiting task is re-keyed at its
    /// payload's level now (a nice that changed), behind its equals. Whether it was here.
    ///
    /// # Errors
    ///
    /// None: the signature is the run queue's.
    pub fn set_weight(&mut self, id: u64, _weight: u32) -> Result<bool, SchedError> {
        if let Some(running) = self.running.as_mut().filter(|running| running.id == id) {
            running.level = running.payload.level();
            return Ok(true);
        }
        for set in &mut self.sets {
            if let Some((id, payload, state, slot)) = set.remove(id) {
                let level = payload.level();
                // NOALLOC: into the task's own run slot, the `Slot<T>` taken out with it, in ferrix-sched's intrusive tree.
                set.push(id, level, payload, state, slot, false);
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The run queue's `get`: the weight it came with, virtual times zero.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<EntityView<'_, T>> {
        if let Some(running) = self.running.as_ref().filter(|running| running.id == id) {
            return Some(Self::view(id, &running.payload, running.state, true));
        }
        self.sets.iter().find_map(|set| {
            let (payload, weight) = set.set().get(id)?;
            Some(Self::view(id, payload, EntityState::new(weight), false))
        })
    }

    fn view(id: u64, payload: &T, state: EntityState, running: bool) -> EntityView<'_, T> {
        EntityView {
            id,
            weight: state.weight,
            vruntime: 0,
            deadline: 0,
            sum_exec: state.sum_exec,
            request_ns: 0,
            lag: 0,
            running,
            payload,
        }
    }

    /// The run queue's `for_each`: every task here, the running one first.
    pub fn for_each(&self, mut visit: impl FnMut(EntityView<'_, T>)) {
        if let Some(running) = self.running.as_ref() {
            visit(Self::view(
                running.id,
                &running.payload,
                running.state,
                true,
            ));
        }
        for set in &self.sets {
            set.set().for_each(|id, _, payload, weight| {
                visit(Self::view(id, payload, EntityState::new(weight), false));
            });
        }
    }

    /// A stub (G.1): no virtual time.
    #[must_use]
    pub const fn avg_vruntime(&self) -> u64 {
        0
    }

    /// For the load average alone: the weights the tasks came with, summed.
    #[must_use]
    pub fn load_weight(&self) -> u64 {
        let mut sum = self
            .running
            .as_ref()
            .map_or(0, |running| u64::from(running.state.weight));
        for set in &self.sets {
            set.set()
                .for_each(|_, _, _, weight| sum = sum.saturating_add(u64::from(weight)));
        }
        sum
    }

    /// A stub (G.1): no request is read. Whether `id` is here.
    pub fn set_request(&mut self, id: u64, _request_ns: u64) -> bool {
        self.current_id() == Some(id) || self.sets.iter().any(|set| set.holds(id))
    }

    /// The quantum.
    #[must_use]
    pub const fn slice_ns(&self) -> u64 {
        QUANTUM_NS
    }

    /// A stub (G.1): the configured slice is kept and not read.
    ///
    /// # Errors
    ///
    /// [`SchedError::ZeroSlice`] for zero, as the run queue's.
    pub fn set_slice_ns(&mut self, slice_ns: u64) -> Result<(), SchedError> {
        if slice_ns == 0 {
            return Err(SchedError::ZeroSlice);
        }
        self.config.slice_ns = slice_ns;
        Ok(())
    }

    /// The configuration as given.
    #[must_use]
    pub const fn config(&self) -> Config {
        self.config
    }

    /// A stub (G.1): nothing to level.
    pub const fn level(&mut self) {}

    /// The worst overrun so far: from a window's end to the pick that followed it.
    #[must_use]
    pub const fn worst_overrun_ns(&self) -> u64 {
        self.worst_overrun_ns
    }

    /// The last window's end a task still ran at, and the time of the pick that ended it.
    #[must_use]
    pub const fn last_overrun(&self) -> Option<(u64, u64)> {
        self.last_overrun
    }

    /// The latest time the class was told.
    #[must_use]
    pub const fn now(&self) -> u64 {
        self.now
    }

    /// How many windows ended with their task still running.
    #[must_use]
    pub const fn windows_ended(&self) -> u64 {
        self.windows_ended
    }

    /// Check every set and that the running task waits in none.
    ///
    /// # Errors
    ///
    /// The first claim that does not hold.
    pub fn check_invariants(&self) -> Result<(), &'static str> {
        for set in &self.sets {
            let _ = set.set().check()?;
        }
        if let Some(id) = self.current_id()
            && self.sets.iter().any(|set| set.holds(id))
        {
            return Err("the running task also waits");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
