//! Per-cgroup bandwidth: when a request may start.
//!
//! `io.max` limits a cgroup's bytes and operations per second on one disk,
//! each way. A [`Limiter`] is the virtual clock of one such limit: the
//! instant at which the work admitted so far has been paid for at the rate,
//! and so the earliest the next may start. A request that finds the clock in
//! the past starts at once and pushes it on by what it costs; one that finds
//! it in the future waits for it. Over any stretch the work started is at most
//! the rate times the stretch, plus the one request that begins it: no
//! burst beyond a request, where Linux's `blk-throttle` allows a slice's worth.
//!
//! A [`Throttle`] is a disk's two limits in one direction, bytes and
//! operations; a request starts when both allow it. A cgroup is throttled by
//! every throttle on the way up its tree, and starts at the latest of the
//! instants they give ([`Throttle::earliest`]), each then paid the same start
//! ([`Throttle::commit`]), so a parent's limit holds however its children
//! share it.
//!
//! Where this meets the queue in [`schedule`](crate::schedule)'s
//! documentation it is on the side of *submitting*: a throttled request waits
//! in its submitter, before it is queued, rather than skipped at dispatch. A
//! barrier cannot then be held back by a throttled request older than it, since
//! a request that has not been queued is not older than anything.

/// Nanoseconds in a second.
const SECOND: u128 = 1_000_000_000;

/// One limit's clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limiter {
    /// Units per second, or `None` for no limit.
    rate: Option<u64>,
    /// When the work admitted so far has been paid for.
    free_at: u64,
}

impl Limiter {
    /// No limit.
    pub const UNLIMITED: Limiter = Limiter {
        rate: None,
        free_at: 0,
    };

    /// A limit of `rate` units per second, `None` for none. Work admitted
    /// already is forgiven: a new rate starts from an empty clock.
    pub fn set_rate(&mut self, rate: Option<u64>) {
        self.rate = rate;
        self.free_at = 0;
    }

    /// Its rate.
    #[must_use]
    pub fn rate(&self) -> Option<u64> {
        self.rate
    }

    /// The earliest instant work may start at `now`.
    #[must_use]
    pub fn earliest(&self, now: u64) -> u64 {
        if self.rate.is_some() {
            now.max(self.free_at)
        } else {
            now
        }
    }

    /// Admit `cost` units starting at `start`, which is at least
    /// [`Limiter::earliest`].
    pub fn commit(&mut self, start: u64, cost: u64) {
        let Some(rate) = self.rate else {
            return;
        };
        let paid = u128::from(cost) * SECOND / u128::from(rate.max(1));
        self.free_at = u64::try_from(u128::from(start) + paid).unwrap_or(u64::MAX);
    }
}

/// A disk's bandwidth limit in one direction: bytes and operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Throttle {
    /// Bytes per second.
    pub bytes: Limiter,
    /// Operations per second.
    pub ios: Limiter,
}

impl Throttle {
    /// No limit in either.
    pub const UNLIMITED: Throttle = Throttle {
        bytes: Limiter::UNLIMITED,
        ios: Limiter::UNLIMITED,
    };

    /// Whether either is limited.
    #[must_use]
    pub fn is_limited(&self) -> bool {
        self.bytes.rate.is_some() || self.ios.rate.is_some()
    }

    /// The earliest instant a request may start at `now`.
    #[must_use]
    pub fn earliest(&self, now: u64) -> u64 {
        self.bytes.earliest(now).max(self.ios.earliest(now))
    }

    /// Admit a request of `bytes` starting at `start`.
    pub fn commit(&mut self, start: u64, bytes: u64) {
        self.bytes.commit(start, bytes);
        self.ios.commit(start, 1);
    }
}
