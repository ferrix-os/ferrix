//! Requests and fences: completions from the back end, submission of what
//! became ready, retirement, the kernel fences signalled for sync files and
//! dmabuf slots, and the waits held as deferred replies.
//!
//! i915's `i915_request.c`, `i915_scheduler.c` and `i915_sw_fence.c` are left
//! out (`docs/INTEL-GPU.md` §3.6); their semantics are kept here: requests
//! complete in order per timeline, a fence signals with the request's error,
//! a request whose fence it waits on failed is skipped with that error
//! (`__i915_request_skip`), and a hang marks the guilty request and bans its
//! context as `context_mark_guilty` does.

use ferrix_linux_abi::errno::Errno;

use super::backend::{Completion, ContextParams, SubmitError};
use super::drm::{Call, KDATA};
use super::objects::{FenceRef, KernelFence, RequestState, Slab, SyncFence, Tables};

/// How long a held wait goes unanswered before the trace names it
/// (`ferrix.intelgpu.uapitrace`): once, with its fences and the requests not
/// done, the call a hung program is in.
pub const STUCK_NS: u64 = 2_000_000_000;
use super::{Backend, Frontend, Host, RequestId, gem, limits, syncobj, vm};

/// A held wait's kind.
#[derive(Clone, Copy, Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "held in a fixed table of limits::WAITS, never boxed: there is no allocator"
)]
pub enum WaitKind {
    /// `GEM_WAIT` on an object.
    Object {
        /// The object's slot.
        object: u16,
        /// Its generation, so a freed and reused slot reads as idle.
        generation: u8,
        /// When the wait began, for the time left.
        start_ns: u64,
    },
    /// `SYNCOBJ_WAIT` or `SYNCOBJ_TIMELINE_WAIT`.
    Syncobjs(syncobj::SyncWait),
}

/// What a handler asks to hold: its deadline (monotonic ns; `None` for
/// ever) and what it waits for.
#[derive(Clone, Copy, Debug)]
pub struct Hold {
    /// When it gives up.
    pub deadline: Option<u64>,
    /// What it waits for.
    pub kind: WaitKind,
}

/// A held wait: the request to answer, and the argument to copy back.
#[derive(Clone, Copy, Debug)]
pub struct Wait {
    /// The chardev request.
    pub request: u64,
    /// The file's slot.
    pub file: u8,
    /// The program's address of the argument.
    pub arg: u64,
    /// The bytes to copy back.
    pub out_size: u8,
    /// The argument, as the handler left it.
    pub data: [u8; 48],
    /// When it gives up.
    pub deadline: Option<u64>,
    /// What it waits for.
    pub kind: WaitKind,
    /// When it was held (monotonic ns).
    pub since: u64,
    /// Whether the trace has named it as unanswered.
    pub told: bool,
}

/// The held waits.
pub struct Waits {
    slots: Slab<Wait, { limits::WAITS }>,
}

impl Waits {
    /// None.
    #[must_use]
    pub const fn new() -> Self {
        Self { slots: Slab::new() }
    }

    /// Hold `call` until `hold` settles.
    ///
    /// # Errors
    ///
    /// `ENOMEM` when [`limits::WAITS`] are held already.
    pub fn hold(
        &mut self,
        call: &Call,
        kdata: &[u8; KDATA],
        hold: Hold,
        now: u64,
    ) -> Result<(), Errno> {
        let mut data = [0_u8; 48];
        let length = call.out_size.min(48);
        if let (Some(target), Some(source)) = (data.get_mut(..length), kdata.get(..length)) {
            target.copy_from_slice(source);
        }
        self.slots
            .insert(Wait {
                request: call.request,
                file: call.file,
                arg: call.arg,
                out_size: u8::try_from(length).unwrap_or(0),
                data,
                deadline: hold.deadline,
                kind: hold.kind,
                since: now,
                told: false,
            })
            .map(|_| ())
            .ok_or(Errno::ENOMEM)
    }

    /// The earliest deadline; with `trace`, also when a wait not yet named
    /// becomes [`STUCK_NS`] old.
    #[must_use]
    pub fn next_deadline(&self, trace: bool) -> Option<u64> {
        self.slots
            .iter()
            .flat_map(|(_, wait)| {
                let stuck = (trace && !wait.told).then(|| wait.since.saturating_add(STUCK_NS));
                [wait.deadline, stuck]
            })
            .flatten()
            .min()
    }

    /// Forget every wait whose program gave up on it: a signal abandons a
    /// request in the chardev core (the program's call returns `EINTR`,
    /// and libdrm calls again), and the driver is not told, so the wait
    /// would keep its slot until what it waits for signals, for ever with
    /// no deadline. The core refuses any copy for an abandoned request, so
    /// a one-byte copy-in from the wait's argument tells. How many went.
    pub fn sweep_abandoned<M, H: Host<M>>(&mut self, host: &mut H) -> usize {
        let mut doomed = [usize::MAX; limits::WAITS];
        let mut count = 0;
        for (index, wait) in self.slots.iter() {
            let mut probe = [0_u8; 1];
            if host.copy_in(wait.request, wait.arg, &mut probe).is_err()
                && let Some(slot) = doomed.get_mut(count)
            {
                *slot = index;
                count += 1;
            }
        }
        for index in doomed.iter().take(count) {
            let _ = self.slots.remove(*index);
        }
        count
    }

    /// Forget every wait of a released file: its program is gone.
    pub fn drop_file(&mut self, file: u8) {
        let mut doomed = [usize::MAX; limits::WAITS];
        let mut count = 0;
        for (index, wait) in self.slots.iter() {
            if wait.file == file
                && let Some(slot) = doomed.get_mut(count)
            {
                *slot = index;
                count += 1;
            }
        }
        for index in doomed.iter().take(count) {
            let _ = self.slots.remove(*index);
        }
    }

    /// How many are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether none is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

impl core::fmt::Debug for Waits {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "Waits({})", self.slots.len())
    }
}

impl Default for Waits {
    fn default() -> Self {
        Self::new()
    }
}

/// A completion's status, as the request's fence carries it.
const fn status_of(completion: Completion) -> i32 {
    match completion {
        Completion::Ok => 0,
        Completion::Hung | Completion::Cancelled => -(Errno::EIO.0 as i32),
    }
}

/// Request `id` finished.
pub fn complete<M>(state: &mut Tables<M>, id: RequestId, completion: Completion) {
    let Some(request) = state.requests.get_mut(id) else {
        return;
    };
    if matches!(request.state, RequestState::Done(_)) {
        return;
    }
    let status = status_of(completion);
    request.state = RequestState::Done(status);
    let context = request.context;
    if let Some(entry) = state.contexts.get_mut(usize::from(context)) {
        match completion {
            Completion::Ok => {}
            Completion::Hung => {
                entry.guilty = entry.guilty.saturating_add(1);
                // `context_mark_guilty`: banned unless recoverable, and
                // never when not bannable.
                if entry.bannable && !entry.recoverable {
                    entry.banned = true;
                }
            }
            Completion::Cancelled => entry.lost = entry.lost.saturating_add(1),
        }
    }
    if status != 0 {
        propagate_error(state, id, status);
    }
}

/// Every fence that names request `id` signals with `status` from now on,
/// even once the request retires (which forgets statuses).
fn propagate_error<M>(state: &mut Tables<M>, id: RequestId, status: i32) {
    let failed = FenceRef::Request(id);
    let fix = |fence: &mut FenceRef| {
        if *fence == failed {
            *fence = FenceRef::Signalled(status);
        }
    };
    for (_, object) in state.objects.iter_mut() {
        fix(&mut object.write);
        object.reads.iter_mut().for_each(fix);
    }
    for (_, sync) in state.syncobjs.iter_mut() {
        match &mut sync.fence {
            SyncFence::None => {}
            SyncFence::Single(fence) => fix(fence),
            SyncFence::Chain { prefix, points, .. } => {
                fix(prefix);
                for (_, fence) in points.iter_mut() {
                    fix(fence);
                }
            }
        }
    }
    for (_, kernel) in state.kernel_fences.iter_mut() {
        fix(&mut kernel.fence);
    }
    for queued in state.requests.live_ids() {
        if let Some(request) = state.requests.get_mut(queued) {
            request.deps.iter_mut().for_each(fix);
        }
    }
}

/// A kernel fence for `fence`, which [`settle`] signals once `fence` has.
///
/// # Errors
///
/// `ENOMEM` when [`limits::KERNEL_FENCES`] are pending.
pub fn track_kernel_fence<M>(
    state: &mut Tables<M>,
    cookie: u64,
    fence: FenceRef,
) -> Result<(), Errno> {
    state
        .kernel_fences
        .insert(KernelFence { cookie, fence })
        .map(|_| ())
        .ok_or(Errno::ENOMEM)
}

/// Offer every queued request whose fences have signalled to the back end,
/// in id order, keeping each hardware context's order. Whether anything
/// moved.
fn pump<B: Backend>(backend: &mut B, state: &mut Tables<B::Memory>) -> bool {
    let mut moved = false;
    // A context slot and engine slot that already holds back a request: no
    // later request of it may pass.
    let mut blocked = [[false; limits::CONTEXT_ENGINES]; limits::CONTEXTS];
    for id in state.requests.live_ids() {
        let Some(request) = state.requests.get(id).copied() else {
            continue;
        };
        if request.state != RequestState::Queued {
            continue;
        }
        let context = usize::from(request.context);
        let slot = usize::from(request.slot);
        if blocked
            .get(context)
            .and_then(|row| row.get(slot))
            .copied()
            .unwrap_or(true)
        {
            continue;
        }
        let mut waiting = false;
        let mut failed = 0;
        for dep in request.deps.iter().take(usize::from(request.dep_count)) {
            match state.requests.status(*dep) {
                None => waiting = true,
                Some(0) => {}
                Some(error) => failed = error,
            }
        }
        let block = |blocked: &mut [[bool; limits::CONTEXT_ENGINES]; limits::CONTEXTS]| {
            if let Some(cell) = blocked.get_mut(context).and_then(|row| row.get_mut(slot)) {
                *cell = true;
            }
        };
        if waiting {
            block(&mut blocked);
            continue;
        }
        let banned = state.contexts.get(context).is_none_or(|entry| entry.banned);
        let outcome = if failed != 0 {
            Err(failed)
        } else if banned {
            Err(-(Errno::EIO.0 as i32))
        } else {
            submit_one(backend, state, id)
        };
        match outcome {
            Ok(true) => moved = true,
            Ok(false) => block(&mut blocked),
            Err(status) => {
                // Skipped, never run, with the error that stopped it.
                if let Some(entry) = state.requests.get_mut(id) {
                    entry.state = RequestState::Done(status);
                }
                propagate_error(state, id, status);
                moved = true;
            }
        }
    }
    moved
}

/// Hand request `id` to the back end, making its hardware context first.
/// `Ok(false)` when the back end is full.
fn submit_one<B: Backend>(
    backend: &mut B,
    state: &mut Tables<B::Memory>,
    id: RequestId,
) -> Result<bool, i32> {
    let errno = |errno: Errno| -(errno.0 as i32);
    let request = state
        .requests
        .get(id)
        .copied()
        .ok_or(errno(Errno::EINVAL))?;
    let context = usize::from(request.context);
    let slot = usize::from(request.slot);
    let entry = state
        .contexts
        .get(context)
        .copied()
        .ok_or(errno(Errno::ENOENT))?;
    let hw = match entry.hw.get(slot).copied().flatten() {
        Some(hw) => hw,
        None => {
            let vm_id = vm::backend_vm(backend, state, entry.vm).map_err(errno)?;
            let hw = backend
                .context_create(vm_id, request.engine, ContextParams::DEFAULT)
                .map_err(errno)?;
            if let Some(cell) = state
                .contexts
                .get_mut(context)
                .and_then(|entry| entry.hw.get_mut(slot))
            {
                *cell = Some(hw);
            }
            hw
        }
    };
    match backend.submit(hw, request.batch, request.length, id) {
        Ok(()) => {
            if let Some(entry) = state.requests.get_mut(id) {
                entry.state = RequestState::Submitted;
            }
            Ok(true)
        }
        Err(SubmitError::Full) => Ok(false),
        Err(SubmitError::Failed(failure)) => Err(errno(failure)),
    }
}

/// Take completions and submit until neither moves, then retire, free what
/// is idle, signal the kernel's fences and answer the waits that settled.
pub fn settle<B: Backend, H: Host<B::Memory>>(frontend: &mut Frontend<B>, host: &mut H) {
    let Frontend { backend, state } = frontend;
    for _ in 0..limits::REQUESTS {
        let mut moved = false;
        backend.completed(&mut |id, completion| {
            complete(state, id, completion);
            moved = true;
        });
        moved |= pump(backend, state);
        if !moved {
            break;
        }
    }
    retire(backend, state);
    backend.set_held(holds_queued(state));
    signal_kernel_fences(state, host);
    answer_waits(state, host);
    boost_waits(backend, state);
}

/// `intel_rps_boost` for every fence a held wait still waits on
/// (`i915_gem_object_wait`'s and `i915_request_wait`'s boost): the back end
/// raises the GT's frequency until each request retires. i915 boosts only a
/// request that has not started; here a started one is boosted too, since a
/// program blocked on it is the case the boost is for and the GT may be
/// running it at any frequency.
fn boost_waits<B: Backend>(backend: &mut B, state: &Tables<B::Memory>) {
    let mut boost = |fence: FenceRef| {
        if state.requests.status(fence).is_none()
            && let FenceRef::Request(id) | FenceRef::UpTo(id) = fence
        {
            backend.boost(id);
        }
    };
    for (_, wait) in state.waits.slots.iter() {
        match &wait.kind {
            WaitKind::Object {
                object, generation, ..
            } => {
                let index = usize::from(*object);
                if state.generations.get(index) != Some(generation) {
                    continue;
                }
                if let Some(entry) = state.objects.get(index) {
                    boost(entry.write);
                    for read in entry.reads {
                        boost(read);
                    }
                }
            }
            WaitKind::Syncobjs(sync) => {
                for fence in sync.fences.iter().take(usize::from(sync.count)).flatten() {
                    boost(*fence);
                }
            }
        }
    }
}

/// Whether a live request is still queued here, not yet given to the back
/// end.
fn holds_queued<M>(state: &Tables<M>) -> bool {
    state.requests.live_ids().any(|id| {
        state
            .requests
            .get(id)
            .is_some_and(|request| matches!(request.state, RequestState::Queued))
    })
}

/// Retire finished requests, then free the objects, contexts and VMs that
/// nothing holds and nothing uses.
fn retire<B: Backend>(backend: &mut B, state: &mut Tables<B::Memory>) {
    while let Some(request) = state.requests.retire_one() {
        if let Some(context) = state.contexts.get_mut(usize::from(request.context)) {
            context.in_flight = context.in_flight.saturating_sub(1);
        }
    }
    gem::free_idle(backend, state);
    vm::free_idle(backend, state);
}

/// Signal every kernel fence whose fence has signalled.
fn signal_kernel_fences<M, H: Host<M>>(state: &mut Tables<M>, host: &mut H) {
    let mut done = [usize::MAX; 64];
    loop {
        let mut count = 0;
        for (index, kernel) in state.kernel_fences.iter() {
            if let Some(status) = state.requests.status(kernel.fence) {
                host.sync_signal(kernel.cookie, status);
                if let Some(slot) = done.get_mut(count) {
                    *slot = index;
                    count += 1;
                }
                if count == done.len() {
                    break;
                }
            }
        }
        for index in done.iter().take(count) {
            let _ = state.kernel_fences.remove(*index);
        }
        if count < done.len() {
            break;
        }
    }
}

/// Answer every held wait that has settled or whose deadline passed.
fn answer_waits<M, H: Host<M>>(state: &mut Tables<M>, host: &mut H) {
    if state.waits.is_empty() {
        return;
    }
    let now = host.now_ns();
    let mut settled = [(usize::MAX, 0_i32); limits::WAITS];
    let mut count = 0;
    // Evaluate on copies, then write the results back: a wait reads the
    // other tables, which the borrow of the waits' slab would keep from it.
    let mut indices = [usize::MAX; limits::WAITS];
    let mut held = 0;
    for (index, _) in state.waits.slots.iter() {
        if let Some(slot) = indices.get_mut(held) {
            *slot = index;
            held += 1;
        }
    }
    for index in indices.iter().take(held).copied() {
        let Some(mut wait) = state.waits.slots.get(index).copied() else {
            continue;
        };
        let expired = wait.deadline.is_some_and(|deadline| now >= deadline);
        let result = match &mut wait.kind {
            WaitKind::Object {
                object,
                generation,
                start_ns,
            } => gem::evaluate_wait(
                state,
                *object,
                *generation,
                *start_ns,
                &mut wait.data,
                now,
                expired,
            ),
            WaitKind::Syncobjs(sync) => {
                syncobj::evaluate_wait(state, sync, &mut wait.data, expired)
            }
        };
        if result.is_none()
            && state.trace
            && !wait.told
            && now.saturating_sub(wait.since) >= STUCK_NS
        {
            wait.told = true;
            say_stuck(state, host, &wait, now);
        }
        if let Some(entry) = state.waits.slots.get_mut(index) {
            *entry = wait;
        }
        if let Some(status) = result
            && let Some(slot) = settled.get_mut(count)
        {
            *slot = (index, status);
            count += 1;
        }
    }
    for (index, status) in settled.iter().take(count).copied() {
        let Some(wait) = state.waits.slots.remove(index) else {
            continue;
        };
        let length = usize::from(wait.out_size);
        let mut status = status;
        if length > 0
            && let Some(bytes) = wait.data.get(..length)
            && host.copy_out(wait.request, wait.arg, bytes).is_err()
        {
            status = -(Errno::EFAULT.0 as i32);
        }
        if state.trace {
            host.say(format_args!(
                "intel: trace: held request {} answered {status}",
                wait.request
            ));
        }
        host.reply(wait.request, status);
    }
}

/// The trace's account of a wait held [`STUCK_NS`] unanswered: what it
/// waits on, each fence's state, and every request not done (the first
/// eight), the one a hung program's call is stuck behind.
fn say_stuck<M, H: Host<M>>(state: &Tables<M>, host: &mut H, wait: &Wait, now: u64) {
    let requests = &state.requests;
    host.say(format_args!(
        "intel: trace: held request {} unanswered for {} ms (file {}); requests next {} retired {}",
        wait.request,
        now.saturating_sub(wait.since) / 1_000_000,
        wait.file,
        requests.next,
        requests.retired
    ));
    match &wait.kind {
        WaitKind::Object { object, .. } => {
            host.say(format_args!(
                "intel: trace:   GEM_WAIT on object slot {object}"
            ));
        }
        WaitKind::Syncobjs(sync) => {
            let name = if sync.timeline {
                "TIMELINE_WAIT"
            } else {
                "WAIT"
            };
            for index in 0..usize::from(sync.count) {
                let slot = usize::from(sync.slots.get(index).copied().unwrap_or(0));
                let point = sync.points.get(index).copied().unwrap_or(0);
                let fence = sync.fences.get(index).copied().flatten();
                let status = fence.map(|fence| requests.status(fence));
                let (head, reached) = state.syncobjs.get(slot).map_or((0, 0), |sync| {
                    (
                        sync.fence.query(true, requests),
                        sync.fence.query(false, requests),
                    )
                });
                host.say(format_args!(
                    "intel: trace:   {name} flags {:#x} entry {index}: syncobj slot {slot} point {point}: fence {fence:?} status {status:?}; timeline head {head} reached {reached}",
                    sync.flags
                ));
            }
        }
    }
    let mut said = 0;
    for id in requests.live_ids() {
        let Some(request) = requests.get(id) else {
            continue;
        };
        if matches!(request.state, RequestState::Done(_)) {
            continue;
        }
        if said == 8 {
            host.say(format_args!("intel: trace:   (more requests not done)"));
            break;
        }
        said += 1;
        let deps = request
            .deps
            .get(..usize::from(request.dep_count))
            .unwrap_or(&[]);
        host.say(format_args!(
            "intel: trace:   request {id} {:?} context {} engine {:?}: deps {deps:?}",
            request.state, request.context, request.engine
        ));
    }
}
