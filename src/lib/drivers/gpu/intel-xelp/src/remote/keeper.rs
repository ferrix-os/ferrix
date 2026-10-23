//! The keeper's back end (`docs/INTEL-GPU.md` §12.3, §12.4): the front end's
//! [`Backend`] answered at once from a journal and sent on to a worker
//! process that drives the GT and may die.
//!
//! The journal holds everything live: each VM, each binding with the token
//! of its memory, each hardware context, and each request from its
//! submission to its completion. A call made while a worker is up is
//! journalled and sent; one made while a new worker is starting is only
//! journalled. When the worker dies, stops answering its pings for
//! [`TDR_NS`], or refuses a call the keeper accepted, the keeper starts
//! another, checks its facts are the first worker's word for word, and
//! replays the journal: the hints, the VMs, the bindings, the contexts, and
//! every request not completed, in submission order. A completion is
//! handed to the front end once, whichever worker sent it.
//!
//! It gives up -- completes every request in flight `Hung` and tells its
//! link, whose program exits, which is §11's fallback -- after
//! [`MAX_RESTARTS`] worker deaths in its life, when a new worker's facts
//! differ, or when one reports an inherited engine it could not reset (the
//! consultant's condition 2, ledger 3156: the dead worker's pages are then
//! never handed back by the keeper, and the kernel quarantines them).
//!
//! Nothing allocates: every table has a fixed size, and a full one is the
//! errno i915 gives for running out of memory.

use core::fmt;

use ferrix_linux_abi::errno::Errno;
pub use ferrix_linux_abi::kmsg::Level;

use super::wire::{Call, Event};
use crate::frontend::limits;
use crate::frontend::{
    Backend, Completion, ContextParams, Engine, Facts, HwContext, RequestId, SubmitError, VmId,
};

/// How long a ping may go unanswered before the worker is taken as hung
/// and replaced: two seconds, Windows' TDR (the customer, 2026-10-23,
/// `docs/INTEL-GPU.md` §12.9 question 3).
pub const TDR_NS: u64 = 2_000_000_000;
/// How often the keeper pings a live worker.
pub const PING_NS: u64 = 500_000_000;
/// How long a new worker may take from its start to READY: the GT's
/// bring-up came up in 2 ms on the fake and well under a second on the
/// box; ten seconds is a worker that will not come up.
pub const START_NS: u64 = 10_000_000_000;
/// Worker deaths one keeper survives, as devmgr's budget for a driver
/// (§11): the ninth gives up.
pub const MAX_RESTARTS: u32 = 8;

/// Binding slots: half again the front end's bindings, so the open
/// addressing never runs long.
const BINDING_SLOTS: usize = limits::BINDINGS + limits::BINDINGS / 2;
/// Request slots: the front end's in-flight requests, four times over,
/// since a completed one leaves a hole until the oldest completes.
const REQUEST_SLOTS: usize = limits::REQUESTS * 4;
/// Completions taken from workers and not yet handed to the front end.
const COMPLETIONS: usize = limits::REQUESTS;

/// What the keeper's program supplies: memory, the channel to the worker,
/// the worker's process, and time.
pub trait KeeperLink {
    /// An object's memory, as the front end keeps it.
    type Memory;
    /// What a bind sends so the worker can find the memory's pages: in the
    /// program, the address VMO's handle, read-only on the way out. Valid
    /// while the memory is: the front end unbinds before it frees.
    type Token: Copy;

    /// Memory for a new object of `bytes`.
    ///
    /// # Errors
    ///
    /// `ENOMEM`, or the pin budget's `EDQUOT`.
    fn create_memory(&mut self, bytes: u64) -> Result<Self::Memory, Errno>;

    /// `memory`'s token and its pages, pinning it at its first bind.
    ///
    /// # Errors
    ///
    /// `ENOMEM`, or the pin budget's `EDQUOT`.
    fn token(&mut self, memory: &Self::Memory) -> Result<(Self::Token, u64), Errno>;

    /// Queue `call` for the worker, with `token`'s handle for a bind.
    /// `false` when the worker cannot be reached: dead, or its channel full
    /// past [`TDR_NS`].
    fn send(&mut self, call: &Call, token: Option<Self::Token>) -> bool;

    /// Send what [`KeeperLink::send`] queued. `false` as for `send`.
    fn flush(&mut self) -> bool;

    /// Hand every event the worker sent to `sink`, in order. `false` once
    /// the worker has died (its process ended, or its channel closed).
    fn receive(&mut self, sink: &mut dyn FnMut(Event)) -> bool;

    /// End the current worker, if any, and wait until it has ended; start
    /// worker `generation` and send it [`Call::Hello`]. `false` when it
    /// could not be started.
    fn start_worker(&mut self, generation: u32) -> bool;

    /// Block until the worker sends something or ends, or until `until_ns`.
    fn wait(&mut self, until_ns: u64);

    /// Monotonic now.
    fn now_ns(&mut self) -> u64;

    /// A line of the driver's log at `level`.
    fn say(&mut self, level: Level, line: fmt::Arguments<'_>);

    /// The keeper gave up ([`GiveUp`]); the program exits after this.
    fn gave_up(&mut self, why: GiveUp);
}

/// Why the keeper gave up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GiveUp {
    /// [`MAX_RESTARTS`] workers died.
    Budget,
    /// A new worker's facts were not the first one's.
    Facts,
    /// A worker found an engine its predecessor left running and could not
    /// reset it.
    NotReset,
    /// No worker could be started, or none came up in [`START_NS`].
    Start,
}

/// Why a worker was replaced.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Death {
    /// Its process ended or its channel closed.
    Died,
    /// A ping went unanswered for [`TDR_NS`].
    Unanswered,
    /// It refused a call the keeper had accepted.
    Refused,
    /// It did not come up in [`START_NS`].
    NeverReady,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Worker `generation` was started at `since` and has not said READY.
    Starting { since: u64 },
    /// It is up: the next ping is due at `next_ping`; the outstanding one,
    /// if any, was sent at `sent`.
    Live {
        next_ping: u64,
        sent: Option<(u32, u64)>,
    },
    /// The keeper gave up.
    GaveUp,
}

#[derive(Clone, Copy, Debug)]
struct Bound<T> {
    vm: u32,
    address: u64,
    bytes: u64,
    pages: u64,
    pat_index: u8,
    token: T,
}

#[derive(Clone, Copy, Debug)]
struct Context {
    vm: u32,
    engine: Engine,
    ring_bytes: u32,
}

#[derive(Clone, Copy, Debug)]
struct Request {
    id: RequestId,
    context: u32,
    batch: u64,
    length: u32,
}

/// What a replay sent, for its line.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Replayed {
    /// VMs.
    pub vms: u32,
    /// Bindings.
    pub bindings: u32,
    /// Contexts.
    pub contexts: u32,
    /// Requests not completed.
    pub requests: u32,
}

/// The journal: every live object the worker must hold. About a megabyte:
/// a program makes it once, in a `static`, and lends it to
/// [`RemoteBackend::start`], since the front end's tables already take most
/// of a process's stack.
pub struct Journal<T> {
    vms: [bool; limits::VMS],
    contexts: [Option<Context>; limits::CONTEXTS],
    bindings: [Option<Bound<T>>; BINDING_SLOTS],
    requests: [Option<Request>; REQUEST_SLOTS],
    /// The oldest request slot that may be live, and one past the newest.
    head: usize,
    tail: usize,
}

impl<T> fmt::Debug for Journal<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Journal")
            .field("requests", &(self.tail - self.head))
            .finish_non_exhaustive()
    }
}

impl<T: Copy> Default for Journal<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy> Journal<T> {
    /// An empty journal.
    #[must_use]
    pub const fn new() -> Self {
        Journal {
            vms: [false; limits::VMS],
            contexts: [None; limits::CONTEXTS],
            bindings: [const { None }; BINDING_SLOTS],
            requests: [None; REQUEST_SLOTS],
            head: 0,
            tail: 0,
        }
    }

    /// Empty it again, for a keeper's start.
    fn clear(&mut self) {
        self.vms = [false; limits::VMS];
        self.contexts = [None; limits::CONTEXTS];
        self.bindings.fill(None);
        self.requests.fill(None);
        self.head = 0;
        self.tail = 0;
    }

    /// The first slot of `(vm, address)`'s probe.
    fn home(vm: u32, address: u64) -> usize {
        let key = (address >> 12) ^ (u64::from(vm) << 36);
        let mixed = key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32;
        (mixed as usize) % BINDING_SLOTS
    }

    fn find_binding(&self, vm: u32, address: u64) -> Option<usize> {
        let start = Self::home(vm, address);
        for step in 0..BINDING_SLOTS {
            let at = (start + step) % BINDING_SLOTS;
            match self.bindings.get(at).copied().flatten() {
                Some(bound) if bound.vm == vm && bound.address == address => return Some(at),
                Some(_) => {}
                None => return None,
            }
        }
        None
    }

    fn insert_binding(&mut self, bound: Bound<T>) -> bool {
        let start = Self::home(bound.vm, bound.address);
        for step in 0..BINDING_SLOTS {
            let at = (start + step) % BINDING_SLOTS;
            if let Some(slot) = self.bindings.get_mut(at)
                && slot.is_none()
            {
                *slot = Some(bound);
                return true;
            }
        }
        false
    }

    /// Remove the binding at `at`, moving back the ones its probe passed
    /// over (backward-shift deletion), so every lookup still finds its
    /// entry before an empty slot.
    fn remove_binding(&mut self, at: usize) {
        if let Some(slot) = self.bindings.get_mut(at) {
            *slot = None;
        }
        let mut hole = at;
        let mut next = (at + 1) % BINDING_SLOTS;
        while let Some(bound) = self.bindings.get(next).copied().flatten() {
            let home = Self::home(bound.vm, bound.address);
            // Whether `home` lies cyclically in (hole, next]: if not, the
            // entry may move into the hole.
            let stays = if hole <= next {
                hole < home && home <= next
            } else {
                hole < home || home <= next
            };
            if !stays {
                if let Some(slot) = self.bindings.get_mut(hole) {
                    *slot = Some(bound);
                }
                if let Some(slot) = self.bindings.get_mut(next) {
                    *slot = None;
                }
                hole = next;
            }
            next = (next + 1) % BINDING_SLOTS;
        }
    }

    /// Forget every binding of `vm`: the front end destroys a VM with its
    /// bindings in it, and the back end's `vm_destroy` clears them all. The
    /// rest are put back where a lookup finds them.
    fn drop_vm_bindings(&mut self, vm: u32) {
        let mut dropped = false;
        for slot in &mut self.bindings {
            if slot.is_some_and(|bound| bound.vm == vm) {
                *slot = None;
                dropped = true;
            }
        }
        if !dropped {
            return;
        }
        for at in 0..BINDING_SLOTS {
            let taken = self.bindings.get_mut(at).and_then(Option::take);
            if let Some(bound) = taken {
                let _ = self.insert_binding(bound);
            }
        }
    }

    /// Journal a request, or `false` when no slot is free.
    fn push_request(&mut self, request: Request) -> bool {
        if self.tail - self.head >= REQUEST_SLOTS {
            return false;
        }
        if let Some(slot) = self.requests.get_mut(self.tail % REQUEST_SLOTS) {
            *slot = Some(request);
        }
        self.tail += 1;
        true
    }

    /// Take request `id` out, if it is in flight.
    fn complete(&mut self, id: RequestId) -> bool {
        let mut found = false;
        for n in self.head..self.tail {
            if let Some(slot) = self.requests.get_mut(n % REQUEST_SLOTS)
                && slot.is_some_and(|request| request.id == id)
            {
                *slot = None;
                found = true;
                break;
            }
        }
        while self.head < self.tail
            && self
                .requests
                .get(self.head % REQUEST_SLOTS)
                .is_some_and(Option::is_none)
        {
            self.head += 1;
        }
        found
    }

    fn requests_in_order(&self) -> impl Iterator<Item = Request> + '_ {
        (self.head..self.tail)
            .filter_map(|n| self.requests.get(n % REQUEST_SLOTS).copied().flatten())
    }
}

/// The keeper's back end.
pub struct RemoteBackend<'j, L: KeeperLink> {
    link: L,
    facts: Facts,
    journal: &'j mut Journal<L::Token>,
    phase: Phase,
    generation: u32,
    restarts: u32,
    held: bool,
    watched: bool,
    seq: u32,
    /// The last timestamp sample, `(timestamp, at)`, and the last value
    /// answered, which a later answer never goes below.
    sample: (u64, u64),
    answered: u64,
    done: [(RequestId, Completion); COMPLETIONS],
    pending: usize,
    /// When the current worker was started, for the stall's length.
    started_at: u64,
}

impl<L: KeeperLink> fmt::Debug for RemoteBackend<'_, L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteBackend")
            .field("phase", &self.phase)
            .field("generation", &self.generation)
            .field("restarts", &self.restarts)
            .finish_non_exhaustive()
    }
}

impl<'j, L: KeeperLink> RemoteBackend<'j, L> {
    /// Start the first worker and wait, until [`START_NS`] has passed, for
    /// its READY: its facts are the ones every later worker must repeat.
    /// `journal` is emptied and kept for the keeper's life.
    ///
    /// # Errors
    ///
    /// The link back, with [`GiveUp::Start`] or [`GiveUp::NotReset`]
    /// (said to the link first).
    pub fn start(mut link: L, journal: &'j mut Journal<L::Token>) -> Result<Self, (L, GiveUp)> {
        if !link.start_worker(1) || !link.flush() {
            link.gave_up(GiveUp::Start);
            return Err((link, GiveUp::Start));
        }
        let since = link.now_ns();
        let until = since.saturating_add(START_NS);
        let mut ready = None;
        loop {
            let alive = link.receive(&mut |event| {
                if let Event::Ready {
                    facts,
                    reset_done,
                    timestamp,
                    now_ns,
                } = event
                {
                    ready = ready.or(Some((facts, reset_done, timestamp, now_ns)));
                }
            });
            let now = link.now_ns();
            if ready.is_some() || !alive || now >= until {
                break;
            }
            link.wait(until);
        }
        let Some((facts, reset_done, timestamp, now_ns)) = ready else {
            link.gave_up(GiveUp::Start);
            return Err((link, GiveUp::Start));
        };
        if !reset_done {
            link.gave_up(GiveUp::NotReset);
            return Err((link, GiveUp::NotReset));
        }
        let now = link.now_ns();
        Ok(RemoteBackend {
            link,
            facts,
            journal: {
                journal.clear();
                journal
            },
            phase: Phase::Live {
                next_ping: now.saturating_add(PING_NS),
                sent: None,
            },
            generation: 1,
            restarts: 0,
            held: false,
            watched: false,
            seq: 0,
            sample: (timestamp, now_ns),
            answered: 0,
            done: [(0, Completion::Ok); COMPLETIONS],
            pending: 0,
            started_at: since,
        })
    }

    /// The link.
    pub fn link(&mut self) -> &mut L {
        &mut self.link
    }

    /// Workers that died and were replaced so far.
    #[must_use]
    pub const fn restarts(&self) -> u32 {
        self.restarts
    }

    /// Whether the keeper gave up.
    #[must_use]
    pub fn gave_up(&self) -> bool {
        self.phase == Phase::GaveUp
    }

    /// Whether a worker is up and has the journal.
    #[must_use]
    pub fn live(&self) -> bool {
        matches!(self.phase, Phase::Live { .. })
    }

    /// Send `call` if a worker is up; a failed send is that worker's death.
    fn forward(&mut self, call: &Call, token: Option<L::Token>) {
        if self.live() && !self.link.send(call, token) {
            self.replace(Death::Died);
        }
    }

    /// Replace the worker: end it, start the next, and wait for its READY
    /// in [`RemoteBackend::service`]; or give up.
    fn replace(&mut self, death: Death) {
        if self.phase == Phase::GaveUp {
            return;
        }
        self.restarts += 1;
        let generation = self.generation;
        self.link.say(Level::Warning, format_args!(
            "intel: the GT's worker {generation} is replaced ({death:?}); worker restart {} of {MAX_RESTARTS}",
            self.restarts
        ));
        if self.restarts > MAX_RESTARTS {
            self.give_up(GiveUp::Budget);
            return;
        }
        self.generation += 1;
        if !self.link.start_worker(self.generation) || !self.link.flush() {
            self.give_up(GiveUp::Start);
            return;
        }
        let now = self.link.now_ns();
        self.started_at = now;
        self.phase = Phase::Starting { since: now };
    }

    /// Give up: every request in flight completes `Hung`, and the link is
    /// told, whose program exits.
    fn give_up(&mut self, why: GiveUp) {
        self.phase = Phase::GaveUp;
        for n in self.journal.head..self.journal.tail {
            let taken = self
                .journal
                .requests
                .get_mut(n % REQUEST_SLOTS)
                .and_then(Option::take);
            if let Some(request) = taken {
                self.push_done(request.id, Completion::Hung);
            }
        }
        self.journal.head = self.journal.tail;
        self.link.gave_up(why);
    }

    fn push_done(&mut self, id: RequestId, how: Completion) {
        if let Some(slot) = self.done.get_mut(self.pending) {
            *slot = (id, how);
            self.pending += 1;
        }
    }

    /// A new worker's READY: check it, then replay the journal into it.
    fn on_ready(&mut self, facts: &Facts, reset_done: bool, sample: (u64, u64)) {
        if !matches!(self.phase, Phase::Starting { .. }) {
            return;
        }
        if !reset_done {
            self.give_up(GiveUp::NotReset);
            return;
        }
        if *facts != self.facts {
            self.give_up(GiveUp::Facts);
            return;
        }
        self.sample = sample;
        let now = self.link.now_ns();
        self.phase = Phase::Live {
            next_ping: now.saturating_add(PING_NS),
            sent: None,
        };
        let replayed = self.replay();
        if !self.live() {
            return;
        }
        let stall = now.saturating_sub(self.started_at);
        self.link.say(Level::Notice, format_args!(
            "intel: the GT's worker {} is up, {} ms after its start; replayed {} VMs, {} bindings, {} contexts, {} requests",
            self.generation,
            stall / 1_000_000,
            replayed.vms,
            replayed.bindings,
            replayed.contexts,
            replayed.requests
        ));
    }

    /// Send the whole journal to the new worker. Stops early, having
    /// replaced the worker again, if a send fails.
    fn replay(&mut self) -> Replayed {
        let mut sent = Replayed::default();
        let mut calls_ok = self.link.send(&Call::Held(self.held), None)
            && self.link.send(&Call::Watched(self.watched), None);
        for (slot, _) in self
            .journal
            .vms
            .iter()
            .enumerate()
            .filter(|(_, live)| **live)
        {
            let vm = slot as u32 + 1;
            calls_ok = calls_ok && self.link.send(&Call::VmCreate { vm }, None);
            sent.vms += 1;
        }
        for bound in self.journal.bindings.iter().flatten() {
            let call = Call::Bind {
                vm: bound.vm,
                address: bound.address,
                bytes: bound.bytes,
                pages: bound.pages,
                pat_index: bound.pat_index,
            };
            calls_ok = calls_ok && self.link.send(&call, Some(bound.token));
            sent.bindings += 1;
        }
        for (slot, context) in self.journal.contexts.iter().enumerate() {
            if let Some(context) = context {
                let call = Call::ContextCreate {
                    context: slot as u32 + 1,
                    vm: context.vm,
                    engine: context.engine,
                    ring_bytes: context.ring_bytes,
                };
                calls_ok = calls_ok && self.link.send(&call, None);
                sent.contexts += 1;
            }
        }
        for request in self.journal.requests_in_order() {
            let call = Call::Submit {
                context: request.context,
                batch: request.batch,
                length: request.length,
                request: request.id,
            };
            calls_ok = calls_ok && self.link.send(&call, None);
            sent.requests += 1;
        }
        if !(calls_ok && self.link.flush()) {
            self.replace(Death::Died);
        }
        sent
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Ready {
                facts,
                reset_done,
                timestamp,
                now_ns,
            } => self.on_ready(&facts, reset_done, (timestamp, now_ns)),
            Event::Completed { request, how } => {
                // Once: a request a dead worker completed and a new one
                // ran again is handed over by whichever said it first.
                if self.journal.complete(request) {
                    self.push_done(request, how);
                }
            }
            Event::Pong {
                seq,
                timestamp,
                now_ns,
            } => {
                if let Phase::Live { next_ping, sent } = self.phase
                    && sent.is_some_and(|(outstanding, _)| outstanding == seq)
                {
                    self.phase = Phase::Live {
                        next_ping,
                        sent: None,
                    };
                    self.sample = (timestamp, now_ns);
                }
            }
            Event::Refused { kind, id, errno } => {
                if self.live() {
                    self.link.say(Level::Warning, format_args!(
                        "intel: the GT's worker {} refused call {kind} ({id:#x}) with errno {errno}",
                        self.generation
                    ));
                    self.replace(Death::Refused);
                }
            }
        }
    }

    /// Take the worker's events, and watch its life: its death, its pings,
    /// its start.
    fn watch(&mut self, now: u64) {
        let mut events = [None; 64];
        loop {
            let mut taken = 0;
            let mut more = false;
            let alive = self.link.receive(&mut |event| {
                if let Some(slot) = events.get_mut(taken) {
                    *slot = Some(event);
                    taken += 1;
                } else {
                    more = true;
                }
            });
            for event in events.iter_mut().take(taken) {
                if let Some(event) = event.take() {
                    self.on_event(event);
                }
            }
            if !alive && self.phase != Phase::GaveUp {
                self.replace(Death::Died);
                return;
            }
            if !more {
                break;
            }
        }
        match self.phase {
            Phase::Starting { since } if now.saturating_sub(since) >= START_NS => {
                self.replace(Death::NeverReady);
            }
            Phase::Live {
                sent: Some((_, at)),
                ..
            } if now.saturating_sub(at) >= TDR_NS => self.replace(Death::Unanswered),
            Phase::Live {
                next_ping,
                sent: None,
            } if now >= next_ping => {
                self.seq = self.seq.wrapping_add(1);
                self.phase = Phase::Live {
                    next_ping: now.saturating_add(PING_NS),
                    sent: Some((self.seq, now)),
                };
                self.forward(&Call::Ping { seq: self.seq }, None);
            }
            _ => {}
        }
    }
}

impl<L: KeeperLink> Backend for RemoteBackend<'_, L> {
    type Memory = L::Memory;

    fn facts(&self) -> &Facts {
        &self.facts
    }

    fn create_memory(&mut self, bytes: u64) -> Result<L::Memory, Errno> {
        self.link.create_memory(bytes)
    }

    fn vm_create(&mut self) -> Result<VmId, Errno> {
        let slot = self
            .journal
            .vms
            .iter()
            .position(|live| !live)
            .ok_or(Errno::ENOMEM)?;
        if let Some(live) = self.journal.vms.get_mut(slot) {
            *live = true;
        }
        let vm = slot as u32 + 1;
        self.forward(&Call::VmCreate { vm }, None);
        Ok(VmId(vm))
    }

    fn vm_destroy(&mut self, vm: VmId) {
        if let Some(live) = self.journal.vms.get_mut((vm.0 as usize).wrapping_sub(1)) {
            *live = false;
        }
        self.journal.drop_vm_bindings(vm.0);
        self.forward(&Call::VmDestroy { vm: vm.0 }, None);
    }

    fn bind(
        &mut self,
        vm: VmId,
        address: u64,
        memory: &L::Memory,
        bytes: u64,
        pat_index: u8,
    ) -> Result<(), Errno> {
        if pat_index > self.facts.max_pat_index {
            return Err(Errno::EINVAL);
        }
        if self.journal.find_binding(vm.0, address).is_some() {
            return Err(Errno::EINVAL);
        }
        let (token, pages) = self.link.token(memory)?;
        let bound = Bound {
            vm: vm.0,
            address,
            bytes,
            pages,
            pat_index,
            token,
        };
        if !self.journal.insert_binding(bound) {
            return Err(Errno::ENOMEM);
        }
        let call = Call::Bind {
            vm: vm.0,
            address,
            bytes,
            pages,
            pat_index,
        };
        self.forward(&call, Some(token));
        Ok(())
    }

    fn unbind(&mut self, vm: VmId, address: u64, bytes: u64) {
        if let Some(at) = self.journal.find_binding(vm.0, address) {
            self.journal.remove_binding(at);
        }
        self.forward(
            &Call::Unbind {
                vm: vm.0,
                address,
                bytes,
            },
            None,
        );
    }

    fn context_create(
        &mut self,
        vm: VmId,
        engine: Engine,
        params: ContextParams,
    ) -> Result<HwContext, Errno> {
        let ring = params.ring_bytes;
        if !ring.is_power_of_two() || !(4 << 10..=2 << 20).contains(&ring) {
            return Err(Errno::EINVAL);
        }
        if !self.facts.engines.contains(&engine) {
            return Err(Errno::EINVAL);
        }
        let slot = self
            .journal
            .contexts
            .iter()
            .position(Option::is_none)
            .ok_or(Errno::ENOMEM)?;
        if let Some(cell) = self.journal.contexts.get_mut(slot) {
            *cell = Some(Context {
                vm: vm.0,
                engine,
                ring_bytes: ring,
            });
        }
        let context = slot as u32 + 1;
        let call = Call::ContextCreate {
            context,
            vm: vm.0,
            engine,
            ring_bytes: ring,
        };
        self.forward(&call, None);
        Ok(HwContext(context))
    }

    fn context_destroy(&mut self, context: HwContext) {
        if let Some(cell) = self
            .journal
            .contexts
            .get_mut((context.0 as usize).wrapping_sub(1))
        {
            *cell = None;
        }
        self.forward(&Call::ContextDestroy { context: context.0 }, None);
    }

    fn submit(
        &mut self,
        context: HwContext,
        batch: u64,
        length: u32,
        request: RequestId,
    ) -> Result<(), SubmitError> {
        if self.phase == Phase::GaveUp {
            return Err(SubmitError::Failed(Errno::EIO));
        }
        let journalled = self.journal.push_request(Request {
            id: request,
            context: context.0,
            batch,
            length,
        });
        if !journalled {
            return Err(SubmitError::Full);
        }
        self.forward(
            &Call::Submit {
                context: context.0,
                batch,
                length,
                request,
            },
            None,
        );
        Ok(())
    }

    fn service(&mut self, now_ns: u64, interrupt: bool) {
        if self.phase == Phase::GaveUp {
            return;
        }
        if interrupt {
            self.forward(&Call::Interrupt, None);
        }
        // What the front end asked since the last wait goes first, so the
        // worker's answers to it can be taken now.
        Backend::flush(self);
        self.watch(now_ns);
        Backend::flush(self);
    }

    fn next_deadline(&self) -> Option<u64> {
        match self.phase {
            Phase::Starting { since } => Some(since.saturating_add(START_NS)),
            Phase::Live {
                sent: Some((_, at)),
                ..
            } => Some(at.saturating_add(TDR_NS)),
            Phase::Live {
                next_ping,
                sent: None,
            } => Some(next_ping),
            Phase::GaveUp => None,
        }
    }

    fn completed(&mut self, sink: &mut dyn FnMut(RequestId, Completion)) {
        for (id, how) in self.done.iter().take(self.pending) {
            sink(*id, *how);
        }
        self.pending = 0;
    }

    fn timestamp(&mut self) -> u64 {
        // Extrapolated from the last sample at the command streamer's
        // frequency, so a timestamp never waits on a worker, and never
        // below an earlier answer, whichever worker took the sample.
        let (timestamp, at) = self.sample;
        let now = self.link.now_ns();
        let elapsed = u128::from(now.saturating_sub(at));
        let ticks = elapsed * u128::from(self.facts.cs_timestamp_hz) / 1_000_000_000;
        let value = timestamp.wrapping_add(u64::try_from(ticks).unwrap_or(u64::MAX));
        self.answered = self.answered.max(value);
        self.answered
    }

    fn set_held(&mut self, held: bool) {
        if held != self.held {
            self.held = held;
            self.forward(&Call::Held(held), None);
        }
    }

    fn set_watched(&mut self, watched: bool) {
        if watched != self.watched {
            self.watched = watched;
            self.forward(&Call::Watched(watched), None);
        }
    }

    fn boost(&mut self, request: RequestId) {
        self.forward(&Call::Boost { request }, None);
    }

    fn flush(&mut self) {
        if self.live() && !self.link.flush() {
            self.replace(Death::Died);
        }
    }
}
