//! A native program's lines, each with the level its writer gives it.
//!
//! Every ring-3 driver and `devmgr` says what it does in lines, and lode
//! (`docs/LOGGING.md`) records each with a level. A line on standard error
//! arrives as the console's, `LOG_USER` at info, which says nothing, so lode
//! could only guess one from the words. Here a line is one record written
//! to `/dev/kmsg` as `<N>text`, `N` the level at [`LOG_DAEMON`]: the kernel
//! keeps both, and since no console line is ever of that facility lode
//! records the level as stated. A `/dev/kmsg` write also never waits for the
//! console's room, where a write to standard error from a task that may
//! sleep waits for the transmit ring to drain.
//!
//! `/dev/kmsg` is opened on the first line. Until it opens (before `/dev`
//! is there, or for a program not allowed to write it) lines go to standard
//! error as before, without a level, and each line asks again; after
//! [`MOST_TRIES`] refusals the program stops asking. [`open`] opens it at once,
//! for a program that says when its lines will not get there.
//!
//! ```text
//! ferrix_rt::warn!("intel: uapi refused {name} ({errno})");
//! ferrix_rt::log::line(Level::Info, format_args!("intel: GT up"));
//! ```

use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

pub use ferrix_linux_abi::kmsg::{LOG_DAEMON, Level};

use ferrix_linux_abi::errno::Errno;

use crate::linux::{self, numbers};

/// The most text one line keeps; the rest is cut.
pub const LINE_MAX: usize = 480;

/// How many times a program asks to open `/dev/kmsg` before it keeps to
/// standard error.
pub const MOST_TRIES: u32 = 64;

/// `/dev/kmsg`'s descriptor once open, else [`UNOPENED`].
static FD: AtomicUsize = AtomicUsize::new(UNOPENED);

/// [`FD`] before `/dev/kmsg` opened.
const UNOPENED: usize = usize::MAX;

/// Opens refused so far.
static TRIES: AtomicU32 = AtomicU32::new(0);

/// Say `arguments` at `level`: one record per line of it on `/dev/kmsg`, or
/// the text on standard error while that is not open.
pub fn line(level: Level, arguments: fmt::Arguments<'_>) {
    let mut text = Buffer::<LINE_MAX>::default();
    let _ = text.write_fmt(arguments);
    let Some(fd) = kmsg() else {
        let _ = text.write_str("\n");
        let _ = linux::write(2, text.as_bytes());
        return;
    };
    let priority = level.priority(LOG_DAEMON);
    for piece in text.as_bytes().split(|&byte| byte == b'\n') {
        if piece.is_empty() {
            continue;
        }
        let mut record = Buffer::<{ LINE_MAX + 8 }>::default();
        let _ = write!(record, "<{priority}>");
        record.push(piece);
        record.push(b"\n");
        let _ = linux::write(fd, record.as_bytes());
    }
}

/// Open `/dev/kmsg` for this program's lines now, rather than at its first
/// line, so that a program can say when its lines will not get there.
///
/// # Errors
///
/// What `openat` answered: `ENOENT` before `/dev` is there, `EACCES` for a
/// program that may not write it.
pub fn open() -> Result<(), Errno> {
    if FD.load(Ordering::Relaxed) != UNOPENED {
        return Ok(());
    }
    const AT_FDCWD: usize = (-100_isize).cast_unsigned();
    const O_WRONLY: usize = 1;
    const O_CLOEXEC: usize = 0o2_000_000;
    let path = b"/dev/kmsg\0";
    // SAFETY: `path` is NUL-terminated and borrowed for the call.
    let fd = unsafe {
        linux::call(
            numbers::OPENAT,
            [
                AT_FDCWD,
                path.as_ptr().addr(),
                O_WRONLY | O_CLOEXEC,
                0,
                0,
                0,
            ],
        )
    }
    .inspect_err(|_| {
        let _ = TRIES.fetch_add(1, Ordering::Relaxed);
    })?;
    // Another thread may have opened it first: keep its descriptor.
    if FD
        .compare_exchange(UNOPENED, fd, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        let _ = linux::close(fd);
    }
    Ok(())
}

/// `/dev/kmsg`'s descriptor, opening it if this program has not yet and
/// may still try.
fn kmsg() -> Option<usize> {
    if FD.load(Ordering::Relaxed) == UNOPENED && TRIES.load(Ordering::Relaxed) < MOST_TRIES {
        let _ = open();
    }
    let fd = FD.load(Ordering::Relaxed);
    (fd != UNOPENED).then_some(fd)
}

/// Text formatted into a fixed buffer and cut at its end.
struct Buffer<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> Default for Buffer<N> {
    fn default() -> Self {
        Buffer {
            bytes: [0; N],
            len: 0,
        }
    }
}

impl<const N: usize> Buffer<N> {
    fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if let Some(slot) = self.bytes.get_mut(self.len) {
                *slot = byte;
                self.len += 1;
            }
        }
    }

    fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

impl<const N: usize> fmt::Write for Buffer<N> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.push(text.as_bytes());
        Ok(())
    }
}

/// A line at [`Level::Err`]: something failed, and what it was for did not
/// happen.
#[macro_export]
macro_rules! err {
    ($($arguments:tt)*) => {
        $crate::log::line($crate::log::Level::Err, ::core::format_args!($($arguments)*))
    };
}

/// A line at [`Level::Warning`]: something refused, lost or worked around.
#[macro_export]
macro_rules! warn {
    ($($arguments:tt)*) => {
        $crate::log::line($crate::log::Level::Warning, ::core::format_args!($($arguments)*))
    };
}

/// A line at [`Level::Notice`]: normal, but worth seeing.
#[macro_export]
macro_rules! notice {
    ($($arguments:tt)*) => {
        $crate::log::line($crate::log::Level::Notice, ::core::format_args!($($arguments)*))
    };
}

/// A line at [`Level::Info`]: a step done, a device found.
#[macro_export]
macro_rules! info {
    ($($arguments:tt)*) => {
        $crate::log::line($crate::log::Level::Info, ::core::format_args!($($arguments)*))
    };
}

/// A line at [`Level::Debug`]: per-call and per-frame figures.
#[macro_export]
macro_rules! debug {
    ($($arguments:tt)*) => {
        $crate::log::line($crate::log::Level::Debug, ::core::format_args!($($arguments)*))
    };
}
