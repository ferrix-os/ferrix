//! Feeding the kernel's user watchdog (`docs/POSTMORTEM.md` §6.9): the
//! policy half of it, ring 3's.
//!
//! With `ferrix.init.watchdog=<seconds>` on the command line, init opens
//! `/dev/watchdog`, sets that timeout and feeds it every third of it from
//! its event loop, whose wait the next feed bounds. A user space in which
//! pid 1 cannot turn its loop for the timeout is a stopped one, and the
//! kernel reports it (FX-9008). Init never writes the magic `V`: a stop on
//! purpose disarms the watchdog in the kernel (`reboot(2)`, init's exit).
//!
//! Without the option, or without the device (`ferrix.watchdog=user` not
//! given), nothing is opened and nothing is fed.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::time::{Duration, Instant};

use crate::sys;

/// Init's option, read from the kernel's command line.
const OPTION: &str = "ferrix.init.watchdog=";

/// The device.
const DEVICE: &str = "/dev/watchdog";

/// The longest timeout the kernel takes.
const MAX_SECONDS: u32 = 600;

/// The open device and when it is next fed.
#[derive(Debug, Default)]
pub(crate) struct Watchdog {
    /// The device, while init feeds it.
    file: Option<File>,
    /// How often it is fed: a third of the timeout.
    every: Duration,
    /// When it is next fed.
    next: Option<Instant>,
}

impl Watchdog {
    /// Open and arm the device if `command_line` asks, saying what was done.
    pub(crate) fn from_command_line(command_line: &str, say: &impl Fn(&str)) -> Watchdog {
        let Some(value) = command_line
            .split_whitespace()
            .find_map(|word| word.strip_prefix(OPTION))
        else {
            return Watchdog::default();
        };
        let seconds = match value.parse::<u32>() {
            Ok(seconds) if (1..=MAX_SECONDS).contains(&seconds) => seconds,
            _ => {
                say(&format!(
                    "{OPTION}{value} is not a number of seconds from 1 to {MAX_SECONDS}; the watchdog is not fed"
                ));
                return Watchdog::default();
            }
        };
        let file = match OpenOptions::new().write(true).open(DEVICE) {
            Ok(file) => file,
            Err(error) => {
                say(&format!(
                    "{DEVICE} could not be opened ({error}); the watchdog is not fed"
                ));
                return Watchdog::default();
            }
        };
        if let Err(error) = sys::watchdog_timeout(file.as_raw_fd(), seconds) {
            say(&format!(
                "{DEVICE}: its timeout could not be set to {seconds} s ({error}); fed at its own"
            ));
        }
        let every = Duration::from_millis(u64::from(seconds) * 1000 / 3);
        say(&format!(
            "feeding {DEVICE} every {} ms, its timeout {seconds} s",
            every.as_millis()
        ));
        let mut watchdog = Watchdog {
            file: Some(file),
            every,
            next: None,
        };
        watchdog.feed();
        watchdog
    }

    /// How long until the next feed is due, if init feeds.
    pub(crate) fn next_feed(&self) -> Option<Duration> {
        self.next
            .map(|at| at.saturating_duration_since(Instant::now()))
    }

    /// Feed it if a feed is due.
    pub(crate) fn feed_if_due(&mut self) {
        if self.next.is_some_and(|at| at <= Instant::now()) {
            self.feed();
        }
    }

    /// Feed it now, and say when next.
    fn feed(&mut self) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        // Any byte but `V`, which would let a close disarm it.
        let _ = file.write_all(b"1");
        self.next = Some(Instant::now() + self.every);
    }
}
