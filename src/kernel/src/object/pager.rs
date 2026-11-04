//! Pagers: pager-backed VMOs and the one fault-forwarding mechanism
//! (`docs/microkernel/K3.md`; the consultant's OK IF, ledger 2858).
//!
//! A **pager** is bound to one port. A VMO made over it
//! ([`PagerLink`], `Vmo::new_paged`) has absent pages that are the pager's:
//! never committed as zeros. A fault on one, or a kernel copy that reaches
//! one, opens a [`Request`] for the run of absent pages from it, queues one
//! `PACKET_PAGE_REQUEST` on the pager's port, and waits, bounded by the
//! pager's `wait_ns`. The wait ends in one of five ways (K3.md §2.3):
//!
//! | Ending | The faulting thread |
//! |---|---|
//! | supplied (`pager_supply`, or `PAGER_OP_DIRTY` for a dirty request) | retries |
//! | failed (`PAGER_OP_FAIL`) | `SIGBUS`, or the copy's error; audited |
//! | timed out at the bound | as failed; audited once per request |
//! | its process's end (a signal does not end it) | returns, nothing installed |
//! | the pager's close or detach | as failed, at once; audited |
//!
//! None of them stops the kernel.
//!
//! # A keeper, and a pager parked (K13)
//!
//! A pager with a [`Keeper`] does not die when its handle closes: it is
//! **parked**. Its requests stay open and their waiters stay bounded by the
//! pager's bound; a fault meanwhile is listed and sends nothing. `Keeper::take_up`
//! sends every open request to a new port under its own id. Live, parked and dead
//! are one state, decided under the registry's lock by the pager's close, the
//! keeper's close and the take-up alike (ledger 5968 (1)).
//!
//! # Who shares a request
//!
//! Threads **of one job** that fault the same run share its request; a fault
//! from another job opens its own, with its own packet and its own charge
//! (2858 (1)), so one job's limit can never fail another job's fault. A
//! request ends supplied when a supply makes its first page present; a joiner
//! whose own page that supply did not reach retries and asks again.
//!
//! # Allocation and locks
//!
//! A request's record, its heap charge and its port room are had before the
//! registry's lock is taken, and listing it allocates nothing: the registry
//! keeps room for one more, grown with the lock let go (2858 (2)). The lock
//! is held for one walk of the pager's outstanding requests, which are
//! bounded by the tasks faulting on its VMOs, since each task waits in at
//! most one. It is never held with a space's, a VMO's or a port's lock: a
//! supply takes the pages lock, lets it go, and only then takes this one,
//! and a packet is queued and a waiter woken after it is let go.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::types::{
    PACKET_PAGE_REQUEST, PAGE_REQUEST_DIRTY, PAGE_REQUEST_READ, PAGER_VMO_NO_COPY_WAIT,
    PAGER_VMO_TRAP_DIRTY, PAGER_WAIT_MAX_NS, PAGER_WAIT_MIN_NS, page_request,
};

use crate::audit;
use crate::fallible::{self, AllocError};
use crate::object::channel::Sender;
use crate::object::port::{Port, Promise};
use crate::object::quota::{Charge, Resource};
use crate::object::ticket::{Ticket, issue_for};
use crate::sched::{self, WaitQueue};
use crate::sync::SpinLock;
use crate::user::vmo::Vmo;

/// The most pages one read request asks for: the run the page cache asks a
/// filesystem for (`fs/pages.rs`'s `MAX_FILL_RUN`), so that a pager sees what
/// a filesystem sees.
pub(crate) const REQUEST_RUN: u64 = 32;

/// How many ended requests a pass of [`PagerCore::end_where`] takes out under
/// the lock before it lets go and wakes them: a stack array, so ending
/// allocates nothing.
const END_BATCH: usize = 16;

/// The links' ids: one counter, never reused.
static NEXT_LINK: AtomicU64 = AtomicU64::new(1);

/// How a fault came to ask: what decides whether it may wait and what may end
/// its wait (K3.md §2.4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FaultMode {
    /// A program's own fault, from the trap path: it always may wait.
    Program,
    /// A kernel copy, or the kernel's own touch of a page: into a
    /// `PAGER_VMO_NO_COPY_WAIT` VMO, or where the copy cannot block, it is
    /// refused at once. Either kind's wait ends early only at its process's
    /// end: killable, not interruptible.
    Copy,
}

/// What a request asks of the pager.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    /// The run is absent.
    Read,
    /// A Clean page of a trap-dirty VMO is about to be written.
    Dirty,
}

impl Command {
    /// Its number in the packet.
    const fn number(self) -> u32 {
        match self {
            Command::Read => PAGE_REQUEST_READ,
            Command::Dirty => PAGE_REQUEST_DIRTY,
        }
    }
}

/// Why a page could not be had from its pager.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// The pager failed the request, with this errno.
    Failed(Errno),
    /// The pager's bound passed first.
    TimedOut,
    /// The process is ending.
    Interrupted,
    /// The pager is closed, or the VMO detached from it.
    Gone,
    /// A copy that may not wait: `PAGER_VMO_NO_COPY_WAIT`, or a context that
    /// cannot block (2858 (3)).
    NoWait,
    /// The request's record or its port room could not be had.
    NoMemory,
}

/// The state of a request, under the registry's lock.
const OPEN: u8 = 0;
/// Its first page was supplied.
const SUPPLIED: u8 = 1;
/// The pager failed it.
const FAILED: u8 = 2;
/// The pager closed or detached the VMO.
const GONE: u8 = 3;
/// Its last waiter left before an answer.
const ABANDONED: u8 = 4;

/// One outstanding request: one run, one job.
#[derive(Debug)]
struct Request {
    /// Its id, its ticket's, in its packet: what `PAGER_OP_FAIL` may name.
    id: u64,
    /// The ticket a pager adopts to charge the job whose fault opened it
    /// (`L.user.157`). It ends with the request.
    ticket: Arc<Ticket>,
    /// The link of the VMO it is for.
    link: u64,
    /// The quota slot of the job whose fault opened it.
    job: u32,
    /// What it asks.
    command: Command,
    /// Its first page, the one whose fault opened it.
    first: u64,
    /// How many pages.
    count: u64,
    /// The VMO's key, for its audit record.
    key: u64,
    /// The process whose fault opened it, for its audit record.
    subject: audit::Subject,
    /// [`OPEN`] until it ends, once.
    state: AtomicU8,
    /// The errno a failure carries.
    errno: AtomicU32,
    /// Threads waiting on it; changed under the registry's lock.
    waiters: AtomicUsize,
    /// Where they wait.
    queue: WaitQueue,
    /// The record's heap, charged to the job whose fault opened it (F-37).
    _heap: ferrix_kmem::Charge,
}

impl Request {
    /// Queue its packet, on the port `promise` promised room on. A packet sent to
    /// a port its pager has since left is lost with that port, and take-up sends
    /// it again; handed the same port back, a second packet is possible and
    /// harmless, since a supply skips what is present, `PAGER_OP_DIRTY` is
    /// idempotent, and an answer to an ended request finds nothing (ledger 5968 (4)).
    fn send(&self, promise: Promise) {
        let signals = page_request(
            self.command.number(),
            u32::try_from(self.count).unwrap_or(u32::MAX),
        );
        Port::queue_promised(
            promise,
            PACKET_PAGE_REQUEST,
            self.key,
            signals,
            [self.first.saturating_mul(PAGE_SIZE), self.id],
        );
    }

    /// Its state.
    fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    /// Whether `index` lies in its run.
    fn covers(&self, index: u64) -> bool {
        index >= self.first && index < self.first.saturating_add(self.count)
    }

    /// Whether its run meets `[first, first + count)`.
    fn meets(&self, first: u64, count: u64) -> bool {
        self.first < first.saturating_add(count) && first < self.first.saturating_add(self.count)
    }
}

/// A pager, as its one handle holds it. Closing the handle is the pager's
/// death (K3.md §2.8).
#[derive(Debug)]
pub struct Pager {
    /// What its VMOs and their requests share.
    core: Arc<PagerCore>,
    /// The kernel object it is, charged to the job that made it.
    _charge: Charge,
    /// Its heap, charged likewise (F-37).
    _heap: ferrix_kmem::Charge,
    /// The criticality level of the job that made it: who holds this handle fills
    /// what its VMOs read (`docs/microkernel/K17.md`).
    level: u8,
}

impl Drop for Pager {
    fn drop(&mut self) {
        self.core.close();
    }
}

/// A pager's state, shared by its handle and every VMO made over it.
#[derive(Debug)]
pub struct PagerCore {
    /// How long a fault waits for it, in nanoseconds.
    wait_ns: u64,
    /// Set once, at its death: no request opens after. Read without the lock
    /// on the way in; [`Registry::state`] decides it.
    closed: AtomicBool,
    /// Its outstanding requests and its state.
    requests: SpinLock<Registry>,
}

/// Whether a pager is served, waiting for its next server, or gone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Served {
    /// Its handle is open.
    Live,
    /// Its handle closed with a keeper holding it (K13).
    Parked,
    /// Gone for good.
    Dead,
}

/// A pager's state, under one lock.
#[derive(Debug)]
struct Registry {
    /// Its outstanding requests, with room for one more kept.
    list: Vec<Arc<Request>>,
    /// See [`Served`].
    state: Served,
    /// Whether a keeper holds it.
    kept: bool,
    /// Where its requests go.
    port: Arc<Port>,
    /// Moved at each park and take-up: a fault that read the port before one
    /// goes round again, so nothing is listed against a port it has no room on.
    generation: u64,
}

/// The capability to take a pager up after its handle closes, as its one keeper
/// holds it. Charged to the job that asked: one object, and the heap of the
/// registry it keeps alive, so a parked pager is never an uncharged pool
/// (ledger 5968 (2)(b)).
#[derive(Debug)]
pub struct Keeper {
    /// The pager's criticality level, which the handle it takes up has.
    level: u8,
    /// The pager's state.
    core: Arc<PagerCore>,
    /// The kernel object it is.
    _charge: Charge,
    /// The registry's heap.
    _heap: ferrix_kmem::Charge,
}

impl Drop for Keeper {
    fn drop(&mut self) {
        self.core.keeper_closed();
    }
}

/// Why a take-up was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TakeUpRefused {
    /// The pager is not parked.
    NotParked,
    /// No memory, or the job's limit: it stays parked and a retry may work.
    NoMemory,
}

/// A pager-backed VMO's tie to its pager.
#[derive(Debug)]
pub struct PagerLink {
    /// The pager.
    core: Arc<PagerCore>,
    /// This link's own id, never reused: requests name it.
    id: u64,
    /// The key the VMO's packets carry.
    key: u64,
    /// `PAGER_VMO_*` bits.
    options: u64,
    /// Set at `PAGER_OP_DETACH` or the pager's close.
    detached: AtomicBool,
}

impl Pager {
    /// A pager whose requests go to `port`, whose faults wait at most
    /// `wait_ns` (clamped; 0 is the maximum). Charged to the running task's
    /// job: one object and its heap.
    ///
    /// # Errors
    ///
    /// [`AllocError`] at the job's limits or with no memory.
    pub fn new(port: Arc<Port>, wait_ns: u64, level: u8) -> Result<Pager, AllocError> {
        let charge = Charge::running(Resource::Objects, 1).map_err(|_| AllocError)?;
        let heap = ferrix_kmem::Charge::bytes(
            ferrix_kmem::arc_footprint::<PagerCore>().saturating_add(4 * size_of::<Arc<Request>>()),
        )
        .map_err(|_| AllocError)?;
        let list = fallible::try_with_capacity(4)?;
        let core = fallible::try_arc(PagerCore {
            wait_ns: clamp_wait(wait_ns),
            closed: AtomicBool::new(false),
            requests: SpinLock::new(Registry {
                list,
                state: Served::Live,
                kept: false,
                port,
                generation: 0,
            }),
        })?;
        Ok(Pager {
            core,
            _charge: charge,
            _heap: heap,
            level,
        })
    }

    /// What its VMOs share.
    pub fn core(&self) -> &Arc<PagerCore> {
        &self.core
    }

    /// The criticality level of the job that made it.
    pub const fn level(&self) -> u8 {
        self.level
    }

    /// `pager_keep`: its one keeper, charged to the running task's job.
    ///
    /// # Errors
    ///
    /// `Ok(None)` when it has one; [`AllocError`] at the job's limits.
    pub fn keep(&self) -> Result<Option<Keeper>, AllocError> {
        let charge = Charge::running(Resource::Objects, 1).map_err(|_| AllocError)?;
        let heap = ferrix_kmem::Charge::bytes(
            ferrix_kmem::arc_footprint::<PagerCore>().saturating_add(4 * size_of::<Arc<Request>>()),
        )
        .map_err(|_| AllocError)?;
        let mut registry = self.core.requests.lock();
        if registry.kept {
            return Ok(None);
        }
        registry.kept = true;
        drop(registry);
        Ok(Some(Keeper {
            level: self.level,
            core: Arc::clone(&self.core),
            _charge: charge,
            _heap: heap,
        }))
    }
}

impl Keeper {
    /// The pager's criticality level: the door reads it (ledger 6575 D1).
    pub const fn level(&self) -> u8 {
        self.level
    }

    /// `pager_take_up`: the pager's handle again, its requests sent to `port`.
    ///
    /// Everything fallible comes first, with the lock let go: the new handle's
    /// charges, a promise and a place on the list per open request. If more
    /// opened meanwhile it grows and goes round. A request's own bound does not
    /// restart: each waiter's deadline was set when it began to wait.
    ///
    /// The room a promise reserves is capacity in the port's queue, as K3's every
    /// promise is: charged to nobody, and bounded by the open requests, each of
    /// which holds a record charged to its own job (ledger 5968 (2)(a)).
    ///
    /// # Errors
    ///
    /// [`TakeUpRefused`]; the pager stays as it was.
    pub fn take_up(&self, port: &Arc<Port>) -> Result<Pager, TakeUpRefused> {
        let charge = Charge::running(Resource::Objects, 1).map_err(|_| TakeUpRefused::NoMemory)?;
        let heap = ferrix_kmem::Charge::bytes(
            ferrix_kmem::arc_footprint::<PagerCore>().saturating_add(4 * size_of::<Arc<Request>>()),
        )
        .map_err(|_| TakeUpRefused::NoMemory)?;
        let mut promises: Vec<Promise>;
        let mut sends: Vec<Arc<Request>>;
        let old = loop {
            let wanted = self.core.outstanding();
            promises = fallible::try_with_capacity(wanted).map_err(|_| TakeUpRefused::NoMemory)?;
            sends = fallible::try_with_capacity(wanted).map_err(|_| TakeUpRefused::NoMemory)?;
            for _ in 0..wanted {
                let promise = Promise::new(port).map_err(|_| TakeUpRefused::NoMemory)?;
                // NOALLOC: room for `wanted`.
                promises.push(promise);
            }
            let mut registry = self.core.requests.lock();
            if registry.state != Served::Parked {
                return Err(TakeUpRefused::NotParked);
            }
            if registry.list.len() > wanted {
                continue;
            }
            for open in registry.list.iter().filter(|open| open.state() == OPEN) {
                // NOALLOC: room for `wanted`, at least the listed ones.
                sends.push(Arc::clone(open));
            }
            registry.state = Served::Live;
            registry.generation = registry.generation.wrapping_add(1);
            break core::mem::replace(&mut registry.port, Arc::clone(port));
        };
        // The old port's reference goes with the lock let go (5968 (3)).
        drop(old);
        for request in &sends {
            if let Some(promise) = promises.pop() {
                request.send(promise);
            }
        }
        Ok(Pager {
            level: self.level,
            core: Arc::clone(&self.core),
            _charge: charge,
            _heap: heap,
        })
    }

    /// Whether it is parked: for the boot check.
    pub fn is_parked(&self) -> bool {
        self.core.requests.lock().state == Served::Parked
    }
}

// CHECKS-ONLY: object/pager_check.rs
/// A pager's bound, clamped to `[PAGER_WAIT_MIN_NS, PAGER_WAIT_MAX_NS]`, 0
/// asking for the maximum.
#[doc(hidden)]
pub const fn clamp_wait(wait_ns: u64) -> u64 {
    if wait_ns == 0 || wait_ns > PAGER_WAIT_MAX_NS {
        PAGER_WAIT_MAX_NS
    } else if wait_ns < PAGER_WAIT_MIN_NS {
        PAGER_WAIT_MIN_NS
    } else {
        wait_ns
    }
}

impl PagerCore {
    /// A link for a new VMO with `key` and `options`.
    ///
    /// # Errors
    ///
    /// [`AllocError`] with no memory.
    pub fn link(self: &Arc<Self>, key: u64, options: u64) -> Result<Arc<PagerLink>, AllocError> {
        fallible::try_arc(PagerLink {
            core: Arc::clone(self),
            id: NEXT_LINK.fetch_add(1, Ordering::Relaxed),
            key,
            options,
            detached: AtomicBool::new(false),
        })
    }

    /// Whether `link` is one of this pager's.
    pub fn serves(self: &Arc<Self>, link: &PagerLink) -> bool {
        Arc::ptr_eq(self, &link.core)
    }

    /// Its bound, in nanoseconds.
    pub fn wait_ns(&self) -> u64 {
        self.wait_ns
    }

    /// Requests outstanding.
    pub fn outstanding(&self) -> usize {
        self.requests.lock().list.len()
    }

    /// Threads waiting on its requests: for the boot check.
    pub fn waiting(&self) -> usize {
        self.requests
            .lock()
            .list
            .iter()
            .map(|request| request.waiters.load(Ordering::Relaxed))
            .sum()
    }

    /// The pager's handle closed: its death, or with a keeper its parking, when
    /// no request ends. Run where the close runs, with no spin lock held
    /// (2858 (4)); takes only the registry's lock and wait queues'.
    fn close(&self) {
        debug_assert!(
            !sched::started() || sched::may_block(),
            "a pager was closed where nothing may block"
        );
        let mut registry = self.requests.lock();
        if registry.kept {
            registry.state = Served::Parked;
            registry.generation = registry.generation.wrapping_add(1);
            drop(registry);
            record_rebind(audit::Subject::KERNEL, 0);
            return;
        }
        registry.state = Served::Dead;
        self.closed.store(true, Ordering::Release);
        drop(registry);
        let _ = self.end_where(&|_| true, GONE, Errno(0));
    }

    /// The keeper closed: if the pager is parked, its death, as the pager's own
    /// close; if live, it simply has no keeper again. Under the one lock with
    /// [`PagerCore::close`], so neither order leaves a pager parked for good.
    fn keeper_closed(&self) {
        debug_assert!(
            !sched::started() || sched::may_block(),
            "a keeper was closed where nothing may block"
        );
        let mut registry = self.requests.lock();
        registry.kept = false;
        if registry.state != Served::Parked {
            return;
        }
        registry.state = Served::Dead;
        self.closed.store(true, Ordering::Release);
        drop(registry);
        let _ = self.end_where(&|_| true, GONE, Errno(0));
    }

    /// The port to promise on for a fault, and the generation that answer holds
    /// for: no port while parked, when a request is listed and sends nothing.
    fn current(&self) -> (Option<Arc<Port>>, u64) {
        let registry = self.requests.lock();
        let port = (registry.state == Served::Live).then(|| Arc::clone(&registry.port));
        (port, registry.generation)
    }

    /// End every outstanding request `pick` accepts with `state`, waking its
    /// waiters once the lock is let go; how many ended. Allocates nothing:
    /// at most [`END_BATCH`] a pass.
    fn end_where(&self, pick: &dyn Fn(&Request) -> bool, state: u8, errno: Errno) -> usize {
        let mut ended = 0;
        loop {
            let mut batch: [Option<Arc<Request>>; END_BATCH] = [const { None }; END_BATCH];
            let taken = take_ended(&mut self.requests.lock().list, pick, &mut batch);
            for request in batch.iter().flatten() {
                request.errno.store(u32::from(errno.0), Ordering::Relaxed);
                request.state.store(state, Ordering::Release);
                let _ = request.ticket.end();
                request.queue.wake_all();
            }
            if state == FAILED || state == GONE {
                let why = if state == GONE { Errno::EIDRM } else { errno };
                batch
                    .iter()
                    .flatten()
                    .for_each(|request| record(request, why));
            }
            ended += taken;
            if taken < END_BATCH {
                return ended;
            }
        }
    }

    /// List `fresh`, or join an open request of the same link, job and
    /// command that covers `index`. The request to wait on and whether it is
    /// `fresh`. Refused [`Refusal::Gone`] once the pager has closed or the
    /// link detached, and [`Refusal::NoMemory`] when the registry could not
    /// grow.
    fn list_or_join(
        &self,
        link: &PagerLink,
        fresh: &Arc<Request>,
        index: u64,
        generation: u64,
    ) -> Result<Listed, Refusal> {
        loop {
            let wanted = {
                let mut registry = self.requests.lock();
                if self.closed.load(Ordering::Acquire) || link.detached.load(Ordering::Acquire) {
                    return Err(Refusal::Gone);
                }
                if registry.generation != generation {
                    return Ok(Listed::Stale);
                }
                if let Some(open) = registry.list.iter().find(|open| {
                    open.link == fresh.link
                        && open.job == fresh.job
                        && open.command == fresh.command
                        && open.covers(index)
                        && open.state() == OPEN
                }) {
                    let _ = open.waiters.fetch_add(1, Ordering::Relaxed);
                    return Ok(Listed::Joined(Arc::clone(open)));
                }
                if fallible::push_within(&mut registry.list, Arc::clone(fresh)).is_ok() {
                    return Ok(Listed::Opened);
                }
                registry.list.capacity().saturating_mul(2).max(4)
            };
            // Grown with the lock let go: the room for the next one is had
            // here, never under it (2858 (2)).
            let mut bigger: Vec<Arc<Request>> =
                fallible::try_with_capacity(wanted).map_err(|_| Refusal::NoMemory)?;
            let mut registry = self.requests.lock();
            if registry.list.capacity() < wanted && registry.list.len() <= wanted {
                // NOALLOC: `bigger` holds `wanted`, at least the listed ones.
                bigger.append(&mut registry.list);
                core::mem::swap(&mut registry.list, &mut bigger);
            }
            drop(registry);
            drop(bigger);
        }
    }

    /// The live ticket of the outstanding request `id` of this pager: what
    /// `CHARGE_TICKET ADOPT` through a pager handle finds, so a pager can adopt
    /// only its own requests' while they wait (K3.md §2.6).
    pub fn adoptable_ticket(&self, id: u64) -> Option<Arc<Ticket>> {
        let registry = self.requests.lock();
        registry
            .list
            .iter()
            .find(|request| request.id == id && request.state() == OPEN)
            .map(|request| Arc::clone(&request.ticket))
            .filter(|ticket| ticket.is_live())
    }

    /// A waiter leaves `request`: the state it found, read under the lock.
    /// The last waiter to leave an open request abandons it, and says so.
    fn leave(&self, request: &Arc<Request>) -> (u8, bool) {
        let mut registry = self.requests.lock();
        let state = request.state();
        let left = request
            .waiters
            .fetch_sub(1, Ordering::Relaxed)
            .saturating_sub(1);
        if state != OPEN || left > 0 {
            return (state, false);
        }
        if let Some(at) = registry
            .list
            .iter()
            .position(|open| Arc::ptr_eq(open, request))
        {
            let _ = registry.list.swap_remove(at);
        }
        request.state.store(ABANDONED, Ordering::Release);
        let _ = request.ticket.end();
        (OPEN, true)
    }
}

impl PagerLink {
    /// Whether a write to a Clean page asks the pager first.
    pub fn traps_dirty(&self) -> bool {
        self.options & PAGER_VMO_TRAP_DIRTY != 0
    }

    /// Whether a kernel copy never waits for it.
    pub(crate) fn copies_never_wait(&self) -> bool {
        self.options & PAGER_VMO_NO_COPY_WAIT != 0
    }

    /// Whether the VMO is detached, or its pager closed.
    pub fn is_gone(&self) -> bool {
        self.detached.load(Ordering::Acquire) || self.core.closed.load(Ordering::Acquire)
    }

    /// Ask the pager for page `index` of `vmo` and wait for the answer:
    /// the run of absent pages from it for [`Command::Read`], the page alone
    /// for [`Command::Dirty`]. `Ok` when the access may be retried. Must be
    /// called with no lock held.
    ///
    /// # Errors
    ///
    /// As [`Refusal`].
    pub fn request(
        &self,
        vmo: &Vmo,
        index: u64,
        command: Command,
        mode: FaultMode,
    ) -> Result<(), Refusal> {
        if self.is_gone() {
            return Err(Refusal::Gone);
        }
        // A copy never waits under a spin lock, nor for a VMO that says so;
        // a program's fault always may, from the trap path (2858 (3)).
        if !sched::may_block() || (mode == FaultMode::Copy && self.copies_never_wait()) {
            return Err(Refusal::NoWait);
        }
        let count = match command {
            Command::Read => vmo.absent_run(index, REQUEST_RUN),
            Command::Dirty => 1,
        };
        if count == 0 {
            // Supplied since the caller looked: retry.
            return Ok(());
        }
        let task = sched::current();
        let faulting = task.as_ref().and_then(|task| task.core_process());
        let subject = faulting.map_or(audit::Subject::KERNEL, audit::Subject::of);
        let issuer = faulting.map_or(Sender::KERNEL, Sender::of);
        let job = sched::running_group();
        let ticket = issue_for(issuer, job).map_err(|_| Refusal::NoMemory)?;
        let heap = ferrix_kmem::Charge::bytes(ferrix_kmem::arc_footprint::<Request>())
            .map_err(|_| Refusal::NoMemory)?;
        let fresh = fallible::try_arc(Request {
            id: ticket.id(),
            ticket,
            link: self.id,
            job,
            command,
            first: index,
            count,
            key: self.key,
            subject,
            state: AtomicU8::new(OPEN),
            errno: AtomicU32::new(0),
            waiters: AtomicUsize::new(1),
            queue: WaitQueue::new(),
            _heap: heap,
        })
        .map_err(|_| Refusal::NoMemory)?;
        let request = self.open_or_join(&fresh, index)?;
        drop(fresh);
        self.wait(&request)
    }

    /// List `fresh` for `index`, sending its packet unless parked, or join the
    /// open request that covers it: the request to wait on.
    fn open_or_join(&self, fresh: &Arc<Request>, index: u64) -> Result<Arc<Request>, Refusal> {
        loop {
            let (port, generation) = self.core.current();
            let promise = port
                .as_ref()
                .map(Promise::new)
                .transpose()
                .map_err(|_| Refusal::NoMemory)?;
            drop(port);
            match self.core.list_or_join(self, fresh, index, generation)? {
                Listed::Stale => {}
                Listed::Joined(open) => return Ok(open),
                Listed::Opened => {
                    if let Some(promise) = promise {
                        fresh.send(promise);
                    }
                    return Ok(Arc::clone(fresh));
                }
            }
        }
    }

    /// Wait on `request`, bounded by the pager's `wait_ns`, and leave it.
    fn wait(&self, request: &Arc<Request>) -> Result<(), Refusal> {
        let deadline = crate::timer::now_nanos().saturating_add(self.core.wait_ns);
        // Killable, not interruptible: only the process's end ends the wait
        // early. A signal that could not be delivered would otherwise send a
        // program's fault back to user mode in a loop (K3.md, "As built").
        let interrupted = || sched::work::own_word() & sched::work::END != 0;
        let _ = request
            .queue
            .wait_until_deadline(|| request.state() != OPEN || interrupted(), deadline);
        let (state, abandoned) = self.core.leave(request);
        match state {
            SUPPLIED => Ok(()),
            FAILED => Err(Refusal::Failed(Errno(
                u16::try_from(request.errno.load(Ordering::Relaxed)).unwrap_or(Errno::EIO.0),
            ))),
            GONE => Err(Refusal::Gone),
            _ if interrupted() => Err(Refusal::Interrupted),
            _ => {
                if abandoned {
                    record(request, Errno::ETIMEDOUT);
                }
                Err(Refusal::TimedOut)
            }
        }
    }

    /// End the read requests whose first page lies in `[first, first +
    /// count)`, now present: what a supply does once its pages are in.
    pub fn supplied(&self, first: u64, count: u64) -> usize {
        let id = self.id;
        self.core.end_where(
            &|request| {
                request.link == id
                    && request.command == Command::Read
                    && request.first >= first
                    && request.first < first.saturating_add(count)
            },
            SUPPLIED,
            Errno(0),
        )
    }

    /// End the dirty requests whose page lies in `[first, first + count)`,
    /// now Dirty: `PAGER_OP_DIRTY`.
    pub fn dirtied(&self, first: u64, count: u64) -> usize {
        let id = self.id;
        self.core.end_where(
            &|request| {
                request.link == id
                    && request.command == Command::Dirty
                    && request.first >= first
                    && request.first < first.saturating_add(count)
            },
            SUPPLIED,
            Errno(0),
        )
    }

    /// Fail the requests touching `[first, first + count)` with `errno`:
    /// `PAGER_OP_FAIL`. Only the one whose id is `only`, unless that is 0:
    /// a pager fails one job's request without failing another job's fault
    /// on the same pages (2858 (1)).
    pub fn fail(&self, first: u64, count: u64, errno: Errno, only: u64) -> usize {
        let id = self.id;
        self.core.end_where(
            &|request| {
                request.link == id
                    && request.meets(first, count)
                    && (only == 0 || request.id == only)
            },
            FAILED,
            errno,
        )
    }

    /// Detach the VMO: no request opens for it again, and every outstanding
    /// one ends at once. `PAGER_OP_DETACH`; run where it may block, as the
    /// close (2858 (4)).
    pub fn detach(&self) {
        debug_assert!(
            sched::may_block(),
            "a pager VMO was detached where nothing may block"
        );
        self.detached.store(true, Ordering::Release);
        let id = self.id;
        let _ = self
            .core
            .end_where(&|request| request.link == id, GONE, Errno(0));
    }
}

/// Take out of `requests` up to [`END_BATCH`] that `pick` accepts, into
/// `batch`; how many. Allocates nothing.
fn take_ended(
    requests: &mut Vec<Arc<Request>>,
    pick: &dyn Fn(&Request) -> bool,
    batch: &mut [Option<Arc<Request>>; END_BATCH],
) -> usize {
    let mut taken = 0;
    let mut at = 0;
    while at < requests.len() && taken < END_BATCH {
        if !requests.get(at).is_some_and(|request| pick(request)) {
            at += 1;
            continue;
        }
        if let Some(slot) = batch.get_mut(taken) {
            *slot = Some(requests.swap_remove(at));
        }
        taken += 1;
    }
    taken
}

/// What [`PagerCore::list_or_join`] did.
enum Listed {
    /// The fresh request is listed.
    Opened,
    /// It joined this one.
    Joined(Arc<Request>),
    /// The pager parked or was taken up since the caller read its port.
    Stale,
}

/// The `PAGER` audit record of a park (the kernel's own, as a dying table's) or a
/// take-up, which is a grant of authority over the pager's clients (ledger 5968 (6)).
pub fn record_rebind(subject: audit::Subject, outstanding: u32) {
    audit::record(
        audit::PAGER,
        audit::Outcome::Done,
        0,
        subject,
        audit::Target {
            kind: audit::target::JOB,
            id: subject.job,
        },
        [outstanding, 0, 0],
    );
}

/// The `PAGER` audit record of `request`, ended without its page.
fn record(request: &Request, why: Errno) {
    audit::record(
        audit::PAGER,
        audit::Outcome::Refused,
        i16::try_from(why.0).unwrap_or(i16::MAX),
        request.subject,
        audit::Target {
            kind: audit::target::JOB,
            id: request.subject.job,
        },
        [
            request.key as u32,
            (request.key >> 32) as u32,
            request.first as u32,
        ],
    );
}
