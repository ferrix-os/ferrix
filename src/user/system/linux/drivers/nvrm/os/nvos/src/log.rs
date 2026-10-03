//! Lines on standard error, which in nvrm is the console (`docs/NVIDIA.md`
//! §4.2: "logging, to the kernel log through the driver log channel"; the
//! channel waits for N1e, so standard error stands in, as it does for
//! every line devmgr and nvrm print today).
//!
//! RM's own lines arrive through `nv_printf` and `out_string`, which C
//! formats (`glue/printf.c`) and hands to [`nvos_write_line`]. This layer's
//! lines start `nvos: `.

use core::ffi::c_char;
use core::fmt::{self, Write};

use crate::libc;

/// The longest line, longer ones cut.
const LINE: usize = 512;

/// A line being built on the stack.
struct Line {
    /// The bytes.
    bytes: [u8; LINE],
    /// How many are used.
    used: usize,
}

impl Write for Line {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for &byte in text.as_bytes() {
            if self.used >= LINE - 1 {
                break;
            }
            if let Some(slot) = self.bytes.get_mut(self.used) {
                *slot = byte;
                self.used += 1;
            }
        }
        Ok(())
    }
}

/// Write `bytes` to standard error in one call, as far as it takes.
fn write_all(bytes: &[u8]) {
    let mut rest = bytes;
    while !rest.is_empty() {
        // SAFETY: `rest` is a live slice of `rest.len()` bytes.
        let wrote = unsafe { libc::write(2, rest.as_ptr().cast(), rest.len()) };
        let Ok(wrote) = usize::try_from(wrote) else {
            return;
        };
        if wrote == 0 {
            return;
        }
        rest = rest.get(wrote..).unwrap_or_default();
    }
}

/// Print `nvos: <args>` and a newline, in one write.
pub(crate) fn say_line(args: fmt::Arguments<'_>) {
    let mut line = Line {
        bytes: [0; LINE],
        used: 0,
    };
    let _ = line.write_str("nvos: ");
    let _ = line.write_fmt(args);
    if let Some(slot) = line.bytes.get_mut(line.used) {
        *slot = b'\n';
        line.used += 1;
    }
    write_all(line.bytes.get(..line.used).unwrap_or_default());
}

/// `say!("...", ...)`: one `nvos:` line on the console.
macro_rules! say {
    ($($arg:tt)*) => {
        $crate::log::say_line(format_args!($($arg)*))
    };
}
pub(crate) use say;

/// Write a NUL-terminated line, as C formatted it, to the console.
///
/// # Safety
///
/// `text` is NULL or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_write_line(text: *const c_char) {
    if text.is_null() {
        return;
    }
    // SAFETY: the caller vouches for the string.
    let bytes = unsafe { core::ffi::CStr::from_ptr(text) }.to_bytes();
    write_all(bytes);
}

/// A string from C, for a line: what fits, lossily.
///
/// # Safety
///
/// `text` is NULL or a NUL-terminated string that outlives the result.
pub(crate) unsafe fn c_str<'a>(text: *const c_char) -> &'a str {
    if text.is_null() {
        return "(null)";
    }
    // SAFETY: the caller vouches for the string.
    let bytes = unsafe { core::ffi::CStr::from_ptr(text) }.to_bytes();
    core::str::from_utf8(bytes).unwrap_or("(not UTF-8)")
}
