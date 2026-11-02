//! The restart policy (`docs/INIT.md` §5.4): whether an end restarts a
//! service or a driver, after how long, and when to give up.
//!
//! Its own crate, and one that allocates nothing, because `devmgr` -- a
//! `no_std` native program with no allocator -- shares it with the service
//! manager (`src/lib/init/svc`, which re-exports every name here from where it
//! always was). It also parses nothing: the text of `Restart=` or a signal's
//! name is the service manager's to read.
//!
//! * [`wanted`]: systemd's table of which ends each `Restart=` restarts.
//! * [`Backoff`]: `RestartSec=`, doubled on each restart in a row, up to 32
//!   times it.
//! * [`Budget`]: the start limit, `StartLimitBurst=` starts within
//!   `StartLimitIntervalSec=`. Every start is counted, asked for or not, as
//!   systemd counts it. Given no clock, it is a plain count of starts.
//!
//! [`Policy`] puts them together: restart now, restart at `t`, or give up.
//! [`Escalation`] is `devmgr`'s alone, over a [`Policy`]: a driver that keeps
//! dying at its start is restarted twice, then started once more on a
//! reinitialised device, then its device is quarantined.
//! [`Instant`], [`Signal`], [`Exit`] and [`Restart`] are the words it is
//! decided in.

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;
use core::ops::Add;
use core::time::Duration;

#[cfg(test)]
mod tests;

/// A point in time: nanoseconds on a monotonic clock whose start is the
/// backend's business.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(u64);

impl Instant {
    /// The clock's start.
    pub const ZERO: Instant = Instant(0);

    /// The instant `nanos` after the clock's start.
    pub const fn from_nanos(nanos: u64) -> Instant {
        Instant(nanos)
    }

    /// The instant `millis` after the clock's start, for tests and for a
    /// clock that counts in milliseconds.
    pub const fn from_millis(millis: u64) -> Instant {
        Instant(millis.saturating_mul(1_000_000))
    }

    /// Nanoseconds since the clock's start.
    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    /// How long after `earlier` this is; zero if it is not after it.
    pub fn saturating_since(self, earlier: Instant) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }
}

impl Add<Duration> for Instant {
    type Output = Instant;

    /// Saturating: a deadline past the clock's end never comes, which is
    /// what an `infinity` timeout means anyway.
    fn add(self, duration: Duration) -> Instant {
        let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        Instant(self.0.saturating_add(nanos))
    }
}

impl fmt::Display for Instant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let millis = self.0 / 1_000_000;
        write!(f, "{}.{:03}s", millis / 1000, millis % 1000)
    }
}

/// Linux's signal names, by number; the same on every architecture Ferrix
/// runs on.
pub const SIGNALS: [(&str, u8); 31] = [
    ("HUP", 1),
    ("INT", 2),
    ("QUIT", 3),
    ("ILL", 4),
    ("TRAP", 5),
    ("ABRT", 6),
    ("BUS", 7),
    ("FPE", 8),
    ("KILL", 9),
    ("USR1", 10),
    ("SEGV", 11),
    ("USR2", 12),
    ("PIPE", 13),
    ("ALRM", 14),
    ("TERM", 15),
    ("STKFLT", 16),
    ("CHLD", 17),
    ("CONT", 18),
    ("STOP", 19),
    ("TSTP", 20),
    ("TTIN", 21),
    ("TTOU", 22),
    ("URG", 23),
    ("XCPU", 24),
    ("XFSZ", 25),
    ("VTALRM", 26),
    ("PROF", 27),
    ("WINCH", 28),
    ("IO", 29),
    ("PWR", 30),
    ("SYS", 31),
];

/// A signal, by number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Signal(pub u8);

impl Signal {
    /// `SIGHUP`.
    pub const HUP: Signal = Signal(1);
    /// `SIGINT`.
    pub const INT: Signal = Signal(2);
    /// `SIGKILL`.
    pub const KILL: Signal = Signal(9);
    /// `SIGTERM`.
    pub const TERM: Signal = Signal(15);
    /// `SIGCONT`.
    pub const CONT: Signal = Signal(18);

    /// Its name without `SIG`, if it has one.
    pub fn name(self) -> Option<&'static str> {
        SIGNALS
            .iter()
            .find(|&&(_, number)| number == self.0)
            .map(|&(name, _)| name)
    }
}

/// How a process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Exit {
    /// It called `exit` with this status.
    Code(i32),
    /// A signal ended it.
    Signal {
        /// Which.
        signal: Signal,
        /// Whether it dumped core.
        core: bool,
    },
}

/// `Restart=`: which ends of a service start it again (§5.4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Restart {
    /// Never.
    #[default]
    No,
    /// After a clean exit only.
    OnSuccess,
    /// After an unclean exit, a signal, a timeout or a watchdog.
    OnFailure,
    /// After a signal, a timeout or a watchdog.
    OnAbnormal,
    /// After a watchdog timeout.
    OnWatchdog,
    /// After a signal that was not caught.
    OnAbort,
    /// After any end that was not asked for.
    Always,
}

/// How a service ended, in the terms `Restart=` is decided by: systemd's
/// service results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ended {
    /// Exit status 0, or a clean signal: `SIGHUP`, `SIGINT`, `SIGTERM` or
    /// `SIGPIPE`.
    Success,
    /// Another exit status.
    ExitCode,
    /// Another signal.
    Signal,
    /// A signal that dumped core.
    CoreDump,
    /// A start or stop that took longer than its timeout.
    Timeout,
    /// A watchdog timeout.
    Watchdog,
    /// The kernel's OOM kill reached it (§5.5).
    OomKill,
    /// It could not be started at all: a spawn that failed.
    Resources,
    /// The start limit was spent (§5.4), so it was not started.
    StartLimitHit,
}

impl Ended {
    /// How an exit ends a service.
    pub fn of(exit: Exit) -> Ended {
        const CLEAN: [Signal; 4] = [Signal::HUP, Signal::INT, Signal::TERM, Signal(13)];
        match exit {
            Exit::Code(0) => Ended::Success,
            Exit::Code(_) => Ended::ExitCode,
            Exit::Signal { signal, .. } if CLEAN.contains(&signal) => Ended::Success,
            Exit::Signal { core: true, .. } => Ended::CoreDump,
            Exit::Signal { .. } => Ended::Signal,
        }
    }

    /// systemd's name for it, as `svc status` shows it.
    pub fn name(self) -> &'static str {
        match self {
            Ended::Success => "success",
            Ended::ExitCode => "exit-code",
            Ended::Signal => "signal",
            Ended::CoreDump => "core-dump",
            Ended::Timeout => "timeout",
            Ended::Watchdog => "watchdog",
            Ended::OomKill => "oom-kill",
            Ended::Resources => "resources",
            Ended::StartLimitHit => "start-limit-hit",
        }
    }
}

/// Whether `restart` restarts a service that ended so: the table in
/// `systemd.service(5)`.
pub fn wanted(restart: Restart, ended: Ended) -> bool {
    if ended == Ended::StartLimitHit {
        return false;
    }
    match restart {
        Restart::No => false,
        Restart::Always => true,
        Restart::OnSuccess => ended == Ended::Success,
        Restart::OnFailure => ended != Ended::Success,
        Restart::OnAbnormal => matches!(
            ended,
            Ended::Signal | Ended::CoreDump | Ended::Timeout | Ended::Watchdog
        ),
        Restart::OnAbort => matches!(ended, Ended::Signal | Ended::CoreDump),
        Restart::OnWatchdog => ended == Ended::Watchdog,
    }
}

/// The delay before a restart: `RestartSec=` for the first, doubled for
/// each restart in a row after it, up to 32 times `RestartSec=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    base: Duration,
    /// Restarts in a row so far.
    steps: u32,
}

impl Backoff {
    /// The most a delay grows to, in multiples of `RestartSec=`.
    pub const CAP: u32 = 32;

    /// A backoff starting at `base`.
    pub fn new(base: Duration) -> Self {
        Self { base, steps: 0 }
    }

    /// The delay before the next restart, counting it as one more in a row.
    pub fn next_delay(&mut self) -> Duration {
        let factor = 1u32
            .checked_shl(self.steps)
            .unwrap_or(Self::CAP)
            .min(Self::CAP);
        self.steps = self.steps.saturating_add(1);
        self.base.saturating_mul(factor)
    }

    /// Restarts in a row so far.
    pub fn steps(&self) -> u32 {
        self.steps
    }

    /// Start counting again: the service was started by request, or
    /// `reset-failed` was asked for.
    pub fn reset(&mut self) {
        self.steps = 0;
    }
}

/// The start limit: at most `burst` starts within `interval`. With a clock
/// it is systemd's `ratelimit_below` (`src/basic/ratelimit.c`): a fixed
/// window that begins at the first start and, once `interval` has passed
/// since it began, begins again at the next, not a sliding window over the
/// last `burst` starts. With no clock, it is a count of every start since
/// the last reset. Either way it holds no list, so `devmgr`, which has no
/// allocator, can keep one per device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    burst: u32,
    interval: Duration,
    /// When the window began, `None` before the first start with a clock.
    begin: Option<Instant>,
    /// The starts counted in the window.
    used: u32,
}

impl Budget {
    /// A budget of `burst` starts in `interval`; an interval of zero, or a
    /// burst of zero, turns the limit off, as in systemd.
    pub const fn new(burst: u32, interval: Duration) -> Self {
        Self {
            burst,
            interval,
            begin: None,
            used: 0,
        }
    }

    /// Whether the limit is off.
    pub const fn unlimited(&self) -> bool {
        self.burst == 0 || self.interval.is_zero()
    }

    /// Count a start at `now`. `false` if it is over the budget, in which
    /// case the start must not happen; it is not counted.
    pub fn start(&mut self, now: Option<Instant>) -> bool {
        if self.unlimited() {
            return true;
        }
        if let Some(now) = now {
            let within = self
                .begin
                .is_some_and(|begin| now.saturating_since(begin) <= self.interval);
            if !within {
                self.begin = Some(now);
                self.used = 0;
            }
        }
        if self.used >= self.burst {
            return false;
        }
        self.used += 1;
        true
    }

    /// Starts counted now.
    pub const fn used(&self) -> usize {
        self.used as usize
    }

    /// Forget every start: `reset-failed`.
    pub const fn reset(&mut self) {
        self.begin = None;
        self.used = 0;
    }
}

/// What to do about an end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Leave it stopped: `Restart=` does not restart this end.
    Stay,
    /// Restart now: the delay is zero, or there is no clock to wait on.
    Now,
    /// Restart at this time.
    At(Instant),
    /// The start limit is spent: the unit is `failed`, with the result
    /// `start-limit-hit`, until `reset-failed` or a new start.
    GiveUp,
}

/// `Restart=`, its backoff and its budget, for one unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// `Restart=`.
    pub restart: Restart,
    /// The delays.
    pub backoff: Backoff,
    /// The start limit.
    pub budget: Budget,
}

impl Policy {
    /// A policy from a unit's settings.
    pub fn new(restart: Restart, delay: Duration, burst: u32, interval: Duration) -> Self {
        Self {
            restart,
            backoff: Backoff::new(delay),
            budget: Budget::new(burst, interval),
        }
    }

    /// Decide about an end at `now`. A restart it decides on is counted
    /// against the budget already, so the start that follows must not be
    /// counted again.
    pub fn decide(&mut self, ended: Ended, now: Option<Instant>) -> Decision {
        if !wanted(self.restart, ended) {
            return Decision::Stay;
        }
        if !self.budget.start(now) {
            return Decision::GiveUp;
        }
        let delay = self.backoff.next_delay();
        match now {
            Some(now) if !delay.is_zero() => Decision::At(now + delay),
            _ => Decision::Now,
        }
    }

    /// A start asked for, rather than decided on: counted against the
    /// budget, and the backoff starts again. `false` if the budget is spent.
    pub fn start(&mut self, now: Option<Instant>) -> bool {
        self.backoff.reset();
        self.budget.start(now)
    }

    /// `reset-failed`.
    pub fn reset(&mut self) {
        self.backoff.reset();
        self.budget.reset();
    }
}

/// What to do about a driver's death, once [`Escalation`] has weighed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Leave it stopped: `Restart=` does not restart this end.
    Stay,
    /// Start it again, at this time, or now for `None`.
    Restart(Option<Instant>),
    /// Reinitialise the device, then start it again, at this time, or now
    /// for `None`: the last start before quarantine.
    Reinit(Option<Instant>),
    /// Start nothing on it again: the start on a reinitialised device died
    /// too.
    Quarantine,
    /// The start limit is spent.
    GiveUp,
}

/// A crash loop's escalation, for one device (the customer, 2026-10-30): a
/// driver that dies within [`Escalation::settle`] of coming up -- or before
/// it came up at all -- has failed its start. After
/// [`Escalation::FAILED_RESTARTS`] failed restarts in a row the next start
/// is made on a reinitialised device, and if that one fails as well the
/// device is quarantined. A driver that stayed up for the whole window ends
/// the streak: its death is restarted as a first one.
///
/// The delays and the start limit are the [`Policy`]'s: every start it
/// decides on is counted against the budget, and the backoff doubles along
/// the streak and starts again with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Escalation {
    /// How long a driver must stay up for its death not to count as a
    /// failed start.
    settle: Duration,
    /// Restarts since the streak began.
    restarts: u32,
    /// Whether the last start was on a reinitialised device.
    reinitialised: bool,
}

impl Escalation {
    /// Failed restarts in a row before the device is reinitialised.
    pub const FAILED_RESTARTS: u32 = 2;

    /// An escalation that counts a death within `settle` of coming up as a
    /// failed start.
    pub const fn new(settle: Duration) -> Self {
        Self {
            settle,
            restarts: 0,
            reinitialised: false,
        }
    }

    /// The window a driver must stay up for.
    pub const fn settle(&self) -> Duration {
        self.settle
    }

    /// Restarts since the streak began.
    pub const fn restarts(&self) -> u32 {
        self.restarts
    }

    /// Decide about a death at `now` of a driver that came up at `up`
    /// (`None`: it never did), under `policy`. With no clock every death is
    /// a failed start, and every start is made at once.
    pub fn decide(
        &mut self,
        policy: &mut Policy,
        ended: Ended,
        now: Option<Instant>,
        up: Option<Instant>,
    ) -> Next {
        let settled = match (now, up) {
            (Some(now), Some(up)) => now.saturating_since(up) >= self.settle,
            _ => false,
        };
        if settled {
            self.restarts = 0;
            self.reinitialised = false;
            policy.backoff.reset();
        } else if self.reinitialised {
            return Next::Quarantine;
        }
        let at = match policy.decide(ended, now) {
            Decision::Stay => return Next::Stay,
            Decision::GiveUp => return Next::GiveUp,
            Decision::Now => None,
            Decision::At(at) => Some(at),
        };
        self.restarts = self.restarts.saturating_add(1);
        if self.restarts > Self::FAILED_RESTARTS {
            self.reinitialised = true;
            Next::Reinit(at)
        } else {
            Next::Restart(at)
        }
    }

    /// Begin again: a start was asked for (a BIND), not decided on.
    pub fn reset(&mut self) {
        self.restarts = 0;
        self.reinitialised = false;
    }
}
