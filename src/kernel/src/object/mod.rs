//! The native ABI's kernel objects, and what a handle names.
//!
//! Stage 9 of `docs/ROADMAP.md`. The rules — which handles are valid, which
//! rights they carry, when a queue is full — are `src/lib/kernel/objects` and
//! `src/lib/proto/native-abi`, where the host tests and the fuzzer reach them. This is
//! the part that needs a kernel: the reference counts, the locks, and freeing
//! what an object held.
//!
//! # Dropping is deferred, and why
//!
//! An object can hold other objects. A channel endpoint holds the messages
//! queued for it, and a message holds handles to anything — including other
//! endpoints, holding further messages. Dropping the last reference to the
//! outermost one would drop everything inside it recursively, on a
//! sixteen-kibibyte kernel stack, to a depth the program chose. A chain of a
//! few thousand endpoints, each queued in the next, is a short loop in ring 3
//! and a guard-page fault in ring 0.
//!
//! So an object that contains objects never drops them itself: it hands them
//! to [`dispose`], which drops them one level at a time in a loop. However
//! deep the chain, the stack holds one drop at a time.

pub(crate) mod alloc_check;
pub(crate) mod channel;
pub(crate) mod check;
pub(crate) mod device_copy_check;
pub(crate) mod domain_check;
pub(crate) mod edge_check;
pub(crate) mod fast_path_check;
pub(crate) mod format_check;
pub(crate) mod interrupt;
pub(crate) mod io_mapping;
pub(crate) mod job;
pub(crate) mod oom;
pub(crate) mod oprof;
pub(crate) mod pin;
pub(crate) mod port;
pub(crate) mod process;
pub(crate) mod quota;
pub(crate) mod quota_check;
pub(crate) mod write_read_check;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::sync::SpinLock;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals;

use crate::device::DeviceNode;
use crate::fallible;
use crate::sched::WaitQueue;
use crate::user::vmo::Vmo;

/// The most handles one process may hold at once.
///
/// A resource limit, not a structural one: the table itself can grow to
/// `ferrix_objects::table::MAX_SLOTS`. Four thousand is far more than a driver
/// needs, and small enough that a program leaking handles in a loop hits it
/// long before the heap notices.
pub(crate) const HANDLE_LIMIT: usize = 4096;

/// Something a handle can name.
#[derive(Debug, Clone)]
pub(crate) enum Object {
    /// One end of a channel.
    Channel(Arc<channel::Endpoint>),
    /// A memory object.
    Vmo(Arc<Vmo>),
    /// A container of processes.
    Job(Arc<job::Job>),
    /// A device node, from which its interrupts and I/O mappings are minted.
    Device(Arc<DeviceNode>),
    /// A device interrupt.
    Interrupt(Arc<interrupt::Interrupt>),
    /// A device aperture a driver may map.
    IoMapping(Arc<io_mapping::IoMapping>),
    /// Pages of a VMO a device may reach.
    Pin(Arc<pin::Pin>),
    /// A process: how it ended, and not the process itself.
    Process(process::ProcessRef),
    /// An event queue.
    Port(Arc<port::Port>),
    /// The capability to ask the kernel to start `devmgr`
    /// (`docs/INIT.md` §7.3, L12): a token with nothing in it. What it
    /// permits is decided where `devmgr_start` is answered -- the kernel
    /// loads `devmgr`, writes its DEVICES messages and starts it in the job
    /// the holder names, and hands back a process handle -- so the device
    /// nodes and the images travel between the kernel and `devmgr` alone.
    /// The one there is goes into pid 1's table with `MANAGE` and no other
    /// right: it is neither duplicated nor sent, and goes when pid 1 goes.
    Starter,
    /// The capability to read the audit record
    /// (`docs/certification/AUDIT.md` §4): a token with nothing in it, as
    /// the record is the kernel's one store. Pid 1 is given the one there
    /// is, with `READ` and no other right, so it is neither duplicated nor
    /// sent; `audit_read` answers the holder, and nobody else can ask.
    Audit,
}

/// A queue nothing is woken on, for objects whose signals never change.
///
/// A wait on one of them still ends: at its deadline, or when the waiting
/// process is killed, which the wait's own periodic recheck notices.
static QUIET: WaitQueue = WaitQueue::new();

impl Object {
    /// What a waiter on this object would see now.
    pub(crate) fn signals(&self) -> Signals {
        match self {
            Object::Channel(endpoint) => endpoint.signals(),
            Object::Vmo(_) => Signals::NONE,
            Object::Job(job) => job.signals(),
            Object::Interrupt(interrupt) if interrupt.is_pending() => Signals::READABLE,
            Object::Port(port) if !port.is_empty() => Signals::READABLE,
            Object::Process(process) if process.exit().is_closed() => Signals::TERMINATED,
            Object::Device(_)
            | Object::Interrupt(_)
            | Object::IoMapping(_)
            | Object::Pin(_)
            | Object::Process(_)
            | Object::Port(_)
            | Object::Starter
            | Object::Audit => Signals::NONE,
        }
    }

    /// The queue woken whenever this object's signals may have changed.
    ///
    /// "May have": a waiter is woken for any change and looks at the level
    /// again, so a queue woken too often costs a recheck and one woken too
    /// rarely is a wait that sleeps out its deadline.
    pub(crate) fn waiters(&self) -> &WaitQueue {
        match self {
            Object::Channel(endpoint) => endpoint.waiters(),
            Object::Job(job) => job.waiters(),
            Object::Port(port) => port.waiters(),
            Object::Process(process) => process.exit().exited(),
            // Woken from the interrupt handler itself; see `interrupt`.
            Object::Interrupt(interrupt) => interrupt.waiters(),
            Object::Vmo(_)
            | Object::Device(_)
            | Object::IoMapping(_)
            | Object::Pin(_)
            | Object::Starter
            | Object::Audit => &QUIET,
        }
    }
}

/// A process's handle table.
pub(crate) type HandleTable = ferrix_objects::table::HandleTable<Object>;

/// An object travelling in a message, with the rights its handle carried.
///
/// The rights travel with it: a read-only VMO sent to another process arrives
/// read-only, which is what lets `devmgr` hand a driver less than it holds.
pub(crate) type Transfer = (Object, Rights);

/// Held by every send that carries a channel endpoint, from its cycle check
/// to its push.
///
/// Endpoints keep each other alive only through their queues, so only such a
/// send adds an edge to the graph of who keeps whom alive — and a cycle in
/// that graph is memory nothing can free. Checking for one is a walk, and a
/// walk is an answer only about a graph nobody is adding to, so the adders
/// take turns. Reads remove edges and never take it; sends of bytes and VMOs
/// add none and never take it.
///
/// Taken before a process's handle table or any queue, never while holding
/// either.
pub(crate) static TOPOLOGY: SpinLock<()> = SpinLock::new(());

/// Objects waiting to be dropped.
static ORPHANS: SpinLock<Vec<Object>> = SpinLock::new(Vec::new());

/// Whether some context is already draining [`ORPHANS`].
static DISPOSING: AtomicBool = AtomicBool::new(false);

/// Objects being dropped where they were disposed of, because [`ORPHANS`]
/// could not grow to queue them.
static DROPPING_IN_PLACE: AtomicUsize = AtomicUsize::new(0);

/// How deep drops in place may nest before an object is given up instead.
pub(crate) const IN_PLACE_DEPTH: usize = 4;

/// Objects given up, because [`ORPHANS`] could not grow and drops in place
/// were already [`IN_PLACE_DEPTH`] deep: see [`defer`].
static ABANDONED: AtomicU64 = AtomicU64::new(0);

/// Drop `objects`, and everything they contain, without recursing.
///
/// An object that holds no other object is dropped here, at once: its drop
/// cannot reach another, so it cannot recurse, and a close of one has to have
/// let go of what it held by the time it returns -- an interrupt's line, which
/// a restarted driver claims again straight away. The rest are queued.
///
/// If another drop is already draining the queue — this one was reached from
/// inside an object's `Drop` — the queued objects are left for that loop and
/// this returns at once, which is what bounds the depth at one. On another
/// processor the same thing happens, and the clear-then-recheck at the bottom
/// is what stops an object queued just as the drainer finished from being
/// stranded.
///
/// A channel end is closed here too, whoever is draining: the last reference
/// to it goes at once, so its peer sees `PEER_CLOSED` by the time the close
/// returns, and only what its unread messages carry is queued. Queued whole,
/// it stayed open until the drainer reached it -- on another processor, some
/// time after the close had returned -- and a quiesce made the moment a
/// driver's channel closed was refused as still served (FX-1004).
///
/// Call it with no lock held that an object's drop might need: a channel's
/// queue, a process's handle table.
pub(crate) fn dispose(objects: impl IntoIterator<Item = Object>) {
    for object in objects {
        match object {
            // Another reference -- a message in flight, a call its holder is
            // making on it -- keeps it open; whoever drops that one closes
            // it. Nothing done on the peer holds it (see `channel`).
            Object::Channel(end) => {
                if let Some(end) = Arc::into_inner(end) {
                    let carried = end
                        .take_unread()
                        .into_iter()
                        .flat_map(|message| message.handles.into_iter().map(|(object, _)| object));
                    carried.for_each(defer);
                    drop(end);
                }
            }
            object if object.drops_at_once() => drop(object),
            object => defer(object),
        }
    }
    loop {
        if DISPOSING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        loop {
            let batch = core::mem::take(&mut *ORPHANS.lock());
            if batch.is_empty() {
                break;
            }
            // The lock is released before anything is dropped, so a drop
            // that disposes further objects can take it again.
            drop(batch);
        }
        DISPOSING.store(false, Ordering::Release);
        if ORPHANS.lock().is_empty() {
            return;
        }
    }
}

/// Queue `object` on [`ORPHANS`] for the drainer.
///
/// A dispose happens as something closes, which has nobody to tell that
/// memory ran out (finding F-23). So when the queue cannot grow, the object
/// is dropped here instead -- which is what the queue was avoiding, a drop
/// that can reach others and recurse, so only while fewer than
/// [`IN_PLACE_DEPTH`] such drops are under way. Past that it is given up:
/// never dropped, its memory lost, and counted in [`ABANDONED`], which the
/// boot report prints. A machine gets there only with its heap exhausted and
/// four closes deep in nested channels at once, and losing an object's memory
/// is the one outcome of the three -- stop, overflow the stack, or this --
/// that leaves it running.
fn defer(object: Object) {
    let refused = {
        let mut orphans = ORPHANS.lock();
        match fallible::try_reserve(&mut orphans, 1) {
            Ok(()) => fallible::push_within(&mut orphans, object).err(),
            Err(_) => Some(object),
        }
    };
    let Some(object) = refused else { return };
    if DROPPING_IN_PLACE.fetch_add(1, Ordering::AcqRel) < IN_PLACE_DEPTH {
        drop(object);
    } else {
        let _ = ABANDONED.fetch_add(1, Ordering::Relaxed);
        #[expect(
            clippy::mem_forget,
            reason = "AUDIT: an object given up under memory exhaustion rather than dropped \
                      where its drop could recurse without bound; see `defer`"
        )]
        core::mem::forget(object);
    }
    let _ = DROPPING_IN_PLACE.fetch_sub(1, Ordering::AcqRel);
}

/// Objects [`defer`] has given up since boot.
pub(crate) fn abandoned() -> u64 {
    ABANDONED.load(Ordering::Relaxed)
}

impl Object {
    /// Whether [`dispose`] drops it where it is rather than queueing it.
    ///
    /// Those whose drop reaches no other object: it cannot recurse, and a
    /// close has let go of what they held by the time it returns. A job holds
    /// its processes, which is what the queue is for; a channel end, which
    /// holds the messages queued for it, is closed at once and only what they
    /// carry queued (see [`dispose`]). A process handle holds only how the process
    /// ended, but is queued as well: the end of a process is the heaviest
    /// teardown in the kernel, and nothing waits on its handle's close.
    fn drops_at_once(&self) -> bool {
        match self {
            Object::Vmo(_)
            | Object::Device(_)
            | Object::Interrupt(_)
            | Object::IoMapping(_)
            | Object::Pin(_)
            | Object::Port(_)
            | Object::Starter
            | Object::Audit => true,
            Object::Channel(_) | Object::Job(_) | Object::Process(_) => false,
        }
    }
}

/// Run `f` as though another context were draining disposed objects -- what a
/// close sees while another processor disposes -- for the checks.
///
/// Waits for a real drain to finish first, so that the mark is this call's to
/// set and to clear, and drains afterwards whatever `f` left queued.
pub(crate) fn as_if_draining_elsewhere<R>(f: impl FnOnce() -> R) -> R {
    while DISPOSING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        core::hint::spin_loop();
    }
    let result = f();
    DISPOSING.store(false, Ordering::Release);
    dispose([]);
    result
}
