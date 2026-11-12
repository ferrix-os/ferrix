//! The console, as a file.
//!
//! Stage 7 answered `read` on descriptor 0 and `write` on 1 and 2 by name,
//! because there was nothing else a descriptor could be. With a descriptor
//! table, the console has to be something a table can hold: an [`Inode`], so
//! that `dup2(1, 5)`, `fcntl(1, F_GETFL)` and `/dev/console` all reach the same
//! object by the same route as a file on tmpfs does.
//!
//! # A character device with no position
//!
//! [`Inode::is_stream`] is true, so an offset is never passed to it and
//! `lseek` on it is `ESPIPE`, as on a Linux terminal. Its identity is Linux's
//! `/dev/console`: character device 5:1. A program that asks `fstat(0)` whether
//! it is talking to a terminal sees a character device, and `TCGETS` answers
//! it, through `crate::syscall::tty`.
//!
//! # The terminal behind it
//!
//! Reads, writes and `poll` go to `crate::fs::terminal`, whose line discipline
//! honours the settings a program sets. With the defaults it is what this file
//! used to do itself: nothing is returned until a whole line has been typed,
//! the line is echoed as it is typed, Enter ends it, Backspace edits it, and
//! Ctrl-D on an empty line is end of file.
//!
//! QEMU puts the host terminal in raw mode for `-serial stdio`, so nothing
//! echoes what is typed and Enter arrives as a carriage return. On Linux the
//! tty layer's `ECHO` and `ICRNL` fix both, between the keyboard and the
//! program, and here the terminal does the same.
//!
//! # Every open is a file of its own, and a hangup ends its reading
//!
//! The device is one inode, but each open of it is a [`ConsoleFile`] that
//! remembers when it was opened. `vhangup` (`syscall/tty.rs`) counts a hangup
//! of the console, and an open made before it may no longer read: a read is
//! `EIO`, `poll` reports a hangup, and every `ioctl` is `EIO`. It may still
//! write. That is what lets getty take the console back for a new login
//! from a program the last one left running, which could otherwise read the
//! next person's password (`docs/AUTH.md` §1), without cutting off the
//! services whose output goes to the console.
//!
//! Two kinds of open are not stamped. Init's own descriptors, which the
//! kernel makes, may always read: no hangup is meant for init. A native
//! process's, which the kernel also makes, may never read: anyone may start
//! one, and it would otherwise be a fresh open of the console, unrevoked,
//! for whoever asked.

use alloc::sync::Arc;
use core::any::Any;

use ferrix_sync::Once;
use ferrix_vfs::initramfs::makedev;
use ferrix_vfs::{
    Errno, FileSystem, FileType, Inode, Location, Metadata, Namespace, OpenFile, OpenFlags,
    Readiness, Timespec,
};

use crate::fs;
use crate::fs::terminal;

/// Linux's major number for `/dev/console` and the `/dev/tty*` family.
const MAJOR: u32 = 5;
/// `/dev/console`'s minor number within it.
const MINOR: u32 = 1;
/// The device number, in the encoding `stat` reports.
const RDEV: u64 = makedev(MAJOR, MINOR);

/// The block size `stat` reports: a page, as Linux does for a device node.
const BLOCK_SIZE: u32 = 4096;

/// The one console inode.
static CONSOLE: Once<Arc<dyn Inode>> = Once::new();

/// A namespace holding nothing but the console, for a console no `/dev` names.
static DETACHED: Once<Namespace> = Once::new();

/// The console device, one shared instance.
///
/// Shared rather than made per caller because it is one device: two inodes
/// would be two objects `stat` and `/proc/self/fd` could tell apart, for a
/// single serial port with a single pending line.
pub(crate) fn console_inode() -> Arc<dyn Inode> {
    Arc::clone(CONSOLE.call_once(|| Arc::new(Console)))
}

/// Whether an open of the console may read, and until when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reading {
    /// Whatever hangs the console up: init's own descriptors.
    Always,
    /// Until the console is next hung up after the hangup count given.
    Since(u64),
    /// Never: a native process's descriptors.
    Never,
}

/// An open of the console: see the module documentation.
#[derive(Debug)]
pub(crate) struct ConsoleFile {
    /// Whether it may read.
    reading: Reading,
}

impl ConsoleFile {
    /// Whether the open may no longer read.
    fn revoked(&self) -> bool {
        match self.reading {
            Reading::Always => false,
            Reading::Since(opened) => terminal::hung_up_since(Some(opened)),
            Reading::Never => true,
        }
    }

    /// The count a read stops at: see [`terminal::read`].
    fn opened(&self) -> Option<u64> {
        match self.reading {
            Reading::Always => None,
            Reading::Since(opened) => Some(opened),
            // A count no hangup is stamped with: always hung up since.
            Reading::Never => Some(u64::MAX),
        }
    }
}

/// A new open of the console, which may read until the console is next hung
/// up: what opening `/dev/console` or `/dev/tty` gives.
pub(crate) fn open_file() -> Arc<dyn Inode> {
    open_reading(Reading::Since(terminal::hangups()))
}

/// A new open of the console, reading as `reading` says.
fn open_reading(reading: Reading) -> Arc<dyn Inode> {
    Arc::new(ConsoleFile { reading })
}

/// Whether `io` -- what an open file reads and writes -- is the console:
/// the device itself, or an open of it.
pub(crate) fn is(io: &Arc<dyn Inode>) -> bool {
    Arc::ptr_eq(io, &console_inode()) || Arc::clone(io).into_any().is::<ConsoleFile>()
}

/// Whether `io` is an open of the console that may no longer read: one made
/// before the console's last hangup, or a native process's.
pub(crate) fn is_revoked(io: &Arc<dyn Inode>) -> bool {
    Arc::clone(io)
        .into_any()
        .downcast::<ConsoleFile>()
        .is_ok_and(|file| file.revoked())
}

/// Open the console for reading and writing, as a new process's descriptors
/// 0, 1 and 2 are.
///
/// Through `/dev/console` when the namespace has the console there, so that a
/// descriptor reports the name a person would recognise. When it does not --
/// no devfs mounted yet, or a `/dev/console` that is some other filesystem's
/// device node rather than this device -- through a location in a namespace of
/// its own. Linux starts `init` with no descriptors at all in that case; a
/// shell whose output goes nowhere is no use to anyone at a serial port, so
/// this keeps the console.
///
/// # Errors
///
/// Whatever [`OpenFile::new`] refuses, which for this inode is nothing, and
/// `ENOMEM` past the job's memory limit.
pub(crate) fn open_console(reading: Reading) -> Result<Arc<OpenFile>, Errno> {
    let flags = OpenFlags {
        read: true,
        write: true,
        ..OpenFlags::default()
    };
    let namespace = fs::namespace();
    let named = namespace.open(&namespace.context(), None, b"/dev/console", &flags, 0);
    let file = match named {
        Ok(file) if Arc::ptr_eq(file.inode(), &console_inode()) => file,
        _ => OpenFile::new(detached(), &flags)?,
    };
    file.with_io(open_reading(reading))
}

/// The console's place in a namespace nothing else is in.
fn detached() -> Location {
    DETACHED
        .call_once(|| {
            let alone = Arc::new(Alone {
                device: fs::anonymous_device(),
            });
            // No cache: there is exactly one dentry, and the mount holds it.
            // Its rename lock is never contended -- nothing can be renamed
            // here -- but it is made the way every namespace's is.
            Namespace::with_cache(alone, 0, Arc::new(crate::sync::SchedParker))
        })
        .root()
}

/// A filesystem whose root is the console, so that it has a location.
#[derive(Debug)]
struct Alone {
    /// The anonymous device `stat` reports for it.
    device: u64,
}

impl FileSystem for Alone {
    fn root(&self) -> Arc<dyn Inode> {
        console_inode()
    }

    fn name(&self) -> &'static str {
        "devtmpfs"
    }

    fn device(&self) -> u64 {
        self.device
    }
}

/// The serial console.
#[derive(Debug)]
struct Console;

impl Inode for Console {
    fn metadata(&self) -> Metadata {
        Metadata {
            // The device number doubles as the inode number: unique among
            // device nodes, and nothing a filesystem's counter will reach.
            ino: RDEV,
            kind: FileType::CharDevice,
            permissions: 0o600,
            nlink: 1,
            uid: 0,
            gid: 0,
            size: 0,
            rdev: RDEV,
            blocks: 0,
            block_size: BLOCK_SIZE,
            atime: Timespec::default(),
            mtime: Timespec::default(),
            ctime: Timespec::default(),
        }
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        true
    }

    /// Reports the number of bytes written, which for the console is all of
    /// them. A short count is legal in the ABI and every correct caller loops
    /// on it, but there is nothing here that can be short: a writer that
    /// finds the console's transmit ring full waits for room, as a Linux tty's
    /// does, rather than returning what fitted (`console::output`).
    fn write_at(
        &self,
        _offset: u64,
        data: &[u8],
        _append: bool,
    ) -> ferrix_vfs::Result<(usize, u64)> {
        terminal::write(data);
        Ok((data.len(), 0))
    }

    /// Refused: a read comes through an open of the console, a
    /// [`ConsoleFile`], which knows whether it may still read. Nothing
    /// reads the device itself but a node that stands for it and was never
    /// opened, which is no reader.
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> ferrix_vfs::Result<usize> {
        Err(Errno::EIO)
    }

    fn poll(&self) -> Readiness {
        terminal::poll()
    }

    fn poll_changes(&self) -> Option<u64> {
        Some(crate::console::input::waiters().wakes())
    }

    /// The input ring's queue, which its receive interrupt wakes: to be
    /// trusted only when there is one, since polled input wakes nobody.
    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        visit(fs::wake::lent(crate::console::input::waiters()));
        crate::console::input::interrupt_driven()
    }

    /// Every open is a file of its own, stamped with the hangup count.
    fn open(&self) -> ferrix_vfs::Result<Option<Arc<dyn Inode>>> {
        Ok(Some(open_file()))
    }
}

/// What an open of the console reads and writes through: the device's, until
/// it is revoked, and then only its writes.
impl Inode for ConsoleFile {
    fn metadata(&self) -> Metadata {
        Console.metadata()
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn write_at(&self, offset: u64, data: &[u8], append: bool) -> ferrix_vfs::Result<(usize, u64)> {
        Console.write_at(offset, data, append)
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> ferrix_vfs::Result<usize> {
        terminal::read(buf, false, self.opened())
    }

    /// A read through a descriptor, which knows whether it may wait.
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> ferrix_vfs::Result<usize> {
        terminal::read(buf, nonblock, self.opened())
    }

    /// Hung up, never readable, once revoked: as a Linux terminal's file is
    /// after `vhangup`, though here it may still be written.
    fn poll(&self) -> Readiness {
        if self.revoked() {
            return Readiness {
                readable: false,
                writable: true,
                hangup: true,
                error: false,
                priority: false,
            };
        }
        Console.poll()
    }

    fn poll_changes(&self) -> Option<u64> {
        Console.poll_changes()
    }

    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        Console.poll_queues(visit)
    }
}
