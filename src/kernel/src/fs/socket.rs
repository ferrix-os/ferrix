//! Unix-domain sockets, as the inodes an open file reads and writes through.
//!
//! `src/lib/fs/vfs`'s [`SocketBuffer`] is one direction of a socket as values: the
//! queue, the record boundaries, and a write that would wait saying so. This is
//! the half that waits, in the shape `fs::pipe` gives a pipe. Each direction is
//! a [`Channel`] -- that buffer behind a lock, with a wait queue for readers and
//! one for writers -- and a [`Socket`] reads from its own channel and writes
//! into its peer's.
//!
//! # What there is so far
//!
//! Sockets made by `socket(AF_UNIX, ...)`, and by `socketpair` connected to each
//! other: stream, sequenced-packet and datagram, read and written through
//! `read`, `write`, `send*` and `recv*`, shut down a direction at a time,
//! polled, and asked their queue lengths and options; named, listened on,
//! connected to and accepted from; descriptors passed with a message
//! (`SCM_RIGHTS`); and the sender's credentials (`SCM_CREDENTIALS`), which a
//! message carries when its receiver asked for them with `SO_PASSCRED` or its
//! sender named them.
//!
//! # Credentials travel when asked for
//!
//! Linux stamps every message with its sender's pid and ids, and a receiver
//! that set `SO_PASSCRED` is handed them. Here a message is stamped only when
//! the socket it goes to has `SO_PASSCRED` set as it is sent, or its sender
//! passed an `SCM_CREDENTIALS` message: the stamp is ancillary data, and on a
//! stream ancillary data ends a read, so stamping everything would cut every
//! stream read at every write. A message sent before its receiver asked
//! arrives with pid 0 and the overflow ids. Chrome's zygote is what needs it:
//! a child it forks says hello on a socket whose reader set `SO_PASSCRED`,
//! and the pid that hello carries is the only way the zygote learns the
//! child's.
//!
//! # A passed descriptor is an open file in a queue
//!
//! `SCM_RIGHTS` takes a reference to each open file the sender names and
//! queues the references with the bytes they were sent with, as [`Passed`]; a
//! receive that takes those bytes installs them as new descriptors, and one
//! that cannot -- a plain `read`, or `recvmsg` with no room for them -- drops
//! them, which closes a file nobody else holds. Dropping one may close a file,
//! so a [`Passed`] is dropped only with no buffer locked: the buffer hands it
//! back rather than dropping it, and every path here binds it to a variable
//! that outlives the guard. A socket passed over its own connection, or two
//! passed over each other's, keep each other alive while they sit in the
//! queues; [`collect_cycles`] finds such sockets and empties their queues.
//!
//! # Sockets in flight, and the cycles they make
//!
//! A socket whose open file is referred to only from queues can be reached by
//! no program unless one of those queues can: `collect_cycles` is Linux's
//! `unix_gc`, run after a descriptor is closed while any socket is in flight.
//! It walks every socket's receive queue, and the queues of connections still
//! waiting to be accepted, without taking a reference to anything. A socket is
//! a candidate when every reference to its open file is one of the queued ones
//! it counted; a candidate is reachable when some queue that is not a
//! candidate's holds it, and so is everything a reachable candidate's queue
//! holds. The rest are emptied, and what their queues held is dropped with no
//! lock held, after every queue is emptied, so a socket dropped on the way
//! finds its own queue empty rather than dropping the next one inside its
//! drop. Every queue and every count is read as it was at one moment only if
//! nothing carrying a socket was queued or taken off a queue meanwhile, which
//! [`FLIGHT_EPOCH`] says; a pass that sees it move gives up, and the next close
//! tries again. A reference taken by a system call in progress only makes a
//! socket look held, which errs towards keeping it.
//!
//! # Never waiting with a buffer locked
//!
//! As for a pipe: every operation binds what the buffer answered to a variable,
//! so the guard is gone before anything below can sleep, and a wait looks at
//! the buffer again under its lock after it has joined the queue.
//!
//! # A direction closes in more than one way
//!
//! A channel's buffer knows whether its writer has gone, which a reader sees as
//! end of file, and whether its reader has, which a writer sees as a broken
//! socket. `shutdown` needs a state between them that Linux has and the buffer
//! does not: a direction whose reader shut it down still holds what was queued
//! for reading, so its reader is not gone, yet a writer must be refused. That
//! is [`Channel::refused`], checked before every write.
//!
//! # `SIGPIPE`
//!
//! Only a stream socket raises it, and only when it has sent nothing, as
//! Linux's `unix_stream_sendmsg` does; `MSG_NOSIGNAL` keeps it back. A
//! sequenced-packet socket gets `EPIPE` alone, and a datagram socket whose peer
//! has gone `ECONNREFUSED`.
//!
//! # A datagram socket's peer leaving is not end of file
//!
//! A stream or sequenced-packet socket reads end of file once its peer has
//! gone and the queue is drained. A datagram socket does not: its reader waits
//! for the next datagram, which on Linux could come from anyone, and only its
//! own `shutdown(SHUT_RD)` ends its reads.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::sync::Arc;
use alloc::sync::Weak;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ferrix_kmem::{Charge, arc_footprint, buffer_footprint, footprint};
use ferrix_linux_abi::socket::{
    AF_UNIX, Linger, MSG_DONTWAIT, MSG_NOSIGNAL, MSG_PEEK, MSG_WAITALL, SHUT_RD, SHUT_RDWR,
    SHUT_WR, SIOCINQ, SIOCOUTQ, SO_ACCEPTCONN, SO_BROADCAST, SO_DEBUG, SO_DOMAIN, SO_DONTROUTE,
    SO_ERROR, SO_KEEPALIVE, SO_LINGER, SO_OOBINLINE, SO_PASSCRED, SO_PEERCRED, SO_PRIORITY,
    SO_PROTOCOL, SO_RCVBUF, SO_RCVBUFFORCE, SO_RCVLOWAT, SO_RCVTIMEO_NEW, SO_RCVTIMEO_OLD,
    SO_REUSEADDR, SO_SNDBUF, SO_SNDBUFFORCE, SO_SNDLOWAT, SO_SNDTIMEO_NEW, SO_SNDTIMEO_OLD,
    SO_TYPE, SOCK_DGRAM, SOCK_RAW, SOCK_SEQPACKET, SOCK_STREAM, SOCKADDR_UN_SIZE,
    SOCKET_BUFFER_DEFAULT, SOCKET_BUFFER_MAX, SOCKET_BUFFER_MIN, SOL_SOCKET, SOMAXCONN, Ucred,
    UnixAddress, Width,
};
use ferrix_sync::Once;
use ferrix_vfs::Context;
use ferrix_vfs::path::NAME_MAX;
use ferrix_vfs::socket::{Kind, ReadOutcome, SocketBuffer, WriteOutcome};
use ferrix_vfs::{
    Errno, FileSystem, FileType, Inode, Location, Metadata, OpenFile, OpenFlags, Readiness, StatFs,
    Timespec,
};

use crate::fs;
use crate::fs::sockname::{self, Name};
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::syscall::process::{self, Process};
use crate::syscall::uaccess;

/// The deadline of a wait with no timeout: none.
const FOREVER: u64 = u64::MAX;

/// The block size `stat` reports for a socket: a page, as Linux reports.
const BLOCK_SIZE: u32 = 4096;

/// `SOCKFS_MAGIC`, what `fstatfs` on a socket reports.
const SOCKFS_MAGIC: u64 = 0x534F_434B;

/// The ids `SO_PEERCRED` reports for a socket that has no peer credentials:
/// Linux's `overflowuid` and `overflowgid`.
const OVERFLOW_ID: u32 = 65_534;

/// Nanoseconds in a second, for the timeouts a `timeval` carries.
const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Nanoseconds in a microsecond.
const NANOS_PER_MICRO: u64 = 1_000;

/// What travels beside the bytes of a message: the open files an
/// `SCM_RIGHTS` message passes. See the module documentation for why one is
/// dropped only with no buffer locked.
#[derive(Debug)]
pub(crate) struct Passed {
    /// The files, in the order the sender named their descriptors.
    files: Vec<Arc<OpenFile>>,
    /// How many of them are `AF_UNIX` sockets, counted in [`IN_FLIGHT`].
    sockets: usize,
    /// Who sent the message, when it carries that: see the module
    /// documentation.
    credentials: Option<Ucred>,
    /// The list of files, charged to the sender's job for as long as the
    /// message is in flight, as Linux charges `scm_fp_dup`'s copy (F-37).
    /// Each file stays charged to whoever opened it.
    _charge: Charge,
}

impl Passed {
    /// Files to pass, counted in flight if any is a socket, and charged to
    /// the sender's job.
    ///
    /// # Errors
    ///
    /// `ENOMEM` past the job's memory limit.
    pub(crate) fn new(files: Vec<Arc<OpenFile>>) -> Result<Passed, Errno> {
        let charge = Charge::bytes(buffer_footprint::<Arc<OpenFile>>(files.capacity()))
            .map_err(|_| Errno::ENOMEM)?;
        let sockets = files.iter().filter(|file| is_socket(file)).count();
        if sockets > 0 {
            let _ = IN_FLIGHT.fetch_add(sockets, Ordering::AcqRel);
            moved_in_flight();
        }
        Ok(Passed {
            files,
            sockets,
            credentials: None,
            _charge: charge,
        })
    }

    /// The same, carrying `credentials` as its sender's.
    pub(crate) fn with_credentials(mut self, credentials: Ucred) -> Passed {
        self.credentials = Some(credentials);
        self
    }

    /// The files, in the order they were sent.
    pub(crate) fn files(&self) -> &[Arc<OpenFile>] {
        &self.files
    }

    /// Who sent the message, if it says.
    pub(crate) fn credentials(&self) -> Option<Ucred> {
        self.credentials
    }

    /// Whether it names its sender: a stream read that has taken bytes stops
    /// before it ([`SocketBuffer::stopping_before`]).
    fn names_its_sender(&self) -> bool {
        self.credentials.is_some()
    }

    /// Whether any of them is an `AF_UNIX` socket.
    fn carries_sockets(&self) -> bool {
        self.sockets > 0
    }
}

impl Drop for Passed {
    fn drop(&mut self) {
        if self.sockets > 0 {
            let _ = IN_FLIGHT.fetch_sub(self.sockets, Ordering::AcqRel);
            moved_in_flight();
        }
    }
}

/// `AF_UNIX` socket files referred to by a live [`Passed`]: queued, or on
/// their way into or out of a queue. Nothing to collect while it is zero.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Moved on whenever a [`Passed`] carrying a socket is made, queued, taken
/// off a queue or dropped: a pass that sees it move gives up.
static FLIGHT_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Whether a pass is running; a second one waits for the next close.
static COLLECTING: AtomicBool = AtomicBool::new(false);

/// Every `AF_UNIX` socket there is, for the pass to walk. Weak, so being
/// listed keeps nothing alive; pruned as it grows.
static SOCKETS: SpinLock<Vec<Weak<Socket>>> = SpinLock::new(Vec::new());

/// Say that what is in flight moved.
fn moved_in_flight() {
    let _ = FLIGHT_EPOCH.fetch_add(1, Ordering::AcqRel);
}

/// Whether `file` is an `AF_UNIX` socket. The reference [`of`] takes is to
/// the socket, never the file's last, and goes at once.
fn is_socket(file: &OpenFile) -> bool {
    of(file).is_some()
}

/// What travels beside the bytes of a message.
type Ancillary = Passed;

/// The three kinds of `AF_UNIX` socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SocketType {
    /// `SOCK_STREAM`: a byte stream.
    Stream,
    /// `SOCK_SEQPACKET`: records, in order, connected.
    SeqPacket,
    /// `SOCK_DGRAM`: records.
    Datagram,
}

impl SocketType {
    /// The type a `socket` call's type names, if `AF_UNIX` has it. `SOCK_RAW`
    /// is a datagram socket, as `unix_create` makes it.
    pub(crate) fn from_linux(kind: u32) -> Option<SocketType> {
        match kind {
            SOCK_STREAM => Some(SocketType::Stream),
            SOCK_SEQPACKET => Some(SocketType::SeqPacket),
            SOCK_DGRAM | SOCK_RAW => Some(SocketType::Datagram),
            _ => None,
        }
    }

    /// The type as `SO_TYPE` reports it.
    pub(crate) fn linux(self) -> u32 {
        match self {
            SocketType::Stream => SOCK_STREAM,
            SocketType::SeqPacket => SOCK_SEQPACKET,
            SocketType::Datagram => SOCK_DGRAM,
        }
    }

    /// Whether its buffer keeps bytes or records.
    fn buffer_kind(self) -> Kind {
        match self {
            SocketType::Stream => Kind::Stream,
            SocketType::SeqPacket | SocketType::Datagram => Kind::Record,
        }
    }
}

/// One direction of a socket: the buffer, and who waits on it.
struct Channel {
    buffer: SpinLock<SocketBuffer<Ancillary>>,
    /// Woken when a read may no longer wait: data arrived, or the writer left
    /// or shut this direction down.
    readable: Arc<WaitQueue>,
    /// Woken when a write may no longer wait: room was made, or the reader
    /// left or shut this direction down.
    writable: Arc<WaitQueue>,
    /// Whether this direction takes no more writes: its reader shut it down,
    /// or went. See the module documentation for why the buffer's own state
    /// is not enough.
    refused: AtomicBool,
    /// Whether the socket reading this direction set `SO_PASSCRED`, which
    /// its senders look at to stamp what they send.
    wants_credentials: AtomicBool,
    /// Its heap and its queue's floor, charged to the job that made it; each
    /// segment written into the queue is charged to its writer (F-37).
    _charge: Charge,
}

impl fmt::Debug for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Channel")
            .field("refused", &self.refused)
            .finish_non_exhaustive()
    }
}

impl Channel {
    /// An empty direction of a socket of `kind`, at the default buffer size,
    /// charged to the running task's job.
    ///
    /// # Errors
    ///
    /// `ENOMEM` past the job's memory limit.
    fn new(kind: SocketType) -> Result<Arc<Channel>, Errno> {
        // The queue's floor too: its segments pay for the rest of it.
        let charge = Charge::bytes(
            arc_footprint::<Channel>()
                .saturating_add(arc_footprint::<WaitQueue>().saturating_mul(2))
                .saturating_add(SocketBuffer::<Ancillary>::floor_footprint()),
        )
        .map_err(|_| Errno::ENOMEM)?;
        Ok(Arc::new(Channel {
            buffer: SpinLock::new(
                SocketBuffer::new(kind.buffer_kind(), SOCKET_BUFFER_DEFAULT)
                    .stopping_before(Passed::names_its_sender),
            ),
            readable: Arc::new(WaitQueue::new()),
            writable: Arc::new(WaitQueue::new()),
            refused: AtomicBool::new(false),
            wants_credentials: AtomicBool::new(false),
            _charge: charge,
        }))
    }

    /// Wake both directions' waiters: an end closed or shut down, which can
    /// end a wait on either side.
    fn wake_both(&self) {
        self.readable.wake_all();
        self.writable.wake_all();
    }

    /// Whether a write into this direction is refused.
    fn is_refused(&self) -> bool {
        self.refused.load(Ordering::Acquire)
    }
}

/// What a socket keeps besides its directions.
#[derive(Debug, Clone, Copy)]
struct Options {
    /// `SO_RCVTIMEO`, in nanoseconds; zero waits forever.
    receive_timeout: u64,
    /// `SO_SNDTIMEO`, likewise.
    send_timeout: u64,
    /// `SO_SNDBUF` as set, which is reported back; the receive side's own
    /// buffer is what limits a sender here.
    send_buffer: usize,
    /// `SO_PASSCRED`, kept for when credentials travel.
    pass_credentials: bool,
    /// `shutdown(SHUT_WR)` was called: this socket sends no more.
    shut_write: bool,
}

/// An `AF_UNIX` socket, as the inode its open file reads and writes through.
pub(crate) struct Socket {
    kind: SocketType,
    /// What this socket reads from.
    receive: Arc<Channel>,
    /// What this socket writes into: its peer's receive direction, while it
    /// has a peer.
    send: SpinLock<Option<Arc<Channel>>>,
    options: SpinLock<Options>,
    /// Whoever made this socket: what the other end of a pair reports.
    credentials: Ucred,
    /// Who called `listen` on it, as they were at that call: what a
    /// connection to it reports as its peer, as Linux's `unix_listen` has
    /// `init_peercred`. `None` until it listens.
    listen_credentials: SpinLock<Option<Ucred>>,
    /// What `SO_PEERCRED` reports: for a pair, whoever made it; for the
    /// connecting end of a connection, who called `listen` on the listener;
    /// for the accepted end, who called `connect`, each as they were at that
    /// call, as Linux takes them (`docs/AUTH.md` §8.3, E-01). A socket with
    /// no peer has none, and answers the overflow ids.
    peer_credentials: SpinLock<Option<Ucred>>,
    /// The network namespace it was made in, by its number: where an
    /// abstract name it binds or connects to is looked for.
    net_ns: u64,
    /// The name `bind` gave it. Set once: Linux's `unix_bind` refuses a
    /// second one.
    bound: SpinLock<Option<Name>>,
    /// The name of whatever it is connected to, when that had one.
    peer_name: SpinLock<Option<Name>>,
    /// The connections waiting to be accepted, once `listen` has been called.
    listener: SpinLock<Option<Backlog>>,
    /// Woken when the backlog changes: an `accept` waits here for a
    /// connection, and a `connect` waits here for room.
    arrivals: Arc<WaitQueue>,
    /// What `stat` reports through it.
    metadata: Metadata,
    /// Its heap, charged to the job that made it: itself, its entry in
    /// [`SOCKETS`], and the names it may hold -- its own, its peer's, one
    /// in a table -- at their longest (F-37).
    _charge: Charge,
}

/// What a listening socket is holding: the connections that have arrived and
/// how many may wait.
#[derive(Debug)]
struct Backlog {
    /// How many connections may wait. `listen(n)` allows `n + 1`, which is
    /// what Linux's `unix_recvq_full` -- a strict `>` against
    /// `sk_max_ack_backlog` -- means.
    limit: usize,
    /// The server ends of connections nobody has accepted yet, oldest first.
    /// Never longer than `limit`, and allocated to `limit` at `listen`, so
    /// that pushing one never allocates under the lock.
    waiting: VecDeque<Waiting>,
    /// The room `waiting` holds, charged to the job that called `listen`
    /// (F-37).
    charge: Charge,
}

/// One connection waiting to be accepted.
#[derive(Debug)]
struct Waiting {
    /// The server end, already wired to the client's.
    socket: Arc<Socket>,
    /// The client's name, if it had bound one.
    peer: Option<Name>,
}

impl fmt::Debug for Socket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Socket")
            .field("kind", &self.kind)
            .field("ino", &self.metadata.ino)
            .finish_non_exhaustive()
    }
}

/// What a receive took: the bytes copied, and for a record the whole record's
/// length, which `MSG_TRUNC` answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Received {
    /// Bytes copied into the caller's buffer.
    pub(crate) bytes: usize,
    /// For a record, its whole length; for a stream, `bytes`.
    pub(crate) full: usize,
}

/// Who sent a message a receive took, as its `SCM_CREDENTIALS` says it:
/// the stamp it carries, or for one sent unstamped pid 0 and the overflow
/// ids, which is what Linux reports for a sender it cannot name.
pub(crate) fn sender_of(passed: Option<&Passed>) -> Ucred {
    passed.and_then(Passed::credentials).unwrap_or(Ucred {
        pid: 0,
        uid: OVERFLOW_ID,
        gid: OVERFLOW_ID,
    })
}

/// `credentials` with the ids the running process's user namespace calls
/// them, or the overflow id for ones it does not map: the stamp holds kernel
/// ids, and a program is only ever told its own namespace's (rule U9).
pub(crate) fn as_seen(credentials: Ucred) -> Ucred {
    Ucred {
        pid: credentials.pid,
        uid: crate::syscall::credentials::show_uid(credentials.uid),
        gid: crate::syscall::credentials::show_gid(credentials.gid),
    }
}

/// The credentials `SO_PEERCRED` reports for a socket `process` made:
/// its pid and effective ids, as Linux's `init_peercred` takes them.
pub(crate) fn credentials_of(process: &Process) -> Ucred {
    let (uid, gid) = process
        .with_credentials(|credentials| (credentials.user.effective, credentials.group.effective));
    Ucred {
        pid: i32::try_from(process.pid()).unwrap_or(0),
        uid,
        gid,
    }
}

/// When a wait with `timeout` nanoseconds of patience must give up; zero
/// waits forever.
fn deadline(timeout: u64) -> u64 {
    if timeout == 0 {
        FOREVER
    } else {
        crate::timer::now_nanos().saturating_add(timeout)
    }
}

/// Sleep on `queue` until `ready`, the caller has a signal to take, or
/// `deadline` passes.
///
/// A signal is a restart code for a wait with no deadline and `EINTR` for one
/// with a timeout, which is Linux's `sock_intr_errno`; a timeout that runs out
/// is `EAGAIN`, as a socket answers.
fn wait(queue: &WaitQueue, mut ready: impl FnMut() -> bool, deadline: u64) -> Result<(), Errno> {
    let caller = process::current();
    let signalled = || {
        caller
            .as_ref()
            .is_some_and(|process| process.signal_pending())
    };
    let _ = queue.wait_until_deadline(|| ready() || signalled(), deadline);
    if signalled() {
        return Err(if deadline == FOREVER {
            Errno::ERESTARTSYS
        } else {
            Errno::EINTR
        });
    }
    if deadline != FOREVER && !ready() && crate::timer::now_nanos() >= deadline {
        return Err(Errno::EAGAIN);
    }
    Ok(())
}

impl Socket {
    /// A socket of `kind` reading from `receive` and writing into `send`,
    /// owned by `owner`: the creator's filesystem user and group ids.
    fn new(
        kind: SocketType,
        send: Option<Arc<Channel>>,
        receive: Arc<Channel>,
        credentials: Ucred,
        peer_credentials: Option<Ucred>,
        (uid, gid): (u32, u32),
        net_ns: u64,
    ) -> Result<Arc<Socket>, Errno> {
        let names = footprint(SOCKADDR_UN_SIZE, 1).saturating_mul(4);
        let charge = Charge::bytes(
            arc_footprint::<Socket>()
                .saturating_add(arc_footprint::<WaitQueue>())
                .saturating_add(size_of::<Weak<Socket>>())
                .saturating_add(names),
        )
        .map_err(|_| Errno::ENOMEM)?;
        let ino = sockfs().next_ino.fetch_add(1, Ordering::Relaxed);
        let now = fs::clock().now();
        let socket = Arc::new(Socket {
            kind,
            receive,
            send: SpinLock::new(send),
            options: SpinLock::new(Options {
                receive_timeout: 0,
                send_timeout: 0,
                send_buffer: SOCKET_BUFFER_DEFAULT,
                pass_credentials: false,
                shut_write: false,
            }),
            credentials,
            net_ns,
            listen_credentials: SpinLock::new(None),
            peer_credentials: SpinLock::new(peer_credentials),
            bound: SpinLock::new(None),
            peer_name: SpinLock::new(None),
            listener: SpinLock::new(None),
            arrivals: Arc::new(WaitQueue::new()),
            metadata: Metadata {
                ino,
                kind: FileType::Socket,
                // A socket's inode is `S_IFSOCK | 0777` on sockfs, as on Linux.
                permissions: 0o777,
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
            },
            _charge: charge,
        });
        let mut listed = SOCKETS.lock();
        if listed.len().is_power_of_two() {
            listed.retain(|socket| socket.strong_count() > 0);
        }
        listed.push(Arc::downgrade(&socket));
        drop(listed);
        Ok(socket)
    }

    /// Whether `SO_PASSCRED` is set: a receive hands back an
    /// `SCM_CREDENTIALS` message with every message it takes.
    pub(crate) fn passes_credentials(&self) -> bool {
        self.options.lock().pass_credentials
    }

    /// Its type.
    pub(crate) fn kind(&self) -> SocketType {
        self.kind
    }

    /// Whether it has a peer to send to.
    pub(crate) fn is_connected(&self) -> bool {
        self.send.lock().is_some()
    }

    // -- Names, and the connections they make -------------------------------

    /// `bind`: give this socket a name.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a socket that already has one, as Linux's `unix_bind`
    /// refuses a second, and for the unnamed address, which Linux answers by
    /// inventing an abstract name a program cannot predict.
    /// `EADDRINUSE` if the name is taken, and whatever the walk to a path
    /// refuses.
    pub(crate) fn bind(
        self: &Arc<Socket>,
        ctx: &Context,
        address: &UnixAddress<'_>,
    ) -> Result<(), Errno> {
        if self.bound.lock().is_some() {
            return Err(Errno::EINVAL);
        }
        // Outside the lock: binding a path walks the filesystem, and a walk
        // may sleep. A `SpinLock` held across one is a processor that cannot
        // be preempted while it waits for a disk.
        let name = match *address {
            UnixAddress::Unnamed => return Err(Errno::EINVAL),
            UnixAddress::Path(path) => sockname::bind_path(self, ctx, None, path)?,
            UnixAddress::Abstract(name) => sockname::bind_abstract(self, self.net_ns, name)?,
        };
        let mut bound = self.bound.lock();
        if bound.is_some() {
            // Two binds at once, and this one lost. Undo it rather than leave
            // a name pointing at a socket that answers by another.
            drop(bound);
            sockname::forget(&name, self.net_ns);
            return Err(Errno::EINVAL);
        }
        *bound = Some(name);
        Ok(())
    }

    /// `listen`: take connections, up to `backlog` of them waiting.
    ///
    /// # Errors
    ///
    /// `EOPNOTSUPP` for a datagram socket, which has no connections, and
    /// `EINVAL` for one that is not bound or is already connected -- Linux's
    /// `unix_listen` refuses both. `caller` is who is calling, as they are
    /// now: what a connection to it will report.
    pub(crate) fn listen(&self, backlog: i32, caller: Ucred) -> Result<(), Errno> {
        if self.kind == SocketType::Datagram {
            return Err(Errno::EOPNOTSUPP);
        }
        if self.bound.lock().is_none() || self.send.lock().is_some() {
            return Err(Errno::EINVAL);
        }
        // Every `listen` takes the caller's ids afresh, as Linux's does.
        *self.listen_credentials.lock() = Some(caller);
        let asked = usize::try_from(backlog.max(0)).unwrap_or(0);
        // `n + 1`, because Linux's queue-full test is a strict `>` against
        // the backlog, so `listen(0)` still takes one connection.
        let limit = asked.min(SOMAXCONN).saturating_add(1);
        let mut charge =
            Charge::bytes(buffer_footprint::<Waiting>(limit)).map_err(|_| Errno::ENOMEM)?;
        let mut waiting = VecDeque::new();
        waiting
            .try_reserve_exact(limit)
            .map_err(|_| Errno::ENOMEM)?;
        let mut listener = self.listener.lock();
        match listener.as_mut() {
            // Listening again only changes the number: the connections
            // already waiting stay, as they do on Linux. A larger number
            // takes the new room and its charge, so that pushing up to it
            // still never allocates under the lock.
            Some(held) => {
                if limit > held.waiting.capacity() {
                    core::mem::swap(&mut held.charge, &mut charge);
                    while let Some(one) = held.waiting.pop_front() {
                        // NOALLOC: `waiting` has room for `limit`, and
                        // `held` never had more than its own limit.
                        waiting.push_back(one);
                    }
                    core::mem::swap(&mut held.waiting, &mut waiting);
                }
                held.limit = limit;
            }
            None => {
                *listener = Some(Backlog {
                    limit,
                    waiting,
                    charge,
                });
                return Ok(());
            }
        }
        // What the old room was, given back with no lock held.
        drop(listener);
        drop((waiting, charge));
        Ok(())
    }

    /// `connect`: reach the socket bound to `address`.
    ///
    /// # Errors
    ///
    /// `EINVAL` for the unnamed address, `EPROTOTYPE` for a socket of another
    /// type, `ECONNREFUSED` for a name nobody is listening on, `EISCONN` for
    /// a socket that is already connected, `EAGAIN` for a full backlog a
    /// non-blocking socket will not wait for, and whatever the walk refuses.
    pub(crate) fn connect(
        self: &Arc<Socket>,
        ctx: &Context,
        address: &UnixAddress<'_>,
        nonblock: bool,
        caller: Ucred,
    ) -> Result<(), Errno> {
        let target = match *address {
            UnixAddress::Unnamed => return Err(Errno::EINVAL),
            UnixAddress::Path(path) => sockname::socket_at(ctx, None, path)?,
            UnixAddress::Abstract(name) => sockname::socket_named(self.net_ns, name)?,
        };
        // A stream may not connect to a datagram socket bound to the same
        // name, which is what Linux answers `EPROTOTYPE` to.
        if target.kind != self.kind {
            return Err(Errno::EPROTOTYPE);
        }
        if Arc::ptr_eq(self, &target) && self.kind != SocketType::Datagram {
            return Err(Errno::ECONNREFUSED);
        }
        let peer_name = target.bound.lock().clone();
        if self.kind == SocketType::Datagram {
            return self.connect_datagram(&target, peer_name);
        }
        self.connect_stream(&target, peer_name, nonblock, caller)
    }

    /// A datagram `connect`, which only chooses where sends go. Linux lets
    /// one be repeated, and a datagram socket has no handshake to fail.
    fn connect_datagram(
        self: &Arc<Socket>,
        target: &Arc<Socket>,
        peer_name: Option<Name>,
    ) -> Result<(), Errno> {
        *self.send.lock() = Some(Arc::clone(&target.receive));
        *self.peer_name.lock() = peer_name;
        *self.peer_credentials.lock() = Some(target.credentials);
        Ok(())
    }

    /// A stream or sequenced-packet `connect`: make the far end, put it in the
    /// listener's backlog, and wire the two together.
    ///
    /// The connection is complete when it is queued, not when it is accepted,
    /// which is what Linux's `unix_stream_connect` does: a client may write
    /// before the server has called `accept`, and what it writes waits in the
    /// buffer.
    fn connect_stream(
        self: &Arc<Socket>,
        target: &Arc<Socket>,
        peer_name: Option<Name>,
        nonblock: bool,
        caller: Ucred,
    ) -> Result<(), Errno> {
        if self.send.lock().is_some() {
            return Err(Errno::EISCONN);
        }
        if self.listener.lock().is_some() {
            return Err(Errno::EINVAL);
        }
        let mine = self.bound.lock().clone();
        // Who listened, as they were when they did: what this end will name as
        // its peer. A socket takes connections only once `listen` has recorded
        // them, so a target without them is refused rather than named by the
        // ids it was made with (ferrix-55's review of K-E).
        let listened = (*target.listen_credentials.lock()).ok_or(Errno::ECONNREFUSED)?;
        let deadline = deadline(self.options.lock().send_timeout);
        loop {
            // Made before the lock is taken and wired after it is dropped: the
            // far end is created with no send channel, so dropping it if there
            // is no room closes nothing of this socket's.
            let far = Channel::new(self.kind)?;
            // The accepted end names who is connecting, as they are now; the
            // connecting end names who listened, as they were then.
            let server = Socket::new(
                self.kind,
                None,
                Arc::clone(&far),
                target.credentials,
                Some(caller),
                (target.metadata.uid, target.metadata.gid),
                target.net_ns,
            )?;
            let queued = {
                let mut listener = target.listener.lock();
                let Some(backlog) = listener.as_mut() else {
                    return Err(Errno::ECONNREFUSED);
                };
                if backlog.waiting.len() < backlog.limit {
                    backlog.waiting.push_back(Waiting {
                        socket: Arc::clone(&server),
                        peer: mine.clone(),
                    });
                    true
                } else {
                    false
                }
            };
            if queued {
                *server.send.lock() = Some(Arc::clone(&self.receive));
                *server.peer_name.lock() = mine;
                *self.send.lock() = Some(far);
                *self.peer_name.lock() = peer_name;
                *self.peer_credentials.lock() = Some(listened);
                target.arrivals.wake_all();
                return Ok(());
            }
            drop(server);
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            wait(&target.arrivals, || target.has_room(), deadline)?;
        }
    }

    /// Whether the backlog has room for one more, for a waiting `connect`.
    fn has_room(&self) -> bool {
        self.listener
            .lock()
            .as_ref()
            .is_none_or(|backlog| backlog.waiting.len() < backlog.limit)
    }

    /// Whether a connection is waiting, for a waiting `accept`.
    fn has_arrival(&self) -> bool {
        self.listener
            .lock()
            .as_ref()
            .is_none_or(|backlog| !backlog.waiting.is_empty())
    }

    /// `accept`: take the oldest waiting connection, and say who made it.
    ///
    /// `nonblock` is the listener's own `O_NONBLOCK`, which decides only
    /// whether to wait for a connection. The new socket is non-blocking when
    /// `accepted_nonblock` says so -- `accept4`'s `SOCK_NONBLOCK` -- and
    /// never because the listener is, as on Linux: a server that accepts on
    /// a non-blocking listener and then reads with a timeout must have its
    /// read wait.
    ///
    /// # Errors
    ///
    /// `EOPNOTSUPP` for a datagram socket, `EINVAL` for one that is not
    /// listening, `EAGAIN` for a non-blocking socket with nothing waiting.
    pub(crate) fn accept(
        self: &Arc<Socket>,
        nonblock: bool,
        accepted_nonblock: bool,
    ) -> Result<(Arc<OpenFile>, Vec<u8>), Errno> {
        if self.kind == SocketType::Datagram {
            return Err(Errno::EOPNOTSUPP);
        }
        let deadline = deadline(self.options.lock().receive_timeout);
        loop {
            let taken = {
                let mut listener = self.listener.lock();
                let Some(backlog) = listener.as_mut() else {
                    return Err(Errno::EINVAL);
                };
                backlog.waiting.pop_front()
            };
            if let Some(waiting) = taken {
                // Room in the backlog now, which a `connect` may be waiting
                // for.
                self.arrivals.wake_all();
                let address = encode_name(waiting.peer.as_ref());
                return Ok((open(waiting.socket, accepted_nonblock)?, address));
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            wait(&self.arrivals, || self.has_arrival(), deadline)?;
        }
    }

    /// What `getsockname` answers: the bound name, or the unnamed address.
    pub(crate) fn sock_name(&self) -> Vec<u8> {
        encode_name(self.bound.lock().as_ref())
    }

    /// What `getpeername` answers.
    ///
    /// # Errors
    ///
    /// `ENOTCONN` for a socket with no peer, which is what Linux answers
    /// whether or not the peer would have had a name.
    pub(crate) fn peer_sock_name(&self) -> Result<Vec<u8>, Errno> {
        if self.send.lock().is_none() {
            return Err(Errno::ENOTCONN);
        }
        Ok(encode_name(self.peer_name.lock().as_ref()))
    }

    /// What a write into a direction nobody reads any more answers: Linux's
    /// signal for a stream, `EPIPE` for a sequenced-packet socket, and
    /// `ECONNREFUSED` for a datagram whose peer has gone.
    fn broken(&self, flags: u32) -> Errno {
        match self.kind {
            SocketType::Stream => {
                if flags & MSG_NOSIGNAL == 0 {
                    crate::syscall::kill::send_to_current(ferrix_linux_abi::types::SIGPIPE);
                }
                Errno::EPIPE
            }
            SocketType::SeqPacket => Errno::EPIPE,
            SocketType::Datagram => Errno::ECONNREFUSED,
        }
    }

    /// Send `data` to the peer: all of it for a stream, waiting for room as
    /// often as it takes unless non-blocking, and one whole record otherwise.
    ///
    /// # Errors
    ///
    /// `ENOTCONN` with no peer; `EPIPE` after `shutdown(SHUT_WR)` or with the
    /// peer gone (see [`Socket::broken`]); `EMSGSIZE` for a record larger than
    /// the peer's buffer; `EAGAIN` when it would wait and may not, or its
    /// timeout ran out; a restart code or `EINTR` for a signal. A stream that
    /// sent part of `data` first reports the count instead.
    pub(crate) fn send(&self, data: &[u8], flags: u32, nonblock: bool) -> Result<usize, Errno> {
        self.send_passing(data, flags, nonblock, None)
    }

    /// [`Socket::send`], with files to pass with the first byte sent.
    ///
    /// Files that were not queued -- the send failed, or sent nothing -- are
    /// dropped on the way out, with no buffer locked.
    ///
    /// # Errors
    ///
    /// As [`Socket::send`].
    pub(crate) fn send_passing(
        &self,
        data: &[u8],
        flags: u32,
        nonblock: bool,
        passed: Option<Passed>,
    ) -> Result<usize, Errno> {
        let sender = || process::current().map(|process| credentials_of(&process));
        self.send_stamped(data, flags, nonblock, passed, sender)
    }

    /// [`Socket::send_passing`] on behalf of `sender`, whose credentials a
    /// reader that asked for them is given: what the system calls use,
    /// since the caller is the process they name, whatever task runs them.
    ///
    /// # Errors
    ///
    /// As [`Socket::send`].
    pub(crate) fn send_from(
        &self,
        sender: &Process,
        data: &[u8],
        flags: u32,
        nonblock: bool,
        passed: Option<Passed>,
    ) -> Result<usize, Errno> {
        self.send_stamped(data, flags, nonblock, passed, || {
            Some(credentials_of(sender))
        })
    }

    /// The send itself, stamped with what `sender` answers when the reader
    /// asked for credentials and the message does not already name them.
    fn send_stamped(
        &self,
        data: &[u8],
        flags: u32,
        nonblock: bool,
        passed: Option<Passed>,
        sender: impl FnOnce() -> Option<Ucred>,
    ) -> Result<usize, Errno> {
        let mut passed = passed;
        let peer = self.send.lock().clone().ok_or(Errno::ENOTCONN)?;
        if peer.wants_credentials.load(Ordering::Acquire)
            && passed
                .as_ref()
                .is_none_or(|passed| passed.credentials.is_none())
            && let Some(stamp) = sender()
        {
            passed = Some(
                passed
                    .map_or_else(|| Passed::new(Vec::new()), Ok)?
                    .with_credentials(stamp),
            );
        }
        let options = *self.options.lock();
        if options.shut_write {
            return Err(match self.kind {
                SocketType::Stream => self.broken(flags),
                SocketType::SeqPacket | SocketType::Datagram => Errno::EPIPE,
            });
        }
        let nonblock = nonblock || flags & MSG_DONTWAIT != 0;
        let deadline = deadline(options.send_timeout);
        let sent = match self.kind {
            SocketType::Stream => {
                self.send_stream(&peer, data, &mut passed, flags, nonblock, deadline)
            }
            SocketType::SeqPacket | SocketType::Datagram => {
                self.send_record(&peer, data, &mut passed, flags, nonblock, deadline)
            }
        };
        drop(passed);
        sent
    }

    /// [`Socket::send`] for a stream.
    fn send_stream(
        &self,
        peer: &Channel,
        data: &[u8],
        passed: &mut Option<Passed>,
        flags: u32,
        nonblock: bool,
        deadline: u64,
    ) -> Result<usize, Errno> {
        // An empty write succeeds without looking for a reader, as on Linux.
        if data.is_empty() {
            return Ok(0);
        }
        let mut done = 0;
        while done < data.len() {
            let rest = data.get(done..).unwrap_or_default();
            let outcome = if peer.is_refused() {
                WriteOutcome::Broken
            } else {
                // The guard is named and given back here, so that nothing
                // below this can wait while the buffer is locked. The files
                // go with the first bytes queued, and stay with the caller
                // otherwise.
                let carried = passed.as_ref().is_some_and(Passed::carries_sockets);
                let mut buffer = peer.buffer.lock();
                let outcome = buffer.write(rest, passed);
                if carried && passed.is_none() {
                    moved_in_flight();
                }
                drop(buffer);
                outcome
            };
            let refusal = match outcome {
                WriteOutcome::Wrote(count) => {
                    done += count;
                    peer.readable.wake_all();
                    continue;
                }
                // Nothing sent: the refusal, with its signal. Something sent:
                // the count, and no signal, as Linux reports a short send.
                WriteOutcome::Broken if done == 0 => return Err(self.broken(flags)),
                WriteOutcome::Broken => return Ok(done),
                WriteOutcome::TooBig => Errno::EMSGSIZE,
                WriteOutcome::NoMemory => Errno::ENOMEM,
                WriteOutcome::WouldBlock if nonblock => Errno::EAGAIN,
                WriteOutcome::WouldBlock => match wait(
                    &peer.writable,
                    || peer.is_refused() || peer.buffer.lock().can_write(1),
                    deadline,
                ) {
                    Ok(()) => continue,
                    Err(errno) => errno,
                },
            };
            return if done > 0 { Ok(done) } else { Err(refusal) };
        }
        Ok(done)
    }

    /// [`Socket::send`] for a sequenced-packet or datagram socket: one record,
    /// queued whole or not at all.
    fn send_record(
        &self,
        peer: &Channel,
        data: &[u8],
        passed: &mut Option<Passed>,
        flags: u32,
        nonblock: bool,
        deadline: u64,
    ) -> Result<usize, Errno> {
        loop {
            let outcome = if peer.is_refused() {
                // A datagram socket that shut its reading down refuses with
                // EPIPE, as `unix_dgram_sendmsg` does; one that has gone, with
                // ECONNREFUSED.
                if self.kind == SocketType::Datagram && !peer.buffer.lock().reader_closed() {
                    return Err(Errno::EPIPE);
                }
                WriteOutcome::Broken
            } else {
                // The guard is named and given back here, so that nothing
                // below this can wait while the buffer is locked.
                let carried = passed.as_ref().is_some_and(Passed::carries_sockets);
                let mut buffer = peer.buffer.lock();
                let outcome = buffer.write(data, passed);
                if carried && passed.is_none() {
                    moved_in_flight();
                }
                drop(buffer);
                outcome
            };
            match outcome {
                WriteOutcome::Wrote(count) => {
                    peer.readable.wake_all();
                    return Ok(count);
                }
                WriteOutcome::Broken => return Err(self.broken(flags)),
                WriteOutcome::TooBig => return Err(Errno::EMSGSIZE),
                WriteOutcome::NoMemory => return Err(Errno::ENOMEM),
                WriteOutcome::WouldBlock if nonblock => return Err(Errno::EAGAIN),
                WriteOutcome::WouldBlock => wait(
                    &peer.writable,
                    || peer.is_refused() || peer.buffer.lock().can_write(data.len()),
                    deadline,
                )?,
            }
        }
    }

    /// Receive into `out`: for a stream, what is queued up to its length --
    /// and with `MSG_WAITALL` until it is full or the stream ends -- and for a
    /// record, one record, truncated to fit.
    ///
    /// # Errors
    ///
    /// `EINVAL` on a stream socket and `ENOTCONN` on a sequenced-packet socket
    /// that was never connected, as Linux answers each; `EAGAIN`
    /// when it would wait and may not, or its timeout ran out; a restart code
    /// or `EINTR` for a signal. A stream that received part first reports it.
    pub(crate) fn recv(
        &self,
        out: &mut [u8],
        flags: u32,
        nonblock: bool,
    ) -> Result<Received, Errno> {
        // Files that came with the bytes are dropped here, with no buffer
        // locked: a receive that cannot install them closes them, as Linux's
        // `read` on a socket does.
        self.recv_passing(out, flags, nonblock)
            .map(|(received, _passed)| received)
    }

    /// [`Socket::recv`], handing back the files that came with the bytes.
    ///
    /// A receive hands back at most one message's files: a stream runs on
    /// through plain bytes into the bytes that brought some, and stops after
    /// them, as Linux's `unix_stream_read_generic` stops, even under
    /// `MSG_WAITALL`. A peek hands back none, which is where this differs
    /// from Linux, whose peek installs duplicates: measured on a 7.0 host, a
    /// peek of 100 plain bytes, 100 with a descriptor and 100 plain is 200
    /// with the descriptor, twice, and the read after it the same.
    ///
    /// # Errors
    ///
    /// As [`Socket::recv`].
    pub(crate) fn recv_passing(
        &self,
        out: &mut [u8],
        flags: u32,
        nonblock: bool,
    ) -> Result<(Received, Option<Passed>), Errno> {
        if self.kind != SocketType::Datagram && self.send.lock().is_none() {
            // `unix_stream_read_generic` answers EINVAL, `unix_seqpacket_recvmsg`
            // ENOTCONN.
            return Err(if self.kind == SocketType::Stream {
                Errno::EINVAL
            } else {
                Errno::ENOTCONN
            });
        }
        if out.is_empty() && self.kind == SocketType::Stream {
            return Ok((Received { bytes: 0, full: 0 }, None));
        }
        let nonblock = nonblock || flags & MSG_DONTWAIT != 0;
        let peek = flags & MSG_PEEK != 0;
        let wait_all = flags & MSG_WAITALL != 0 && self.kind == SocketType::Stream && !peek;
        let deadline = deadline(self.options.lock().receive_timeout);
        let mut done = 0;
        loop {
            let rest = out.get_mut(done..).unwrap_or_default();
            // Bound first, so the guard is gone before anything below waits.
            let mut buffer = self.receive.buffer.lock();
            let outcome = buffer.read(rest, peek);
            if matches!(
                &outcome,
                ReadOutcome::Read { ancillary: Some(passed), .. } if passed.carries_sockets()
            ) {
                moved_in_flight();
            }
            drop(buffer);
            let refusal = match outcome {
                ReadOutcome::Read {
                    bytes,
                    full,
                    ancillary,
                } => {
                    if !peek {
                        self.receive.writable.wake_all();
                    }
                    if self.kind != SocketType::Stream {
                        return Ok((Received { bytes, full }, ancillary));
                    }
                    done += bytes;
                    if wait_all && done < out.len() && ancillary.is_none() {
                        continue;
                    }
                    return Ok((
                        Received {
                            bytes: done,
                            full: done,
                        },
                        ancillary,
                    ));
                }
                ReadOutcome::EndOfFile => {
                    return Ok((
                        Received {
                            bytes: done,
                            full: done,
                        },
                        None,
                    ));
                }
                ReadOutcome::WouldBlock if nonblock => Errno::EAGAIN,
                ReadOutcome::WouldBlock => match wait(
                    &self.receive.readable,
                    || self.receive.buffer.lock().can_read(),
                    deadline,
                ) {
                    Ok(()) => continue,
                    Err(errno) => errno,
                },
            };
            return if done > 0 {
                Ok((
                    Received {
                        bytes: done,
                        full: done,
                    },
                    None,
                ))
            } else {
                Err(refusal)
            };
        }
    }

    /// `shutdown`: stop receiving, sending, or both.
    ///
    /// Stopping receiving lets this socket read what is queued and then end of
    /// file, and refuses what its peer would still send; stopping sending
    /// refuses this socket's own writes and, for a stream or sequenced-packet
    /// socket, gives its peer end of file once drained -- `unix_shutdown`'s
    /// two halves.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a `how` that is none of the three.
    pub(crate) fn shutdown(&self, how: u32) -> Result<(), Errno> {
        let (read, write) = match how {
            SHUT_RD => (true, false),
            SHUT_WR => (false, true),
            SHUT_RDWR => (true, true),
            _ => return Err(Errno::EINVAL),
        };
        if read {
            self.receive.buffer.lock().close_writer();
            self.receive.refused.store(true, Ordering::Release);
            self.receive.wake_both();
        }
        if write {
            self.options.lock().shut_write = true;
            let peer = self.send.lock().clone();
            if let Some(peer) = peer {
                if self.kind != SocketType::Datagram {
                    peer.buffer.lock().close_writer();
                }
                peer.wake_both();
            }
        }
        Ok(())
    }

    /// What `poll` reports: readable with data or end of file waiting,
    /// writable with room or a write that would fail at once, and hung up
    /// once nothing can come in and nothing can go out.
    ///
    /// A listening socket is none of those. It is readable while a
    /// connection waits to be accepted and is never hung up: it has no peer
    /// because it never has one, not because one went away. That is what
    /// Linux's `unix_poll` answers for one. Answering it hung up, as a socket
    /// with no peer, made every `poll` on it return at once -- and a
    /// compositor polls its display socket and its two control sockets on
    /// every turn of its loop, which spun about a processor of an idle
    /// desktop (`docs/COMPOSITOR-DAMAGE-HANDOFF.md` §2.8).
    fn readiness(&self) -> Readiness {
        if let Some(backlog) = self.listener.lock().as_ref() {
            return Readiness {
                readable: !backlog.waiting.is_empty(),
                writable: false,
                hangup: false,
                error: false,
                priority: false,
            };
        }
        let (readable, ended) = {
            let buffer = self.receive.buffer.lock();
            (buffer.can_read(), buffer.writer_closed())
        };
        let shut_write = self.options.lock().shut_write;
        let peer = self.send.lock().clone();
        let (room, peer_refuses) = match &peer {
            Some(peer) => {
                let refused = peer.is_refused();
                (refused || peer.buffer.lock().can_write(1), refused)
            }
            None => (true, false),
        };
        Readiness {
            readable,
            writable: room || shut_write,
            hangup: (peer.is_none() && self.kind != SocketType::Datagram)
                || (ended && (shut_write || peer_refuses)),
            error: false,
            priority: false,
        }
    }

    /// The socket requests of `ioctl`: `SIOCINQ` (`FIONREAD`), what a read
    /// would find -- the whole queue for a stream or sequenced-packet socket,
    /// the next datagram for a datagram socket -- and `SIOCOUTQ`, what this
    /// socket sent that its peer has not read.
    ///
    /// # Errors
    ///
    /// `ENOTTY` for any other request; `EFAULT` for an unwritable `arg`.
    pub(crate) fn ioctl(&self, process: &Process, request: u32, arg: u64) -> Result<usize, Errno> {
        let count = match request {
            SIOCINQ => {
                let buffer = self.receive.buffer.lock();
                if self.kind == SocketType::Datagram {
                    buffer.next_record().unwrap_or(0)
                } else {
                    buffer.queued()
                }
            }
            SIOCOUTQ => {
                let peer = self.send.lock().clone();
                peer.map_or(0, |peer| peer.buffer.lock().queued())
            }
            _ => return Err(Errno::ENOTTY),
        };
        uaccess::put_u32(
            process.space(),
            arg,
            u32::try_from(count).unwrap_or(u32::MAX),
        )?;
        Ok(0)
    }

    /// The value `getsockopt(SOL_SOCKET, name)` answers, as the bytes a
    /// program reads, with time structures of `width`.
    ///
    /// The options a socket without a network has no use for -- keep-alive,
    /// broadcast, routing, priority, lingering -- read as their defaults.
    ///
    /// # Errors
    ///
    /// `EOPNOTSUPP` for a level other than `SOL_SOCKET`, and `ENOPROTOOPT` for
    /// an option this socket does not have.
    pub(crate) fn get_option(&self, level: i32, name: i32, width: Width) -> Result<Vec<u8>, Errno> {
        if level != SOL_SOCKET {
            return Err(Errno::EOPNOTSUPP);
        }
        let options = *self.options.lock();
        let int = |value: i32| value.to_le_bytes().to_vec();
        let size = |bytes: usize| int(i32::try_from(bytes).unwrap_or(i32::MAX));
        Ok(match name {
            SO_TYPE => int(i32::try_from(self.kind.linux()).unwrap_or(0)),
            SO_DOMAIN => int(i32::from(AF_UNIX)),
            SO_ACCEPTCONN => int(i32::from(self.listener.lock().is_some())),
            SO_PROTOCOL | SO_ERROR | SO_DEBUG | SO_REUSEADDR | SO_KEEPALIVE | SO_BROADCAST
            | SO_DONTROUTE | SO_OOBINLINE | SO_PRIORITY => int(0),
            SO_RCVLOWAT | SO_SNDLOWAT => int(1),
            SO_SNDBUF => size(options.send_buffer),
            SO_RCVBUF => size(self.receive.buffer.lock().capacity()),
            SO_PASSCRED => int(i32::from(options.pass_credentials)),
            SO_PEERCRED => as_seen((*self.peer_credentials.lock()).unwrap_or(Ucred {
                pid: 0,
                uid: OVERFLOW_ID,
                gid: OVERFLOW_ID,
            }))
            .to_bytes()
            .to_vec(),
            SO_LINGER => Linger {
                onoff: 0,
                linger: 0,
            }
            .to_bytes()
            .to_vec(),
            SO_RCVTIMEO_OLD => timeval(options.receive_timeout, width),
            SO_SNDTIMEO_OLD => timeval(options.send_timeout, width),
            SO_RCVTIMEO_NEW => timeval(options.receive_timeout, Width::Bits64),
            SO_SNDTIMEO_NEW => timeval(options.send_timeout, Width::Bits64),
            _ => return Err(Errno::ENOPROTOOPT),
        })
    }

    /// `setsockopt(SOL_SOCKET, name, value)`, with time structures of `width`.
    ///
    /// The options with no effect here are accepted and ignored, as a socket
    /// family that does not use them accepts them on Linux.
    ///
    /// # Errors
    ///
    /// `EOPNOTSUPP` for another level, `ENOPROTOOPT` for an unknown option,
    /// `EINVAL` for a value shorter than an `int` or than its option; `EDOM` for a timeout's microseconds out of range.
    pub(crate) fn set_option(
        &self,
        level: i32,
        name: i32,
        value: &[u8],
        width: Width,
    ) -> Result<(), Errno> {
        if level != SOL_SOCKET {
            return Err(Errno::EOPNOTSUPP);
        }
        // `sk_setsockopt` refuses a value shorter than an `int` before it looks
        // at the option.
        if value.len() < size_of::<i32>() {
            return Err(Errno::EINVAL);
        }
        let int = || {
            value
                .get(..4)
                .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
                .map(i32::from_le_bytes)
                .ok_or(Errno::EINVAL)
        };
        match name {
            SO_SNDBUF | SO_SNDBUFFORCE => self.options.lock().send_buffer = buffer_size(int()?),
            SO_RCVBUF | SO_RCVBUFFORCE => {
                let bytes = buffer_size(int()?);
                self.receive.buffer.lock().set_capacity(bytes);
                self.receive.writable.wake_all();
            }
            SO_PASSCRED => {
                let on = int()? != 0;
                self.options.lock().pass_credentials = on;
                self.receive.wants_credentials.store(on, Ordering::Release);
            }
            SO_RCVTIMEO_OLD => self.options.lock().receive_timeout = read_timeval(value, width)?,
            SO_SNDTIMEO_OLD => self.options.lock().send_timeout = read_timeval(value, width)?,
            SO_RCVTIMEO_NEW => {
                self.options.lock().receive_timeout = read_timeval(value, Width::Bits64)?;
            }
            SO_SNDTIMEO_NEW => {
                self.options.lock().send_timeout = read_timeval(value, Width::Bits64)?;
            }
            SO_DEBUG | SO_REUSEADDR | SO_KEEPALIVE | SO_BROADCAST | SO_DONTROUTE | SO_OOBINLINE
            | SO_PRIORITY | SO_RCVLOWAT | SO_LINGER => {
                let _ = int()?;
            }
            _ => return Err(Errno::ENOPROTOOPT),
        }
        Ok(())
    }
}

/// A buffer size as `SO_SNDBUF` and `SO_RCVBUF` store it: capped at the
/// maximum, doubled for bookkeeping, and no smaller than the minimum --
/// `__sock_set_rcvbuf`'s arithmetic, so a program reads back twice what it set.
fn buffer_size(requested: i32) -> usize {
    // A negative request is a huge unsigned one to Linux, capped like any other.
    let requested = usize::try_from(requested.cast_unsigned()).unwrap_or(SOCKET_BUFFER_MAX);
    requested
        .min(SOCKET_BUFFER_MAX)
        .saturating_mul(2)
        .max(SOCKET_BUFFER_MIN)
}

/// A timeout in nanoseconds as the `timeval` of `width` a program reads.
pub(crate) fn timeval(nanos: u64, width: Width) -> Vec<u8> {
    let mut bytes = vec![0_u8; width.bytes() * 2];
    let _ = width.put_word(&mut bytes, 0, nanos / NANOS_PER_SECOND);
    let _ = width.put_word(
        &mut bytes,
        width.bytes(),
        (nanos % NANOS_PER_SECOND) / NANOS_PER_MICRO,
    );
    bytes
}

/// A `timeval` of `width` as a timeout in nanoseconds: `sock_set_timeout`'s
/// rules. All zero waits forever; negative seconds give up at once.
pub(crate) fn read_timeval(value: &[u8], width: Width) -> Result<u64, Errno> {
    let seconds = width.word(value, 0).ok_or(Errno::EINVAL)?;
    let micros = width.word(value, width.bytes()).ok_or(Errno::EINVAL)?;
    // Signed fields, of the width's size.
    let (seconds, micros) = match width {
        Width::Bits64 => (seconds.cast_signed(), micros.cast_signed()),
        Width::Bits32 => (
            i64::from((seconds as u32).cast_signed()),
            i64::from((micros as u32).cast_signed()),
        ),
    };
    let micros = u64::try_from(micros)
        .ok()
        .filter(|&micros| micros < NANOS_PER_SECOND / NANOS_PER_MICRO)
        .ok_or(Errno::EDOM)?;
    let Ok(seconds) = u64::try_from(seconds) else {
        // The shortest wait there is: a timeout that has already passed.
        return Ok(1);
    };
    Ok(seconds
        .saturating_mul(NANOS_PER_SECOND)
        .saturating_add(micros * NANOS_PER_MICRO))
}

impl Drop for Socket {
    fn drop(&mut self) {
        // The name goes first, so that a `connect` racing this drop finds
        // nothing rather than a socket whose channels are already closing.
        if let Some(name) = self.bound.lock().take() {
            sockname::forget(&name, self.net_ns);
        }
        // Nobody reads this direction again: what is queued goes, and a writer
        // sees a broken socket. Taken out under the lock, dropped after it.
        let dropped = self.receive.buffer.lock().close_reader();
        self.receive.refused.store(true, Ordering::Release);
        self.receive.wake_both();
        drop(dropped);
        // Nothing more is sent: the peer reads end of file once drained, unless
        // it is a datagram socket, whose reads end only with its own shutdown.
        let peer = self.send.lock().take();
        if let Some(peer) = peer {
            if self.kind != SocketType::Datagram {
                peer.buffer.lock().close_writer();
            }
            peer.wake_both();
        }
    }
}

impl Inode for Socket {
    fn metadata(&self) -> Metadata {
        self.metadata
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn poll(&self) -> Readiness {
        self.readiness()
    }

    /// Every queue its readiness reads, as [`Inode::poll_changes`] sums them.
    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        visit(fs::wake::shared(&self.receive.readable));
        visit(fs::wake::shared(&self.receive.writable));
        if let Some(peer) = self.send.lock().as_ref() {
            visit(fs::wake::shared(&peer.readable));
            visit(fs::wake::shared(&peer.writable));
        }
        visit(fs::wake::shared(&self.arrivals));
        true
    }

    /// Every queue its readiness reads: its own direction's two, its peer's
    /// two while it has one, and the backlog's.
    fn poll_changes(&self) -> Option<u64> {
        let own = self
            .receive
            .readable
            .wakes()
            .wrapping_add(self.receive.writable.wakes());
        let peer = self.send.lock().as_ref().map_or(0, |peer| {
            peer.readable.wakes().wrapping_add(peer.writable.wakes())
        });
        Some(own.wrapping_add(peer).wrapping_add(self.arrivals.wakes()))
    }

    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> ferrix_vfs::Result<usize> {
        self.recv(buf, 0, nonblock).map(|received| received.bytes)
    }

    /// A stream socket's read takes all that is queued, as Linux's
    /// `unix_stream_read_generic` goes on through the socket's buffers:
    /// measured on a 7.0 host, `readv` of a socketpair holding six bytes
    /// into two segments of four is 6, and `read` of 65536 from one holding
    /// writes of 4096, 4096 and 100 bytes is 8292. A sequenced-packet or
    /// datagram socket reads one record, and does not.
    fn fills_reads(&self) -> bool {
        self.kind == SocketType::Stream
    }

    /// The rest of a stream read, up to the ancillary boundaries one read
    /// into the whole buffer would have stopped at
    /// ([`SocketBuffer::read_on`]), and never waiting. What came with bytes it
    /// ran into is dropped here, with no buffer locked, closing the files, as
    /// the `read` it goes on does and as Linux's `read` of a socket does.
    fn read_on(&self, buf: &mut [u8]) -> ferrix_vfs::Result<usize> {
        // The guard goes at the end of this statement, before `passed` does.
        let (taken, passed) = self.receive.buffer.lock().read_on(buf);
        if passed.as_ref().is_some_and(Passed::carries_sockets) {
            moved_in_flight();
        }
        if taken > 0 {
            self.receive.writable.wake_all();
        }
        drop(passed);
        Ok(taken)
    }

    fn write_stream(&self, data: &[u8], nonblock: bool) -> ferrix_vfs::Result<usize> {
        self.send(data, 0, nonblock)
    }
}

// ---------------------------------------------------------------------------
// sockfs
// ---------------------------------------------------------------------------

/// The filesystem every socket is on: Linux's `sockfs`, which a program only
/// ever meets as `fstat`'s device and `fstatfs`'s magic number.
#[derive(Debug)]
struct SockFs {
    device: u64,
    root: Arc<dyn Inode>,
    /// The next socket's inode number; each socket has its own, as on Linux.
    next_ino: AtomicU64,
}

/// The one sockfs.
static SOCKFS: Once<Arc<SockFs>> = Once::new();

/// Whether `file` is a socket: an open file on sockfs, which a path reaches
/// only through `/proc/<pid>/fd/<n>`.
pub(crate) fn holds(file: &OpenFile) -> bool {
    file.location().mount.filesystem().name() == sockfs().name()
}

/// The one sockfs, made on first use.
fn sockfs() -> &'static Arc<SockFs> {
    SOCKFS.call_once(|| {
        Arc::new(SockFs {
            device: fs::anonymous_device(),
            root: Arc::new(SockRoot),
            next_ino: AtomicU64::new(2),
        })
    })
}

impl FileSystem for SockFs {
    fn root(&self) -> Arc<dyn Inode> {
        Arc::clone(&self.root)
    }

    fn name(&self) -> &'static str {
        "sockfs"
    }

    fn device(&self) -> u64 {
        self.device
    }

    fn statfs(&self) -> StatFs {
        StatFs {
            magic: SOCKFS_MAGIC,
            block_size: u64::from(BLOCK_SIZE),
            name_max: NAME_MAX as u64,
            ..StatFs::default()
        }
    }
}

/// sockfs's root directory, which is empty and which nothing can reach: every
/// socket's location is detached. It exists because a filesystem has a root.
#[derive(Debug)]
struct SockRoot;

impl Inode for SockRoot {
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

/// An open file on `socket`, at a location of its own on sockfs, named as
/// `/proc/self/fd` shows a socket.
fn open(socket: Arc<Socket>, nonblock: bool) -> Result<Arc<OpenFile>, Errno> {
    let name = format!("socket:[{}]", socket.metadata.ino);
    let flags = OpenFlags {
        read: true,
        write: true,
        nonblock,
        ..OpenFlags::default()
    };
    let sockfs: Arc<SockFs> = Arc::clone(sockfs());
    // Where the description's sleeping lock waits; a stream never takes it.
    let parker = Arc::clone(fs::namespace().parker());
    OpenFile::new(
        Location::detached(sockfs, socket, name.as_bytes(), parker)?,
        &flags,
    )
}

/// A socket of `kind` connected to nothing, as the open file `socket`
/// installs, owned by `owner`.
///
/// # Errors
///
/// Whatever [`OpenFile::new`] refuses, which for a socket is nothing.
pub(crate) fn new_socket(
    kind: SocketType,
    nonblock: bool,
    creator: &Process,
) -> Result<Arc<OpenFile>, Errno> {
    let credentials = credentials_of(creator);
    let owner = crate::syscall::path::creator_ids(creator);
    open(
        Socket::new(
            kind,
            None,
            Channel::new(kind)?,
            credentials,
            None,
            owner,
            creator.net_ns().id(),
        )?,
        nonblock,
    )
}

/// Two sockets of `kind` connected to each other, as the open files
/// `socketpair` installs; each reports `creator`'s credentials as its peer's.
///
/// # Errors
///
/// Whatever [`OpenFile::new`] refuses, which for a socket is nothing.
pub(crate) fn new_pair(
    kind: SocketType,
    nonblock: bool,
    creator: &Process,
) -> Result<(Arc<OpenFile>, Arc<OpenFile>), Errno> {
    let first = Channel::new(kind)?;
    let second = Channel::new(kind)?;
    let credentials = Some(credentials_of(creator));
    let owner = crate::syscall::path::creator_ids(creator);
    let mine = credentials_of(creator);
    let one = Socket::new(
        kind,
        Some(Arc::clone(&second)),
        Arc::clone(&first),
        mine,
        credentials,
        owner,
        creator.net_ns().id(),
    )?;
    let other = Socket::new(
        kind,
        Some(first),
        second,
        mine,
        credentials,
        owner,
        creator.net_ns().id(),
    )?;
    Ok((open(one, nonblock)?, open(other, nonblock)?))
}

/// A bound name as `getsockname` and `getpeername` write it, or the unnamed
/// address -- `sun_family` and nothing after it -- when there is none.
fn encode_name(name: Option<&Name>) -> Vec<u8> {
    let address = match name {
        Some(Name::Path { path, .. }) => UnixAddress::Path(path),
        Some(Name::Abstract(bytes)) => UnixAddress::Abstract(bytes),
        None => UnixAddress::Unnamed,
    };
    let mut encoded = [0_u8; SOCKADDR_UN_SIZE + 1];
    let written = address.encode(&mut encoded).unwrap_or(0);
    encoded.get(..written).unwrap_or_default().to_vec()
}

/// One socket as a pass over sockets in flight sees it.
#[derive(Debug)]
struct Seen {
    /// The socket, held for the length of the pass.
    socket: Arc<Socket>,
    /// The sockets its receive queue holds, and its waiting connections'
    /// queues, by their index in the pass: once for every time one is queued.
    holds: Vec<usize>,
    /// References to its open file, as the last queue that holds it saw them.
    references: usize,
    /// How many times a queue holds it.
    queued: usize,
}

/// Find the sockets that only queues refer to and that no queue a program can
/// read holds, and empty their queues: Linux's `unix_gc`. Answers how many
/// sockets' queues were emptied. See the module documentation.
///
/// Called after a descriptor is closed; costs one atomic while nothing that
/// is a socket is in flight. Must not be called holding a lock.
pub(crate) fn collect_cycles() -> usize {
    if IN_FLIGHT.load(Ordering::Acquire) == 0 {
        return 0;
    }
    if COLLECTING.swap(true, Ordering::AcqRel) {
        return 0;
    }
    let collected = collect_once();
    COLLECTING.store(false, Ordering::Release);
    collected
}

/// One pass of [`collect_cycles`].
fn collect_once() -> usize {
    let epoch = FLIGHT_EPOCH.load(Ordering::Acquire);
    let seen = scan();
    let unreachable = unreachable(&seen);
    // Nothing carrying a socket moved while the queues were looked at, or the
    // picture is not one moment's and the next close tries again.
    if FLIGHT_EPOCH.load(Ordering::Acquire) != epoch {
        return 0;
    }
    let mut dropped: Vec<Passed> = Vec::new();
    for entry in unreachable.iter().filter_map(|&at| seen.get(at)) {
        for queue in queues_of(&entry.socket) {
            let mut buffer = queue.receive.buffer.lock();
            // Everything queued leaves with the lock held and is dropped after
            // it, sockets or not.
            dropped.extend(buffer.drain());
            drop(buffer);
            queue.receive.wake_both();
        }
    }
    // The files first, then the pass's own references to the sockets, each with
    // no lock held: a socket that goes here finds its own queue empty.
    drop(dropped);
    drop(seen);
    unreachable.len()
}

/// A socket's receive queue and its waiting connections' queues, as sockets.
fn queues_of(socket: &Arc<Socket>) -> Vec<Arc<Socket>> {
    let mut queues = vec![Arc::clone(socket)];
    if let Some(backlog) = socket.listener.lock().as_ref() {
        queues.extend(
            backlog
                .waiting
                .iter()
                .map(|waiting| Arc::clone(&waiting.socket)),
        );
    }
    queues
}

/// Every live socket, with what each one's queues hold and how often each is
/// held.
fn scan() -> Vec<Seen> {
    let sockets: Vec<Arc<Socket>> = {
        let mut listed = SOCKETS.lock();
        listed.retain(|socket| socket.strong_count() > 0);
        listed.iter().filter_map(Weak::upgrade).collect()
    };
    let mut seen: Vec<Seen> = sockets
        .into_iter()
        .map(|socket| Seen {
            socket,
            holds: Vec::new(),
            references: 0,
            queued: 0,
        })
        .collect();
    for at in 0..seen.len() {
        let found = seen
            .get(at)
            .map(|entry| held_by(&seen, &entry.socket))
            .unwrap_or_default();
        for (held, references) in found {
            if let Some(entry) = seen.get_mut(at) {
                entry.holds.push(held);
            }
            if let Some(target) = seen.get_mut(held) {
                target.queued += 1;
                target.references = references;
            }
        }
    }
    seen
}

/// The sockets `socket`'s queues hold, by index in `seen`, each with the
/// references to its open file as they are now.
fn held_by(seen: &[Seen], socket: &Arc<Socket>) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    for queue in queues_of(socket) {
        let buffer = queue.receive.buffer.lock();
        let files = buffer
            .ancillary()
            .filter(|passed| passed.carries_sockets())
            .flat_map(Passed::files);
        for file in files {
            let inode = Arc::as_ptr(file.io()).cast::<()>();
            let held = seen
                .iter()
                .position(|entry| Arc::as_ptr(&entry.socket).cast::<()>() == inode);
            if let Some(held) = held {
                found.push((held, Arc::strong_count(file)));
            }
        }
    }
    found
}

/// The indices of the sockets only queues refer to that no queue a program
/// can read holds.
fn unreachable(seen: &[Seen]) -> Vec<usize> {
    let candidate: Vec<bool> = seen
        .iter()
        .map(|entry| entry.queued > 0 && entry.references == entry.queued)
        .collect();
    let is_candidate = |at: usize| candidate.get(at).copied().unwrap_or(false);
    let mut inside = vec![0_usize; seen.len()];
    let held_by_candidates = seen
        .iter()
        .enumerate()
        .filter(|&(at, _)| is_candidate(at))
        .flat_map(|(_, entry)| entry.holds.iter().copied());
    for held in held_by_candidates {
        if let Some(count) = inside.get_mut(held) {
            *count += 1;
        }
    }
    let mut reachable: Vec<bool> = seen
        .iter()
        .enumerate()
        .map(|(at, entry)| is_candidate(at) && entry.queued > inside.get(at).copied().unwrap_or(0))
        .collect();
    let mut pending: Vec<usize> = (0..seen.len())
        .filter(|&at| reachable.get(at).copied().unwrap_or(false))
        .collect();
    while let Some(at) = pending.pop() {
        let holds = seen.get(at).map_or(&[][..], |entry| entry.holds.as_slice());
        for &held in holds {
            if is_candidate(held) && reachable.get(held) == Some(&false) {
                if let Some(mark) = reachable.get_mut(held) {
                    *mark = true;
                }
                pending.push(held);
            }
        }
    }
    (0..seen.len())
        .filter(|&at| is_candidate(at) && reachable.get(at) == Some(&false))
        .collect()
}

/// The socket an open file reads and writes through, if it is one.
pub(crate) fn of(file: &OpenFile) -> Option<Arc<Socket>> {
    Arc::clone(file.io()).into_any().downcast::<Socket>().ok()
}

/// The next inode number on sockfs.
///
/// Every socket has one of its own, whatever family it is: `/proc/self/fd`
/// shows it and `fstat` reports it, and two sockets sharing a number would be
/// two sockets a program cannot tell apart.
pub(crate) fn next_ino() -> u64 {
    sockfs().next_ino.fetch_add(1, Ordering::Relaxed)
}

/// The metadata a socket of any family reports: `S_IFSOCK | 0777` on sockfs,
/// as on Linux.
pub(crate) fn socket_metadata(ino: u64, (uid, gid): (u32, u32)) -> Metadata {
    let now = fs::clock().now();
    Metadata {
        ino,
        kind: FileType::Socket,
        permissions: 0o777,
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
    }
}

/// An open file on a socket of any family, at a detached location on sockfs.
///
/// This is what puts an `AF_INET` socket on the same filesystem as an
/// `AF_UNIX` one, so that `fstat`, `fstatfs` and `/proc/self/fd` answer the
/// same way for both without the net core knowing what sockfs is.
///
/// # Errors
///
/// Whatever [`OpenFile::new`] refuses, which for a socket is nothing.
pub(crate) fn open_on_sockfs(
    inode: Arc<dyn Inode>,
    ino: u64,
    nonblock: bool,
) -> Result<Arc<OpenFile>, Errno> {
    let name = format!("socket:[{ino}]");
    let flags = OpenFlags {
        read: true,
        write: true,
        nonblock,
        ..OpenFlags::default()
    };
    let sockfs: Arc<SockFs> = Arc::clone(sockfs());
    let parker = Arc::clone(fs::namespace().parker());
    OpenFile::new(
        Location::detached(sockfs, inode, name.as_bytes(), parker)?,
        &flags,
    )
}

/// Sleep on `queue` until `ready`, the caller has a signal to take, or
/// `deadline` passes, answering the way a socket call answers.
///
/// Shared with the net core so that an `AF_INET` socket's wait is the same
/// wait an `AF_UNIX` one makes, down to which errno a signal turns into.
pub(crate) fn wait_on(
    queue: &WaitQueue,
    ready: impl FnMut() -> bool,
    deadline: u64,
) -> Result<(), Errno> {
    wait(queue, ready, deadline)
}

/// When a wait with `timeout` nanoseconds of patience must give up; zero waits
/// forever.
pub(crate) fn deadline_after(timeout: u64) -> u64 {
    deadline(timeout)
}
