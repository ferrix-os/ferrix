//! The guest half of `cargo xtask test-watchdog`'s DISABLECARD case
//! (`docs/POSTMORTEM.md` §6.11, `L.lockup.7`).
//!
//! It opens `/dev/watchdog`, which arms it, sets a 5 s timeout with
//! `WDIOC_SETTIMEOUT`, so that a deadline armed by mistake would pass inside
//! the host's watch, disarms it with `WDIOC_SETOPTIONS` `WDIOS_DISABLECARD`,
//! then writes to it -- a feed, which
//! must not arm a disarmed deadline -- says `watchdog-test: disabled and
//! written`, and holds the file open, never closing it, so that only the
//! kernel's deadline decides what happens next. The host watches for a
//! report past the timeout.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::io::AsRawFd as _;
use std::process::ExitCode;

/// `WDIOC_SETOPTIONS` (`linux/watchdog.h`): `_IOR('W', 4, int)`.
const WDIOC_SETOPTIONS: libc::Ioctl = 0x8004_5704_u32 as libc::Ioctl;
/// `WDIOC_SETTIMEOUT`: `_IOWR('W', 6, int)`.
const WDIOC_SETTIMEOUT: libc::Ioctl = 0xC004_5706_u32 as libc::Ioctl;
/// The timeout it sets, in seconds: the host watches for twice that.
const TIMEOUT: libc::c_int = 5;
/// Its word that disarms.
const WDIOS_DISABLECARD: libc::c_int = 0x0001;

fn say(line: &str) {
    let _ = writeln!(std::io::stdout(), "{line}");
    let _ = std::io::stdout().flush();
}

fn main() -> ExitCode {
    let mut file = match OpenOptions::new().write(true).open("/dev/watchdog") {
        Ok(file) => file,
        Err(error) => {
            say(&format!("watchdog-test: FAILED open: {error}"));
            return ExitCode::FAILURE;
        }
    };
    let mut seconds: libc::c_int = TIMEOUT;
    // SAFETY: the descriptor is open for the call's length, and the
    // argument points at an `int` that lives across it, as the request
    // reads and writes one.
    let answer = unsafe { libc::ioctl(file.as_raw_fd(), WDIOC_SETTIMEOUT, &raw mut seconds) };
    if answer != 0 || seconds != TIMEOUT {
        say(&format!(
            "watchdog-test: FAILED SETTIMEOUT: {} ({seconds} s)",
            std::io::Error::last_os_error()
        ));
        return ExitCode::FAILURE;
    }
    let word: libc::c_int = WDIOS_DISABLECARD;
    // SAFETY: the descriptor is open for the call's length, and the
    // argument points at an `int` that lives across it, as the request
    // reads one.
    let answer = unsafe { libc::ioctl(file.as_raw_fd(), WDIOC_SETOPTIONS, &raw const word) };
    if answer != 0 {
        say(&format!(
            "watchdog-test: FAILED SETOPTIONS: {}",
            std::io::Error::last_os_error()
        ));
        return ExitCode::FAILURE;
    }
    if let Err(error) = file.write_all(b"x") {
        say(&format!("watchdog-test: FAILED write: {error}"));
        return ExitCode::FAILURE;
    }
    say("watchdog-test: disabled and written");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
