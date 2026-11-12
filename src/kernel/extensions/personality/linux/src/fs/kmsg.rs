//! `/dev/kmsg`, and the kernel log's lines as Linux's records.
//!
//! The kernel log is the console's bytes, with where each line starts, when
//! and at what priority (`console::log`). Read through `syslog(2)` a line is
//! `<6>[   12.345678] text` (`syscall::system::sys_syslog`); read through
//! `/dev/kmsg` it is one record a `read`, `6,42,12345678,-;text`, as Linux
//! v7.0's `devkmsg_read` gives it (`ferrix_linux_abi::kmsg` writes both).
//! journald, systemd and util-linux's `dmesg` read the second.
//!
//! What an open of `/dev/kmsg` does, after `kernel/printk/printk.c`:
//!
//! * It starts at the first line still kept, and each `read` takes the next
//!   whole line: `EINVAL` for a buffer it does not fit (the line is passed),
//!   `EAGAIN` under `O_NONBLOCK` when there is none, and otherwise a wait,
//!   which a signal ends. A line still being written is not one yet.
//! * A reader the log ran past gets `EPIPE` once, and goes on from the first
//!   line kept: the sign a reader has that lines were dropped (`dmesg` says
//!   how many).
//! * `lseek` takes offset 0 only: `SEEK_SET` to the first line kept,
//!   `SEEK_DATA` to the first after the last `syslog(2)` clear, `SEEK_END`
//!   past the last; `ESPIPE` for another offset, `EINVAL` for another whence.
//! * `poll` is readable while there is a line to read, with `POLLERR` and
//!   `POLLPRI` too when the reader's place is gone.
//! * A `write` is one record: up to [`kmsg::RECORD_MAX`] bytes, a leading
//!   `<N>` taken as its priority but never the kernel's facility, written to
//!   the console as a line of its own. Linux rate-limits these by default;
//!   this does not, as a program's own console writes are not limited either.
//! * Reading is privileged, as `syslog(2)` is here (`dmesg_restrict`):
//!   checked when the node is opened for reading ([`may_read`], from
//!   `devfs::attach_device`). Opening it write-only asks nothing; the node's
//!   mode, root's to write, does.
//!
//! The console never wakes a reader (`console::log`), so a reader waiting
//! for a line looks again every `READ_POLL_NANOS`, as `syslog(2)`'s does, and
//! [`Inode::poll_changes`] moves with every line begun.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use ferrix_linux_abi::kmsg;
use ferrix_vfs::initramfs::makedev;
use ferrix_vfs::{Errno, FileType, Inode, Metadata, Readiness, Result, Timespec, Whence};

use crate::console::log::{self, Line};
use crate::sync::SpinLock;
use crate::syscall::{credentials, process};

/// `/dev/kmsg`'s device number: major 1, minor 11.
pub(crate) const MINOR: u32 = 11;

/// Copy `line`'s text into `out`: its bytes up to its newline, as many as
/// `out` holds. Answers how many; none if the log went round the line while
/// it was copied.
fn text_of(line: &Line, out: &mut [u8]) -> usize {
    let want = usize::try_from(line.end.saturating_sub(line.start))
        .unwrap_or(usize::MAX)
        .min(out.len());
    let mut cursor = line.start;
    let room = out.get_mut(..want).unwrap_or_default();
    let read = log::read(&mut cursor, room);
    if read.lost != 0 {
        return 0;
    }
    let copied = room.get(..read.copied).unwrap_or_default();
    copied
        .iter()
        .position(|&byte| byte == b'\n')
        .unwrap_or(copied.len())
}

/// Write `line` into `buf` as `syslog(2)` reads it, `<6>[   12.345678] text`
/// and a newline, and answer the length: `buf` must hold
/// [`kmsg::MESSAGE_MAX`].
pub(crate) fn syslog_text(line: &Line, buf: &mut [u8]) -> usize {
    let Some(prefix) = kmsg::syslog_prefix(line.priority, line.usec, buf) else {
        return 0;
    };
    let end = prefix.saturating_add(kmsg::RECORD_MAX);
    let text = text_of(line, buf.get_mut(prefix..end).unwrap_or_default());
    let newline = prefix + text;
    match buf.get_mut(newline) {
        Some(slot) => {
            *slot = b'\n';
            newline + 1
        }
        None => 0,
    }
}

/// Whether the caller may open `/dev/kmsg` for reading: what `syslog(2)`
/// asks of a reader. A check with no process behind it may.
///
/// # Errors
///
/// `EPERM` for a caller without the privilege.
pub(crate) fn may_read() -> Result<()> {
    match process::current() {
        Some(caller) => credentials::require_privilege(&caller),
        None => Ok(()),
    }
}

/// One open of `/dev/kmsg`.
#[derive(Debug)]
pub(crate) struct KmsgFile {
    /// Its place and its buffers, behind one lock: the next line it reads.
    state: SpinLock<State>,
}

/// An open's place, and the room it formats a record in.
#[derive(Debug)]
struct State {
    /// The next line to read (`Line::seq`).
    seq: u64,
    /// A line's text, as the log has it.
    text: Vec<u8>,
    /// The record, as a reader is given it.
    record: Vec<u8>,
}

/// What a look for the next line found.
enum Next {
    /// A record, this long, in the state's buffer.
    Record(usize),
    /// The reader's place is gone: `EPIPE`, and it moved on.
    Lost,
    /// Nothing whole to read yet.
    Nothing,
}

impl KmsgFile {
    /// An open, at the first line kept.
    ///
    /// # Errors
    ///
    /// `ENOMEM` for its buffers.
    pub(crate) fn open() -> Result<Arc<KmsgFile>> {
        let text = crate::fallible::try_filled(0, kmsg::RECORD_MAX).map_err(|_| Errno::ENOMEM)?;
        let record =
            crate::fallible::try_filled(0, kmsg::MESSAGE_MAX).map_err(|_| Errno::ENOMEM)?;
        let state = State {
            seq: log::first_line(),
            text,
            record,
        };
        Ok(Arc::new(KmsgFile {
            state: SpinLock::new(state),
        }))
    }

    /// Take the next line into a record, if there is one, and copy it to
    /// `buf`; `EINVAL` when it does not fit, the line passed all the same.
    fn take(&self, buf: &mut [u8]) -> Option<Result<usize>> {
        let mut state = self.state.lock();
        let next = {
            let State { seq, text, record } = &mut *state;
            match log::line_from(*seq) {
                Some(line) if line.seq != *seq => {
                    *seq = line.seq;
                    Next::Lost
                }
                Some(line) => {
                    let len = text_of(&line, text);
                    let shown = text.get(..len).unwrap_or_default();
                    *seq = line.seq.wrapping_add(1);
                    let made =
                        kmsg::devkmsg_record(line.priority, line.seq, line.usec, shown, record);
                    Next::Record(made.unwrap_or(0))
                }
                None => Next::Nothing,
            }
        };
        match next {
            Next::Lost => Some(Err(Errno::EPIPE)),
            Next::Nothing => None,
            Next::Record(len) => {
                let made = state.record.get(..len).unwrap_or_default();
                Some(match buf.get_mut(..len) {
                    Some(room) => {
                        room.copy_from_slice(made);
                        Ok(len)
                    }
                    None => Err(Errno::EINVAL),
                })
            }
        }
    }
}

impl Inode for KmsgFile {
    fn metadata(&self) -> Metadata {
        let zero = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        Metadata {
            ino: 0,
            kind: FileType::CharDevice,
            permissions: 0o644,
            nlink: 1,
            uid: 0,
            gid: 0,
            size: 0,
            rdev: makedev(1, MINOR),
            blocks: 0,
            block_size: 4096,
            atime: zero,
            mtime: zero,
            ctime: zero,
        }
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.read_stream(buf, false)
    }

    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize> {
        let caller = process::current();
        loop {
            if let Some(answer) = self.take(buf) {
                return answer;
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            let Some(caller) = caller.as_ref() else {
                return Err(Errno::EAGAIN);
            };
            let deadline =
                crate::timer::now_nanos().saturating_add(crate::syscall::system::READ_POLL_NANOS);
            let _ = caller
                .signalled()
                .wait_until_deadline(|| caller.signal_pending(), deadline);
            if caller.signal_pending() {
                return Err(Errno::ERESTARTSYS);
            }
        }
    }

    fn write_at(&self, _offset: u64, data: &[u8], _append: bool) -> Result<(usize, u64)> {
        self.write_stream(data, false).map(|count| (count, 0))
    }

    fn write_stream(&self, data: &[u8], _nonblock: bool) -> Result<usize> {
        if data.len() > kmsg::RECORD_MAX {
            return Err(Errno::EINVAL);
        }
        let (priority, text) = kmsg::parse_write(data);
        crate::console::write_record(priority, text);
        Ok(data.len())
    }

    fn poll(&self) -> Readiness {
        let state = self.state.lock();
        let line = log::line_from(state.seq);
        let lost = line.is_some_and(|line| line.seq != state.seq);
        Readiness {
            readable: line.is_some(),
            writable: false,
            hangup: false,
            error: lost,
            priority: lost,
        }
    }

    fn poll_changes(&self) -> Option<u64> {
        Some(log::lines_begun())
    }

    fn seek_stream(&self, offset: i64, whence: Whence) -> Option<Result<u64>> {
        if offset != 0 {
            return Some(Err(Errno::ESPIPE));
        }
        let seq = match whence {
            Whence::Set => log::first_line(),
            Whence::Data => crate::syscall::system::cleared(),
            Whence::End => log::lines_begun(),
            Whence::Current | Whence::Hole => return Some(Err(Errno::EINVAL)),
        };
        self.state.lock().seq = seq;
        Some(Ok(0))
    }

    /// Linux's `/dev/kmsg` has no `splice_read`.
    fn splices_out(&self) -> bool {
        false
    }
}
