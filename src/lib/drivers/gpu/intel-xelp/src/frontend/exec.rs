//! `I915_GEM_EXECBUFFER2` (`gem/i915_gem_execbuffer.c`), softpin only.
//!
//! The order is i915's: the argument's checks, the extensions and fence
//! array, the in fence, the context and engine, then each object's lookup
//! and checks, its binding at the address the program pinned it at, the
//! request with every fence it waits on (syncobjs, the in fence, and the
//! objects' implicit sync), and finally the signals: syncobjs, the objects'
//! fences, the dmabuf slots of shared objects (K5, ledger 2360: for every
//! execbuffer that reads or writes one), and the out fence.
//!
//! Departures from i915, each because Mesa never needs it: an object not
//! `EXEC_OBJECT_PINNED` is refused (i915 would choose its address), and a
//! binding that would overlap another still in use answers `EAGAIN`, which
//! `intel_ioctl` retries, where i915 would sleep until it is idle and evict it.

use ferrix_linux_abi::errno::Errno;

use super::backend::Engine;
use super::drm::{Call, put, unserved};
use super::fence;
use super::objects::{Binding, FenceRef, Request, RequestState, SyncFence, Tables};
use super::{Backend, Frontend, Host, gem, limits, syncobj, vm};
use crate::uapi::{ExecFence, ExecObject2, Execbuffer2, ExecbufferExtTimelineFences, exec, query};

/// `I915_GTT_PAGE_SIZE`.
const PAGE: u64 = 4096;

/// `__I915_EXEC_ILLEGAL_FLAGS`.
const ILLEGAL_FLAGS: u64 = exec::UNKNOWN_FLAGS | exec::CONSTANTS_MASK | exec::RESOURCE_STREAMER;

/// The exec objects read from the program at a time.
const CHUNK: usize = 32;

/// One fence an execbuffer names (`struct eb_fence`).
#[derive(Clone, Copy, Debug)]
struct EbFence {
    /// The syncobj's slot.
    slot: u16,
    /// `I915_EXEC_FENCE_WAIT` and `_SIGNAL`.
    flags: u32,
    /// The timeline point, or 0.
    point: u64,
    /// The fence the request waits on, if any.
    fence: Option<FenceRef>,
}

/// The fences of one execbuffer.
struct Fences {
    entries: [EbFence; limits::EXEC_FENCES],
    count: usize,
}

impl Fences {
    fn push(&mut self, fence: EbFence) -> Result<(), Errno> {
        let cell = self.entries.get_mut(self.count).ok_or(Errno::ENOMEM)?;
        *cell = fence;
        self.count += 1;
        Ok(())
    }

    fn iter(&self) -> impl Iterator<Item = &EbFence> {
        self.entries.iter().take(self.count)
    }
}

/// `gen8_canonical_addr`: bit 47 extended through bit 63.
const fn canonical(address: u64) -> u64 {
    (((address << 16) as i64) >> 16) as u64
}

/// The engine the legacy ring selector names (`user_ring_map`).
fn legacy_engine(flags: u64) -> Option<Engine> {
    let (class, instance) = match flags & exec::RING_MASK {
        exec::DEFAULT | exec::RENDER => (query::ENGINE_CLASS_RENDER, 0),
        exec::BLT => (query::ENGINE_CLASS_COPY, 0),
        exec::BSD => (query::ENGINE_CLASS_VIDEO, 0),
        exec::VEBOX => (query::ENGINE_CLASS_VIDEO_ENHANCE, 0),
        _ => return None,
    };
    Some(Engine { class, instance })
}

/// `i915_gem_execbuffer2_ioctl` and `i915_gem_do_execbuffer`.
pub fn execbuffer2<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let mut args = Execbuffer2::from_bytes(data).ok_or(Errno::EFAULT)?;
    let result = execute(frontend, host, call, &mut args);
    args.flags &= !exec::UNKNOWN_FLAGS;
    put(data, &args.to_bytes());
    result
}

fn execute<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    args: &mut Execbuffer2,
) -> Result<(), Errno> {
    let count = check(args)?;
    let mut fences = Fences {
        entries: [EbFence {
            slot: 0,
            flags: 0,
            point: 0,
            fence: None,
        }; limits::EXEC_FENCES],
        count: 0,
    };
    parse_extensions(frontend, host, call, args, &mut fences)?;
    add_fence_array(frontend, host, call, args, &mut fences)?;
    let in_fences = exec::FENCE_IN | exec::FENCE_SUBMIT;
    let mut in_fence = None;
    if args.flags & in_fences != 0 {
        if args.flags & in_fences == in_fences {
            return Err(Errno::EINVAL);
        }
        let fd = args.rsvd2 as u32 as i32;
        in_fence = Some(syncobj::import_sync_file(&frontend.state, host, call, fd)?);
    }
    let (context_slot, vm_slot, engine, engine_slot) = select(frontend, call, args)?;
    let (batch, shared) = validate_all(frontend, host, call, args, count)?;
    // Room for every kernel fence this execbuffer makes.
    let out = usize::from(args.flags & exec::FENCE_OUT != 0);
    if frontend.state.kernel_fences.len() + shared + out > limits::KERNEL_FENCES {
        return Err(frontend
            .state
            .refuse("kernel fence table full", Errno::ENOMEM));
    }
    if !frontend.state.requests.has_room() {
        return Err(frontend.state.refuse("request ring full", Errno::EAGAIN));
    }
    each_object(host, call, args.buffers_ptr, count, &mut |_, _, entry| {
        let object = gem::lookup(&frontend.state, call.file, entry.handle)?;
        bind(frontend, vm_slot, entry, object)
    })?;
    let mut request = Request {
        id: 0,
        state: RequestState::Queued,
        context: context_slot,
        slot: u8::try_from(engine_slot).map_err(|_| Errno::EINVAL)?,
        engine,
        batch: batch.0,
        length: batch.1,
        deps: [FenceRef::Signalled(0); limits::REQUEST_DEPS],
        dep_count: 0,
    };
    for entry in fences.iter() {
        if let Some(fence) = entry.fence {
            add_dep(&mut request, fence);
        }
    }
    if let Some(fence) = in_fence {
        add_dep(&mut request, fence);
    }
    each_object(host, call, args.buffers_ptr, count, &mut |_, _, entry| {
        if entry.flags & exec::OBJECT_ASYNC != 0 {
            return Ok(());
        }
        let object = gem::lookup(&frontend.state, call.file, entry.handle)?;
        if let Some(object) = frontend.state.objects.get(usize::from(object)) {
            add_dep(&mut request, object.write);
            if entry.flags & exec::OBJECT_WRITE != 0 {
                for read in object.reads {
                    add_dep(&mut request, read);
                }
            }
        }
        Ok(())
    })?;
    let id = frontend
        .state
        .requests
        .push(request)
        .ok_or_else(|| frontend.state.refuse("request ring full", Errno::EAGAIN))?;
    if let Some(entry) = frontend.state.contexts.get_mut(usize::from(context_slot)) {
        entry.in_flight += 1;
    }
    each_object(
        host,
        call,
        args.buffers_ptr,
        count,
        &mut |host, _, entry| {
            let object = gem::lookup(&frontend.state, call.file, entry.handle)?;
            mark_used(frontend, host, vm_slot, entry, object, id)
        },
    )?;
    signal(frontend, host, call, args, &fences, id)
}

/// `check_buffer_count` and `i915_gem_check_execbuffer`: the object count.
fn check(args: &mut Execbuffer2) -> Result<usize, Errno> {
    // Past this table's bound is i915's failed allocation.
    let count = usize::try_from(args.buffer_count).map_err(|_| Errno::EINVAL)?;
    if count < 1 {
        return Err(Errno::EINVAL);
    }
    if count > limits::EXEC_OBJECTS {
        return Err(Errno::ENOMEM);
    }
    if args.flags & ILLEGAL_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }
    if args.flags & (exec::FENCE_ARRAY | exec::USE_EXTENSIONS) == 0
        && (args.num_cliprects != 0 || args.cliprects_ptr != 0)
    {
        return Err(Errno::EINVAL);
    }
    if args.dr4 == 0xffff_ffff {
        args.dr4 = 0;
    }
    if args.dr1 != 0 || args.dr4 != 0 {
        return Err(Errno::EINVAL);
    }
    if (args.batch_start_offset | args.batch_len) & 0x7 != 0 {
        return Err(Errno::EINVAL);
    }
    if args.flags & exec::SECURE != 0 {
        return Err(Errno::ENODEV);
    }
    Ok(count)
}

/// `eb_select_context` and `eb_select_engine`: the context's slot, its VM's,
/// the engine and its slot in the context.
fn select<B: Backend>(
    frontend: &mut Frontend<B>,
    call: &Call,
    args: &Execbuffer2,
) -> Result<(u16, u16, Engine, usize), Errno> {
    let context_slot = vm::lookup(&mut frontend.state, call.file, args.rsvd1 as u32)?;
    let context = frontend
        .state
        .contexts
        .get(usize::from(context_slot))
        .copied()
        .ok_or(Errno::ENOENT)?;
    let (engine, engine_slot) = match context.engines {
        Some(map) => {
            let index = (args.flags & exec::RING_MASK) as usize;
            if index >= usize::from(context.engine_count) {
                return Err(Errno::EINVAL);
            }
            (
                map.get(index).copied().flatten().ok_or(Errno::EINVAL)?,
                index,
            )
        }
        None => {
            let engine = legacy_engine(args.flags).ok_or(Errno::EINVAL)?;
            (engine, usize::from(engine.class))
        }
    };
    if !frontend.backend.facts().engines.contains(&engine) {
        return Err(Errno::EINVAL);
    }
    if context.banned {
        return Err(Errno::EIO);
    }
    Ok((context_slot, context.vm, engine, engine_slot))
}

/// `eb_lookup_vmas`, `eb_validate_vma` and the batch checks of `eb_add_vma`:
/// the batch's address and length, and how many objects are shared.
fn validate_all<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    args: &Execbuffer2,
    count: usize,
) -> Result<((u64, u32), usize), Errno> {
    let batch_index = if args.flags & exec::BATCH_FIRST != 0 {
        0
    } else {
        count - 1
    };
    let mut batch = None;
    let mut shared = 0_usize;
    each_object(
        host,
        call,
        args.buffers_ptr,
        count,
        &mut |host, index, entry| {
            let object = gem::lookup(&frontend.state, call.file, entry.handle)?;
            let size = validate(frontend, host, call, entry, object)?;
            if index == batch_index {
                batch = Some(batch_of(args, entry, size)?);
            }
            if frontend
                .state
                .objects
                .get(usize::from(object))
                .is_some_and(|entry| entry.shared && entry.dmabuf_alive)
            {
                shared += 1;
            }
            Ok(())
        },
    )?;
    Ok((batch.ok_or(Errno::EINVAL)?, shared))
}

/// The checks of the batch in `eb_add_vma`: not written, inside its object,
/// not empty; its address and length.
fn batch_of(args: &Execbuffer2, entry: &ExecObject2, size: u64) -> Result<(u64, u32), Errno> {
    if entry.flags & exec::OBJECT_WRITE != 0 {
        return Err(Errno::EINVAL);
    }
    let begin = u64::from(args.batch_start_offset);
    let len = u64::from(args.batch_len);
    if begin >= size || begin.checked_add(len).is_none_or(|end| end > size) {
        return Err(Errno::EINVAL);
    }
    let length = if args.batch_len == 0 {
        size - begin
    } else {
        len
    };
    if length == 0 {
        return Err(Errno::EINVAL);
    }
    Ok((
        (entry.offset & ((1 << 48) - 1)) + begin,
        u32::try_from(length).map_err(|_| Errno::EINVAL)?,
    ))
}

/// Add a fence `request` waits on, merging into the last when all are
/// taken.
fn add_dep(request: &mut Request, fence: FenceRef) {
    if matches!(fence, FenceRef::Signalled(0)) {
        return;
    }
    let count = usize::from(request.dep_count);
    if count < limits::REQUEST_DEPS {
        if let Some(cell) = request.deps.get_mut(count) {
            *cell = fence;
        }
        request.dep_count += 1;
    } else if let Some(last) = request.deps.last_mut() {
        *last = last.merge(fence);
    }
}

/// `signal_fence_array`, then the out fence.
fn signal<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    args: &mut Execbuffer2,
    fences: &Fences,
    id: u64,
) -> Result<(), Errno> {
    let done = FenceRef::Request(id);
    for entry in fences.iter() {
        if entry.flags & exec::FENCE_SIGNAL == 0 {
            continue;
        }
        if entry.point != 0 {
            syncobj::add_point(
                &mut frontend.state,
                usize::from(entry.slot),
                entry.point,
                done,
            );
        } else {
            syncobj::replace(
                &mut frontend.state,
                usize::from(entry.slot),
                SyncFence::Single(done),
            );
        }
    }
    if args.flags & exec::FENCE_OUT != 0 {
        // Installed even for `EXECBUFFER2` without `_IOC_READ`, whose
        // `rsvd2` is never copied back, as on Linux; Mesa never asks there.
        let fd = syncobj::export_sync_file(frontend, host, call, done)?;
        args.rsvd2 &= 0xffff_ffff;
        args.rsvd2 |= u64::from(fd as u32) << 32;
    }
    Ok(())
}

/// Read exec objects `start..start + out.len()`.
fn read_objects<M, H: Host<M>>(
    host: &mut H,
    call: &Call,
    base: u64,
    start: usize,
    out: &mut [ExecObject2],
) -> Result<(), Errno> {
    let mut raw = [0_u8; CHUNK * ExecObject2::SIZE];
    let bytes = raw
        .get_mut(..out.len() * ExecObject2::SIZE)
        .ok_or(Errno::EFAULT)?;
    host.copy_in(
        call.request,
        base + (start * ExecObject2::SIZE) as u64,
        bytes,
    )
    .map_err(|_| Errno::EFAULT)?;
    for (entry, source) in out.iter_mut().zip(bytes.chunks_exact(ExecObject2::SIZE)) {
        *entry = ExecObject2::from_bytes(source).ok_or(Errno::EFAULT)?;
    }
    Ok(())
}

/// Each exec object, read [`CHUNK`] at a time, with its index.
fn each_object<M, H: Host<M>>(
    host: &mut H,
    call: &Call,
    base: u64,
    count: usize,
    each: &mut dyn FnMut(&mut H, usize, &ExecObject2) -> Result<(), Errno>,
) -> Result<(), Errno> {
    let mut chunk = [ExecObject2::default(); CHUNK];
    let mut start = 0;
    while start < count {
        let length = (count - start).min(CHUNK);
        let part = chunk.get_mut(..length).ok_or(Errno::EFAULT)?;
        read_objects(host, call, base, start, part)?;
        for (offset, entry) in part.iter().enumerate() {
            each(host, start + offset, entry)?;
        }
        start += length;
    }
    Ok(())
}

/// `eb_validate_vma` with the softpin placement checks of `eb_reserve_vma`:
/// the reservation's size.
fn validate<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    entry: &ExecObject2,
    object: u16,
) -> Result<u64, Errno> {
    if entry.relocation_count != 0 {
        return Err(Errno::EINVAL);
    }
    // Full PPGTT: `EXEC_OBJECT_NEEDS_GTT` is invalid too.
    if entry.flags & (exec::OBJECT_UNKNOWN_FLAGS | exec::OBJECT_NEEDS_GTT) != 0 {
        return Err(Errno::EINVAL);
    }
    if entry.alignment != 0 && !entry.alignment.is_power_of_two() {
        return Err(Errno::EINVAL);
    }
    if entry.flags & exec::OBJECT_PINNED == 0 {
        return Err(unserved(
            frontend,
            host,
            call,
            "an execbuffer object not EXEC_OBJECT_PINNED",
        ));
    }
    if entry.offset != canonical(entry.offset & !(PAGE - 1)) {
        return Err(Errno::EINVAL);
    }
    let pad = if entry.flags & exec::OBJECT_PAD_TO_SIZE != 0 {
        if !entry.rsvd1.is_multiple_of(PAGE) {
            return Err(Errno::EINVAL);
        }
        entry.rsvd1
    } else {
        0
    };
    let size = frontend
        .state
        .objects
        .get(usize::from(object))
        .ok_or(Errno::ENOENT)?
        .size;
    let reserved = size.max(pad);
    let address = entry.offset & ((1 << 48) - 1);
    let total = 1_u64 << frontend.backend.facts().ppgtt_bits;
    let end = address.checked_add(reserved).ok_or(Errno::EINVAL)?;
    if end > total {
        return Err(Errno::EINVAL);
    }
    // `eb_vma_misplaced`: without 48-bit addresses, below 4 GiB, as
    // `PIN_ZONE_4G` places it.
    if entry.flags & exec::OBJECT_SUPPORTS_48B_ADDRESS == 0 && (end + PAGE - 1) >> 32 != 0 {
        return Err(Errno::EINVAL);
    }
    if entry.alignment > 1 && !address.is_multiple_of(entry.alignment) {
        return Err(Errno::EINVAL);
    }
    Ok(reserved)
}

/// Bind `object` at its pinned address in VM `vm_slot`, unless it is bound
/// there already: idle bindings in the way are unbound (i915 evicts them),
/// busy ones answer `EAGAIN`.
fn bind<B: Backend>(
    frontend: &mut Frontend<B>,
    vm_slot: u16,
    entry: &ExecObject2,
    object: u16,
) -> Result<(), Errno> {
    let address = entry.offset & ((1 << 48) - 1);
    let state = &mut frontend.state;
    let (size, pat) = state
        .objects
        .get(usize::from(object))
        .map(|entry| (entry.size, entry.pat_index))
        .ok_or(Errno::ENOENT)?;
    let reserved = if entry.flags & exec::OBJECT_PAD_TO_SIZE != 0 {
        size.max(entry.rsvd1)
    } else {
        size
    };
    let key = Binding::key_of(vm_slot, address);
    if let Some(index) = state.bindings.at(key)
        && let Some(existing) = state.bindings.as_slice().get(index)
        && existing.object == object
        && existing.size >= reserved
    {
        return Ok(());
    }
    // What is in the way.
    loop {
        let range = state.bindings.overlapping(key, reserved);
        if range.is_empty() {
            break;
        }
        let Some(other) = state.bindings.as_slice().get(range.start).copied() else {
            break;
        };
        if other.last_use != 0
            && state
                .requests
                .status(FenceRef::Request(other.last_use))
                .is_none()
        {
            return Err(state.refuse("a busy binding in the way", Errno::EAGAIN));
        }
        if let Some(vm) = state
            .vms
            .get(usize::from(vm_slot))
            .and_then(|vm| vm.backend)
        {
            frontend.backend.unbind(vm, other.address(), other.size);
        }
        let _ = state.bindings.remove(range.start);
        if let Some(owner) = state.objects.get_mut(usize::from(other.object)) {
            owner.bindings = owner.bindings.saturating_sub(1);
        }
    }
    let vm = vm::backend_vm(&mut frontend.backend, state, vm_slot)
        .map_err(|errno| state.refuse("no backend VM", errno))?;
    let memory = &state
        .objects
        .get(usize::from(object))
        .ok_or(Errno::ENOENT)?
        .memory;
    if let Err(errno) = frontend.backend.bind(vm, address, memory, size, pat) {
        // ENOMEM: no page for a table; EDQUOT: the GPU's pin budget.
        return Err(state.refuse("bind failed", errno));
    }
    if !state.bindings.insert(Binding {
        key,
        size: reserved,
        object,
        last_use: 0,
    }) {
        frontend.backend.unbind(vm, address, size);
        return Err(state.refuse("binding table full", Errno::ENOMEM));
    }
    if let Some(owner) = state.objects.get_mut(usize::from(object)) {
        owner.bindings += 1;
    }
    Ok(())
}

/// After the request: the object's implicit-sync fences, its binding's last
/// use, and, for a shared object, its dmabuf's slot.
fn mark_used<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    vm_slot: u16,
    entry: &ExecObject2,
    object: u16,
    id: u64,
) -> Result<(), Errno> {
    let done = FenceRef::Request(id);
    let state = &mut frontend.state;
    let key = Binding::key_of(vm_slot, entry.offset & ((1 << 48) - 1));
    if let Some(index) = state.bindings.at(key)
        && let Some(binding) = state.bindings.get_mut(index)
    {
        binding.last_use = binding.last_use.max(id);
    }
    let cookie = state.dmabuf_cookie(object);
    let Some(owner) = state.objects.get_mut(usize::from(object)) else {
        return Ok(());
    };
    if entry.flags & exec::OBJECT_WRITE != 0 {
        owner.write = done;
    } else {
        gem::add_read(owner, done);
    }
    let shared = owner.shared && owner.dmabuf_alive;
    if shared {
        let fence_cookie = state.cookie();
        match host.dmabuf_fence(cookie, fence_cookie) {
            Ok(()) => fence::track_kernel_fence(state, fence_cookie, done)?,
            Err(errno) => {
                if state.say_once(63) {
                    host.say(format_args!(
                        "intel: a shared object's dmabuf slot could not be set (errno {}): implicit sync with other programs is lost for this request",
                        errno.0
                    ));
                }
            }
        }
    }
    Ok(())
}

/// `parse_execbuf2_extensions`: the timeline-fences extension, which shares
/// `cliprects_ptr` with the fence array.
fn parse_extensions<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    args: &Execbuffer2,
    fences: &mut Fences,
) -> Result<(), Errno> {
    if args.flags & exec::USE_EXTENSIONS == 0 {
        return Ok(());
    }
    if args.flags & exec::FENCE_ARRAY != 0 || args.num_cliprects != 0 {
        return Err(Errno::EINVAL);
    }
    let mut found = [0_u64; 8];
    let mut count = 0;
    vm::walk_extensions(host, call, args.cliprects_ptr, &mut |name, at| {
        if name != exec::EXT_TIMELINE_FENCES {
            return Err(Errno::EINVAL);
        }
        let cell = found.get_mut(count).ok_or(Errno::E2BIG)?;
        *cell = at;
        count += 1;
        Ok(())
    })?;
    for at in found.iter().take(count).copied() {
        let mut raw = [0_u8; ExecbufferExtTimelineFences::SIZE];
        host.copy_in(call.request, at, &mut raw)
            .map_err(|_| Errno::EFAULT)?;
        let ext = ExecbufferExtTimelineFences::from_bytes(&raw).ok_or(Errno::EFAULT)?;
        add_timeline_fences(&frontend.state, host, call, &ext, fences)?;
    }
    Ok(())
}

/// `add_timeline_fence_array`.
fn add_timeline_fences<M, H: Host<M>>(
    state: &Tables<M>,
    host: &mut H,
    call: &Call,
    ext: &ExecbufferExtTimelineFences,
    fences: &mut Fences,
) -> Result<(), Errno> {
    let count = usize::try_from(ext.fence_count).map_err(|_| Errno::EINVAL)?;
    if count > limits::EXEC_FENCES - fences.count {
        return Err(Errno::ENOMEM);
    }
    for index in 0..count {
        let mut raw = [0_u8; ExecFence::SIZE];
        host.copy_in(
            call.request,
            ext.handles_ptr + (index * ExecFence::SIZE) as u64,
            &mut raw,
        )
        .map_err(|_| Errno::EFAULT)?;
        let user = ExecFence::from_bytes(&raw).ok_or(Errno::EFAULT)?;
        if user.flags & exec::FENCE_UNKNOWN_FLAGS != 0 {
            return Err(Errno::EINVAL);
        }
        let mut raw = [0_u8; 8];
        host.copy_in(call.request, ext.values_ptr + 8 * index as u64, &mut raw)
            .map_err(|_| Errno::EFAULT)?;
        let point = u64::from_le_bytes(raw);
        let slot = state
            .syncobj_of(call.file, user.handle)
            .ok_or(Errno::ENOENT)?;
        let fence = state
            .syncobjs
            .get(slot)
            .map_or(SyncFence::None, |sync| sync.fence);
        let signal = user.flags & exec::FENCE_SIGNAL != 0;
        let whole = fence.find(0).ok();
        if whole.is_none() && user.flags != 0 && !signal {
            return Err(Errno::EINVAL);
        }
        // `dma_fence_chain_find_seqno`: on failure the fence stays the
        // whole chain, which a signal-only entry then waits on, as in i915.
        let found = match whole {
            None => None,
            Some(whole) => match fence.find(point) {
                Ok(at) => Some(at),
                Err(_) if signal => Some(whole),
                Err(_) => return Err(Errno::EINVAL),
            },
        };
        if found.is_none() && !signal {
            continue;
        }
        if point != 0 && signal && user.flags & exec::FENCE_WAIT != 0 {
            return Err(Errno::EINVAL);
        }
        fences.push(EbFence {
            slot: u16::try_from(slot).map_err(|_| Errno::ENOENT)?,
            flags: user.flags,
            point,
            fence: found,
        })?;
    }
    Ok(())
}

/// `add_fence_array`: binary syncobjs, waited on and signalled.
fn add_fence_array<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    args: &Execbuffer2,
    fences: &mut Fences,
) -> Result<(), Errno> {
    if args.flags & exec::FENCE_ARRAY == 0 {
        return Ok(());
    }
    let count = usize::try_from(args.num_cliprects).map_err(|_| Errno::EINVAL)?;
    if count > limits::EXEC_FENCES - fences.count {
        return Err(Errno::ENOMEM);
    }
    let state = &frontend.state;
    for index in 0..count {
        let mut raw = [0_u8; ExecFence::SIZE];
        host.copy_in(
            call.request,
            args.cliprects_ptr + (index * ExecFence::SIZE) as u64,
            &mut raw,
        )
        .map_err(|_| Errno::EFAULT)?;
        let user = ExecFence::from_bytes(&raw).ok_or(Errno::EFAULT)?;
        if user.flags & exec::FENCE_UNKNOWN_FLAGS != 0 {
            return Err(Errno::EINVAL);
        }
        let slot = state
            .syncobj_of(call.file, user.handle)
            .ok_or(Errno::ENOENT)?;
        let mut fence = None;
        if user.flags & exec::FENCE_WAIT != 0 {
            fence = Some(syncobj::fence_at(state, slot, 0)?);
        }
        fences.push(EbFence {
            slot: u16::try_from(slot).map_err(|_| Errno::ENOENT)?,
            flags: user.flags,
            point: 0,
            fence,
        })?;
    }
    Ok(())
}
