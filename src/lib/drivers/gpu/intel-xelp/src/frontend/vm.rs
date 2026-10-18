//! VMs and contexts: `gem/i915_gem_context.c`'s ioctls for Gen12 with full
//! PPGTT, and `I915_REG_READ` (`intel_uncore.c`, `i915_reg_read_ioctl`).
//!
//! A context made by `CONTEXT_CREATE` is a proto context until something
//! looks it up (an execbuffer or `GETPARAM`), as on Gen12 in i915: only
//! then are its `VM` and `ENGINES` fixed. Its hardware contexts are made by
//! the back end at its first request on each engine.

use ferrix_linux_abi::errno::Errno;

use super::backend::{Engine, VmId};
use super::drm::{Call, default_context, put, unserved};
use super::objects::{Binding, Tables, Vm, VmName};
use super::{Backend, Frontend, Host, limits};
use crate::uapi::{
    ContextCreateExt, ContextCreateExtSetparam, ContextParam, EngineClassInstance, RegRead,
    ResetStats, UserExtension, VmControl, context as consts, gem, query,
};

/// A new VM for `file`, with no ids.
///
/// # Errors
///
/// `ENOMEM` when [`limits::VMS`] are in use.
pub fn new_vm<M>(state: &mut Tables<M>, file: u8) -> Result<u16, Errno> {
    let slot = state
        .vms
        .insert(Vm {
            file,
            backend: None,
            refs: 0,
        })
        .ok_or(Errno::ENOMEM)?;
    u16::try_from(slot).map_err(|_| Errno::ENOMEM)
}

/// A new context of `file` in VM `vm`, holding it.
///
/// # Errors
///
/// `ENOMEM` when [`limits::CONTEXTS`] are in use.
pub fn new_context<M>(
    state: &mut Tables<M>,
    file: u8,
    vm: u16,
    finalized: bool,
) -> Result<u16, Errno> {
    let slot = state
        .contexts
        .insert(default_context(file, vm, finalized))
        .ok_or(Errno::ENOMEM)?;
    if let Some(entry) = state.vms.get_mut(usize::from(vm)) {
        entry.refs += 1;
    }
    u16::try_from(slot).map_err(|_| Errno::ENOMEM)
}

/// A new id of `file` for VM `vm`, holding it.
fn new_vm_name<M>(state: &mut Tables<M>, file: u8, vm: u16) -> Result<u32, Errno> {
    let slot = state
        .vm_names
        .insert(VmName { file, vm })
        .ok_or(Errno::ENOMEM)?;
    if let Some(entry) = state.vms.get_mut(usize::from(vm)) {
        entry.refs += 1;
    }
    u32::try_from(slot + 1).map_err(|_| Errno::ENOMEM)
}

/// Drop one hold on VM `vm`.
fn put_vm<M>(state: &mut Tables<M>, vm: u16) {
    if let Some(entry) = state.vms.get_mut(usize::from(vm)) {
        entry.refs = entry.refs.saturating_sub(1);
    }
}

/// The back end's PPGTT for VM slot `vm`, made at first need.
///
/// # Errors
///
/// What the back end refused it with.
pub fn backend_vm<B: Backend>(
    backend: &mut B,
    state: &mut Tables<B::Memory>,
    vm: u16,
) -> Result<VmId, Errno> {
    let entry = state.vms.get_mut(usize::from(vm)).ok_or(Errno::ENOENT)?;
    if let Some(id) = entry.backend {
        return Ok(id);
    }
    let id = backend.vm_create()?;
    entry.backend = Some(id);
    Ok(id)
}

/// Free the contexts closed and idle, and the VMs nothing holds, with their
/// bindings.
pub fn free_idle<B: Backend>(backend: &mut B, state: &mut Tables<B::Memory>) {
    let mut doomed = [usize::MAX; 64];
    let mut count = 0;
    for (slot, context) in state.contexts.iter() {
        if context.closed && context.in_flight == 0 && count < doomed.len() {
            if let Some(cell) = doomed.get_mut(count) {
                *cell = slot;
            }
            count += 1;
        }
    }
    for slot in doomed.iter().take(count).copied() {
        if let Some(context) = state.contexts.remove(slot) {
            for hw in context.hw.iter().flatten() {
                backend.context_destroy(*hw);
            }
            put_vm(state, context.vm);
        }
    }
    let mut count = 0;
    for (slot, vm) in state.vms.iter() {
        if vm.refs == 0 && count < doomed.len() {
            if let Some(cell) = doomed.get_mut(count) {
                *cell = slot;
            }
            count += 1;
        }
    }
    for slot in doomed.iter().take(count).copied() {
        let Ok(vm_slot) = u16::try_from(slot) else {
            continue;
        };
        // Every binding of the VM: idle, since no context of it is left.
        let first = Binding::key_of(vm_slot, 0);
        loop {
            let range = state.bindings.overlapping(first, 1 << 48);
            if range.is_empty() {
                break;
            }
            if let Some(binding) = state.bindings.remove(range.start)
                && let Some(object) = state.objects.get_mut(usize::from(binding.object))
            {
                object.bindings = object.bindings.saturating_sub(1);
            }
        }
        if let Some(vm) = state.vms.remove(slot)
            && let Some(id) = vm.backend
        {
            backend.vm_destroy(id);
        }
    }
}

/// A released file's contexts and VM ids: each context is closed, and goes
/// once its requests retire.
pub fn release_file<M>(state: &mut Tables<M>, file: u8) {
    for (_, context) in state.contexts.iter_mut() {
        if context.file == file {
            context.closed = true;
        }
    }
    let mut doomed = [usize::MAX; limits::VM_IDS];
    let mut count = 0;
    for (slot, name) in state.vm_names.iter() {
        if name.file == file
            && let Some(cell) = doomed.get_mut(count)
        {
            *cell = slot;
            count += 1;
        }
    }
    for slot in doomed.iter().take(count).copied() {
        if let Some(name) = state.vm_names.remove(slot) {
            put_vm(state, name.vm);
        }
    }
}

/// The context `id` names, for an execbuffer or `GETPARAM`: past its proto
/// stage from now on (`i915_gem_context_lookup`).
///
/// # Errors
///
/// `ENOENT`.
pub fn lookup<M>(state: &mut Tables<M>, file: u8, id: u32) -> Result<u16, Errno> {
    let slot = state.context_of(file, id).ok_or(Errno::ENOENT)?;
    if let Some(context) = state.contexts.get_mut(usize::from(slot)) {
        context.finalized = true;
    }
    Ok(slot)
}

/// `i915_gem_vm_create_ioctl`: a new PPGTT, named by a new id. Its
/// extension chain has no known names.
pub fn vm_create<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let mut args = VmControl::from_bytes(data).ok_or(Errno::EFAULT)?;
    if args.flags != 0 {
        return Err(Errno::EINVAL);
    }
    if args.extensions != 0 {
        walk_extensions(host, call, args.extensions, &mut |_, _| Err(Errno::EINVAL))?;
    }
    let state = &mut frontend.state;
    let vm = new_vm(state, call.file)?;
    match new_vm_name(state, call.file, vm) {
        Ok(id) => args.vm_id = id,
        Err(errno) => {
            let _ = state.vms.remove(usize::from(vm));
            return Err(errno);
        }
    }
    put(data, &args.to_bytes());
    Ok(())
}

/// `i915_gem_vm_destroy_ioctl`: drop one id.
pub fn vm_destroy<B: Backend>(
    frontend: &mut Frontend<B>,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let args = VmControl::from_bytes(data).ok_or(Errno::EFAULT)?;
    if args.flags != 0 || args.extensions != 0 {
        return Err(Errno::EINVAL);
    }
    let state = &mut frontend.state;
    let slot = usize::try_from(args.vm_id)
        .ok()
        .and_then(|id| id.checked_sub(1))
        .filter(|slot| {
            state
                .vm_names
                .get(*slot)
                .is_some_and(|name| name.file == call.file)
        })
        .ok_or(Errno::ENOENT)?;
    if let Some(name) = state.vm_names.remove(slot) {
        put_vm(state, name.vm);
    }
    Ok(())
}

/// `i915_user_extensions`: walk a chain of at most 512 extensions, each with
/// its `flags` and `rsvd` zero, calling `each` with its name and address.
///
/// # Errors
///
/// `E2BIG` past 512, `EINVAL` for a nonzero reserved field or what `each`
/// refuses, `EFAULT`.
pub fn walk_extensions<M, H: Host<M>>(
    host: &mut H,
    call: &Call,
    first: u64,
    each: &mut dyn FnMut(u32, u64) -> Result<(), Errno>,
) -> Result<(), Errno> {
    let mut at = first;
    let mut depth = 512_u32;
    while at != 0 {
        if depth == 0 {
            return Err(Errno::E2BIG);
        }
        depth -= 1;
        let mut raw = [0_u8; UserExtension::SIZE];
        host.copy_in(call.request, at, &mut raw)
            .map_err(|_| Errno::EFAULT)?;
        let ext = UserExtension::from_bytes(&raw).ok_or(Errno::EFAULT)?;
        if ext.flags != 0 || ext.rsvd0 != 0 || ext.rsvd1 != 0 || ext.rsvd2 != 0 || ext.rsvd3 != 0 {
            return Err(Errno::EINVAL);
        }
        each(ext.name, at)?;
        at = ext.next_extension;
    }
    Ok(())
}

/// `i915_gem_context_create_ioctl`, with `CONTEXT_CREATE` as the same call
/// on the first eight bytes.
pub fn context_create<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let mut args = ContextCreateExt::from_bytes(data).ok_or(Errno::EFAULT)?;
    if args.flags & consts::CREATE_FLAGS_UNKNOWN != 0 {
        return Err(Errno::EINVAL);
    }
    let file = call.file;
    // i915 gives every new context a VM of its own unless `VM` is set; the
    // proto context's VM is made only when it is finalized. Here it is made
    // now, and dropped if `VM` replaces it.
    let own_vm = new_vm(&mut frontend.state, file)?;
    let slot = match new_context(&mut frontend.state, file, own_vm, false) {
        Ok(slot) => slot,
        Err(errno) => {
            let _ = frontend.state.vms.remove(usize::from(own_vm));
            return Err(errno);
        }
    };
    if args.flags & consts::CREATE_FLAGS_USE_EXTENSIONS != 0
        && let Err(errno) = apply_create_extensions(frontend, host, call, args.extensions, slot)
    {
        close_context(&mut frontend.state, slot);
        return Err(errno);
    }
    args.ctx_id = frontend.state.context_id(file, slot);
    put(data, &args.to_bytes());
    Ok(())
}

/// The most `CONTEXT_CREATE` extensions applied (anv gives five).
const CREATE_EXTENSIONS: usize = 16;

/// `create_extensions[]`: `SETPARAM` is `create_setparam`, which applies
/// `set_proto_ctx_param` to the new context and wants `param.ctx_id` 0;
/// `CLONE` is `invalid_ext`. The chain is walked (and its reserved fields
/// checked) first, then each is applied in order.
fn apply_create_extensions<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    first: u64,
    slot: u16,
) -> Result<(), Errno> {
    let mut found = [0_u64; CREATE_EXTENSIONS];
    let mut count = 0;
    walk_extensions(host, call, first, &mut |name, at| {
        if name != consts::CREATE_EXT_SETPARAM {
            return Err(Errno::EINVAL);
        }
        let cell = found.get_mut(count).ok_or(Errno::E2BIG)?;
        *cell = at;
        count += 1;
        Ok(())
    })?;
    for at in found.iter().take(count).copied() {
        let mut raw = [0_u8; ContextCreateExtSetparam::SIZE];
        host.copy_in(call.request, at, &mut raw)
            .map_err(|_| Errno::EFAULT)?;
        let ext = ContextCreateExtSetparam::from_bytes(&raw).ok_or(Errno::EFAULT)?;
        if ext.ctx_id != 0 {
            return Err(Errno::EINVAL);
        }
        let param = ContextParam {
            ctx_id: 0,
            size: ext.size,
            param: ext.param,
            value: ext.value,
        };
        set_param(frontend, host, call, slot, &param, true)?;
    }
    Ok(())
}

/// Close context `slot`: it goes once its requests retire.
fn close_context<M>(state: &mut Tables<M>, slot: u16) {
    if let Some(context) = state.contexts.get_mut(usize::from(slot)) {
        context.closed = true;
    }
}

/// `i915_gem_context_destroy_ioctl`. The default context cannot be destroyed
/// (`ENOENT` for id 0).
pub fn context_destroy<B: Backend>(
    frontend: &mut Frontend<B>,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let id = u32::from_le_bytes(
        data.get(..4)
            .and_then(|b| b.try_into().ok())
            .ok_or(Errno::EFAULT)?,
    );
    let pad = u32::from_le_bytes(
        data.get(4..8)
            .and_then(|b| b.try_into().ok())
            .ok_or(Errno::EFAULT)?,
    );
    if pad != 0 {
        return Err(Errno::EINVAL);
    }
    if id == 0 {
        return Err(Errno::ENOENT);
    }
    let slot = frontend
        .state
        .context_of(call.file, id)
        .ok_or(Errno::ENOENT)?;
    close_context(&mut frontend.state, slot);
    Ok(())
}

/// `i915_gem_context_setparam_ioctl`: a proto context takes
/// `set_proto_ctx_param`, a finalized one `ctx_setparam`.
pub fn context_setparam<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let args = ContextParam::from_bytes(data).ok_or(Errno::EFAULT)?;
    let slot = frontend
        .state
        .context_of(call.file, args.ctx_id)
        .ok_or(Errno::ENOENT)?;
    let proto = frontend
        .state
        .contexts
        .get(usize::from(slot))
        .is_some_and(|context| !context.finalized);
    set_param(frontend, host, call, slot, &args, proto)
}

/// `set_proto_ctx_param` (`proto`) or `ctx_setparam`.
fn set_param<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    slot: u16,
    args: &ContextParam,
    proto: bool,
) -> Result<(), Errno> {
    let privileged = host.privileged(call.request);
    match args.param {
        consts::PARAM_NO_ERROR_CAPTURE => {
            if args.size != 0 {
                return Err(Errno::EINVAL);
            }
            with_context(frontend, slot, |context| {
                context.no_error_capture = args.value != 0;
            })
        }
        consts::PARAM_BANNABLE => {
            if args.size != 0 {
                return Err(Errno::EINVAL);
            }
            if !privileged && args.value == 0 {
                return Err(Errno::EPERM);
            }
            with_context(frontend, slot, |context| context.bannable = args.value != 0)
        }
        consts::PARAM_RECOVERABLE => {
            if args.size != 0 {
                return Err(Errno::EINVAL);
            }
            with_context(frontend, slot, |context| {
                context.recoverable = args.value != 0;
            })
        }
        consts::PARAM_PRIORITY => {
            // `validate_priority`: execlists' scheduler has
            // `I915_SCHEDULER_CAP_PRIORITY`; above the default needs
            // `CAP_SYS_NICE`. The value is kept; execlists here are FIFO.
            if args.size != 0 {
                return Err(Errno::EINVAL);
            }
            let priority = args.value as i64;
            if !(consts::MIN_USER_PRIORITY..=consts::MAX_USER_PRIORITY).contains(&priority) {
                return Err(Errno::EINVAL);
            }
            if priority > consts::DEFAULT_PRIORITY && !privileged {
                return Err(Errno::EPERM);
            }
            with_context(frontend, slot, |context| context.priority = priority)
        }
        consts::PARAM_PERSISTENCE => {
            if args.size != 0 {
                return Err(Errno::EINVAL);
            }
            // `proto_context_set_persistence`: turning it off needs
            // preemption, which these execlists do not do.
            if args.value == 0 {
                return Err(Errno::ENODEV);
            }
            with_context(frontend, slot, |context| context.persistence = true)
        }
        // `LOW_LATENCY` is for GuC submission only; `SSEU` for Gen11 only.
        consts::PARAM_LOW_LATENCY => Err(Errno::EINVAL),
        consts::PARAM_SSEU => Err(Errno::ENODEV),
        consts::PARAM_PROTECTED_CONTENT if proto => {
            if args.value == 0 {
                Ok(())
            } else {
                Err(Errno::ENODEV)
            }
        }
        consts::PARAM_VM if proto => set_vm(frontend, call, slot, args),
        consts::PARAM_ENGINES if proto => set_engines(frontend, host, call, slot, args),
        consts::PARAM_CONTEXT_IMAGE => Err(unserved(
            frontend,
            host,
            call,
            "I915_CONTEXT_PARAM_CONTEXT_IMAGE",
        )),
        _ => Err(Errno::EINVAL),
    }
}

fn with_context<B: Backend>(
    frontend: &mut Frontend<B>,
    slot: u16,
    change: impl FnOnce(&mut super::objects::Context),
) -> Result<(), Errno> {
    let context = frontend
        .state
        .contexts
        .get_mut(usize::from(slot))
        .ok_or(Errno::ENOENT)?;
    change(context);
    Ok(())
}

/// `set_proto_ctx_vm`.
fn set_vm<B: Backend>(
    frontend: &mut Frontend<B>,
    call: &Call,
    slot: u16,
    args: &ContextParam,
) -> Result<(), Errno> {
    if args.size != 0 {
        return Err(Errno::EINVAL);
    }
    if args.value >> 32 != 0 {
        return Err(Errno::ENOENT);
    }
    let state = &mut frontend.state;
    let vm = state.vm_of(call.file, args.value).ok_or(Errno::ENOENT)?;
    let old = state
        .contexts
        .get(usize::from(slot))
        .ok_or(Errno::ENOENT)?
        .vm;
    if let Some(entry) = state.vms.get_mut(usize::from(vm)) {
        entry.refs += 1;
    }
    put_vm(state, old);
    if let Some(context) = state.contexts.get_mut(usize::from(slot)) {
        context.vm = vm;
    }
    Ok(())
}

/// `set_proto_ctx_engines`, for physical engines: load balancing and bonds
/// (its extensions) are refused, as nothing here has two engines of a class.
fn set_engines<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    slot: u16,
    args: &ContextParam,
) -> Result<(), Errno> {
    if frontend
        .state
        .contexts
        .get(usize::from(slot))
        .is_some_and(|context| context.engines.is_some())
    {
        return Err(Errno::EINVAL);
    }
    let size = args.size as usize;
    let entry = EngineClassInstance::SIZE;
    if size < 8 || !(size - 8).is_multiple_of(entry) {
        return Err(Errno::EINVAL);
    }
    let count = (size - 8) / entry;
    if count > 64 {
        return Err(Errno::EINVAL);
    }
    if count > limits::CONTEXT_ENGINES {
        return Err(Errno::ENOMEM);
    }
    let mut map = [None; limits::CONTEXT_ENGINES];
    let engines = frontend.backend.facts().engines;
    for (index, cell) in map.iter_mut().enumerate().take(count) {
        let mut raw = [0_u8; 4];
        host.copy_in(
            call.request,
            args.value + 8 + (index * entry) as u64,
            &mut raw,
        )
        .map_err(|_| Errno::EFAULT)?;
        let ci = EngineClassInstance::from_bytes(&raw).ok_or(Errno::EFAULT)?;
        if ci.engine_class == query::ENGINE_CLASS_INVALID
            && ci.engine_instance == query::ENGINE_CLASS_INVALID_NONE
        {
            continue;
        }
        let wanted = Engine {
            class: ci.engine_class,
            instance: ci.engine_instance,
        };
        if !engines.contains(&wanted) {
            return Err(Errno::ENOENT);
        }
        *cell = Some(wanted);
    }
    let mut raw = [0_u8; 8];
    host.copy_in(call.request, args.value, &mut raw)
        .map_err(|_| Errno::EFAULT)?;
    let extensions = u64::from_le_bytes(raw);
    walk_extensions(host, call, extensions, &mut |_, _| Err(Errno::EINVAL))?;
    let context = frontend
        .state
        .contexts
        .get_mut(usize::from(slot))
        .ok_or(Errno::ENOENT)?;
    context.engines = Some(map);
    context.engine_count = u8::try_from(count).unwrap_or(0);
    Ok(())
}

/// `i915_gem_context_getparam_ioctl`.
pub fn context_getparam<B: Backend>(
    frontend: &mut Frontend<B>,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let mut args = ContextParam::from_bytes(data).ok_or(Errno::EFAULT)?;
    let slot = lookup(&mut frontend.state, call.file, args.ctx_id)?;
    let context = frontend
        .state
        .contexts
        .get(usize::from(slot))
        .copied()
        .ok_or(Errno::ENOENT)?;
    let facts = *frontend.backend.facts();
    let flag = |value: bool| u64::from(value);
    match args.param {
        consts::PARAM_GTT_SIZE => {
            args.size = 0;
            args.value = 1 << facts.ppgtt_bits;
        }
        consts::PARAM_NO_ERROR_CAPTURE => {
            args.size = 0;
            args.value = flag(context.no_error_capture);
        }
        consts::PARAM_BANNABLE => {
            args.size = 0;
            args.value = flag(context.bannable);
        }
        consts::PARAM_RECOVERABLE => {
            args.size = 0;
            args.value = flag(context.recoverable);
        }
        consts::PARAM_PRIORITY => {
            args.size = 0;
            args.value = context.priority as u64;
        }
        consts::PARAM_PERSISTENCE => {
            args.size = 0;
            args.value = flag(context.persistence);
        }
        consts::PARAM_PROTECTED_CONTENT => {
            args.size = 0;
            args.value = 0;
        }
        consts::PARAM_SSEU => return Err(Errno::ENODEV),
        // `get_ppgtt`: a new id for the context's VM, each call.
        consts::PARAM_VM => {
            let id = new_vm_name(&mut frontend.state, call.file, context.vm)?;
            args.value = u64::from(id);
            args.size = 0;
        }
        _ => return Err(Errno::EINVAL),
    }
    put(data, &args.to_bytes());
    Ok(())
}

/// `i915_gem_context_reset_stats_ioctl`.
pub fn reset_stats<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let mut args = ResetStats::from_bytes(data).ok_or(Errno::EFAULT)?;
    if args.flags != 0 || args.pad != 0 {
        return Err(Errno::EINVAL);
    }
    let slot = lookup(&mut frontend.state, call.file, args.ctx_id)?;
    let context = frontend
        .state
        .contexts
        .get(usize::from(slot))
        .copied()
        .ok_or(Errno::ENOENT)?;
    args.reset_count = if host.privileged(call.request) {
        frontend
            .state
            .contexts
            .iter()
            .map(|(_, context)| context.guilty)
            .sum()
    } else {
        0
    };
    args.batch_active = context.guilty;
    args.batch_pending = context.lost;
    put(data, &args.to_bytes());
    Ok(())
}

/// `RING_TIMESTAMP(RENDER_RING_BASE)`, the one register i915's whitelist
/// lets a program read on Gen12 (`reg_read_whitelist`, `intel_uncore.c`).
const RCS_TIMESTAMP: u64 = 0x2358;

/// `i915_reg_read_ioctl`: the render ring's timestamp, as 64 bits with
/// `I915_REG_READ_8B_WA` or as its low 32 without.
pub fn reg_read<B: Backend>(frontend: &mut Frontend<B>, data: &mut [u8]) -> Result<(), Errno> {
    let mut args = RegRead::from_bytes(data).ok_or(Errno::EFAULT)?;
    let flags = args.offset & (8 - 1);
    let offset = args.offset & !(8 - 1);
    if offset != RCS_TIMESTAMP {
        return Err(Errno::EINVAL);
    }
    if flags & !gem::REG_READ_8B_WA != 0 {
        return Err(Errno::EINVAL);
    }
    let value = frontend.backend.timestamp();
    // The 8-byte read is the low and high halves; without the flag, i915's
    // whitelist entry is 8 bytes wide as well and reads both (`intel_uncore_read64_2x32`).
    args.val = value;
    put(data, &args.to_bytes());
    Ok(())
}
