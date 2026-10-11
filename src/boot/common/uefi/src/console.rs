//! The loader's console.
//!
//! Firmware's text output takes NUL-terminated UCS-2, so this buffers a line at
//! a time, widens it, and hands it over. Under QEMU with `-display none`,
//! firmware routes that to the serial port, which is where the boot test is
//! watching.
//!
//! There is exactly one console and exactly one thread using it: UEFI boot
//! services are single-threaded by specification, and the loader has not
//! started any CPU but its own.

use core::cell::UnsafeCell;
use core::fmt::{self, Write};

use crate::uefi::protocols::SimpleTextOutput;

/// Characters buffered before a flush. One line of a boot message, plus room
/// for the carriage returns added on the way out.
const BUFFER: usize = 256;

/// The output protocol, once [`init`] has been called.
struct Console(UnsafeCell<Option<*mut SimpleTextOutput>>);

// SAFETY: UEFI boot services are single-threaded, and the loader starts no
// other CPU, so there is never a second accessor. The loader also never calls
// the console after `exit_boot_services`, where the pointer would stop being
// valid.
unsafe impl Sync for Console {}

static CONSOLE: Console = Console(UnsafeCell::new(None));

/// Point the console at firmware's text output protocol.
///
/// # Safety
///
/// `output` must be the live `con_out` from the system table, valid until
/// [`shutdown`] is called.
pub(crate) unsafe fn init(output: *mut SimpleTextOutput) {
    // SAFETY: single-threaded, as documented on the `Sync` impl above.
    unsafe { *CONSOLE.0.get() = Some(output) };
}

/// Forget the console, because firmware is about to stop existing.
///
/// Called immediately before `exit_boot_services`. Anything printed after this
/// goes nowhere rather than into a freed protocol.
pub(crate) fn shutdown() {
    // SAFETY: single-threaded, as documented on the `Sync` impl above.
    unsafe { *CONSOLE.0.get() = None };
}

/// A borrowed console that knows how to widen bytes.
struct Writer(*mut SimpleTextOutput);

impl Writer {
    /// Hand one NUL-terminated UCS-2 run to firmware.
    fn emit(&mut self, wide: &[u16]) {
        // SAFETY: `self.0` is the live protocol pointer `init` was given, and
        // the loader stops using the console before boot services end.
        let protocol = unsafe { &*self.0 };
        // SAFETY: `wide` is NUL terminated by the caller, so firmware reads
        // only inside it.
        let _ = unsafe { (protocol.output_string)(self.0, wide.as_ptr()) };
    }
}

impl Write for Writer {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut wide = [0u16; BUFFER];
        let mut used = 0;

        for character in text.chars() {
            // Firmware wants CRLF; a bare newline leaves the cursor in column
            // whatever-it-was and the log becomes a staircase.
            if character == '\n'
                && let Some(slot) = wide.get_mut(used)
            {
                *slot = u16::from(b'\r');
                used += 1;
            }

            // Anything outside the basic multilingual plane cannot be UCS-2, and
            // a loader has no business printing it.
            let unit = u32::from(character);
            let unit = if unit < 0x1_0000 {
                unit as u16
            } else {
                u16::from(b'?')
            };
            if let Some(slot) = wide.get_mut(used) {
                *slot = unit;
                used += 1;
            }

            // Leave room for the terminator and a possible carriage return.
            if used + 2 >= BUFFER {
                if let Some(slot) = wide.get_mut(used) {
                    *slot = 0;
                }
                self.emit(&wide);
                used = 0;
            }
        }

        if used != 0 {
            if let Some(slot) = wide.get_mut(used) {
                *slot = 0;
            }
            self.emit(&wide);
        }
        Ok(())
    }
}

/// Write formatted output to the console, if there is one, and to the boot
/// log (`bootlog.rs`) whether or not there is.
pub(crate) fn write(arguments: fmt::Arguments<'_>) {
    crate::bootlog::record(arguments);
    // SAFETY: single-threaded, as documented on the `Sync` impl above.
    let Some(output) = (unsafe { *CONSOLE.0.get() }) else {
        return;
    };
    let _ = Writer(output).write_fmt(arguments);
}

/// Print to the loader's console, with a newline.
macro_rules! println {
    () => { $crate::console::write(format_args!("\n")) };
    ($($argument:tt)*) => {
        $crate::console::write(format_args!("{}\n", format_args!($($argument)*)))
    };
}

pub(crate) use println;
