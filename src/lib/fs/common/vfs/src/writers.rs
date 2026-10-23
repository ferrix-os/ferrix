//! Who may write a file, and who has said nobody may: Linux's `i_writecount`.
//!
//! A program's pages are its file's pages, mapped (`syscall/load.rs`). So a
//! file truncated or rewritten while it runs takes the code out from under
//! the program: on 2026-10-23 `cat > /bin/hyprix` while hyprix ran cut the
//! file to nothing and the compositor died of `SIGBUS` on its next
//! instruction fetch. Linux refuses that write with `ETXTBSY`, and this is its
//! mechanism, from `include/linux/fs.h` (v7.0):
//!
//! * a counter per inode, positive while that many open files may write it,
//!   negative while that many running programs (address spaces) say nobody
//!   may, zero for neither;
//! * opening a regular file for writing takes a write ([`get_write_access`]),
//!   and so does `truncate(2)` by path for as long as it runs; either fails
//!   with `ETXTBSY` while the count is negative;
//! * `execve` denies writes on the file it runs ([`deny_write_access`]), which
//!   fails with `ETXTBSY` while the count is positive: a file open for writing
//!   does not run. The program holds its denial until its address space goes,
//!   at exit or at the next `execve`, and a `fork` child takes one of its own.
//!
//! Only the program itself is held, as on Linux since 5.15: a shared library
//! or the dynamic linker may be rewritten while it is mapped (`MAP_DENYWRITE`
//! has been ignored since), and a script runs from its interpreter's file,
//! not its own.
//!
//! A filesystem takes part by answering [`Inode::writers`] with a counter of
//! its inode's. One that answers `None` -- a filesystem that cannot be
//! written, or whose files are not programs -- is never refused either way.
//! The counter must belong to the one inode object a file has while anyone
//! holds it, which tmpfs's nodes and btrfs's node cache both are.

use alloc::sync::Arc;
use core::fmt;
use core::sync::atomic::{AtomicIsize, Ordering};

use ferrix_linux_abi::errno::Errno;

use crate::Result;
use crate::node::Inode;

/// One open file's right to write its inode, given back when it is dropped:
/// Linux's `get_write_access` and `put_write_access`.
pub struct WriteAccess(Arc<dyn Inode>);

/// One running program's word that nobody may write its file, given back when
/// it is dropped: Linux's `deny_write_access` and `allow_write_access`.
///
/// A clone is another denial of the same file, which is what a `fork` child's
/// copy of its parent's program is (Linux's `dup_mm_exe_file`).
pub struct DenyWrite(Arc<dyn Inode>);

/// Take a write on `inode`: `None` for an inode that keeps no count.
///
/// # Errors
///
/// `ETXTBSY` while a running program denies writes on it.
pub fn get_write_access(inode: &Arc<dyn Inode>) -> Result<Option<WriteAccess>> {
    let Some(count) = inode.writers() else {
        return Ok(None);
    };
    // Linux's `atomic_inc_unless_negative`.
    let _ = count
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |now| {
            (now >= 0).then(|| now.saturating_add(1))
        })
        .map_err(|_| Errno::ETXTBSY)?;
    Ok(Some(WriteAccess(Arc::clone(inode))))
}

/// Deny writes on `inode` for as long as the answer is held: `None` for an
/// inode that keeps no count.
///
/// # Errors
///
/// `ETXTBSY` while it is open for writing.
pub fn deny_write_access(inode: &Arc<dyn Inode>) -> Result<Option<DenyWrite>> {
    let Some(count) = inode.writers() else {
        return Ok(None);
    };
    // Linux's `atomic_dec_unless_positive`.
    let _ = count
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |now| {
            (now <= 0).then(|| now.saturating_sub(1))
        })
        .map_err(|_| Errno::ETXTBSY)?;
    Ok(Some(DenyWrite(Arc::clone(inode))))
}

/// Move `inode`'s count by `by`, for a guard that already holds one of the
/// sign it moves towards, so it cannot cross zero.
fn adjust(inode: &Arc<dyn Inode>, by: isize) {
    if let Some(count) = inode.writers() {
        let _ = count.fetch_add(by, Ordering::AcqRel);
    }
}

impl Clone for WriteAccess {
    /// Another write on the same inode, which one already held keeps positive.
    fn clone(&self) -> WriteAccess {
        adjust(&self.0, 1);
        WriteAccess(Arc::clone(&self.0))
    }
}

impl Drop for WriteAccess {
    fn drop(&mut self) {
        adjust(&self.0, -1);
    }
}

impl Clone for DenyWrite {
    /// Another denial of the same file, which one already held keeps negative.
    fn clone(&self) -> DenyWrite {
        adjust(&self.0, -1);
        DenyWrite(Arc::clone(&self.0))
    }
}

impl Drop for DenyWrite {
    fn drop(&mut self) {
        adjust(&self.0, 1);
    }
}

impl fmt::Debug for WriteAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WriteAccess")
    }
}

impl fmt::Debug for DenyWrite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DenyWrite")
    }
}

/// The count's value, for a test to read.
#[cfg(test)]
pub(crate) fn count_of(inode: &Arc<dyn Inode>) -> Option<isize> {
    inode.writers().map(|count| count.load(Ordering::Acquire))
}

/// A counter for an inode that keeps one, at zero.
#[must_use]
pub const fn new_count() -> AtomicIsize {
    AtomicIsize::new(0)
}
