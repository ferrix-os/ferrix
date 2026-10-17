//! Epoll sets: the object behind `epoll_create1`'s descriptor.
//!
//! An epoll set is a list of registrations, each a descriptor number, the open
//! file it named when it was added, the events asked for and the program's
//! 64-bit cookie. What a wait reports is asked of each file's
//! [`OpenFile::poll`], but only of the files that may have something to say,
//! as Linux's ready list arranges.
//!
//! # The ready list
//!
//! Each registration hooks the wait queues its file names
//! ([`ferrix_vfs::Inode::poll_queues`]) with a [`Mark`]. A wake of any of them
//! puts the registration's serial on the set's marks, once until a wait takes
//! it, and wakes the set's own queue ([`crate::sched::wait`]'s hooks). A wait
//! takes the marks onto the set's `pending` list, Linux's `rdllist`, and
//! looks at the files on it, not at every file: one that has nothing to report
//! leaves the list, one that reports stays until the program receives it, and
//! a level-triggered one received goes to the back, so a wait with room for
//! few events reaches the others first, as Linux puts it back at the end of its
//! ready list. The wait sleeps on the set's queue alone. So a wait costs what is
//! woken, not what is registered: wineserver's 794 registrations cost every one
//! of its 8,500 waits a second a look and a listing each before.
//!
//! Hooking is lazy: a registration is added to `pending`, and the wait that
//! looks at it hooks every queue its file names *before* it looks, so a wake
//! after the look is never missed; a queue the file names later, as a unix
//! socket names its peer's once it connects, is hooked at the next look.
//!
//! Three kinds of registration are looked at by every wait instead, as every
//! registration was before: a file that says not every change wakes one of its
//! queues, a queue there was no memory to hook or no room for in a
//! registration's [`MAX_HOOKS`], and a set registered in this one, whose own
//! wait is cheap. A file that says it can be trusted and changes without a wake
//! is a defect in that file. It is found within a second all the same: once a
//! second (`fs::wake::TRUSTED_RECHECK_NANOS`, the bound a trusted `poll` sleeps
//! between its own looks) a wait puts every registration on `pending`, and the
//! same rescan drops a registration whose file went while nothing woke it.
//!
//! Hooks are only added while a registration lives: a queue its file no
//! longer names but that is still alive keeps its slot until the
//! registration goes, so a socket reconnected more than [`MAX_HOOKS`] times
//! over is looked at by every wait from then on (fail closed). A new
//! registration is untrusted until a wait has hooked it, and a wait with no
//! memory to gather what to hook distrusts every registration it could not.
//!
//! A registration whose file says it names the same queues for its whole
//! life (`Inode::poll_queues_fixed`) is hooked once and its file not asked
//! again; any other is asked at every look whether it names a queue not yet
//! hooked, which costs the visit and no lock. Registrations live in a slab,
//! each in the slot its serial names, so a mark or `pending` finds one in a
//! step.
//!
//! # What a file promises
//!
//! `Inode::poll_queues` answering true promises that every change `poll` can
//! report wakes a queue it names, and that a change of which queues it names
//! wakes one it named before (its doc). Every implementation was read
//! against that on 2026-10-17, and against the rule that it runs inside a
//! wait's last look (no blocking, no sleeping lock): pipes, eventfd,
//! timerfd, inotify, the two pty ends, signalfd, pidfd, cgroupfs's event
//! files, port descriptors, the three net sockets (the net core's progress
//! queue), sync files, fences, evdev, DRM, the two PCM nodes, the console
//! (trusted only once its input is interrupt-driven) and epoll itself name
//! queues that every change wakes, take a spin lock or none, and never
//! change which queues they name; a render node names none and answers
//! true, which a wait treats as untrusted (no queue to sleep on), as
//! `Sources::recheck` does. Two needed a word: a unix stream or datagram
//! socket's `connect` changed what it reports and named its peer's queues
//! while waking only the listener's, and now wakes its own; and a signalfd
//! names the signal queue of the process that looks, so a registration
//! hooks the one of the process whose wait first looked at it, as Linux's
//! binds to the registering context.
//!
//! # Edge-triggered registrations
//!
//! `EPOLLET` reports a file once when it becomes ready and again only when
//! something happens to it: new data, room made, the peer leaving. Linux
//! knows that happened because the file woke its wait queue. Here every
//! pollable file answers [`OpenFile::poll_changes`], a count of those wakes,
//! and a registration is due a report when the count moved or a readiness bit
//! appeared since the set last looked -- the second so that an object whose
//! readiness changes without a wake is not missed when it is looked at. A due
//! report is kept until a wait delivers it, and dropped, as on Linux, if the
//! file is no longer ready by then.
//!
//! # Registrations and closing
//!
//! A registration is keyed by the open file and the number together, as
//! Linux keys it, and holds the file weakly: closing the number leaves the
//! registration reporting while a `dup` of it keeps the file open, and the
//! registration goes when the file does. That is Linux's documented surprise,
//! kept because a program that relies on it exists. A file that goes drops its
//! queues, and a queue dropped marks the registrations hooked on it, so the
//! next wait finds the file gone.
//!
//! # Nesting
//!
//! An epoll set is itself pollable: readable when a wait on it would report
//! something. So one can be registered in another, which is how a Wayland
//! server's event loop sits inside its toolkit's. `epoll_ctl` refuses a set
//! that would contain itself, and a chain of sets deeper than Linux's
//! `EP_MAX_NESTS`, with `ELOOP`, looking down from the set being added and up
//! from the set it is added to. A set knows which sets hold it for the upward
//! look. The look and the add are made under one sleeping lock, [`NESTING`],
//! as Linux makes them under `epnested_mutex`: two adds racing could otherwise
//! each pass the look and make a loop -- before it, a loop made so recursed in
//! the first wait that polled it, and now a loop of marks would wake its own
//! queue under that queue's hook lock. Only an add of a set to a set takes it.
//!
//! # Locks
//!
//! A set's lock is never held while a file is polled: polling a nested set
//! takes that set's lock, and polling a socket takes the net core's. A wait
//! copies the registrations it will look at out, polls them unlocked, and
//! takes the lock again to record what it saw against whichever are still
//! there, by serial. The order is [`NESTING`], then a set's lock, then a
//! queue's hook and waiter locks (hooking and unhooking) or the set's marks
//! (taking them). A [`Mark`] runs under a queue's hook lock and takes only
//! the marks lock, a leaf, and the set's queue's locks after it: never a set's
//! lock.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use ferrix_kmem::{Charge, arc_footprint};
use ferrix_linux_abi::types::{
    EPOLLERR, EPOLLET, EPOLLHUP, EPOLLIN, EPOLLONESHOT, EPOLLOUT, EPOLLPRI, EPOLLRDNORM,
    EPOLLWRNORM,
};
use ferrix_vfs::{Errno, Inode, Metadata, OpenFile, Readiness, WakeSource};

use crate::fallible;
use crate::fs;
use crate::sched::WaitQueue;
use crate::sched::{Mark, ReadyList};
use crate::sync::SpinLock;

/// How deep sets may nest: Linux's `EP_MAX_NESTS`. A chain may hold this many
/// sets below the top one.
pub(crate) const MAX_NESTS: usize = 4;

/// The most queues one registration hooks. A file naming more is looked at by
/// every wait instead; the most any names today is a connected unix socket's
/// five.
pub(crate) const MAX_HOOKS: usize = 8;

/// How often a wait looks at every registration, whatever woke: see the
/// module doc.
const RESCAN_NANOS: u64 = fs::wake::TRUSTED_RECHECK_NANOS;

/// The room a hooked queue's list may hold for each mark on it, which a
/// registration is charged for: `WaitQueue::unhook` gives room back once a
/// list is down to a quarter of it.
const HOOK_ROOM: usize = 4;

/// The name `/proc/self/fd` shows.
const NAME: &[u8] = b"anon_inode:[eventpoll]";

/// Held across `epoll_ctl`'s nesting look and the add it allows: see the
/// module doc's "Nesting". A sleeping lock -- a flag and the queue its
/// waiters sleep on -- since the look walks sets, allocating as it goes.
static NESTING: Nesting = Nesting {
    held: AtomicBool::new(false),
    waiters: WaitQueue::new(),
};

/// The task a boot check holds in its next [`Epoll::ready`] between hooking
/// and looking, or [`NO_MEET`]: see [`arm_look_meet`].
static LOOK_MEET: AtomicU64 = AtomicU64::new(NO_MEET);

/// Where the armed task is in the meet: 1 there, 2 let go.
static LOOK_MEET_STAGE: AtomicU32 = AtomicU32::new(0);

/// No task.
const NO_MEET: u64 = u64::MAX;

/// How long a task waits in the meet for the check to let it go.
const LOOK_MEET_PATIENCE_NANOS: u64 = 2_000_000_000;

/// Hold `task`'s next [`Epoll::ready`] after it hooked its registrations'
/// queues and before it looks at their files, until [`release_look_meet`],
/// for the check that a file changed there is reported by a wake
/// (`fs::epoll_check`). One look only.
pub(crate) fn arm_look_meet(task: crate::sched::TaskId) {
    LOOK_MEET_STAGE.store(0, Ordering::Release);
    LOOK_MEET.store(task, Ordering::Release);
}

/// Whether the armed task has reached the meet.
pub(crate) fn look_meet_arrived() -> bool {
    LOOK_MEET_STAGE.load(Ordering::Acquire) == 1
}

/// Let the task held in the meet go on, and disarm it.
pub(crate) fn release_look_meet() {
    LOOK_MEET.store(NO_MEET, Ordering::Release);
    LOOK_MEET_STAGE.store(2, Ordering::Release);
}

/// The meet itself: nothing unless a check armed it for the running task.
/// It spins, holding no lock, so the check runs on another processor.
fn look_meet() {
    let armed = LOOK_MEET.load(Ordering::Relaxed);
    if armed == NO_MEET || crate::sched::current().is_none_or(|task| task.id != armed) {
        return;
    }
    LOOK_MEET.store(NO_MEET, Ordering::Relaxed);
    LOOK_MEET_STAGE.store(1, Ordering::Release);
    let until = crate::timer::now_nanos().saturating_add(LOOK_MEET_PATIENCE_NANOS);
    while LOOK_MEET_STAGE.load(Ordering::Acquire) != 2 && crate::timer::now_nanos() < until {
        core::hint::spin_loop();
    }
}

/// [`NESTING`]'s type.
struct Nesting {
    /// Whether an `epoll_ctl` holds it.
    held: AtomicBool,
    /// Who waits for it.
    waiters: WaitQueue,
}

/// [`NESTING`] held, until dropped.
struct NestingHeld;

impl Nesting {
    /// Take it, sleeping while another `epoll_ctl` holds it. Task context
    /// only, with no spin lock held.
    fn lock(&'static self) -> NestingHeld {
        while self
            .held
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            let _ = self
                .waiters
                .wait_until_deadline(|| !self.held.load(Ordering::Acquire), u64::MAX);
        }
        NestingHeld
    }
}

impl Drop for NestingHeld {
    fn drop(&mut self) {
        NESTING.held.store(false, Ordering::Release);
        NESTING.waiters.wake_all();
    }
}

/// A queue a registration hooked, kept to unhook it.
#[derive(Debug)]
enum Hooked {
    /// One that lives as long as the kernel.
    Static(&'static WaitQueue),
    /// One inside an object that can go: held weakly, so a registration
    /// keeps no file's queue alive.
    Shared(Weak<WaitQueue>),
}

impl Hooked {
    /// The queue's address.
    fn addr(&self) -> usize {
        match self {
            Hooked::Static(held) => core::ptr::from_ref(*held).addr(),
            Hooked::Shared(held) => held.as_ptr().addr(),
        }
    }

    /// Whether this is `queue`.
    fn is(&self, queue: &WaitQueue) -> bool {
        match self {
            Hooked::Static(held) => core::ptr::eq(*held, queue),
            Hooked::Shared(held) => core::ptr::eq(held.as_ptr(), queue),
        }
    }
}

/// One registration.
struct Item {
    /// The incarnation of this registration: what marks, events and the
    /// unlocked looks name it by. Serials only grow, so `items` sorted by
    /// them is in the order registrations were made.
    serial: u64,
    /// The number it was added under.
    fd: i32,
    /// The open file's address, which with `fd` is the key. Stable for as
    /// long as `file` is held, because a weak reference keeps the allocation.
    key: usize,
    /// The file, held weakly.
    file: Weak<OpenFile>,
    /// The set it is, when it is one.
    nested: Option<Weak<Epoll>>,
    /// The events asked for, with `EPOLLERR` and `EPOLLHUP`, which are
    /// always asked for, and the flags.
    events: u32,
    /// The program's cookie.
    data: u64,
    /// Whether it may report: `EPOLLONESHOT` clears it once it has.
    armed: bool,
    /// For `EPOLLET`: a report is due.
    due: bool,
    /// For `EPOLLET`: the file's change count when the set last looked.
    seen_changes: u64,
    /// For `EPOLLET`: the readiness bits when the set last looked.
    seen_bits: u32,
    /// Its mark, which every queue in `hooked` lists.
    mark: Arc<Mark>,
    /// The queues it hooked.
    hooked: [Option<Hooked>; MAX_HOOKS],
    /// A queue could not be hooked: looked at by every wait from now on.
    unhookable: bool,
    /// Its file's last answer that every change wakes a queue it named, and
    /// each of them hooked.
    trusted: bool,
    /// Its file says it names the same queues for its whole life
    /// (`Inode::poll_queues_fixed`): once hooked, never asked again.
    fixed: bool,
    /// On `pending`.
    pending: bool,
    /// The wait that last copied it out to look at: only that wait's look is
    /// recorded, so an older look finishing late never undoes a newer one.
    looked_by: u64,
    /// The file's allocation, which the weak reference keeps once the file
    /// is closed until a wait drops the registration, its mark, and room for
    /// its hooks in the queues it names: charged to the job that registered
    /// it (F-37).
    _pinned: Charge,
}

impl Item {
    /// Whether every wait looks at it, pending or not.
    fn always(&self) -> bool {
        self.unhookable || !self.trusted || self.nested.is_some()
    }
}

impl fmt::Debug for Item {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Item")
            .field("serial", &self.serial)
            .field("fd", &self.fd)
            .field("events", &self.events)
            .finish_non_exhaustive()
    }
}

/// A registration goes off every queue it hooked.
impl Drop for Item {
    fn drop(&mut self) {
        for hooked in self.hooked.iter().flatten() {
            match hooked {
                Hooked::Static(queue) => queue.unhook(&self.mark),
                Hooked::Shared(queue) => {
                    if let Some(queue) = queue.upgrade() {
                        queue.unhook(&self.mark);
                    }
                }
            }
        }
    }
}

/// What a set holds.
#[derive(Debug)]
struct State {
    /// The registrations, each in the slot its serial names.
    items: Items,
    /// The serials a wait looks at next, in the order it reports them:
    /// Linux's ready list. Room for every registration is reserved as each
    /// is added, and each is here at most once (`Item::pending`).
    pending: Vec<u64>,
    /// Where a wait takes the ready list's numbers to, with the same room.
    taken: Vec<u64>,
    /// Where a wait copies registrations out to, kept between waits with
    /// the room for every registration, so a wait allocates nothing for it.
    scratch: Vec<Sample>,
    /// The serial handed to the next registration.
    next_serial: u64,
    /// The number handed to the next wait that copies registrations out.
    next_look: u64,
    /// When a wait last looked at every registration.
    rescanned_at: u64,
    /// How many registrations are not `trusted`.
    distrusted: usize,
    /// How many files waits have looked at, for the checks.
    looks: u64,
    /// How many times a wait looked at every registration, for the checks.
    rescans: u64,
    /// The sets this one is registered in, for the depth check.
    parents: Vec<Weak<Epoll>>,
    /// Bumped by every change to `items`, so a set holding this one sees a
    /// change.
    generation: u64,
    /// The room `items` holds, charged to the job that made the set (F-37).
    items_room: Charge,
    /// The room `pending` holds, likewise.
    pending_room: Charge,
    /// The room `scratch` holds, likewise.
    scratch_room: Charge,
    /// The room the ready list and `taken` hold, likewise.
    marks_room: Charge,
    /// The room `parents` holds, likewise.
    parents_room: Charge,
}

impl State {
    /// The registration with `serial`.
    fn item(&mut self, serial: u64) -> Option<&mut Item> {
        let at = self.items.find(serial).ok()?;
        self.items.get_mut(at)
    }

    /// Put the registration at `at` on `pending`, if it is not there.
    fn queue_at(&mut self, at: usize) {
        let Some(item) = self.items.get_mut(at) else {
            return;
        };
        if item.pending {
            return;
        }
        // Room for every registration was reserved as each was added.
        if fallible::push_within(&mut self.pending, item.serial).is_ok() {
            item.pending = true;
        }
    }

    /// Take the marks onto `pending`, and every registration as well when a
    /// rescan is due or the list overflowed.
    fn take_marks(&mut self, ready: &ReadyList, now: u64) {
        let overflowed = ready.take(&mut self.taken);
        let mut taken = core::mem::take(&mut self.taken);
        for &serial in taken.iter() {
            let Ok(at) = self.items.find(serial) else {
                continue;
            };
            // Cleared before the file is looked at: a wake after this lists
            // it again.
            if let Some(item) = self.items.get(at) {
                item.mark.clear();
            }
            self.queue_at(at);
        }
        taken.clear();
        self.taken = taken;
        if overflowed || now >= self.rescanned_at.saturating_add(RESCAN_NANOS) {
            self.rescanned_at = now;
            self.rescans = self.rescans.wrapping_add(1);
            // Every mark cleared, not only those taken: a mark that found
            // the list full is not on it, and left marked it would never
            // list itself again (the consultant's B1, ledger line 2050).
            for item in self.items.iter() {
                item.mark.clear();
            }
            for at in 0..self.items.slots() {
                self.queue_at(at);
            }
        }
    }

    /// Set a registration's trust, keeping the count.
    fn set_trusted(&mut self, at: usize, trusted: bool) {
        let Some(item) = self.items.get_mut(at) else {
            return;
        };
        if item.trusted != trusted {
            item.trusted = trusted;
            if trusted {
                self.distrusted = self.distrusted.saturating_sub(1);
            } else {
                self.distrusted = self.distrusted.saturating_add(1);
            }
        }
    }

    /// Take the registration at `at` out, keeping the counts.
    fn take_out(&mut self, at: usize) -> Option<Item> {
        let item = self.items.remove(at)?;
        if !item.trusted {
            self.distrusted = self.distrusted.saturating_sub(1);
        }
        if item.pending {
            let serial = item.serial;
            self.pending.retain(|pending| *pending != serial);
        }
        self.generation = self.generation.wrapping_add(1);
        Some(item)
    }
}

/// The registrations: a slab, each in a slot the low half of its serial
/// names, so a wait finds a registration a mark or `pending` names in one
/// step, not by a search. The high half counts registrations made, so a
/// number left on the ready list by a registration removed never names the
/// one that took its slot.
#[derive(Debug)]
struct Items {
    /// The slots, `None` where a registration was removed.
    slots: Vec<Option<Item>>,
    /// The slots free for the next registration, room reserved for every
    /// slot.
    free: Vec<u32>,
    /// How many slots hold one.
    live: usize,
}

impl Items {
    /// None.
    const fn new() -> Items {
        Items {
            slots: Vec::new(),
            free: Vec::new(),
            live: 0,
        }
    }

    /// How many registrations there are.
    fn len(&self) -> usize {
        self.live
    }

    /// How many slots there are, held or free.
    fn slots(&self) -> usize {
        self.slots.len()
    }

    /// The slot the registration with `serial` is in, if it is still there.
    fn find(&self, serial: u64) -> Result<usize, ()> {
        let at = usize::try_from(serial & u64::from(u32::MAX)).map_err(|_| ())?;
        match self.slots.get(at) {
            Some(Some(item)) if item.serial == serial => Ok(at),
            _ => Err(()),
        }
    }

    /// The registration in slot `at`.
    fn get(&self, at: usize) -> Option<&Item> {
        self.slots.get(at)?.as_ref()
    }

    /// The registration in slot `at`, to change.
    fn get_mut(&mut self, at: usize) -> Option<&mut Item> {
        self.slots.get_mut(at)?.as_mut()
    }

    /// The slot of the first registration `matches`.
    fn position(&self, mut matches: impl FnMut(&Item) -> bool) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.as_ref().is_some_and(&mut matches))
    }

    /// Every registration.
    fn iter(&self) -> impl Iterator<Item = &Item> {
        self.slots.iter().flatten()
    }

    /// Every registration, to change.
    fn iter_mut(&mut self) -> impl Iterator<Item = &mut Item> {
        self.slots.iter_mut().flatten()
    }

    /// The slot the next registration goes in.
    fn next_slot(&self) -> usize {
        self.free
            .last()
            .and_then(|&at| usize::try_from(at).ok())
            .unwrap_or(self.slots.len())
    }

    /// Room for one more registration, charged to `room`.
    fn reserve(&mut self, room: &mut Charge) -> Result<(), Errno> {
        if self.free.is_empty() {
            ferrix_kmem::reserve(&mut self.slots, 1, room).map_err(|_| Errno::ENOMEM)?;
        }
        let more = self
            .slots
            .len()
            .saturating_add(1)
            .saturating_sub(self.free.len());
        fallible::try_reserve(&mut self.free, more).map_err(|_| Errno::ENOMEM)?;
        room.resize(
            ferrix_kmem::buffer_footprint::<Option<Item>>(self.slots.capacity())
                .saturating_add(ferrix_kmem::buffer_footprint::<u32>(self.free.capacity())),
        )
        .map_err(|_| Errno::ENOMEM)
    }

    /// Put `item` in [`Items::next_slot`], room for which [`Items::reserve`]
    /// had.
    fn insert(&mut self, item: Item) {
        match self.free.pop().and_then(|at| usize::try_from(at).ok()) {
            Some(at) => {
                if let Some(slot) = self.slots.get_mut(at) {
                    *slot = Some(item);
                }
            }
            // NOALLOC: room was had by `reserve`.
            None => self.slots.push(Some(item)),
        }
        self.live = self.live.saturating_add(1);
    }

    /// Take the registration in slot `at` out, freeing the slot.
    fn remove(&mut self, at: usize) -> Option<Item> {
        let item = self.slots.get_mut(at)?.take()?;
        self.live = self.live.saturating_sub(1);
        // Room for every slot was reserved as each was made.
        if let Ok(slot) = u32::try_from(at) {
            let _ = fallible::push_within(&mut self.free, slot);
        }
        Some(item)
    }
}

/// One event a wait may deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Event {
    /// The events that happened, within those asked for.
    pub(crate) events: u32,
    /// The program's cookie.
    pub(crate) data: u64,
    /// Which registration, for [`Epoll::delivered`].
    serial: u64,
    /// Which wait looked, for [`Epoll::delivered`].
    look: u64,
}

/// What `epoll_ctl` asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Interest {
    /// `epoll_event.events`.
    pub(crate) events: u32,
    /// `epoll_event.data`.
    pub(crate) data: u64,
}

/// An epoll set.
pub(crate) struct Epoll {
    state: SpinLock<State>,
    /// Itself, so a registration in another set can name it weakly.
    this: Weak<Epoll>,
    /// Its ready list and the queue a wait on it sleeps on, which its
    /// registrations' marks share.
    ready: Arc<ReadyList>,
    /// Its heap but its lists', charged to the job that made it (F-37).
    _charge: Charge,
}

impl fmt::Debug for Epoll {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Epoll").finish_non_exhaustive()
    }
}

/// A new epoll set, as the open file `epoll_create1` installs.
///
/// # Errors
///
/// `ENOMEM` for no memory, or a job past its limit.
pub(crate) fn create() -> Result<Arc<OpenFile>, Errno> {
    let refused = |_| Errno::ENOMEM;
    let charge = Charge::bytes(
        arc_footprint::<Epoll>()
            .saturating_add(arc_footprint::<WaitQueue>())
            .saturating_add(arc_footprint::<ReadyList>()),
    )
    .map_err(refused)?;
    // Nothing is never refused; these grow with the set.
    let (items_room, pending_room, scratch_room, marks_room, parents_room) = (
        Charge::bytes(0).map_err(refused)?,
        Charge::bytes(0).map_err(refused)?,
        Charge::bytes(0).map_err(refused)?,
        Charge::bytes(0).map_err(refused)?,
        Charge::bytes(0).map_err(refused)?,
    );
    let ready = ReadyList::new().map_err(|_| Errno::ENOMEM)?;
    let set = fallible::try_arc_cyclic(|this| Epoll {
        state: SpinLock::new(State {
            items: Items::new(),
            pending: Vec::new(),
            taken: Vec::new(),
            scratch: Vec::new(),
            next_serial: 0,
            next_look: 0,
            rescanned_at: crate::timer::now_nanos(),
            distrusted: 0,
            looks: 0,
            rescans: 0,
            parents: Vec::new(),
            generation: 0,
            items_room,
            pending_room,
            scratch_room,
            marks_room,
            parents_room,
        }),
        this: this.clone(),
        ready,
        _charge: charge,
    })
    .map_err(|_| Errno::ENOMEM)?;
    fs::anon::open(set, NAME, false)
}

/// The epoll set an open file is, if it is one.
pub(crate) fn of(file: &OpenFile) -> Option<Arc<Epoll>> {
    Arc::clone(file.io()).into_any().downcast::<Epoll>().ok()
}

/// A file's readiness as epoll bits.
const fn bits(ready: Readiness) -> u32 {
    let mut bits = 0;
    if ready.readable {
        bits |= EPOLLIN | EPOLLRDNORM;
    }
    if ready.writable {
        bits |= EPOLLOUT | EPOLLWRNORM;
    }
    if ready.hangup {
        bits |= EPOLLHUP;
    }
    if ready.error {
        bits |= EPOLLERR;
    }
    if ready.priority {
        bits |= EPOLLPRI;
    }
    bits
}

/// What one look at a registration's file found: its bits and change count,
/// or nothing when the file is gone.
type Look = Option<(u32, u64)>;

/// A registration a wait copied out to look at with the set unlocked.
#[derive(Debug)]
struct Sample {
    serial: u64,
    file: Weak<OpenFile>,
    /// The addresses of the queues its registration has hooked, 0 for none.
    hooked: [usize; MAX_HOOKS],
    /// Whether its registration was trusted.
    trusted: bool,
    /// Whether its file names the same queues for its whole life.
    fixed: bool,
    /// What the look found.
    seen: Look,
}

impl Sample {
    /// A copy of `item` to look at unlocked.
    fn of(item: &Item) -> Sample {
        let mut hooked = [0; MAX_HOOKS];
        for (slot, held) in hooked.iter_mut().zip(item.hooked.iter()) {
            *slot = held.as_ref().map_or(0, Hooked::addr);
        }
        Sample {
            serial: item.serial,
            file: item.file.clone(),
            hooked,
            trusted: item.trusted,
            fixed: item.fixed,
            seen: None,
        }
    }

    /// Whether every queue its file names now is one its registration has
    /// hooked, and the file still says it can be trusted: nothing to hook,
    /// so the wait need not take the set's lock to hook it. Asked with no
    /// lock held.
    fn settled(&self) -> bool {
        if !self.trusted {
            return false;
        }
        if self.fixed {
            // Trusted with its queues hooked, which only a wait that hooked
            // every one of them made it.
            return true;
        }
        let Some(file) = self.file.upgrade() else {
            // Gone: its look drops it.
            return true;
        };
        let mut named = 0_usize;
        let mut known = true;
        let trusted = file.poll_queues(&mut |source| {
            named += 1;
            let at = match &source {
                WakeSource::Static(any) => any
                    .downcast_ref::<WaitQueue>()
                    .map(|queue| core::ptr::from_ref(queue).addr()),
                WakeSource::Shared(any) => (**any)
                    .downcast_ref::<WaitQueue>()
                    .map(|queue| core::ptr::from_ref(queue).addr()),
            };
            known &= at.is_some_and(|at| self.hooked.contains(&at));
        });
        trusted && known && named != 0
    }
}

/// The queues a file named, gathered with no lock held, for a wait to hook.
#[derive(Debug, Default)]
struct Named {
    /// The queues, as many as fit.
    queues: [Option<WakeSource>; MAX_HOOKS],
    /// Whether the file said every change wakes one of them.
    trusted: bool,
    /// More than [`MAX_HOOKS`] were named.
    overflowed: bool,
    /// The file says it names the same queues for its whole life.
    fixed: bool,
    /// The file is gone.
    gone: bool,
}

/// Gather what a file names, with no lock held.
fn named(file: &Weak<OpenFile>) -> Named {
    let mut named = Named::default();
    let Some(file) = file.upgrade() else {
        named.gone = true;
        return named;
    };
    let mut at = 0;
    let mut overflowed = false;
    let mut slots = core::mem::take(&mut named.queues);
    let trusted = file.poll_queues(&mut |source| {
        match slots.get_mut(at) {
            Some(slot) => *slot = Some(source),
            None => overflowed = true,
        }
        at += 1;
    });
    named.queues = slots;
    named.trusted = trusted;
    named.fixed = file.poll_queues_fixed();
    named.overflowed = overflowed;
    named
}

/// A queue a file named, recognised as a wait queue.
enum Source {
    /// One that lives as long as the kernel.
    Static(&'static WaitQueue),
    /// One inside an object, held for as long as it is being hooked.
    Owned(Arc<WaitQueue>),
}

impl Source {
    /// The queue `source` names, or `None` for one that is no wait queue.
    fn of(source: &WakeSource) -> Option<Source> {
        match source {
            WakeSource::Static(any) => any.downcast_ref::<WaitQueue>().map(Source::Static),
            WakeSource::Shared(any) => Arc::clone(any)
                .downcast::<WaitQueue>()
                .ok()
                .map(Source::Owned),
        }
    }

    /// The queue.
    fn queue(&self) -> &WaitQueue {
        match self {
            Source::Static(queue) => queue,
            Source::Owned(queue) => queue,
        }
    }

    /// What a registration keeps to unhook it.
    fn hooked(&self) -> Hooked {
        match self {
            Source::Static(queue) => Hooked::Static(queue),
            Source::Owned(queue) => Hooked::Shared(Arc::downgrade(queue)),
        }
    }
}

/// Copy `item` out for wait `ticket` to look at, marked as that wait's; with
/// no room to, it is left for a later wait.
fn copy_out(item: &mut Item, samples: &mut Vec<Sample>, ticket: u64) {
    item.looked_by = ticket;
    if fallible::try_push(samples, Sample::of(item)).is_err() {
        item.looked_by = 0;
    }
}

/// Look at a file, with no lock held.
fn look(file: &Weak<OpenFile>) -> Look {
    let file = file.upgrade()?;
    let changes = file.poll_changes().unwrap_or(0);
    Some((bits(file.poll()), changes))
}

/// Record what a wait found for one registration, returning whether its file
/// went away and whether it reports.
fn record(
    item: &mut Item,
    seen: Look,
    events: &mut Vec<Event>,
    limit: usize,
    look: u64,
) -> Recorded {
    let Some((now, changes)) = seen else {
        return Recorded::Gone;
    };
    let wanted = now & item.events;
    if item.events & EPOLLET != 0 {
        let happened = changes != item.seen_changes || now & !item.seen_bits != 0;
        if happened && wanted != 0 {
            item.due = true;
        }
        item.seen_changes = changes;
        item.seen_bits = now;
    }
    let reports = item.armed && wanted != 0 && (item.events & EPOLLET == 0 || item.due);
    if reports && events.len() < limit {
        // The caller's room for `limit` events was had before the look.
        if fallible::push_within(
            events,
            Event {
                events: wanted,
                data: item.data,
                serial: item.serial,
                look,
            },
        )
        .is_err()
        {
            return Recorded::Reports;
        }
    }
    if reports {
        Recorded::Reports
    } else {
        Recorded::Quiet
    }
}

/// What [`record`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recorded {
    /// The file is gone.
    Gone,
    /// It reports, or would with more room.
    Reports,
    /// It has nothing to report.
    Quiet,
}

impl Epoll {
    /// `EPOLL_CTL_ADD`.
    ///
    /// # Errors
    ///
    /// `EEXIST` for a file and number already registered; `ELOOP` for a set
    /// that would contain itself or nest too deep; `ENOMEM` for no room.
    pub(crate) fn add(
        &self,
        fd: i32,
        file: &Arc<OpenFile>,
        interest: Interest,
    ) -> Result<(), Errno> {
        let nested = of(file);
        let key = key_of(file);
        let weak = Arc::downgrade(file);
        // Looked at before the lock, as every look is.
        let seen = look(&weak).unwrap_or((0, 0));
        let pinned = Charge::bytes(
            arc_footprint::<OpenFile>()
                .saturating_add(arc_footprint::<Mark>())
                .saturating_add(HOOK_ROOM * MAX_HOOKS * size_of::<Arc<Mark>>())
                // At most one boxed list a hooked queue: an upper bound.
                .saturating_add(MAX_HOOKS * ferrix_kmem::buffer_footprint::<Vec<Arc<Mark>>>(1)),
        )
        .map_err(|_| Errno::ENOMEM)?;
        match &nested {
            Some(inner) => {
                // The look and the add under one lock: the module doc's
                // "Nesting".
                let _nesting = NESTING.lock();
                self.check_nesting(inner)?;
                // The inner set learns of this one first, so the depth check
                // never misses a parent; a registration refused below takes
                // it back.
                inner.add_parent(&self.this)?;
                let added = self.insert(fd, key, weak, Some(inner), interest, seen, pinned);
                if added.is_err() {
                    inner.remove_parent(&self.this);
                }
                added?;
            }
            None => self.insert(fd, key, weak, None, interest, seen, pinned)?,
        }
        self.ready.queue().wake_all();
        Ok(())
    }

    /// Put a registration in, charged and pending: the rest of
    /// [`Epoll::add`].
    ///
    /// # Errors
    ///
    /// `EEXIST` for a file and number already registered, `ENOMEM` for no
    /// room.
    #[expect(
        clippy::too_many_arguments,
        reason = "AUDIT: one registration's fields, taken apart once"
    )]
    fn insert(
        &self,
        fd: i32,
        key: usize,
        weak: Weak<OpenFile>,
        nested: Option<&Arc<Epoll>>,
        interest: Interest,
        seen: (u32, u64),
        pinned: Charge,
    ) -> Result<(), Errno> {
        let mut state = self.state.lock();
        if state
            .items
            .iter()
            .any(|item| (item.key, item.fd) == (key, fd))
        {
            return Err(Errno::EEXIST);
        }
        // The slot in the low half, the count of registrations made above it.
        let slot = state.items.next_slot();
        let serial = (state.next_serial << 32) | u64::try_from(slot).map_err(|_| Errno::ENOMEM)?;
        if slot > u32::MAX as usize {
            return Err(Errno::ENOMEM);
        }
        let mark = Mark::new(&self.ready, serial).map_err(|_| Errno::ENOMEM)?;
        let room = state.items.len().saturating_add(1);
        let State {
            items,
            items_room,
            pending,
            pending_room,
            scratch,
            scratch_room,
            taken,
            marks_room,
            ..
        } = &mut *state;
        items.reserve(items_room)?;
        ferrix_kmem::reserve(pending, room.saturating_sub(pending.len()), pending_room)
            .map_err(|_| Errno::ENOMEM)?;
        ferrix_kmem::reserve(scratch, room.saturating_sub(scratch.len()), scratch_room)
            .map_err(|_| Errno::ENOMEM)?;
        // The ready list and the room a wait takes it into, charged together.
        let listed = self.ready.reserve(room).map_err(|_| Errno::ENOMEM)?;
        fallible::try_reserve(taken, room.max(listed).saturating_sub(taken.len()))
            .map_err(|_| Errno::ENOMEM)?;
        let footprint = ferrix_kmem::buffer_footprint::<u64>(listed)
            .saturating_add(ferrix_kmem::buffer_footprint::<u64>(taken.capacity()));
        // Refused, the room stays, uncharged, until the set goes: the job is
        // at its limit and gets no registration for it.
        marks_room.resize(footprint).map_err(|_| Errno::ENOMEM)?;
        state.next_serial = state.next_serial.wrapping_add(1);
        let events = interest.events | EPOLLERR | EPOLLHUP;
        // NOALLOC: room was had just above.
        state.items.insert(Item {
            serial,
            fd,
            key,
            file: weak,
            nested: nested.map(Arc::downgrade),
            events,
            data: interest.data,
            armed: true,
            due: seen.0 & events != 0,
            seen_changes: seen.1,
            seen_bits: seen.0,
            mark,
            hooked: Default::default(),
            unhookable: false,
            // Trusted only once a wait has hooked it: the module doc's
            // "What a file promises".
            trusted: false,
            fixed: false,
            pending: false,
            looked_by: 0,
            _pinned: pinned,
        });
        state.distrusted = state.distrusted.saturating_add(1);
        state.queue_at(slot);
        state.generation = state.generation.wrapping_add(1);
        Ok(())
    }

    /// Note that `parent` holds this set, for the depth check, charged to
    /// this set's job. Parents gone are pruned first, so a set added and
    /// removed again and again does not grow the list.
    ///
    /// # Errors
    ///
    /// `ENOMEM` for no room.
    fn add_parent(&self, parent: &Weak<Epoll>) -> Result<(), Errno> {
        let mut state = self.state.lock();
        let State {
            parents,
            parents_room,
            ..
        } = &mut *state;
        parents.retain(|held| held.strong_count() > 0);
        ferrix_kmem::reserve(parents, 1, parents_room).map_err(|_| Errno::ENOMEM)?;
        // NOALLOC: room was had just above.
        parents.push(parent.clone());
        Ok(())
    }

    /// Forget one note that `parent` holds this set.
    fn remove_parent(&self, parent: &Weak<Epoll>) {
        let mut state = self.state.lock();
        if let Some(at) = state
            .parents
            .iter()
            .position(|held| Weak::ptr_eq(held, parent))
        {
            let _ = state.parents.remove(at);
        }
    }

    /// `EPOLL_CTL_MOD`: new events and cookie, armed again, due a report if
    /// the file is ready for them now, and looked at by the next wait.
    ///
    /// # Errors
    ///
    /// `ENOENT` for a file and number not registered; `EINVAL` for one
    /// registered with `EPOLLEXCLUSIVE`, which may not be modified.
    pub(crate) fn modify(
        &self,
        fd: i32,
        file: &Arc<OpenFile>,
        interest: Interest,
    ) -> Result<(), Errno> {
        self.change(fd, file, interest)?;
        self.ready.queue().wake_all();
        Ok(())
    }

    /// [`Epoll::modify`]'s change, under the lock.
    fn change(&self, fd: i32, file: &Arc<OpenFile>, interest: Interest) -> Result<(), Errno> {
        let key = key_of(file);
        let seen = look(&Arc::downgrade(file)).unwrap_or((0, 0));
        let mut state = self.state.lock();
        let at = state
            .items
            .position(|item| (item.key, item.fd) == (key, fd))
            .ok_or(Errno::ENOENT)?;
        let item = state.items.get_mut(at).ok_or(Errno::ENOENT)?;
        if item.events & ferrix_linux_abi::types::EPOLLEXCLUSIVE != 0 {
            return Err(Errno::EINVAL);
        }
        item.events = interest.events | EPOLLERR | EPOLLHUP;
        item.data = interest.data;
        item.armed = true;
        item.due = seen.0 & item.events != 0;
        item.seen_changes = seen.1;
        item.seen_bits = seen.0;
        state.queue_at(at);
        state.generation = state.generation.wrapping_add(1);
        Ok(())
    }

    /// `EPOLL_CTL_DEL`.
    ///
    /// # Errors
    ///
    /// `ENOENT` for a file and number not registered.
    pub(crate) fn remove(&self, fd: i32, file: &Arc<OpenFile>) -> Result<(), Errno> {
        let key = key_of(file);
        let removed = {
            let mut state = self.state.lock();
            let at = state
                .items
                .position(|item| (item.key, item.fd) == (key, fd))
                .ok_or(Errno::ENOENT)?;
            state.take_out(at)
        };
        if let Some(inner) = removed
            .as_ref()
            .and_then(|item| item.nested.as_ref())
            .and_then(Weak::upgrade)
        {
            inner.remove_parent(&self.this);
        }
        drop(removed);
        self.ready.queue().wake_all();
        Ok(())
    }

    /// How many registrations it has, for the checks.
    pub(crate) fn len(&self) -> usize {
        self.state.lock().items.len()
    }

    /// How many files its waits have looked at, for the checks.
    pub(crate) fn looks(&self) -> u64 {
        self.state.lock().looks
    }

    /// How many times its waits looked at every registration, for the
    /// checks.
    pub(crate) fn rescans(&self) -> u64 {
        self.state.lock().rescans
    }

    /// How many queues its registrations have hooked, for the checks.
    pub(crate) fn hooked(&self) -> usize {
        self.state
            .lock()
            .items
            .iter()
            .map(|item| item.hooked.iter().flatten().count())
            .sum()
    }

    /// Fill its ready list's room, for the check that a mark finding no room
    /// is not lost.
    pub(crate) fn crowd(&self) {
        self.ready.crowd();
    }

    /// The queue a wait on the set sleeps on.
    pub(crate) fn queue(&self) -> &WaitQueue {
        self.ready.queue()
    }

    /// Whether every registration's file says every change wakes a queue
    /// it named, and all of them are hooked: how long a wait may sleep
    /// between its own looks.
    pub(crate) fn trusted(&self) -> bool {
        self.state.lock().distrusted == 0
    }

    /// `ELOOP` if adding `inner` to this set would make a set contain itself,
    /// or a chain more than [`MAX_NESTS`] sets below its top, as
    /// `ep_loop_check` refuses.
    fn check_nesting(&self, inner: &Arc<Epoll>) -> Result<(), Errno> {
        let below = inner.depth_below(&self.this, 0).ok_or(Errno::ELOOP)?;
        let above = self.depth_above(0).ok_or(Errno::ELOOP)?;
        if below + 1 + above > MAX_NESTS {
            return Err(Errno::ELOOP);
        }
        Ok(())
    }

    /// How many sets deep the longest chain below this one goes, or `None`
    /// if `target` is among them or the chain is deeper than any allowed.
    fn depth_below(&self, target: &Weak<Epoll>, depth: usize) -> Option<usize> {
        if Weak::ptr_eq(&self.this, target) || depth > MAX_NESTS {
            return None;
        }
        let children: Vec<Arc<Epoll>> = self
            .state
            .lock()
            .items
            .iter()
            .filter_map(|item| item.nested.as_ref().and_then(Weak::upgrade))
            .collect();
        let mut deepest = 0;
        for child in children {
            deepest = deepest.max(child.depth_below(target, depth + 1)? + 1);
        }
        Some(deepest)
    }

    /// How many sets deep the longest chain above this one goes, or `None`
    /// past any allowed.
    fn depth_above(&self, depth: usize) -> Option<usize> {
        if depth > MAX_NESTS {
            return None;
        }
        let parents: Vec<Arc<Epoll>> = {
            let mut state = self.state.lock();
            state.parents.retain(|parent| parent.strong_count() > 0);
            state.parents.iter().filter_map(Weak::upgrade).collect()
        };
        let mut highest = 0;
        for parent in parents {
            highest = highest.max(parent.depth_above(depth + 1)? + 1);
        }
        Some(highest)
    }

    /// What a wait would deliver now, at most `limit` events, without
    /// delivering it: [`Epoll::delivered`] does that for the ones the program
    /// received. A registration whose file is gone is dropped here.
    ///
    /// Only the registrations on `pending` -- woken since, reported and not
    /// yet received, or level-triggered and received last time -- and those
    /// every wait looks at are looked at: the module doc's "The ready list".
    /// Each is hooked on the queues its file names before its file is looked
    /// at, and the look is recorded by serial, with the set unlocked between.
    pub(crate) fn ready(&self, limit: usize) -> Vec<Event> {
        let now = crate::timer::now_nanos();
        let (ticket, mut samples) = {
            let mut state = self.state.lock();
            state.take_marks(&self.ready, now);
            let ticket = state.next_look.wrapping_add(1);
            state.next_look = ticket;
            let State {
                items,
                pending,
                scratch,
                ..
            } = &mut *state;
            // The kept room, or with another wait using it, fresh room.
            let mut samples = core::mem::take(scratch);
            for &serial in pending.iter() {
                if let Some(item) = items.find(serial).ok().and_then(|at| items.get_mut(at)) {
                    copy_out(item, &mut samples, ticket);
                }
            }
            for item in items
                .iter_mut()
                .filter(|item| !item.pending && item.always())
            {
                copy_out(item, &mut samples, ticket);
            }
            state.looks = state
                .looks
                .wrapping_add(u64::try_from(samples.len()).unwrap_or(u64::MAX));
            (ticket, samples)
        };

        if samples.is_empty() {
            self.keep_scratch(samples);
            return Vec::new();
        }

        // Hooked before looked at: a wake after the look marks it again. Only
        // a registration whose file names a queue it has not hooked, or that
        // is not trusted, takes the lock to be hooked.
        self.hook_unsettled(&samples);
        look_meet();
        for sample in &mut samples {
            sample.seen = look(&sample.file);
        }

        let mut events = fallible::try_with_capacity(limit.min(samples.len())).unwrap_or_default();
        let mut gone = Vec::new();
        {
            let mut state = self.state.lock();
            for &Sample { serial, seen, .. } in &samples {
                let Ok(at) = state.items.find(serial) else {
                    continue;
                };
                let Some(item) = state.items.get_mut(at) else {
                    continue;
                };
                if item.looked_by != ticket {
                    continue;
                }
                match record(item, seen, &mut events, limit, ticket) {
                    Recorded::Gone => {
                        // With no room to keep it until the lock is let go,
                        // it is dropped here, which unhooking allows (the
                        // module doc's "Locks").
                        let item = state.take_out(at);
                        let _ = item.map(|item| fallible::try_push(&mut gone, item));
                    }
                    Recorded::Reports => state.queue_at(at),
                    Recorded::Quiet => item.pending = false,
                }
            }
            let State { items, pending, .. } = &mut *state;
            pending.retain(|serial| {
                items
                    .find(*serial)
                    .ok()
                    .and_then(|at| items.get(at))
                    .is_some_and(|item| item.pending)
            });
        }
        self.keep_scratch(samples);
        // The nesting notes of sets that went, with no lock held.
        for item in &gone {
            if let Some(inner) = item.nested.as_ref().and_then(Weak::upgrade) {
                inner.remove_parent(&self.this);
            }
        }
        drop(gone);
        events
    }

    /// Hook what the registrations copied out name and have not hooked:
    /// only one whose file names a queue it has not hooked, or that is not
    /// trusted, takes the set's lock for it.
    fn hook_unsettled(&self, samples: &[Sample]) {
        let unsettled: usize = samples.iter().filter(|sample| !sample.settled()).count();
        if unsettled == 0 {
            return;
        }
        let Ok(mut gathered) = fallible::try_with_capacity::<(u64, Named)>(unsettled) else {
            // No room to gather what to hook: every registration not hooked
            // is distrusted for this wait, which then looks again every few
            // milliseconds, and stays to be hooked by the next (fail closed).
            let mut state = self.state.lock();
            for sample in samples.iter().filter(|sample| !sample.settled()) {
                if let Ok(at) = state.items.find(sample.serial) {
                    state.set_trusted(at, false);
                }
            }
            return;
        };
        for sample in samples.iter().filter(|sample| !sample.settled()) {
            let _ = fallible::push_within(&mut gathered, (sample.serial, named(&sample.file)));
        }
        let mut state = self.state.lock();
        for (serial, named) in &gathered {
            self.hook_named(&mut state, *serial, named);
        }
    }

    /// Give the room a wait copied registrations out to back to the set,
    /// emptied with no lock held, unless another wait gave its own back first.
    fn keep_scratch(&self, mut samples: Vec<Sample>) {
        samples.clear();
        let mut state = self.state.lock();
        if state.scratch.capacity() < samples.capacity() {
            state.scratch = samples;
        }
    }

    /// Hook the queues a file named that its registration has not, under the
    /// set's lock, and note whether it can be trusted. A queue that is no
    /// wait queue, one past [`MAX_HOOKS`] or one there is no memory to hook
    /// makes the registration one every wait looks at, as `Sources::add`
    /// leaves the same queue out and stops trusting its wait.
    fn hook_named(&self, state: &mut State, serial: u64, named: &Named) {
        if named.gone {
            return;
        }
        let Ok(at) = state.items.find(serial) else {
            return;
        };
        let mut trusted = named.trusted && !named.overflowed;
        if let Some(item) = state.items.get_mut(at) {
            for source in named.queues.iter().flatten() {
                let Some(queue) = Source::of(source) else {
                    trusted = false;
                    continue;
                };
                if item
                    .hooked
                    .iter()
                    .flatten()
                    .any(|held| held.is(queue.queue()))
                {
                    continue;
                }
                let Some(slot) = item.hooked.iter_mut().find(|slot| slot.is_none()) else {
                    item.unhookable = true;
                    continue;
                };
                if queue.queue().hook(&item.mark).is_err() {
                    item.unhookable = true;
                    continue;
                }
                *slot = Some(queue.hooked());
            }
            // A file that names no queue wakes nothing a wait could sleep
            // on: trusted by no wait, as `Sources::recheck` trusts none.
            if item.unhookable || item.hooked.iter().flatten().next().is_none() {
                trusted = false;
            }
        }
        if let Some(item) = state.items.get_mut(at) {
            item.fixed = named.fixed;
        }
        state.set_trusted(at, trusted);
    }

    /// Record that the program received `events`, the first of what
    /// [`Epoll::ready`] answered: an edge-triggered registration's report is
    /// no longer due and it leaves `pending`, a one-shot one is disarmed and
    /// leaves it, and a level-triggered one moves to the back of it, so the
    /// next wait with a small limit reaches the others first and drops it if
    /// it is no longer ready, as Linux puts it back at the end of its ready
    /// list. A registration a later wait has looked at since stays where that
    /// wait left it.
    pub(crate) fn delivered(&self, events: &[Event]) {
        let mut state = self.state.lock();
        for event in events {
            let Ok(at) = state.items.find(event.serial) else {
                continue;
            };
            let Some(item) = state.items.get_mut(at) else {
                continue;
            };
            item.due = false;
            let newer = item.looked_by != event.look;
            if item.events & EPOLLONESHOT != 0 {
                item.armed = false;
                if !newer {
                    item.pending = false;
                }
            } else if item.events & EPOLLET != 0 {
                if !newer {
                    item.pending = false;
                }
            } else if item.pending {
                let serial = item.serial;
                // To the back: taken out and put again, in the room it had.
                state.pending.retain(|pending| *pending != serial);
                if fallible::push_within(&mut state.pending, serial).is_err()
                    && let Some(item) = state.item(serial)
                {
                    item.pending = false;
                }
            }
        }
        let State { items, pending, .. } = &mut *state;
        pending.retain(|serial| {
            items
                .find(*serial)
                .ok()
                .and_then(|at| items.get(at))
                .is_some_and(|item| item.pending)
        });
    }
}

/// The address a registration keys a file by.
fn key_of(file: &Arc<OpenFile>) -> usize {
    Arc::as_ptr(file).addr()
}

impl Inode for Epoll {
    fn metadata(&self) -> Metadata {
        fs::anon::metadata()
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        true
    }

    /// `lseek` on an epoll set is 0, as Linux's `noop_llseek` answers, while
    /// `pread64` and `pwrite64` stay `ESPIPE`.
    fn seek_is_noop(&self) -> bool {
        true
    }

    /// Readable when a wait would report something, as `ep_eventpoll_poll`
    /// answers. Never writable: Linux's answer has only `EPOLLIN`.
    fn poll(&self) -> Readiness {
        Readiness {
            readable: !self.ready(1).is_empty(),
            writable: false,
            hangup: false,
            error: false,
            priority: false,
        }
    }

    /// Its own queue, which every wake of a registration's woken: to be
    /// trusted when every registration's is.
    fn poll_queues(&self, visit: &mut dyn FnMut(WakeSource)) -> bool {
        visit(fs::wake::shared(self.ready.queue()));
        self.trusted()
    }

    /// The set's own changes, every wake of a registration's queue, and the
    /// changes of the registrations every wait looks at.
    fn poll_changes(&self) -> Option<u64> {
        let (generation, files): (u64, Vec<Weak<OpenFile>>) = {
            let state = self.state.lock();
            (
                state.generation,
                state
                    .items
                    .iter()
                    .filter(|item| item.always())
                    .map(|item| item.file.clone())
                    .collect(),
            )
        };
        let mut sum = generation.wrapping_add(self.ready.tells());
        for file in files {
            if let Some(changes) = file.upgrade().and_then(|file| file.poll_changes()) {
                sum = sum.wrapping_add(changes);
            }
        }
        Some(sum)
    }
}
