//! Inotify: what happens to files and directories, read from a descriptor --
//! the object behind `inotify_init1`, `inotify_add_watch` and
//! `inotify_rm_watch`.
//!
//! A program asks to watch a node and names the events it wants; each one
//! that then happens is queued on its descriptor as a `struct inotify_event`,
//! which `read` returns and `poll` and epoll wait for. A watch on a
//! directory also hears its entries' events, with the entry's name: a file
//! created, changed, closed after writing, moved or deleted in it. It is what
//! a desktop's file watchers, a compositor's device rescan and Chrome use.
//!
//! Linux's `fs/notify/inotify`, for what is here:
//!
//! * A node is its device and inode number, what `stat` reports, so a watch
//!   holds on to no inode and follows the node under any of its names.
//! * The calls that change a node report after they have succeeded:
//!   [`node_event`] for what happened to a node, sent to its watches and,
//!   named, to its directory's; [`dir_event`] for a name that came or went;
//!   [`self_event`] for a node deleted or moved, which ends a deleted node's
//!   watches with `IN_IGNORED`.
//! * An event the same as the last one queued is not queued again, as
//!   Linux's `inotify_merge` drops it; a full queue ends in one
//!   `IN_Q_OVERFLOW` with wd -1.
//! * A name is padded with zero bytes to a multiple of the event's own
//!   sixteen bytes, and `read` returns whole events only: `EINVAL` for a
//!   buffer too small for the first.
//!
//! Nothing here costs a call that changes a file anything until a watch
//! exists: every report first reads one counter.
//!
//! Not yet: `IN_UNMOUNT`, `IN_EXCL_UNLINK` (accepted and not acted on), the
//! per-user limits under `/proc/sys/fs/inotify`, and events a file mapped
//! shared and written through its mapping would give.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use ferrix_kmem::{Charge, arc_footprint};
use ferrix_vfs::{Errno, FileType, Inode, Location, Metadata, OpenFile, Readiness};

use crate::fallible;
use crate::fs;
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::syscall::process::{self, Process};

/// The node was read.
pub(crate) const IN_ACCESS: u32 = 0x0000_0001;
/// The node was written.
pub(crate) const IN_MODIFY: u32 = 0x0000_0002;
/// Its metadata changed: permissions, owner, times or links.
pub(crate) const IN_ATTRIB: u32 = 0x0000_0004;
/// A description open for writing was closed.
pub(crate) const IN_CLOSE_WRITE: u32 = 0x0000_0008;
/// A description not open for writing was closed.
pub(crate) const IN_CLOSE_NOWRITE: u32 = 0x0000_0010;
/// It was opened.
pub(crate) const IN_OPEN: u32 = 0x0000_0020;
/// A name was moved out of the directory.
pub(crate) const IN_MOVED_FROM: u32 = 0x0000_0040;
/// A name was moved into the directory.
pub(crate) const IN_MOVED_TO: u32 = 0x0000_0080;
/// A name was made in the directory.
pub(crate) const IN_CREATE: u32 = 0x0000_0100;
/// A name was removed from the directory.
pub(crate) const IN_DELETE: u32 = 0x0000_0200;
/// The watched node itself was deleted.
pub(crate) const IN_DELETE_SELF: u32 = 0x0000_0400;
/// The watched node itself was moved.
pub(crate) const IN_MOVE_SELF: u32 = 0x0000_0800;
/// The queue overflowed; its wd is -1.
const IN_Q_OVERFLOW: u32 = 0x0000_4000;
/// The watch is gone: removed, one-shot and fired, or its node deleted.
const IN_IGNORED: u32 = 0x0000_8000;
/// Watch the path only if it is a directory.
const IN_ONLYDIR: u32 = 0x0100_0000;
/// Do not follow a final symbolic link.
const IN_DONT_FOLLOW: u32 = 0x0200_0000;
/// Ignore events on names unlinked from a watched directory (accepted).
const IN_EXCL_UNLINK: u32 = 0x0400_0000;
/// Refuse to change a watch that exists: `EEXIST`.
const IN_MASK_CREATE: u32 = 0x1000_0000;
/// Add to a watch's mask rather than replace it.
const IN_MASK_ADD: u32 = 0x2000_0000;
/// The event's subject is a directory.
const IN_ISDIR: u32 = 0x4000_0000;
/// Remove the watch after its first event.
const IN_ONESHOT: u32 = 0x8000_0000;
/// Every event a watch can ask for.
const IN_ALL_EVENTS: u32 = 0x0000_0fff;
/// Events only a watch on the node itself hears, never its directory's.
const SELF_ONLY: u32 = IN_DELETE_SELF | IN_MOVE_SELF;
/// What `inotify_add_watch` accepts beyond the events.
const WATCH_FLAGS: u32 =
    IN_ONLYDIR | IN_DONT_FOLLOW | IN_EXCL_UNLINK | IN_MASK_CREATE | IN_MASK_ADD | IN_ONESHOT;

/// `IN_CLOEXEC`, which is `O_CLOEXEC`.
pub(crate) const IN_CLOEXEC: u32 = ferrix_linux_abi::types::O_CLOEXEC;
/// `IN_NONBLOCK`, which is `O_NONBLOCK`.
pub(crate) const IN_NONBLOCK: u32 = ferrix_linux_abi::types::O_NONBLOCK;

/// Bytes of a `struct inotify_event` before its name, and what a name is
/// padded to a multiple of.
const EVENT_BYTES: usize = 16;

/// Linux's default `max_queued_events`.
const MAX_QUEUED: usize = 16_384;

/// Linux's default `max_user_watches` on a small machine, here per instance.
const MAX_WATCHES: usize = 8_192;

/// The name `/proc/self/fd` shows.
const NAME: &[u8] = b"anon_inode:inotify";

/// How long a blocked read sleeps between its own looks, as an eventfd's:
/// every queued event wakes the queue.
const RECHECK: u64 = fs::wake::TRUSTED_RECHECK_NANOS;

/// A node: its filesystem's device and its inode number.
type Key = (u64, u64);

/// One instance's watch on one node.
struct Watch {
    instance: Weak<Inotify>,
    wd: i32,
    mask: u32,
}

/// Every watch, by the node it is on.
static WATCHES: SpinLock<BTreeMap<Key, Vec<Watch>>> = SpinLock::new(BTreeMap::new());

/// How many watches exist, which is what every report reads first.
static WATCHING: AtomicUsize = AtomicUsize::new(0);

/// The cookie that joins a rename's two halves.
static COOKIE: AtomicU32 = AtomicU32::new(1);

/// An inotify instance.
pub(crate) struct Inotify {
    state: SpinLock<State>,
    /// Woken as an event is queued.
    readable: Arc<WaitQueue>,
    /// Itself, charged to the job that made it (F-37).
    _charge: Charge,
}

/// What an instance holds.
struct State {
    /// The events, as `read` returns them.
    queue: VecDeque<u8>,
    /// Where the last event starts in `queue`, for merging a repeat.
    last: Option<usize>,
    /// How many events are queued.
    events: usize,
    /// The queue's heap and the watches', charged to the maker's job.
    charge: Charge,
    /// Its watches: wd to node.
    watches: BTreeMap<i32, Key>,
    /// The next watch descriptor.
    next_wd: i32,
}

impl fmt::Debug for Inotify {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Inotify").finish_non_exhaustive()
    }
}

/// Whether anything is watched at all.
pub(crate) fn watching() -> bool {
    WATCHING.load(Ordering::Relaxed) != 0
}

/// A cookie for a rename's `IN_MOVED_FROM` and `IN_MOVED_TO`.
pub(crate) fn next_cookie() -> u32 {
    COOKIE.fetch_add(1, Ordering::Relaxed)
}

/// The node at `at`, and whether it is a directory.
pub(crate) fn key_of(at: &Location) -> Option<(Key, bool)> {
    let stat = fs::namespace().stat(at).ok()?;
    Some((
        (stat.dev, stat.metadata.ino),
        stat.metadata.kind == FileType::Directory,
    ))
}

/// `mask` happened to the node at `at`: tell its watches, and its
/// directory's with its name.
pub(crate) fn node_event(at: &Location, mask: u32) {
    if !watching() || at.is_detached() {
        return;
    }
    let Some((key, dir)) = key_of(at) else {
        return;
    };
    let mask = if dir { mask | IN_ISDIR } else { mask };
    report(key, mask, 0, &[]);
    if mask & SELF_ONLY != 0 || at.dentry.is_unhashed() {
        return;
    }
    let parent = at.parent();
    // A mount's root has no directory on its own filesystem to be named in.
    if parent.same(at) || !Arc::ptr_eq(&parent.mount, &at.mount) {
        return;
    }
    if let Some((parent_key, _)) = key_of(&parent) {
        report(parent_key, mask, 0, &at.dentry.name());
    }
}

/// `mask` happened to `name` in the directory at `dir`: made, removed or
/// moved. `is_dir` says what the name is.
pub(crate) fn dir_event(dir: &Location, name: &[u8], mask: u32, cookie: u32, is_dir: bool) {
    if !watching() {
        return;
    }
    if let Some((key, _)) = key_of(dir) {
        report(
            key,
            if is_dir { mask | IN_ISDIR } else { mask },
            cookie,
            name,
        );
    }
}

/// `mask` happened to the node `key` itself, which may be gone from every
/// directory: `IN_DELETE_SELF` ends its watches.
pub(crate) fn self_event(key: Key, mask: u32, is_dir: bool) {
    if !watching() {
        return;
    }
    report(key, if is_dir { mask | IN_ISDIR } else { mask }, 0, &[]);
}

/// Queue `mask` on every watch of `key` that asked for it.
fn report(key: Key, mask: u32, cookie: u32, name: &[u8]) {
    let mut all = WATCHES.lock();
    let Some(list) = all.get_mut(&key) else {
        return;
    };
    let ends = mask & IN_DELETE_SELF != 0;
    let before = list.len();
    list.retain(|watch| {
        let Some(instance) = watch.instance.upgrade() else {
            return false;
        };
        let wanted = watch.mask & mask & IN_ALL_EVENTS != 0;
        if wanted {
            instance.queue(watch.wd, mask, cookie, name);
        }
        let over = ends || (wanted && watch.mask & IN_ONESHOT != 0);
        if over {
            instance.queue(watch.wd, IN_IGNORED, 0, &[]);
            instance.forget(watch.wd);
        }
        // Should this be the last hold -- its descriptor closed meanwhile --
        // its drop finds the lock taken and leaves its watches to be pruned
        // here, dead, by a later report.
        drop(instance);
        !over
    });
    let removed = before - list.len();
    if list.is_empty() {
        let _ = all.remove(&key);
    }
    drop(all);
    let _ = WATCHING.fetch_sub(removed, Ordering::Relaxed);
}

impl Inotify {
    /// Queue one event and wake readers; past the limit, or with no room,
    /// one `IN_Q_OVERFLOW` instead.
    fn queue(&self, wd: i32, mask: u32, cookie: u32, name: &[u8]) {
        let mut state = self.state.lock();
        let padded = if name.is_empty() {
            0
        } else {
            (name.len() + 1).next_multiple_of(EVENT_BYTES)
        };
        let event = header(wd, mask, cookie, padded);
        if state.repeats(&event, name, padded) {
            return;
        }
        let overflow = state.events >= MAX_QUEUED - 1;
        let fits = !overflow && state.room(EVENT_BYTES + padded);
        if fits {
            let at = state.queue.len();
            state.queue.extend(event);
            state.queue.extend(name);
            state
                .queue
                .extend(core::iter::repeat_n(0, padded - name.len()));
            state.last = Some(at);
            state.events += 1;
        } else if !state.overflowed() && state.room(EVENT_BYTES) {
            let at = state.queue.len();
            state.queue.extend(header(-1, IN_Q_OVERFLOW, 0, 0));
            state.last = Some(at);
            state.events += 1;
        }
        drop(state);
        self.readable.wake_all();
    }

    /// Forget watch `wd`, which its node's list no longer holds.
    fn forget(&self, wd: i32) {
        let mut state = self.state.lock();
        let _ = state.watches.remove(&wd);
        state
            .charge
            .shrink(size_of::<Watch>() + size_of::<(i32, Key)>());
    }

    /// The events queued, as bytes: `FIONREAD`'s answer.
    pub(crate) fn queued_bytes(&self) -> usize {
        self.state.lock().queue.len()
    }
}

/// A `struct inotify_event` before its name, in the machine's byte order.
fn header(wd: i32, mask: u32, cookie: u32, padded: usize) -> [u8; EVENT_BYTES] {
    let length = u32::try_from(padded).unwrap_or(0);
    let mut event = [0_u8; EVENT_BYTES];
    let fields = wd
        .to_ne_bytes()
        .into_iter()
        .chain(mask.to_ne_bytes())
        .chain(cookie.to_ne_bytes())
        .chain(length.to_ne_bytes());
    for (slot, byte) in event.iter_mut().zip(fields) {
        *slot = byte;
    }
    event
}

impl State {
    /// Whether the event is the one queued last, name and all.
    fn repeats(&self, event: &[u8; EVENT_BYTES], name: &[u8], padded: usize) -> bool {
        let Some(at) = self.last else {
            return false;
        };
        let length = EVENT_BYTES + padded;
        if self.queue.len() != at + length {
            return false;
        }
        self.queue
            .range(at..at + EVENT_BYTES)
            .copied()
            .eq(event.iter().copied())
            && self
                .queue
                .range(at + EVENT_BYTES..at + EVENT_BYTES + name.len())
                .copied()
                .eq(name.iter().copied())
    }

    /// Whether the last event queued is an overflow.
    fn overflowed(&self) -> bool {
        let Some(at) = self.last else {
            return false;
        };
        let mut mask = [0_u8; 4];
        for (slot, byte) in mask.iter_mut().zip(self.queue.range(at + 4..at + 8)) {
            *slot = *byte;
        }
        u32::from_ne_bytes(mask) == IN_Q_OVERFLOW
    }

    /// Move as many whole events as fit into `buf`: `None` while there are
    /// none, `EINVAL` when not even the first fits.
    fn take(&mut self, buf: &mut [u8]) -> Option<ferrix_vfs::Result<usize>> {
        if self.queue.is_empty() {
            return None;
        }
        let mut taken = 0;
        let mut events = 0;
        while taken < self.queue.len() {
            let whole = EVENT_BYTES + self.name_bytes(taken);
            if taken + whole > buf.len() {
                break;
            }
            taken += whole;
            events += 1;
        }
        if taken == 0 {
            return Some(Err(Errno::EINVAL));
        }
        for (slot, byte) in buf.iter_mut().zip(self.queue.drain(..taken)) {
            *slot = byte;
        }
        self.events -= events;
        self.last = self.last.and_then(|at| at.checked_sub(taken));
        Some(Ok(taken))
    }

    /// The padded name length of the event queued at `at`.
    fn name_bytes(&self, at: usize) -> usize {
        let mut length = [0_u8; 4];
        for (slot, byte) in length.iter_mut().zip(self.queue.range(at + 12..at + 16)) {
            *slot = *byte;
        }
        u32::from_ne_bytes(length) as usize
    }

    /// Make room for `bytes` more, charged to the maker's job.
    fn room(&mut self, bytes: usize) -> bool {
        ferrix_kmem::reserve_deque(&mut self.queue, bytes, &mut self.charge).is_ok()
    }
}

/// A new instance, as the open file `inotify_init1` installs.
///
/// # Errors
///
/// `ENOMEM` past the job's memory limit.
pub(crate) fn create(nonblock: bool) -> Result<Arc<OpenFile>, Errno> {
    let charge =
        Charge::bytes(arc_footprint::<Inotify>().saturating_add(arc_footprint::<WaitQueue>()))
            .map_err(|_| Errno::ENOMEM)?;
    let owner = charge.owner();
    let instance = fallible::try_arc(Inotify {
        state: SpinLock::new(State {
            queue: VecDeque::new(),
            last: None,
            events: 0,
            charge: Charge::to(owner, 0).map_err(|_| Errno::ENOMEM)?,
            watches: BTreeMap::new(),
            next_wd: 1,
        }),
        readable: fallible::try_arc(WaitQueue::new()).map_err(|_| Errno::ENOMEM)?,
        _charge: charge,
    })
    .map_err(|_| Errno::ENOMEM)?;
    fs::anon::open(instance, NAME, nonblock)
}

/// The instance an open file is, if it is one.
pub(crate) fn of(file: &OpenFile) -> Option<Arc<Inotify>> {
    Arc::clone(file.io()).into_any().downcast::<Inotify>().ok()
}

/// `inotify_add_watch` on the node at `at`: its wd, the same one again for
/// a node this instance watches already.
///
/// # Errors
///
/// `EINVAL` for a mask with no event, both `IN_MASK_ADD` and
/// `IN_MASK_CREATE`, or an unknown flag; `ENOTDIR` for `IN_ONLYDIR` on
/// something else; `EEXIST` for `IN_MASK_CREATE` on a watched node;
/// `ENOSPC` past the watch limit; `ENOMEM`.
pub(crate) fn add_watch(instance: &Arc<Inotify>, at: &Location, mask: u32) -> Result<i32, Errno> {
    if mask & IN_ALL_EVENTS == 0
        || mask & !(IN_ALL_EVENTS | WATCH_FLAGS) != 0
        || mask & (IN_MASK_ADD | IN_MASK_CREATE) == IN_MASK_ADD | IN_MASK_CREATE
    {
        return Err(Errno::EINVAL);
    }
    let (key, dir) = key_of(at).ok_or(Errno::ENOENT)?;
    if mask & IN_ONLYDIR != 0 && !dir {
        return Err(Errno::ENOTDIR);
    }
    let kept = mask & (IN_ALL_EVENTS | IN_ONESHOT | IN_EXCL_UNLINK);
    // The node's list first, then the instance's state, as `report` takes them.
    let mut all = WATCHES.lock();
    let mut state = instance.state.lock();
    if let Some(list) = all.get_mut(&key)
        && let Some(watch) = list
            .iter_mut()
            .find(|watch| Weak::ptr_eq(&watch.instance, &Arc::downgrade(instance)))
    {
        if mask & IN_MASK_CREATE != 0 {
            return Err(Errno::EEXIST);
        }
        watch.mask = if mask & IN_MASK_ADD != 0 {
            watch.mask | kept
        } else {
            kept
        };
        return Ok(watch.wd);
    }
    if state.watches.len() >= MAX_WATCHES {
        return Err(Errno::ENOSPC);
    }
    state
        .charge
        .grow(size_of::<Watch>() + size_of::<(i32, Key)>())
        .map_err(|_| Errno::ENOMEM)?;
    let wd = state.next_wd;
    let watch = Watch {
        instance: Arc::downgrade(instance),
        wd,
        mask: kept,
    };
    let added = match all.get_mut(&key) {
        Some(list) => list.try_reserve(1).map(|()| list.push(watch)).is_ok(),
        None => {
            let mut list = Vec::new();
            list.try_reserve(1).is_ok() && {
                list.push(watch);
                fallible::insert(&mut all, key, list).is_ok()
            }
        }
    };
    if !added || fallible::insert(&mut state.watches, wd, key).is_err() {
        state
            .charge
            .shrink(size_of::<Watch>() + size_of::<(i32, Key)>());
        if added {
            remove_from(&mut all, key, wd, instance);
        }
        return Err(Errno::ENOMEM);
    }
    state.next_wd = wd.checked_add(1).unwrap_or(1);
    let _ = WATCHING.fetch_add(1, Ordering::Relaxed);
    Ok(wd)
}

/// Take watch `wd` of `instance` out of `key`'s list.
fn remove_from(all: &mut BTreeMap<Key, Vec<Watch>>, key: Key, wd: i32, instance: &Arc<Inotify>) {
    if let Some(list) = all.get_mut(&key) {
        list.retain(|watch| {
            !(watch.wd == wd && Weak::ptr_eq(&watch.instance, &Arc::downgrade(instance)))
        });
        if list.is_empty() {
            let _ = all.remove(&key);
        }
    }
}

/// `inotify_rm_watch`: end watch `wd`, which queues its `IN_IGNORED`.
///
/// # Errors
///
/// `EINVAL` for a wd this instance does not have.
pub(crate) fn rm_watch(instance: &Arc<Inotify>, wd: i32) -> Result<(), Errno> {
    let mut all = WATCHES.lock();
    let key = *instance
        .state
        .lock()
        .watches
        .get(&wd)
        .ok_or(Errno::EINVAL)?;
    remove_from(&mut all, key, wd, instance);
    drop(all);
    let _ = WATCHING.fetch_sub(1, Ordering::Relaxed);
    instance.forget(wd);
    instance.queue(wd, IN_IGNORED, 0, &[]);
    Ok(())
}

impl Drop for Inotify {
    /// Take its watches out of every node's list, if the lists are free.
    ///
    /// `report` can drop the last hold on an instance while it holds their
    /// lock, when the descriptor closes on another processor during the
    /// report; waiting for the lock there would wait on itself. So a lock
    /// held elsewhere leaves the watches in place, dead, and whichever
    /// report next reaches their node prunes them and counts them off.
    fn drop(&mut self) {
        let watches = core::mem::take(&mut self.state.get_mut().watches);
        if watches.is_empty() {
            return;
        }
        let Some(mut all) = WATCHES.try_lock() else {
            return;
        };
        let mut removed = 0;
        for (wd, key) in watches {
            if let Some(list) = all.get_mut(&key) {
                let before = list.len();
                // Its own watches have a dead `Weak` by now.
                list.retain(|watch| !(watch.wd == wd && watch.instance.strong_count() == 0));
                removed += before - list.len();
                if list.is_empty() {
                    let _ = all.remove(&key);
                }
            }
        }
        drop(all);
        let _ = WATCHING.fetch_sub(removed, Ordering::Relaxed);
    }
}

/// Whether the process a wait is on behalf of has a signal to take.
fn interrupted(caller: Option<&Arc<Process>>) -> bool {
    caller.is_some_and(|process| process.signal_pending())
}

impl Inode for Inotify {
    fn metadata(&self) -> Metadata {
        fs::anon::metadata()
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn poll(&self) -> Readiness {
        Readiness {
            readable: !self.state.lock().queue.is_empty(),
            writable: false,
            hangup: false,
            error: false,
            priority: false,
        }
    }

    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        visit(fs::wake::shared(&self.readable));
        true
    }

    /// The same queues, and the same answer, for its whole life.
    fn poll_queues_fixed(&self) -> bool {
        true
    }

    fn poll_changes(&self) -> Option<u64> {
        Some(self.readable.wakes())
    }

    /// As many whole events as fit; `EINVAL` when not even the first does,
    /// and `EAGAIN` or a wait while there are none.
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> ferrix_vfs::Result<usize> {
        let caller = process::current();
        loop {
            if let Some(read) = self.state.lock().take(buf) {
                return read;
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            let _ = WaitQueue::wait_on_any(
                &[&self.readable],
                || !self.state.lock().queue.is_empty() || interrupted(caller.as_ref()),
                u64::MAX,
                RECHECK,
            );
            if interrupted(caller.as_ref()) {
                return Err(Errno::ERESTARTSYS);
            }
        }
    }

    /// Nothing may be written to it.
    fn write_stream(&self, _data: &[u8], _nonblock: bool) -> ferrix_vfs::Result<usize> {
        Err(Errno::EINVAL)
    }
}

/// `inotify_init1`.
///
/// # Errors
///
/// `EINVAL` for a flag other than `IN_CLOEXEC` and `IN_NONBLOCK`; `EMFILE`
/// for a full table; `ENOMEM`.
pub(crate) fn sys_inotify_init1(process: &Process, flags: u32) -> Result<usize, Errno> {
    if flags & !(IN_CLOEXEC | IN_NONBLOCK) != 0 {
        return Err(Errno::EINVAL);
    }
    let file = create(flags & IN_NONBLOCK != 0)?;
    let fd = process
        .files()
        .lock()
        .insert(file, flags & IN_CLOEXEC != 0)?;
    usize::try_from(fd).map_err(|_| Errno::EMFILE)
}

/// `inotify_rm_watch`.
///
/// # Errors
///
/// `EBADF` for a closed descriptor, `EINVAL` for one that is not an inotify
/// instance or a wd it does not have.
pub(crate) fn sys_inotify_rm_watch(process: &Process, fd: i32, wd: i32) -> Result<usize, Errno> {
    let file = crate::syscall::fd::file(process, fd)?;
    let instance = of(&file).ok_or(Errno::EINVAL)?;
    rm_watch(&instance, wd)?;
    Ok(0)
}

/// Whether `mask` asks not to follow a final link, for the walk
/// `inotify_add_watch` makes.
pub(crate) fn follows(mask: u32) -> bool {
    mask & IN_DONT_FOLLOW == 0
}

/// The events a close of `file` gives.
pub(crate) fn closed(file: &OpenFile) {
    if watching() && !file.is_path() {
        let mask = if file.writable() {
            IN_CLOSE_WRITE
        } else {
            IN_CLOSE_NOWRITE
        };
        node_event(file.location(), mask);
    }
}

/// `IN_OPEN` for a file just opened.
pub(crate) fn opened(file: &OpenFile) {
    if watching() && !file.is_path() {
        node_event(file.location(), IN_OPEN);
    }
}
