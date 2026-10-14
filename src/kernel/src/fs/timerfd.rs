//! Timerfds: a timer whose expirations are read from a descriptor, the object
//! behind `timerfd_create`.
//!
//! A timer is armed with a first expiration and an interval, both on the clock
//! it was made for. Each expiration adds one to a count, and a read takes the
//! whole count as eight bytes and leaves zero; a read of a zero count waits,
//! or is `EAGAIN` on a non-blocking descriptor. `poll` answers readable while
//! there is something to take. It is how an event loop sleeps until a
//! deadline among its other descriptors: foot blinks its cursor, repeats keys
//! and delays a render this way.
//!
//! Linux's `fs/timerfd.c`, check for check: a buffer shorter than eight bytes
//! is `EINVAL`, a write is `EINVAL`, and a periodic timer read late counts
//! every interval that passed. As there, an expiration nobody has read stops
//! the timer announcing more: the next wake is armed again by the read that
//! takes the count, and the read counts what passed meanwhile. So a periodic
//! timer nobody reads costs one wake, not one per interval.
//!
//! # Woken at the deadline
//!
//! A `poll` or `epoll_wait` trusts a timerfd's queue (`fs::wake`), so it
//! sleeps up to a second between looks of its own; a timer that only became
//! readable when somebody looked would be up to a second late, which a
//! blinking cursor shows. So expirations are driven, not discovered: one
//! kernel thread, `timerfds`, sleeps until the earliest deadline across every
//! timer, counts the expiration and wakes that timer's queue, and exits when
//! no timer is armed. It is `ITIMER_REAL`'s design (`syscall::kill`), with the
//! timers held weakly, so that closing the last descriptor frees one whether
//! or not the thread is running. The thread is the only thing that raises a
//! count from zero on its own; a read or a `poll` that looks after a deadline
//! and before the thread has run sees the expiration as it is, so nobody
//! reads a stale answer while the thread is on its way.
//!
//! # Clocks
//!
//! `CLOCK_MONOTONIC` and `CLOCK_BOOTTIME` are the counter, which does not stop
//! across a suspend here because nothing suspends. `CLOCK_REALTIME` deadlines
//! are kept on the real-time clock, so a clock set moves them as it moves
//! Linux's: the thread converts to the counter on every pass, and a set tells
//! it to pass again. `TFD_TIMER_CANCEL_ON_SET` is kept as Linux keeps it: an
//! absolute real-time timer armed with it becomes readable when the clock is
//! set, and the read, or the next arming, answers `ECANCELED`.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_kmem::{Charge, arc_footprint};
use ferrix_vfs::{Errno, Inode, Metadata, OpenFile, Readiness};

use crate::fs;
use crate::sched::{self, Task, WaitQueue};
use crate::sync::SpinLock;
use crate::syscall::{process, time};

/// The name `/proc/self/fd` shows.
const NAME: &[u8] = b"anon_inode:[timerfd]";

/// Bytes in what a read returns: the expiration count.
pub(crate) const VALUE_BYTES: usize = 8;

/// The deadline a blocked read passes: none. The thread's wake ends it.
const FOREVER: u64 = u64::MAX;

/// How long a blocked read sleeps between its own looks: the long one `poll`
/// gives files it trusts, because every rise of the count wakes the queue.
const RECHECK: u64 = fs::wake::TRUSTED_RECHECK_NANOS;

/// Which clock a timer counts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Clock {
    /// `CLOCK_MONOTONIC`: the counter.
    Monotonic,
    /// `CLOCK_BOOTTIME`: the counter, which counts through a suspend because
    /// there is none.
    Boottime,
    /// `CLOCK_REALTIME`: the counter plus the offset the clock was last set
    /// to.
    Realtime,
}

impl Clock {
    /// The clock now, in nanoseconds.
    pub(crate) fn now(self) -> u64 {
        match self {
            Clock::Realtime => time::realtime_nanos(),
            Clock::Monotonic | Clock::Boottime => crate::timer::now_nanos(),
        }
    }
}

/// A timer's setting in nanoseconds: what `timerfd_settime` asks for and
/// `timerfd_gettime` reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Setting {
    /// The period, or zero for a timer that expires once.
    pub(crate) interval: u64,
    /// When armed: the time to the next expiration, or with
    /// `TFD_TIMER_ABSTIME` the time of it. Zero disarms.
    pub(crate) value: u64,
}

/// How `timerfd_settime` reads its setting's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SetFlags {
    /// `TFD_TIMER_ABSTIME`: the value is a time on the clock.
    pub(crate) absolute: bool,
    /// `TFD_TIMER_CANCEL_ON_SET`: end the timer when the clock is set.
    pub(crate) cancel_on_set: bool,
}

/// What a timer holds.
#[derive(Debug, Default)]
struct State {
    /// The next expiration on the timer's clock, or `None` when disarmed.
    deadline: Option<u64>,
    /// The period, or zero. Kept when disarmed, as Linux keeps it.
    interval: u64,
    /// Expirations not yet read.
    ticks: u64,
    /// Armed with `TFD_TIMER_CANCEL_ON_SET`, absolute, on the real-time clock.
    might_cancel: bool,
    /// The clock was set since: the next read is `ECANCELED`.
    canceled: bool,
}

impl State {
    /// Count every expiration due by `now`, moving the deadline past it.
    /// Answers how many there were.
    fn count(&mut self, now: u64) -> u64 {
        let Some(deadline) = self.deadline.filter(|&deadline| deadline <= now) else {
            return 0;
        };
        // A zero interval is a one-shot timer, which expires once and disarms.
        let passed = match (now - deadline).checked_div(self.interval) {
            None => {
                self.deadline = None;
                1
            }
            Some(whole) => {
                let passed = whole + 1;
                // Past the end of the clock is never: a deadline of
                // `u64::MAX` is not reached.
                self.deadline = Some(deadline.saturating_add(passed.saturating_mul(self.interval)));
                passed
            }
        };
        self.ticks = self.ticks.saturating_add(passed);
        passed
    }

    /// Whether an expiration is due at `now` that the count does not hold yet.
    fn due(&self, now: u64) -> bool {
        self.deadline.is_some_and(|deadline| deadline <= now)
    }

    /// The setting now: the time left and the period.
    ///
    /// An expired periodic timer is moved on first, as Linux's `hrtimer_forward`
    /// moves it, so the time left is to the next expiration. One that is due
    /// and not yet counted has nothing left, which is Linux's answer too.
    fn setting(&mut self, now: u64) -> Setting {
        if self.ticks > 0 {
            let _ = self.count(now);
        }
        Setting {
            interval: self.interval,
            value: self
                .deadline
                .map_or(0, |deadline| deadline.saturating_sub(now)),
        }
    }
}

/// A timerfd.
pub(crate) struct TimerFd {
    /// The clock it was made for.
    clock: Clock,
    /// What it holds.
    state: SpinLock<State>,
    /// Woken when a read may no longer wait: the count rose from zero.
    readable: Arc<WaitQueue>,
    /// Its heap and its entry in the timer list, charged to the job that
    /// made it (F-37). The list's weak entry keeps the allocation until the
    /// next timer made prunes it, which the charge does not outlive: what
    /// the list holds of gone timers is at most what was live, and charged,
    /// when the last one was made.
    _charge: Charge,
}

impl fmt::Debug for TimerFd {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TimerFd")
            .field("clock", &self.clock)
            .finish_non_exhaustive()
    }
}

/// A new, disarmed timer on `clock`, as the open file `timerfd_create`
/// installs.
///
/// # Errors
///
/// Whatever [`fs::anon::open`] refuses, which for a timerfd is nothing.
pub(crate) fn create(clock: Clock, nonblock: bool) -> Result<Arc<OpenFile>, Errno> {
    let charge = Charge::bytes(
        arc_footprint::<TimerFd>()
            .saturating_add(arc_footprint::<WaitQueue>())
            .saturating_add(size_of::<Weak<TimerFd>>()),
    )
    .map_err(|_| Errno::ENOMEM)?;
    let timer = Arc::new(TimerFd {
        clock,
        state: SpinLock::new(State::default()),
        readable: Arc::new(WaitQueue::new()),
        _charge: charge,
    });
    enlist(&timer);
    fs::anon::open(timer, NAME, nonblock)
}

impl TimerFd {
    /// The clock it counts on.
    pub(crate) fn clock(&self) -> Clock {
        self.clock
    }

    /// Arm or disarm the timer, answering the setting it replaced, as
    /// `timerfd_settime` does.
    ///
    /// Arming an absolute timer that may be canceled, after the clock was set
    /// and before a read said so, arms it and answers `ECANCELED`, which is
    /// `timerfd_setup`'s order.
    pub(crate) fn set(&self, flags: SetFlags, new: Setting) -> Result<Setting, Errno> {
        let (answer, armed) = {
            let mut state = self.state.lock();
            let now = self.clock.now();
            let old = state.setting(now);
            state.might_cancel =
                self.clock == Clock::Realtime && flags.absolute && flags.cancel_on_set;
            if !state.might_cancel {
                state.canceled = false;
            }
            state.ticks = 0;
            state.interval = new.interval;
            state.deadline = match new.value {
                0 => None,
                value if flags.absolute => Some(value),
                value => Some(now.saturating_add(value)),
            };
            let armed = state.deadline.is_some();
            let answer = if armed && state.canceled {
                state.canceled = false;
                Err(Errno::ECANCELED)
            } else {
                Ok(old)
            };
            (answer, armed)
        };
        if armed {
            timers_changed();
        } else {
            // Nothing to time: a thread already running looks again and may
            // leave, and none is started for it.
            nudge();
        }
        answer
    }

    /// The setting now, as `timerfd_gettime` reports it.
    pub(crate) fn get(&self) -> Setting {
        let mut state = self.state.lock();
        let now = self.clock.now();
        state.setting(now)
    }

    /// Take the count, counting what is due first: `None` while there is
    /// nothing, `ECANCELED` once for a clock set.
    fn take(&self) -> Option<Result<u64, Errno>> {
        let (taken, periodic) = {
            let mut state = self.state.lock();
            let now = self.clock.now();
            let _ = state.count(now);
            if state.ticks == 0 {
                return None;
            }
            let ticks = core::mem::take(&mut state.ticks);
            let taken = if core::mem::take(&mut state.canceled) {
                Err(Errno::ECANCELED)
            } else {
                Ok(ticks)
            };
            (taken, state.deadline.is_some())
        };
        if periodic {
            // The thread stopped timing this one while the count was unread.
            timers_changed();
        }
        Some(taken)
    }

    /// Whether a read would take something now.
    fn ready(&self) -> bool {
        let state = self.state.lock();
        state.ticks > 0 || state.due(self.clock.now())
    }

    /// Count a due expiration if the count is empty, as the thread does at a
    /// deadline.
    fn fire(&self, counter_now: u64) -> Fired {
        let mut state = self.state.lock();
        let now = self.clock.now();
        let fired = state.ticks == 0 && state.count(now) > 0;
        let next = match (state.ticks, state.deadline) {
            (0, Some(deadline)) => counter_now.saturating_add(deadline.saturating_sub(now)),
            _ => u64::MAX,
        };
        Fired {
            fired,
            next,
            armed: state.deadline.is_some(),
        }
    }

    /// Mark the timer canceled for a set of the clock, if it asked to be.
    fn cancel_for_clock_set(&self) -> bool {
        let mut state = self.state.lock();
        if !state.might_cancel {
            return false;
        }
        state.canceled = true;
        state.ticks = state.ticks.saturating_add(1);
        true
    }

    /// Whether the thread has anything to time for this one on the real-time
    /// clock, which a set of that clock moves.
    fn on_the_realtime_clock(&self) -> bool {
        self.clock == Clock::Realtime && self.state.lock().deadline.is_some()
    }

    /// How many waits on the timer a wake has ended, for the checks.
    pub(crate) fn waits_ended_by_a_wake(&self) -> u32 {
        self.readable.waits_ended_by_a_wake()
    }

    /// How many tasks wait on the timer becoming readable now, for the checks.
    pub(crate) fn readers_listed(&self) -> usize {
        self.readable.listed()
    }
}

/// What the thread's look at one timer found.
#[derive(Debug, Clone, Copy)]
struct Fired {
    /// The count rose from zero: wake the readers.
    fired: bool,
    /// When the timer next needs the thread, on the counter's reckoning, or
    /// `u64::MAX` when it does not: disarmed, or an unread count holding it.
    next: u64,
    /// Armed. An armed timer with an unread count needs no deadline, but its
    /// read will want the thread again, so the thread stays for it rather
    /// than being started anew once a period.
    armed: bool,
}

impl Drop for TimerFd {
    /// An armed timer closed may have been what kept the thread: tell it to
    /// look again, so it leaves now rather than at its next recheck.
    fn drop(&mut self) {
        if self.state.lock().deadline.is_some() {
            nudge();
        }
    }
}

/// Whether the process a wait is on behalf of has a signal to take.
fn interrupted(caller: Option<&Arc<process::Process>>) -> bool {
    caller.is_some_and(|process| process.signal_pending())
}

impl Inode for TimerFd {
    fn metadata(&self) -> Metadata {
        fs::anon::metadata()
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        true
    }

    /// `lseek` on a timerfd is 0, as Linux's `noop_llseek` answers, while
    /// `pread64` and `pwrite64` stay `ESPIPE`.
    fn seek_is_noop(&self) -> bool {
        true
    }

    fn poll(&self) -> Readiness {
        Readiness {
            readable: self.ready(),
            writable: false,
            hangup: false,
            error: false,
            priority: false,
        }
    }

    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        visit(fs::wake::shared(&self.readable));
        true
    }

    fn poll_changes(&self) -> Option<u64> {
        Some(self.readable.wakes())
    }

    /// Eight bytes of the count, taken; `EAGAIN` or a wait while it is zero.
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> ferrix_vfs::Result<usize> {
        let out = buf.get_mut(..VALUE_BYTES).ok_or(Errno::EINVAL)?;
        let caller = process::current();
        loop {
            if let Some(taken) = self.take() {
                out.copy_from_slice(&taken?.to_ne_bytes());
                return Ok(VALUE_BYTES);
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            let _ = WaitQueue::wait_on_any(
                &[&self.readable],
                || self.ready() || interrupted(caller.as_ref()),
                FOREVER,
                RECHECK,
            );
            if interrupted(caller.as_ref()) {
                // `timerfd_read`'s wait is interruptible and restarts under
                // `SA_RESTART`, as an eventfd read does.
                return Err(Errno::ERESTARTSYS);
            }
        }
    }
}

/// The timerfd an open file is, if it is one.
pub(crate) fn of(file: &OpenFile) -> Option<Arc<TimerFd>> {
    Arc::clone(file.io()).into_any().downcast::<TimerFd>().ok()
}

// ---------------------------------------------------------------------------
// The thread
// ---------------------------------------------------------------------------

/// Every timerfd there is, weakly. Enlisted when made and dropped from the
/// list once gone, so the thread never keeps a closed timer alive.
static TIMERS: SpinLock<Vec<Weak<TimerFd>>> = SpinLock::new(Vec::new());
/// Whether the `timerfds` thread is running. Taken to decide it may exit, and
/// to decide one must be started, so the two decisions cannot cross.
static CLOCK_RUNNING: SpinLock<bool> = SpinLock::new(false);
/// Set when any timer's deadline may have changed, so the thread looks again.
static CHANGED: AtomicBool = AtomicBool::new(false);
/// Where the thread sleeps until the next deadline.
static CLOCK: WaitQueue = WaitQueue::new();
/// The last thread started, for a check that must see it gone before it
/// counts frames.
static CLOCK_TASK: SpinLock<Option<Arc<Task>>> = SpinLock::new(None);

/// Put a new timer on the list.
fn enlist(timer: &Arc<TimerFd>) {
    forget_closed();
    TIMERS.lock().push(Arc::downgrade(timer));
}

/// Drop the timers that are gone from the list: when one is made, and for a
/// check about to count frames.
pub(crate) fn forget_closed() {
    let gone = {
        let mut timers = TIMERS.lock();
        let (live, gone): (Vec<_>, Vec<_>) = core::mem::take(&mut *timers)
            .into_iter()
            .partition(|entry| entry.strong_count() > 0);
        *timers = live;
        gone
    };
    // Freed with the list unlocked.
    drop(gone);
}

/// Every timer still open. The strong references are dropped by the caller
/// with the list unlocked, since one may be a timer's last.
fn live() -> Vec<Arc<TimerFd>> {
    TIMERS.lock().iter().filter_map(Weak::upgrade).collect()
}

/// Tell the thread a deadline changed, starting it if it is not running.
fn timers_changed() {
    let start = {
        let mut running = CLOCK_RUNNING.lock();
        CHANGED.store(true, Ordering::Release);
        // Claimed under the lock and spawned outside it, as `itimers` is:
        // a failed spawn frees a half-made stack, which may not happen under
        // a lock that disables preemption.
        let start = !*running;
        *running = true;
        start
    };
    if start {
        match sched::spawn("timerfds", run_clock, 0, ferrix_sched::NICE_0_WEIGHT) {
            Ok(task) => {
                let previous = CLOCK_TASK.lock().replace(task);
                drop(previous);
            }
            // Only out of memory: the next change tries again.
            Err(_) => *CLOCK_RUNNING.lock() = false,
        }
    }
    CLOCK.wake_all();
}

/// Tell a running thread to look again, starting none: for a timer closed,
/// which needs no thread, only for one to notice it is gone.
fn nudge() {
    CHANGED.store(true, Ordering::Release);
    CLOCK.wake_all();
}

/// The `timerfds` thread: count every expiration that is due and wake its
/// readers, then sleep until the next deadline or a change. Returns, ending
/// the thread, when nothing is armed.
fn run_clock(_argument: usize) {
    loop {
        let (next, armed) = pass();
        if !armed {
            let mut running = CLOCK_RUNNING.lock();
            if !CHANGED.swap(false, Ordering::AcqRel) {
                *running = false;
                return;
            }
            continue;
        }
        // With no deadline -- every armed timer holding an unread count --
        // only a change ends the sleep, and the trusted recheck stands in
        // for one that went missing.
        let _ = WaitQueue::wait_on_any(
            &[&CLOCK],
            || CHANGED.swap(false, Ordering::AcqRel),
            next,
            fs::wake::TRUSTED_RECHECK_NANOS,
        );
    }
}

/// One look at every timer: fire what is due, and answer the earliest next
/// deadline on the counter (`u64::MAX` for none) and whether any is armed.
fn pass() -> (u64, bool) {
    let timers = live();
    let now = crate::timer::now_nanos();
    let mut next = u64::MAX;
    let mut armed = false;
    for timer in &timers {
        let look = timer.fire(now);
        if look.fired {
            timer.readable.wake_all();
        }
        next = next.min(look.next);
        armed |= look.armed;
    }
    (next, armed)
}

/// `CLOCK_REALTIME` was set: cancel the timers that asked to be, waking their
/// readers, and have the thread time the real-time deadlines again.
pub(crate) fn clock_was_set() {
    let mut realtime = false;
    for timer in live() {
        if timer.cancel_for_clock_set() {
            timer.readable.wake_all();
        }
        realtime |= timer.on_the_realtime_clock();
    }
    if realtime {
        timers_changed();
    }
}

/// Wait until the `timerfds` thread has exited and been reaped, for a check
/// that counts frames: once nothing is armed it leaves by itself.
pub(crate) fn wait_until_clock_gone(patience_nanos: u64) -> Result<(), &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(patience_nanos);
    while *CLOCK_RUNNING.lock() {
        if crate::timer::now_nanos() >= deadline {
            return Err("the timerfds thread did not exit with nothing armed");
        }
        sched::sleep_for(1_000_000);
    }
    let task = CLOCK_TASK.lock().take();
    match task {
        Some(task) => {
            let left = deadline.saturating_sub(crate::timer::now_nanos());
            sched::wait_until_gone(&task, left)
        }
        None => Ok(()),
    }
}
