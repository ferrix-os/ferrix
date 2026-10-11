//! The boot log: what the loader said on this boot, kept on the ESP.
//!
//! A machine that boots with no picture and no serial port leaves nothing
//! to read after the reset into another system -- unless the loader wrote it
//! down. So every line the console prints is also kept here, and before
//! firmware is left (or the loader gives up) the lot goes to
//! `FERRIX/BOOTLOG.TXT` on the loader's own volume, the previous one moved
//! to `BOOTLOG.PRV` (`docs/POSTMORTEM.md` §2). The host's
//! `ferrix-postmortem.sh` prints both.
//!
//! **Nothing here can stop a boot.** The buffer stops taking bytes when it is
//! full; a file that cannot be read, deleted or written is said on the
//! console and forgotten. The loader writes no other file, and these two only
//! through firmware's file system on its own volume.

use core::cell::UnsafeCell;
use core::fmt::{self, Write};

use crate::console::println;
use crate::services::Services;
use crate::uefi::tables::MemoryType;

/// The log's file, and where the one before goes.
const LOG_PATH: &str = "/FERRIX/BOOTLOG.TXT";
const PREVIOUS_PATH: &str = "/FERRIX/BOOTLOG.PRV";

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
            // CRLF, so the file reads on any system that opens the ESP.
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
        if self.used + CUT.len() >= CAPACITY {
            self.full = true;
            return;
        }
        if let Some(slot) = self.bytes.get_mut(self.used) {
            *slot = byte;
            self.used += 1;
        }
    }

    /// The log as it goes to the file.
    fn text(&mut self) -> &[u8] {
        if self.full {
            let end = self.used + CUT.len();
            if let Some(tail) = self.bytes.get_mut(self.used..end) {
                tail.copy_from_slice(CUT);
            }
            self.used = end;
            self.full = false;
        }
        self.bytes.get(..self.used).unwrap_or(&[])
    }
}

/// Keep formatted output for the file.
pub(crate) fn record(arguments: fmt::Arguments<'_>) {
    // SAFETY: single-threaded and not reentered, as documented on `Shared`.
    let log = unsafe { &mut *LOG.0.get() };
    let _ = log.write_fmt(arguments);
}

/// Say who made the firmware, for a log read on another day.
pub(crate) fn firmware(services: &Services) {
    let mut vendor = [0u8; 64];
    let (len, revision) = services.firmware_vendor(&mut vendor);
    let name = vendor
        .get(..len)
        .and_then(|bytes| core::str::from_utf8(bytes).ok())
        .unwrap_or("?");
    println!("  firmware {name} revision {revision:#x}");
}

/// Write the log to the ESP, the previous one kept beside it. Whatever goes
/// wrong is said and ignored.
pub(crate) fn save(services: &Services) {
    match services.read_file(LOG_PATH, MemoryType::LOADER_DATA) {
        Ok((file, len)) => {
            // SAFETY: `read_file` filled `len` bytes of its own allocation,
            // identity mapped under boot services, and nothing else holds it.
            let old = unsafe { core::slice::from_raw_parts(file.address as *const u8, len as usize) };
            if let Err(error) = services.replace_file(PREVIOUS_PATH, old) {
                println!("  bootlog  {PREVIOUS_PATH} not written: {error}");
            }
            let _ = services.free("freeing the previous boot log", file);
        }
        Err(error) if error.status == Some(crate::uefi::Status::NOT_FOUND) => {}
        Err(error) => println!("  bootlog  {LOG_PATH} not read: {error}"),
    }
    // SAFETY: single-threaded and not reentered, as documented on `Shared`;
    // nothing prints while `text` is borrowed.
    let log = unsafe { &mut *LOG.0.get() };
    let text = log.text();
    if let Err(error) = services.replace_file(LOG_PATH, text) {
        println!("  bootlog  {LOG_PATH} not written: {error}");
    }
}
