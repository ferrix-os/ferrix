//! The service watchdog's ping (`docs/WATCHDOG-STAGES.md` §5.5):
//! `sd_notify`'s `WATCHDOG=1` on the descriptor init names in `NOTIFY_FD`.
//!
//! The ping comes from the server's loop ([`Notify::ping`] once a turn), so
//! a loop that stops stops it: a thread or a timer would ping on while the
//! loop is stuck. It goes at most every quarter of `WATCHDOG_USEC`, as
//! `sd_watchdog_enabled(3)` advises half. Without `NOTIFY_FD` nothing is
//! written; a write that fails is no ping, and init's watchdog says so.

use std::time::{Duration, Instant};

/// The descriptor and the ping's period, read once at start.
pub(crate) struct Notify {
    fd: Option<libc::c_int>,
    every: Option<Duration>,
    last: Option<Instant>,
}

impl Notify {
    /// From the environment init gave.
    pub(crate) fn from_environment() -> Notify {
        let fd = std::env::var("NOTIFY_FD")
            .ok()
            .and_then(|text| text.parse::<libc::c_int>().ok())
            .filter(|fd| *fd > 2);
        let every = std::env::var("WATCHDOG_USEC")
            .ok()
            .and_then(|text| text.parse::<u64>().ok())
            .filter(|micros| *micros > 0)
            .map(|micros| Duration::from_micros(micros / 4));
        Notify {
            fd,
            every,
            last: None,
        }
    }

    /// `WATCHDOG=1`, if one is due.
    pub(crate) fn ping(&mut self, now: Instant) {
        let Some(every) = self.every else {
            return;
        };
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < every)
        {
            return;
        }
        self.write(b"WATCHDOG=1\n");
        self.last = Some(now);
    }

    fn write(&self, line: &[u8]) {
        if let Some(fd) = self.fd {
            // SAFETY: `line` is readable for its length; the descriptor is
            // init's notify pipe, which nothing else in authd closes.
            let _ = unsafe { libc::write(fd, line.as_ptr().cast(), line.len()) };
        }
    }
}
