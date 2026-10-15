//! The boot log: what the loader said on this boot, kept in memory.
//!
//! A machine that boots with no picture and no serial port leaves nothing
//! to read afterwards -- unless the loader kept what it said. So every line
//! the console prints is also kept here (`docs/POSTMORTEM.md` §2, PM2), and
//! a loader started over the network sends the lot to the server it came
//! from before it leaves firmware (`netreport.rs`, `docs/NETBOOT.md` §6).
//! Writing it to the ESP as `FERRIX/BOOTLOG.TXT` is PM2's, still to come.
//!
//! **Nothing here can stop a boot.** The buffer stops taking bytes when it is
//! full, and says so at its end.

use core::cell::UnsafeCell;
use core::fmt::{self, Write};

/// The most a boot's log keeps: the first lines say the most, so a log that
/// runs over keeps its beginning and says it was cut.
const CAPACITY: usize = 32 * 1024;

/// What closes a log that ran over.
const CUT: &[u8] = b"\r\n[boot log full: the rest went to the console only]\r\n";

/// The log so far: its bytes, how many, and whether it ran over.
struct Log {
    bytes: [u8; CAPACITY],
    used: usize,
    full: bool,
}

/// The one log, in the loader's own data.
struct Shared(UnsafeCell<Log>);

// SAFETY: UEFI boot services are single-threaded and the loader starts no
// other CPU, so there is never a second accessor; and [`record`] is not
// reentered, since formatting into it calls nothing that prints.
unsafe impl Sync for Shared {}

static LOG: Shared = Shared(UnsafeCell::new(Log {
    bytes: [0; CAPACITY],
    used: 0,
    full: false,
}));

impl Write for Log {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for &byte in text.as_bytes() {
            // CRLF, so the text reads the same on any system.
            if byte == b'\n' {
                self.put(b'\r');
            }
            self.put(byte);
        }
        Ok(())
    }
}

impl Log {
    fn put(&mut self, byte: u8) {
        if self.full || self.used + CUT.len() >= CAPACITY {
            self.full = true;
            return;
        }
        if let Some(slot) = self.bytes.get_mut(self.used) {
            *slot = byte;
            self.used += 1;
        }
    }

    /// The log as it is sent: what was kept, and [`CUT`] after it when the
    /// rest was not.
    fn text(&mut self) -> &[u8] {
        if self.full {
            let end = self.used + CUT.len();
            if let Some(tail) = self.bytes.get_mut(self.used..end) {
                tail.copy_from_slice(CUT);
                self.used = end;
            }
            // Closed for good: [`Log::put`] takes nothing more once `full`.
        }
        self.bytes.get(..self.used).unwrap_or(&[])
    }
}

/// Keep formatted output for the log.
pub(crate) fn record(arguments: fmt::Arguments<'_>) {
    // SAFETY: single-threaded and not reentered, as documented on `Shared`.
    let log = unsafe { &mut *LOG.0.get() };
    let _ = log.write_fmt(arguments);
}

/// Close the log if it ran over and say how many bytes it holds. Bytes
/// recorded after this (lines printed while it is sent) are kept but not
/// counted, so a sender that took the length sends what it took.
pub(crate) fn len() -> usize {
    // SAFETY: single-threaded and not reentered, as documented on `Shared`;
    // the borrow ends before this returns.
    let log = unsafe { &mut *LOG.0.get() };
    log.text().len()
}

/// Copy the log's bytes from `from` into `into`; how many were copied. A
/// copy, so that the sender holds no reference into the log while anything
/// prints and records.
pub(crate) fn copy(from: usize, into: &mut [u8]) -> usize {
    // SAFETY: single-threaded and not reentered, as documented on `Shared`;
    // the borrow ends before this returns.
    let log = unsafe { &*LOG.0.get() };
    let kept = log.bytes.get(from..log.used).unwrap_or(&[]);
    let count = kept.len().min(into.len());
    if let (Some(to), Some(source)) = (into.get_mut(..count), kept.get(..count)) {
        to.copy_from_slice(source);
    }
    count
}
