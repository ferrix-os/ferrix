//! The service watchdog's ping (`docs/WATCHDOG-STAGES.md` §5.5): `WATCHDOG=1`
//! on the descriptor init gave `hyprix.service` (`NOTIFY_FD`, passed down
//! by sessiond), from the compositor's own loop ([`ping`] in its wait), so
//! a loop that stops stops it. `hyprix.service` is `WatchdogAction=report`:
//! a restart would end every client's connection, so a miss is reported and
//! the desktop is left as it is (§5.6).
//!
//! [`adopt`] takes the descriptor and both variables out of the environment
//! and makes the descriptor close-on-exec, before any thread or child, so
//! no client can ping for the compositor.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The descriptor, the period and the last ping, once adopted.
struct Pinger {
    fd: libc::c_int,
    every: Duration,
    last: Option<Instant>,
}

static PINGER: Mutex<Option<Pinger>> = Mutex::new(None);

/// Take `NOTIFY_FD` and `WATCHDOG_USEC` out of the environment: whether the
/// compositor now pings. Before any thread or child.
pub fn adopt() -> bool {
    let fd = std::env::var("NOTIFY_FD")
        .ok()
        .and_then(|text| text.parse::<libc::c_int>().ok())
        .filter(|fd| *fd > 2);
    let micros = std::env::var("WATCHDOG_USEC")
        .ok()
        .and_then(|text| text.parse::<u64>().ok())
        .filter(|micros| *micros > 0);
    for variable in ["NOTIFY_FD", "WATCHDOG_USEC"] {
        // SAFETY: `main` calls this before it starts any thread, so nothing
        // reads the environment while it changes.
        unsafe { std::env::remove_var(variable) };
    }
    let (Some(fd), Some(micros)) = (fd, micros) else {
        return false;
    };
    // SAFETY: fcntl on a descriptor number with constants.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return false;
    }
    if let Ok(mut pinger) = PINGER.lock() {
        *pinger = Some(Pinger {
            fd,
            every: Duration::from_micros(micros / 4),
            last: None,
        });
        return true;
    }
    false
}

/// The longest the loop may sleep and still ping in time; `None` when it
/// does not ping.
pub(crate) fn period() -> Option<Duration> {
    PINGER.lock().ok()?.as_ref().map(|pinger| pinger.every)
}

/// `WATCHDOG=1`, if one is due: from the loop's wait.
pub(crate) fn ping(now: Instant) {
    let Ok(mut pinger) = PINGER.lock() else {
        return;
    };
    let Some(pinger) = pinger.as_mut() else {
        return;
    };
    if pinger
        .last
        .is_some_and(|last| now.duration_since(last) < pinger.every)
    {
        return;
    }
    let line = b"WATCHDOG=1\n";
    // SAFETY: `line` is readable for its length; the descriptor is init's
    // notify pipe, which the compositor holds for its life.
    let _ = unsafe { libc::write(pinger.fd, line.as_ptr().cast(), line.len()) };
    pinger.last = Some(now);
}
