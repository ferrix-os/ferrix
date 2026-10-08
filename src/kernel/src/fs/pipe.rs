//! Pipes, and the named pipes that are pipes found by a path.
//!
//! `src/lib/fs/vfs`'s [`PipeBuffer`] is the queue and the rules at its edges, as
//! values: a read that would wait says so rather than waiting. This is the
//! half that waits. A [`Pipe`] is that buffer behind a lock with a wait queue
//! for each direction, and each end is an [`Inode`] an open file can hold.
//!
//! # Never waiting with the buffer locked
//!
//! Every operation takes the lock, asks the buffer, and lets go before it
//! decides anything: the outcome is bound to a variable first, so the guard is
//! gone before a `match` on it can sleep. A writer asleep on a full pipe with
//! the lock held would stop the very reader that was about to make room. The
//! wait looks at the buffer again, under the lock, after it has joined the
//! queue, which is the order `sched::wait` needs to lose no wake-up.
//!
//! # Ends are counted by their inodes
//!
//! An end counts itself into the buffer when it is made and out when it is
//! dropped. The open file description holds its end, and `dup` and `fork`
//! share the description, so the last reference to an end goes exactly when
//! the last descriptor that could use it closes -- which is when a reader must
//! see end of file and a writer `EPIPE`.
//!
//! # `EPIPE` and `SIGPIPE`
//!
//! Linux raises `SIGPIPE` on a write with no reader left, and most programs
//! die of it before they see the error; one that ignores or handles the signal
//! gets `EPIPE`. [`End::write_stream`] does both.
//!
//! # Named pipes
//!
//! A FIFO is a node in a filesystem, and opening it gives an end of the pipe
//! every other opener of that node shares. The node cannot hold the pipe --
//! tmpfs is `src/lib/fs/vfs`, which has no wait queues -- so the pipe is found in a
//! table here, keyed by the node's device and inode number and held weakly,
//! and [`attach_fifo`] swaps an end of it into the open file `openat` made.
//!
//! That is one line in `openat` and one method on `OpenFile`. The other two
//! ways in were worse: a callback in `Namespace::open` would put the first
//! kernel hook into a crate that has none, and telling `Inode::open` the flags
//! would change a method every filesystem implements, for the one kind of node
//! that needs them.
//!
//! Opening waits as Linux's does: a reader until a writer has opened, a writer
//! until a reader has -- or `ENXIO` at once under `O_NONBLOCK` -- and an open
//! for both for nothing. What an opener waits for is a partner *having opened*
//! since it began, counted, rather than one being open when it looks, so that
//! a writer which opens, writes and closes before the reader looks has still
//! been there, and the reader reads what it wrote.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use core::any::Any;
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_kmem::{Charge, arc_footprint};
use ferrix_sync::Once;

use crate::sync::SpinLock;
use ferrix_vfs::path::NAME_MAX;
use ferrix_vfs::pipe::{PIPE_CAPACITY, PIPEFS_MAGIC, PipeBuffer, ReadOutcome, WriteOutcome};
use ferrix_vfs::{
    Errno, FileSystem, FileType, Inode, Location, Metadata, OpenFile, OpenFlags, Readiness, StatFs,
    Timespec,
};

use crate::fs;
use crate::sched::WaitQueue;
use crate::syscall::process::{self, Process};

/// The deadline a pipe's wait passes: none. A pipe waits for something to
/// happen, and the process being killed is one of those things.
const FOREVER: u64 = u64::MAX;

/// The block size `stat` reports for a pipe: a page, as Linux reports.
const BLOCK_SIZE: u32 = 4096;

/// One pipe: the buffer, and who waits on it.
pub(crate) struct Pipe {
    buffer: SpinLock<PipeBuffer>,
    /// Woken when a read may no longer wait: bytes arrived, an end opened, or
    /// the last writer left.
    readable: Arc<WaitQueue>,
    /// Woken when a write may no longer wait: room was made, an end opened, or
    /// the last reader left.
    writable: Arc<WaitQueue>,
    /// Read ends ever opened. Changed only under `buffer`'s lock, so a FIFO
    /// opener can read it consistently with the count of ends open.
    readers_opened: AtomicU64,
    /// Write ends ever opened, likewise.
    writers_opened: AtomicU64,
    /// The heap the pipe holds but its buffer, charged to the job that made
    /// it; the buffer charges its own growth to the same job (F-37).
    _charge: Charge,
}

impl fmt::Debug for Pipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pipe")
            .field("readers_opened", &self.readers_opened)
            .field("writers_opened", &self.writers_opened)
            .finish_non_exhaustive()
    }
}

impl Pipe {
    /// An empty pipe with no ends, charged to the running task's job: a
    /// packet pipe when `packets`, as `pipe2(O_DIRECT)` makes one (see
    /// `ferrix_vfs::pipe`), and a byte stream otherwise.
    ///
    /// # Errors
    ///
    /// `ENOMEM` past the job's memory limit.
    fn new(packets: bool) -> Result<Arc<Pipe>, Errno> {
        let charge = Charge::bytes(
            arc_footprint::<Pipe>().saturating_add(arc_footprint::<WaitQueue>().saturating_mul(2)),
        )
        .map_err(|_| Errno::ENOMEM)?;
        let buffer = if packets {
            PipeBuffer::packets(PIPE_CAPACITY)
        } else {
            PipeBuffer::new(PIPE_CAPACITY)
        };
        Ok(Arc::new(Pipe {
            buffer: SpinLock::new(buffer),
            readable: Arc::new(WaitQueue::new()),
            writable: Arc::new(WaitQueue::new()),
            readers_opened: AtomicU64::new(0),
            writers_opened: AtomicU64::new(0),
            _charge: charge,
        }))
    }

    /// Wake both directions: an end opened or closed, which can end a wait
    /// on either side.
    fn wake_both(&self) {
        self.readable.wake_all();
        self.writable.wake_all();
    }
}

/// Whether the process a wait is on behalf of has been killed, or has a signal
/// to take, either of which ends the wait with `EINTR`. The boot self-check
/// calls with no process, and is never killed.
fn killed(caller: Option<&Arc<Process>>) -> bool {
    caller.is_some_and(|process| process.signal_pending())
}

/// A count so far, or `errno` if nothing was done: a transfer that stopped
/// part-way reports what it managed, as `read` and `write` must.
fn partial(done: usize, errno: Errno) -> ferrix_vfs::Result<usize> {
    if done > 0 { Ok(done) } else { Err(errno) }
}

/// One end of a pipe, as the inode an open file reads or writes through.
#[derive(Debug)]
struct End {
    pipe: Arc<Pipe>,
    reads: bool,
    writes: bool,
    /// What `stat` reports through this end.
    metadata: Metadata,
    /// Its heap, charged to the job that opened it (F-37).
    _charge: Charge,
}

impl End {
    /// Make an end, counting it into the pipe, and wake anyone whose open was
    /// waiting for one.
    ///
    /// # Errors
    ///
    /// `ENOMEM` past the job's memory limit, with nothing counted.
    fn open(
        pipe: &Arc<Pipe>,
        reads: bool,
        writes: bool,
        metadata: Metadata,
    ) -> Result<Arc<End>, Errno> {
        let charge = Charge::bytes(arc_footprint::<End>()).map_err(|_| Errno::ENOMEM)?;
        {
            let mut buffer = pipe.buffer.lock();
            if reads {
                buffer.open_reader();
                let _ = pipe.readers_opened.fetch_add(1, Ordering::Relaxed);
            }
            if writes {
                buffer.open_writer();
                let _ = pipe.writers_opened.fetch_add(1, Ordering::Relaxed);
            }
        }
        pipe.wake_both();
        Ok(Arc::new(End {
            pipe: Arc::clone(pipe),
            reads,
            writes,
            metadata,
            _charge: charge,
        }))
    }

    /// Bytes were taken out: a writer waiting for room may now have it.
    fn took(&self, count: usize) -> usize {
        if count > 0 {
            self.pipe.writable.wake_all();
        }
        count
    }

    /// Sleep until a read would not wait. `EINTR` if the caller is killed
    /// first, which is what ends a `cat` blocked on a pipe nobody writes to.
    ///
    /// # Trusting the queue
    ///
    /// The wait files no recheck deadline (`WaitQueue::wait_trusting`): each
    /// thing that can end it wakes it, so a block is a listing and a switch,
    /// not also a sleeper filed in the run queue and taken out again
    /// (po10-pipe P3, the consultant's W1). `ready` reads the buffer under
    /// its lock, and `killed` the caller's signal state under the signal
    /// lock. Every waker, and what orders its change before the wait's last
    /// look (W4):
    ///
    /// 1. **Bytes written** (`write_stream`): the buffer changed under its
    ///    lock, then `readable` woken. The lock orders the change before a
    ///    look that comes after it; a look before it finds the task listed.
    /// 2. **Room made** (`took`, for [`End::wait_to_write`]; and a splice out
    ///    of the pipe): the same, on `writable`.
    /// 3. **An end opened or closed**, the last writer or reader included
    ///    (`End::open`, `Drop for End`): the count changed under the buffer's
    ///    lock, then `wake_both`.
    /// 4. **Bytes put back** (`unread_stream`): under the lock, then
    ///    `readable`.
    /// 5. **A signal** to the thread or the process: recorded under the
    ///    signal lock, then the thread woken through `sched::work`'s
    ///    `wake_posted`, whose `SeqCst` fence pairs with the one
    ///    `wait_trusting` makes after the task is marked blocked.
    /// 6. **A kill**, `exit_group`, `cgroup.kill` or the OOM killer:
    ///    `end_record` stores the end, fences, and posts `END` to the tasks.
    /// 7. **Another thread's `execve`**, 8. **a stop** and 9. **a freeze**:
    ///    stored, then `wake_other_tasks`, which fences and posts. Each ends
    ///    the wait through `killed`; a thaw or `SIGCONT` need not wake it,
    ///    because the waiter has already returned `ERESTARTSYS` and parks on
    ///    its way out.
    /// 10. A waiter there was no memory to list sleeps to the recheck by
    ///     `wait_sliced`'s own rule (F-23).
    ///
    /// A waker added later belongs in this list, with its order, or the wait
    /// it misses never ends: there is no recheck to find it.
    fn wait_to_read(&self) -> ferrix_vfs::Result<()> {
        let caller = process::current();
        let _ = self.pipe.readable.wait_trusting(|| {
            let ready = self.pipe.buffer.lock().can_read();
            ready || killed(caller.as_ref())
        });
        if killed(caller.as_ref()) {
            // A restart code, not `EINTR`: a pipe read restarts under
            // `SA_RESTART`. A read that already moved bytes returns the count
            // instead (see `partial`); only one that moved none is restarted.
            return Err(Errno::ERESTARTSYS);
        }
        Ok(())
    }

    /// Sleep until a write of `len` bytes would not wait, or `EINTR`. Trusts
    /// its queue as [`End::wait_to_read`] does, by the same list of wakers
    /// read for `writable`: room made, an end opened or closed, and the
    /// caller's signals, kills, stops and freezes.
    fn wait_to_write(&self, len: usize) -> ferrix_vfs::Result<()> {
        let caller = process::current();
        let _ = self.pipe.writable.wait_trusting(|| {
            let ready = self.pipe.buffer.lock().can_write(len);
            ready || killed(caller.as_ref())
        });
        if killed(caller.as_ref()) {
            // A restart code, not `EINTR`, for the same reason a read gives one.
            return Err(Errno::ERESTARTSYS);
        }
        Ok(())
    }
}

impl Drop for End {
    fn drop(&mut self) {
        {
            let mut buffer = self.pipe.buffer.lock();
            if self.reads {
                buffer.close_reader();
            }
            if self.writes {
                buffer.close_writer();
            }
        }
        // A reader waiting on the last writer now reads end of file, and a
        // writer waiting on the last reader now gets `EPIPE`.
        self.pipe.wake_both();
    }
}

impl Inode for End {
    fn metadata(&self) -> Metadata {
        self.metadata
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        visit(fs::wake::shared(&self.pipe.readable));
        visit(fs::wake::shared(&self.pipe.writable));
        true
    }

    fn poll_changes(&self) -> Option<u64> {
        Some(
            self.pipe
                .readable
                .wakes()
                .wrapping_add(self.pipe.writable.wakes()),
        )
    }

    fn poll(&self) -> Readiness {
        let buffer = self.pipe.buffer.lock();
        let read = buffer.read_readiness();
        let write = buffer.write_readiness();
        Readiness {
            readable: self.reads && read.readable,
            writable: self.writes && write.writable,
            // An end open for both is its own partner, and never hangs up.
            hangup: self.reads && !self.writes && read.hangup,
            error: self.writes && !self.reads && write.error,
            priority: false,
        }
    }

    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> ferrix_vfs::Result<usize> {
        loop {
            // Bound first, so the guard is gone before anything below waits.
            let outcome = self.pipe.buffer.lock().read(buf);
            match outcome {
                ReadOutcome::Read(count) => return Ok(self.took(count)),
                ReadOutcome::EndOfFile => return Ok(0),
                ReadOutcome::WouldBlock if nonblock => return Err(Errno::EAGAIN),
                ReadOutcome::WouldBlock => self.wait_to_read()?,
            }
        }
    }

    /// A read takes everything the pipe has, as Linux's `pipe_read` does,
    /// not one page of it; see [`Inode::fills_reads`].
    fn fills_reads(&self) -> bool {
        true
    }

    /// Put back bytes a read took and could not copy out to the reader, as
    /// Linux leaves them in the pipe: a `read` or `readv` into a bad buffer
    /// is `EFAULT` and the pipe still holds everything it held. Another
    /// reader of the same pipe taking bytes between the read and this is
    /// the one order this cannot keep, where Linux's pipe lock would.
    fn unread_stream(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.pipe.buffer.lock().unread(bytes);
        self.pipe.readable.wake_all();
    }

    /// Queue all of `data`, waiting for room as often as it takes, unless
    /// `nonblock`: then as much as fits now, or `EAGAIN` for none. A write
    /// with no reader left is `EPIPE`, or the count queued before the reader
    /// went.
    fn write_stream(&self, data: &[u8], nonblock: bool) -> ferrix_vfs::Result<usize> {
        // An empty write succeeds without looking for a reader, as on Linux.
        if data.is_empty() {
            return Ok(0);
        }
        let mut done = 0;
        while done < data.len() {
            let rest = data.get(done..).unwrap_or_default();
            // Bound first, so the guard is gone before anything below waits.
            let outcome = self.pipe.buffer.lock().write(rest);
            let refusal = match outcome {
                WriteOutcome::Wrote(count) => {
                    done += count;
                    self.pipe.readable.wake_all();
                    continue;
                }
                WriteOutcome::Broken => {
                    // `SIGPIPE` to the writer as well, as Linux sends it; a
                    // writer that survives it still gets `EPIPE`.
                    crate::syscall::kill::send_to_current(ferrix_linux_abi::types::SIGPIPE);
                    Errno::EPIPE
                }
                WriteOutcome::NoMemory => Errno::ENOMEM,
                WriteOutcome::WouldBlock if nonblock => Errno::EAGAIN,
                WriteOutcome::WouldBlock => match self.wait_to_write(rest.len()) {
                    Ok(()) => continue,
                    Err(errno) => errno,
                },
            };
            return partial(done, refusal);
        }
        Ok(done)
    }
}

// ---------------------------------------------------------------------------
// Anonymous pipes
// ---------------------------------------------------------------------------

/// The filesystem every anonymous pipe is on: Linux's `pipefs`, which a
/// program only ever meets as `fstat`'s device and `fstatfs`'s magic number.
#[derive(Debug)]
struct PipeFs {
    device: u64,
    root: Arc<dyn Inode>,
    /// The next pipe's inode number. Both ends of one pipe share one, as on
    /// Linux, which is how a program can tell two descriptors are one pipe.
    next_ino: AtomicU64,
}

/// The one pipefs.
static PIPEFS: Once<Arc<PipeFs>> = Once::new();

/// The one pipefs, made on first use.
fn pipefs() -> &'static Arc<PipeFs> {
    PIPEFS.call_once(|| {
        Arc::new(PipeFs {
            device: fs::anonymous_device(),
            root: Arc::new(PipeRoot),
            next_ino: AtomicU64::new(2),
        })
    })
}

impl FileSystem for PipeFs {
    fn root(&self) -> Arc<dyn Inode> {
        Arc::clone(&self.root)
    }

    fn name(&self) -> &'static str {
        "pipefs"
    }

    fn device(&self) -> u64 {
        self.device
    }

    fn statfs(&self) -> StatFs {
        StatFs {
            magic: PIPEFS_MAGIC,
            block_size: u64::from(BLOCK_SIZE),
            name_max: NAME_MAX as u64,
            ..StatFs::default()
        }
    }
}

/// pipefs's root directory, which is empty and which nothing can reach: every
/// pipe's location is detached. It exists because a filesystem has a root.
#[derive(Debug)]
struct PipeRoot;

impl Inode for PipeRoot {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 1,
            kind: FileType::Directory,
            permissions: 0o700,
            nlink: 2,
            uid: 0,
            gid: 0,
            size: 0,
            rdev: 0,
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
}

/// A new pipe, as the two open files `pipe2` installs: the read end, then the
/// write end, each non-blocking if asked, and owned by `owner`, the creator's
/// filesystem user and group ids.
///
/// # Errors
///
/// Whatever [`OpenFile::new`] refuses, which for a pipe end is nothing.
pub(crate) fn new_pipe(
    nonblock: bool,
    owner: (u32, u32),
) -> Result<(Arc<OpenFile>, Arc<OpenFile>), Errno> {
    new_pipe_of(nonblock, false, owner)
}

/// [`new_pipe`], a packet pipe when `packets`: what `pipe2(O_DIRECT)` makes.
///
/// # Errors
///
/// As [`new_pipe`].
pub(crate) fn new_pipe_of(
    nonblock: bool,
    packets: bool,
    (uid, gid): (u32, u32),
) -> Result<(Arc<OpenFile>, Arc<OpenFile>), Errno> {
    let pipefs = pipefs();
    let ino = pipefs.next_ino.fetch_add(1, Ordering::Relaxed);
    let now = fs::clock().now();
    let metadata = Metadata {
        ino,
        kind: FileType::Fifo,
        permissions: 0o600,
        nlink: 1,
        uid,
        gid,
        size: 0,
        rdev: 0,
        blocks: 0,
        block_size: BLOCK_SIZE,
        atime: now,
        mtime: now,
        ctime: now,
    };
    // What `/proc/self/fd` will show, in Linux's spelling.
    let name = format!("pipe:[{ino}]");
    let pipe = Pipe::new(packets)?;
    let reader = open_end(
        End::open(&pipe, true, false, metadata)?,
        name.as_bytes(),
        nonblock,
    )?;
    let writer = open_end(
        End::open(&pipe, false, true, metadata)?,
        name.as_bytes(),
        nonblock,
    )?;
    Ok((reader, writer))
}

/// An open file on `end`, at a location of its own on pipefs.
fn open_end(end: Arc<End>, name: &[u8], nonblock: bool) -> Result<Arc<OpenFile>, Errno> {
    let flags = OpenFlags {
        read: end.reads,
        write: end.writes,
        nonblock,
        ..OpenFlags::default()
    };
    let pipefs: Arc<PipeFs> = Arc::clone(pipefs());
    let parker = Arc::clone(super::namespace().parker());
    OpenFile::new(Location::detached(pipefs, end, name, parker)?, &flags)
}

// ---------------------------------------------------------------------------
// Named pipes
// ---------------------------------------------------------------------------

/// Every FIFO somebody has open, by the node's device and inode number.
///
/// Weak, so that a FIFO nobody has open holds no pipe, and a pipe goes away
/// with its last end as an anonymous one does.
static FIFOS: SpinLock<BTreeMap<(u64, u64), Weak<Pipe>>> = SpinLock::new(BTreeMap::new());

/// The pipe behind the FIFO `key` names, made if nobody has it open.
///
/// # Errors
///
/// `ENOMEM` past the job's memory limit.
fn shared_pipe(key: (u64, u64)) -> Result<Arc<Pipe>, Errno> {
    let mut table = FIFOS.lock();
    if let Some(pipe) = table.get(&key).and_then(Weak::upgrade) {
        return Ok(pipe);
    }
    // Forget the pipes nobody holds any more, so the table is as large as the
    // FIFOs open now rather than every FIFO ever opened. Only weak references
    // are dropped here, so nothing is freed under the lock.
    table.retain(|_, pipe| pipe.strong_count() > 0);
    let pipe = Pipe::new(false)?;
    let _ = table.insert(key, Arc::downgrade(&pipe));
    Ok(pipe)
}

/// An open file of a named pipe, made into an end of the pipe every opener of
/// that node shares; or of an anonymous pipe's end, reached through
/// `/proc/<pid>/fd`, made into a new end of that pipe. Anything else -- and a
/// FIFO opened with `O_PATH`, which is a handle on the name -- comes back as
/// it was.
///
/// # Errors
///
/// `ENXIO` for a non-blocking open for writing only with no reader, `EINVAL`
/// for an open for neither reading nor writing, which Linux refuses on a FIFO,
/// and `EINTR` if the opener is killed while it waits for its partner.
pub(crate) fn attach_fifo(file: Arc<OpenFile>) -> Result<Arc<OpenFile>, Errno> {
    if file.kind() != FileType::Fifo || file.is_path() {
        return Ok(file);
    }
    let (reads, writes) = (file.readable(), file.writable());
    if !reads && !writes {
        return Err(Errno::EINVAL);
    }
    let nonblock = file.status().nonblock;
    let metadata = file.inode().metadata();
    // An anonymous pipe opened again through `/proc/<pid>/fd/<n>`, as bash's
    // `<(...)` and `>(...)` open `/dev/fd/63`: the walk arrived at the end
    // the descriptor holds, and the new end joins that end's pipe, as Linux's
    // `fifo_open` finds the pipe on the inode.
    if let Some(end) = end_of(&file) {
        let pipe = Arc::clone(&end.pipe);
        // The open file the walk made holds the old end as its inode, which
        // would count that end in for as long as the new one is open; the
        // new end gets a location of its own, as `pipe2`'s do.
        drop((end, file));
        let end = new_end(&pipe, reads, writes, nonblock, metadata)?;
        return open_end(end, format!("pipe:[{}]", metadata.ino).as_bytes(), nonblock);
    }
    let key = (file.location().mount.filesystem().device(), metadata.ino);
    let pipe = shared_pipe(key)?;
    file.with_io(new_end(&pipe, reads, writes, nonblock, metadata)?)
}

/// A new end on `pipe`, reading or writing or both, once `open(2)`'s rules
/// for a FIFO allow it: see [`attach_fifo`].
fn new_end(
    pipe: &Arc<Pipe>,
    reads: bool,
    writes: bool,
    nonblock: bool,
    metadata: Metadata,
) -> Result<Arc<End>, Errno> {
    let no_reader = pipe.buffer.lock().readers() == 0;
    if writes && !reads && nonblock && no_reader {
        return Err(Errno::ENXIO);
    }
    let end = End::open(pipe, reads, writes, metadata)?;
    if !nonblock && reads != writes {
        // On failure `end` is dropped, which counts it out again.
        wait_for_partner(pipe, reads)?;
    }
    Ok(end)
}

/// Block a FIFO opener until the other kind of end has opened: a writer for a
/// reader, a reader for a writer. See the module documentation for why this
/// counts opens rather than looking at who is open.
fn wait_for_partner(pipe: &Pipe, reader: bool) -> Result<(), Errno> {
    let (queue, opened) = if reader {
        (&pipe.readable, &pipe.writers_opened)
    } else {
        (&pipe.writable, &pipe.readers_opened)
    };
    let (partners, before) = {
        let buffer = pipe.buffer.lock();
        let open = if reader {
            buffer.writers()
        } else {
            buffer.readers()
        };
        (open, opened.load(Ordering::Relaxed))
    };
    if partners > 0 {
        return Ok(());
    }
    let caller = process::current();
    let _ = queue.wait_until_deadline(
        || opened.load(Ordering::Relaxed) != before || killed(caller.as_ref()),
        FOREVER,
    );
    if killed(caller.as_ref()) {
        // A restart code, not `EINTR`: opening a FIFO restarts under
        // `SA_RESTART`, as Linux's `fifo_open` does.
        return Err(Errno::ERESTARTSYS);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Splicing
// ---------------------------------------------------------------------------

/// What one look at both pipes of a pipe-to-pipe `splice` found.
enum Joined {
    /// This many bytes went from one to the other.
    Moved(usize),
    /// The source is empty and has no writer left.
    EndOfFile,
    /// The source is empty and a writer may still fill it.
    Empty,
    /// The sink has no room.
    Full,
    /// The sink has no reader left.
    Broken,
    /// The sink's buffer could not grow: its job is at its memory limit.
    NoMemory,
}

/// The pipe end `file` reads or writes through: an anonymous pipe's, or an
/// opened FIFO's. `None` for anything else.
fn end_of(file: &OpenFile) -> Option<Arc<End>> {
    Arc::clone(file.io()).into_any().downcast::<End>().ok()
}

/// Whether `file` is a pipe to `splice`: an anonymous pipe or an opened FIFO,
/// the two Linux's `get_pipe_info` finds.
pub(crate) fn is_pipe(file: &OpenFile) -> bool {
    end_of(file).is_some()
}

/// Whether `a` and `b` are ends of one pipe, which `splice` refuses to join.
pub(crate) fn same_pipe(a: &OpenFile, b: &OpenFile) -> bool {
    matches!((end_of(a), end_of(b)), (Some(a), Some(b)) if Arc::ptr_eq(&a.pipe, &b.pipe))
}

/// Take up to `buf.len()` bytes out of the pipe `file` is, waiting unless
/// `nonblock`. `splice` decides that from its flags and the other
/// descriptor, not from the pipe's own `O_NONBLOCK`, so this does not ask
/// the file.
pub(crate) fn read(file: &OpenFile, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
    end_of(file)
        .ok_or(Errno::EINVAL)?
        .read_stream(buf, nonblock)
}

/// Queue `data` into the pipe `file` is, as [`read`] takes from one.
pub(crate) fn write(file: &OpenFile, data: &[u8], nonblock: bool) -> Result<usize, Errno> {
    end_of(file)
        .ok_or(Errno::EINVAL)?
        .write_stream(data, nonblock)
}

/// Wait until the pipe `file` is has room, and say how much: as much as a
/// `splice` into it may read from its source, so that what it reads it can
/// put down. `EPIPE`, with `SIGPIPE`, once no reader is left, as Linux's
/// `wait_for_space` answers; `EAGAIN` for a full pipe under `nonblock`.
pub(crate) fn room(file: &OpenFile, nonblock: bool) -> Result<usize, Errno> {
    let end = end_of(file).ok_or(Errno::EINVAL)?;
    loop {
        // Bound first, so the guard is gone before anything below waits.
        let (readers, room) = {
            let buffer = end.pipe.buffer.lock();
            (buffer.readers(), buffer.room())
        };
        if readers == 0 {
            crate::syscall::kill::send_to_current(ferrix_linux_abi::types::SIGPIPE);
            return Err(Errno::EPIPE);
        }
        if room > 0 {
            return Ok(room);
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        end.wait_to_write(1)?;
    }
}

/// `splice` from one pipe into another: up to `len` bytes of what the source
/// holds, as many as the sink has room for, moved with both locks held so
/// that no byte is ever out of both pipes. Waits for bytes, then for room,
/// unless `nonblock`. Two ends of one pipe are `EINVAL`, as on Linux; the
/// caller checks that first, and this checks it again rather than take one
/// lock twice.
pub(crate) fn splice_pipes(
    input: &OpenFile,
    output: &OpenFile,
    len: usize,
    nonblock: bool,
) -> Result<usize, Errno> {
    let (Some(from), Some(to)) = (end_of(input), end_of(output)) else {
        return Err(Errno::EINVAL);
    };
    if Arc::ptr_eq(&from.pipe, &to.pipe) {
        return Err(Errno::EINVAL);
    }
    let mut bounce = vec![0_u8; len.min(PIPE_CAPACITY)];
    loop {
        // Bound first, so both guards are gone before anything below waits.
        let joined = join(&from.pipe, &to.pipe, &mut bounce);
        let refusal = match joined {
            Joined::Moved(count) => {
                from.pipe.writable.wake_all();
                to.pipe.readable.wake_all();
                return Ok(count);
            }
            Joined::EndOfFile => return Ok(0),
            Joined::NoMemory => Errno::ENOMEM,
            Joined::Broken => {
                crate::syscall::kill::send_to_current(ferrix_linux_abi::types::SIGPIPE);
                Errno::EPIPE
            }
            Joined::Empty | Joined::Full if nonblock => Errno::EAGAIN,
            Joined::Empty => match from.wait_to_read() {
                Ok(()) => continue,
                Err(errno) => errno,
            },
            Joined::Full => match to.wait_to_write(1) {
                Ok(()) => continue,
                Err(errno) => errno,
            },
        };
        return Err(refusal);
    }
}

/// Move what fits from `from`'s buffer into `to`'s, through `bounce`, with
/// both locks held. They are taken in address order, so two splices crossing
/// the same two pipes in opposite directions cannot each hold the lock the
/// other waits for. The checks go in the order Linux's
/// `splice_pipe_to_pipe` makes them: an empty source a writer may still fill
/// waits whatever the sink is, then a sink with no reader is broken, then an
/// empty source is its end, then a full sink waits.
fn join(from: &Pipe, to: &Pipe, bounce: &mut [u8]) -> Joined {
    let (mut source, mut sink) = if core::ptr::from_ref(from) < core::ptr::from_ref(to) {
        let source = from.buffer.lock();
        (source, to.buffer.lock())
    } else {
        let sink = to.buffer.lock();
        (from.buffer.lock(), sink)
    };
    if source.is_empty() && source.writers() > 0 {
        return Joined::Empty;
    }
    if sink.readers() == 0 {
        return Joined::Broken;
    }
    if source.is_empty() {
        return Joined::EndOfFile;
    }
    // Out of a packet pipe, one packet, which waits for room for all of it
    // rather than lose what the sink could not take.
    let next = source.next_read();
    if source.is_packets() && sink.room() < next.min(bounce.len()) {
        return Joined::Full;
    }
    let count = bounce.len().min(next).min(sink.room());
    if count == 0 {
        return Joined::Full;
    }
    let Some(slot) = bounce.get_mut(..count) else {
        return Joined::Full;
    };
    // Charged before the source gives anything up, which it could not have
    // back if the sink's job were then refused the room.
    if !sink.reserve(count) {
        return Joined::NoMemory;
    }
    // Neither can come back short: the source holds `count` bytes, and the
    // sink has room for them, which is all a write of any size needs.
    let ReadOutcome::Read(read) = source.read(slot) else {
        return Joined::Empty;
    };
    match sink.write(slot.get(..read).unwrap_or_default()) {
        WriteOutcome::Wrote(wrote) => Joined::Moved(wrote),
        WriteOutcome::Broken => Joined::Broken,
        WriteOutcome::WouldBlock => Joined::Full,
        // The room was had before the source was read, so this is not met.
        WriteOutcome::NoMemory => Joined::NoMemory,
    }
}

/// For the boot check of the pipe's waits (`fs::pipe_check`): the tasks
/// listed to read and to write on the pipe `file` is an end of, and the waits
/// on it a wake ended. `None` for a file that is no pipe.
pub(crate) fn waiting_on(file: &OpenFile) -> Option<(usize, usize, u32)> {
    let end = end_of(file)?;
    Some((
        end.pipe.readable.listed_now(),
        end.pipe.writable.listed_now(),
        end.pipe
            .readable
            .waits_ended_by_a_wake()
            .wrapping_add(end.pipe.writable.waits_ended_by_a_wake()),
    ))
}
