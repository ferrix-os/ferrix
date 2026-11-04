//! The calls of regions and shared processes (`docs/microkernel/K11.md` §2.2),
//! and of restricted mode that are ordinary calls (`docs/microkernel/K1.md`
//! §2.1). The three that go into restricted mode own a frame, and are the
//! architecture's entry's.

use alloc::sync::Arc;

use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr::{self, NativeCall};
use ferrix_native_abi::restricted::{HANDLE_KEEP, SHARED_BASE, VECTORS_BYTES};
use ferrix_native_abi::rights::{Requested, Rights};
use ferrix_native_abi::status;

use super::{copy_in, fault, insert_new, job_in, record_level, returned, table_error};
use crate::arch;
use crate::audit;
use crate::object::bare::{self, Bare};
use crate::object::job::Job;
use crate::object::process::{Host, Process, ProcessRef};
use crate::object::region::Region;
use crate::object::space::SpaceRef;
use crate::object::{self, Object};
use crate::sched;
use crate::syscall::uaccess;
use crate::user::space::AddressSpace;

/// Refuse a thread of no region: only those have restricted state to read.
fn of_a_region(process: &Process) -> Result<(), Errno> {
    supported()?;
    process.region().map(|_| ()).ok_or(status::BAD_STATE)
}

/// The calls of this file, from `answer`.
pub(super) fn answer(call: NativeCall, process: &Process, a: &[u64; 6]) -> Result<usize, Errno> {
    match call {
        NativeCall::RegionCreate => region_create(process, handle(a[0]), a[1]),
        NativeCall::ProcessCreateShared => process_create_shared(process, a),
        NativeCall::ProcessStartShared => {
            process_start_shared(process, handle(a[0]), a[1], a[2], handle(a[3]))
        }
        NativeCall::ThreadCreate => thread_create(process, a[0], a[1], a[2]),
        NativeCall::ThreadSelf => thread_self(process),
        NativeCall::ThreadExit => thread_exit(a[0], a[1]),
        NativeCall::RestrictedKick => restricted_kick(process, handle(a[0])),
        NativeCall::HandleToShared => handle_to_shared(process, handle(a[0]), a[1], a[2]),
        NativeCall::HandleToPrivate => handle_to_private(process, handle(a[0]), a[1]),
        NativeCall::RestrictedContextRead => context_read(process, a[0]),
        NativeCall::RestrictedVectorsRead => vectors_read(process, a[0], a[1]),
        NativeCall::RestrictedVectorsWrite => vectors_write(process, a[0], a[1]),
        NativeCall::RestrictedCopyWindow => copy_window(process, a[0], a[1], a[2]),
        // The entry's, which own a frame: a call that reaches here came by no
        // way that has one.
        _ => Err(if arch::HAS_RESTRICTED {
            status::BAD_STATE
        } else {
            status::NOT_SUPPORTED
        }),
    }
}

/// Refuse on an architecture without restricted mode (K1 §8).
fn supported() -> Result<(), Errno> {
    if arch::HAS_RESTRICTED {
        Ok(())
    } else {
        Err(status::NOT_SUPPORTED)
    }
}

/// `region_create`: `(job, *space) -> region`.
fn region_create(process: &Process, job: Handle, space_at: u64) -> Result<usize, Errno> {
    supported()?;
    let job = job_in(process, job, Rights::MANAGE)?;
    if !process.job().may_start_in(&job, true) {
        return Err(status::ACCESS_DENIED);
    }
    let region = Region::new(job).map_err(|_| status::NO_MEMORY)?;
    // In a measured job the region's code is what its processes run in normal mode: only
    // sealed images, sticky (K14 §3). A process's restricted space is the exception.
    if region.job().measured() {
        region.space().measure();
    }
    let space = SpaceRef::of(
        Arc::clone(region.space()),
        region.job().quota_index(),
        region.job().level(),
    )
    .map_err(|_| status::NO_MEMORY)?;
    let space = insert_new(process, Object::Space(space), Rights::SPACE)?;
    let close_space = |process: &Process| {
        let handle = Handle(space as u32);
        if let Ok(gone) = process.with_handles(|table| table.remove(handle)) {
            object::dispose([gone.0]);
        }
    };
    if let Err(why) =
        uaccess::copy_to_user(process.own_space(), space_at, &(space as u64).to_ne_bytes())
    {
        close_space(process);
        return Err(fault(why));
    }
    insert_new(process, Object::Region(region), Rights::REGION)
        .inspect_err(|_| close_space(process))
}

/// The region a handle names, if it carries `needed`.
fn region_in(process: &Process, region: Handle, needed: Rights) -> Result<Arc<Region>, Errno> {
    process.with_handles(|table| {
        let (object, rights) = table.get(region).map_err(table_error)?;
        let Object::Region(region) = object else {
            return Err(status::WRONG_TYPE);
        };
        if !rights.contains(needed) {
            return Err(status::ACCESS_DENIED);
        }
        Ok(Arc::clone(region))
    })
}

/// `process_create_shared`: `(region, job, space, name, name_length,
/// *out_space) -> process`.
fn process_create_shared(process: &Process, a: &[u64; 6]) -> Result<usize, Errno> {
    supported()?;
    let [region, job, space, name, name_length, out_space] = *a;
    let region = region_in(process, handle(region), Rights::MANAGE)?;
    let job = job_in(process, handle(job), Rights::MANAGE)?;
    if !process.job().may_start_in(&job, true) {
        return Err(status::ACCESS_DENIED);
    }
    if !region.job().contains(&job) {
        return Err(status::BAD_STATE);
    }
    // A region is one level: its shared table crosses its processes' jobs.
    if job.level() != region.job().level() {
        record_level(
            process,
            audit::target::JOB,
            job.id(),
            job.level(),
            region.job().level(),
        );
        return Err(status::ACCESS_DENIED);
    }
    let name_length = usize::try_from(name_length)
        .ok()
        .filter(|&count| count <= nr::PROCESS_NAME_MAX)
        .ok_or(status::INVALID_ARGS)?;
    let _name = copy_in(process, name, name_length)?;
    let given = handle(space) != Handle::default();
    // Where the space's handle will go, proved writable before anything is made:
    // no handle is ever left that nobody knows the number of.
    if !given {
        uaccess::copy_to_user(process.own_space(), out_space, &0_u64.to_ne_bytes())
            .map_err(fault)?;
    }
    let bound = if given {
        space_given(process, handle(space), job.level())?
    } else {
        AddressSpace::new().map_err(|_| status::NO_MEMORY)?
    };
    bound
        .bind_region(region.space(), job.quota_index())
        .map_err(|_| status::BAD_STATE)?;
    // Only a bound space gives its handle up: a refusal above left it held.
    if given {
        let gone = process
            .with_handles(|table| table.remove(handle(space)))
            .map_err(table_error)?;
        object::dispose([gone.0]);
    }
    let created = new_shared(&region, &bound, &job)?;
    let made = insert_new(process, Object::Process(created), Rights::PROCESS_CREATED)?;
    if !given {
        give_out_space(
            process,
            bound,
            job.quota_index(),
            job.level(),
            out_space,
            made,
        )?;
    }
    Ok(made)
}

/// The space behind a handle a caller gives: `MAP`, `READ`, `WRITE` and
/// `TRANSFER`, left in its table until the bind has taken.
fn space_given(process: &Process, given: Handle, level: u8) -> Result<Arc<AddressSpace>, Errno> {
    let needed = Rights::MAP | Rights::READ | Rights::WRITE | Rights::TRANSFER;
    process.with_handles(|table| {
        let (object, rights) = table.get(given).map_err(table_error)?;
        let Object::Space(space) = object else {
            return Err(status::WRONG_TYPE);
        };
        if !rights.contains(needed) {
            return Err(status::ACCESS_DENIED);
        }
        if space.level() != level {
            record_level(process, audit::target::HANDLE, 0, level, space.level());
            return Err(status::ACCESS_DENIED);
        }
        Ok(Arc::clone(space.space()))
    })
}

/// A process of `region` over `bound`, in `job`, counted in; the handle's view.
fn new_shared(
    region: &Arc<Region>,
    bound: &Arc<AddressSpace>,
    job: &Arc<Job>,
) -> Result<ProcessRef, Errno> {
    region.enter().map_err(|_| status::BAD_STATE)?;
    // A failure in `new` leaves the count to give back here; past it, the host's
    // own end or drop does.
    let Ok(host) = Bare::new(Arc::clone(bound), Arc::clone(job), Some(Arc::clone(region))) else {
        region.leave();
        return Err(status::NO_MEMORY);
    };
    let host: Arc<dyn Host> = host;
    ProcessRef::created(host).map_err(|_| status::NO_MEMORY)
}

/// Give `process` the handle to the new space at `out_space`; on a failure the
/// process it asked for goes too, and it hears why.
fn give_out_space(
    process: &Process,
    bound: Arc<AddressSpace>,
    owner: u32,
    level: u8,
    out_space: u64,
    made: usize,
) -> Result<(), Errno> {
    let close = |value: usize| {
        if let Ok(gone) = process.with_handles(|table| table.remove(Handle(value as u32))) {
            object::dispose([gone.0]);
        }
    };
    let space = SpaceRef::of(bound, owner, level)
        .map_err(|_| status::NO_MEMORY)
        .and_then(|space| insert_new(process, Object::Space(space), Rights::SPACE))
        .inspect_err(|_| close(made))?;
    uaccess::copy_to_user(
        process.own_space(),
        out_space,
        &(space as u64).to_ne_bytes(),
    )
    .map_err(fault)
    .inspect_err(|_| {
        close(space);
        close(made);
    })
}

/// The register a handle travels in.
const fn handle(value: u64) -> Handle {
    Handle(value as u32)
}

/// Whether a thread of `process` may start at `entry` on `stack`. In a process of
/// a region both lie in the region, since normal mode runs `linuxd`'s code and
/// never the program's (`docs/microkernel/K11.md` §2.2); in any other in its user
/// half, as a loader checks an image's (`sysretq` raises `#GP` in ring 0 on a bad
/// `RCX`).
fn entry_is_good(in_region: bool, entry: u64, stack: u64) -> bool {
    let end = ferrix_bootinfo::USER_VIRT_END;
    let floor = if in_region { SHARED_BASE } else { 0 };
    (floor..end).contains(&entry) && stack > floor && stack <= end
}

/// `process_start_shared`: `(process, entry, stack, bootstrap) -> 0`.
fn process_start_shared(
    process: &Process,
    target: Handle,
    entry: u64,
    stack: u64,
    bootstrap: Handle,
) -> Result<usize, Errno> {
    supported()?;
    if !entry_is_good(true, entry, stack) {
        return Err(status::INVALID_ARGS);
    }
    let control = process.with_handles(|table| {
        let (object, rights) = table.get(target).map_err(table_error)?;
        let Object::Process(child) = object else {
            return Err(status::WRONG_TYPE);
        };
        if !rights.contains(Rights::MANAGE) {
            return Err(status::ACCESS_DENIED);
        }
        child.control().map(Arc::clone).ok_or(status::BAD_STATE)
    })?;
    let host = control.host().ok_or(status::BAD_STATE)?;
    let child = control.process::<Bare>().ok_or(status::BAD_STATE)?;
    let region = host.core().region().ok_or(status::BAD_STATE)?;
    let argument = if bootstrap == Handle::default() {
        0
    } else {
        move_bootstrap(process, region, bootstrap)?
    };
    child
        .start(entry, stack, u64::from(argument))
        .map_err(|_| status::BAD_STATE)?;
    control.started();
    Ok(0)
}

/// Move `handle` from the caller's table to the region's, and answer its value
/// there; if they are the same table, the same value. Needs `TRANSFER`.
fn move_bootstrap(process: &Process, region: &Arc<Region>, handle: Handle) -> Result<u32, Errno> {
    let level = region.job().level();
    let same = process.region().is_some_and(|own| Arc::ptr_eq(own, region))
        && !sched::running_restricted();
    if same {
        return process.with_handles(|table| {
            let (_, rights) = table.get(handle).map_err(table_error)?;
            if !rights.contains(Rights::TRANSFER) {
                return Err(status::ACCESS_DENIED);
            }
            Ok(handle.0)
        });
    }
    let (object, rights) = process.with_handles(|table| {
        let (object, rights) = table.get(handle).map_err(table_error)?;
        if !rights.contains(Rights::TRANSFER) {
            return Err(status::ACCESS_DENIED);
        }
        object::level::may_hold(level, object, rights).map_err(|_| status::ACCESS_DENIED)?;
        table.remove(handle).map_err(table_error)
    })?;
    // FALLIBLE: the handle table's insert hands the object back.
    match region.with_handles(|table| table.insert(object, rights)) {
        Ok(placed) => Ok(placed.0),
        Err(object) => {
            object::dispose([object]);
            Err(status::NO_HANDLES)
        }
    }
}

/// `thread_create`: `(entry, stack, argument) -> thread`, of the caller's own
/// process alone, so that `MANAGE` on a process never lets its holder run code
/// inside it.
fn thread_create(process: &Process, entry: u64, stack: u64, argument: u64) -> Result<usize, Errno> {
    supported()?;
    if !entry_is_good(process.region().is_some(), entry, stack) {
        return Err(status::INVALID_ARGS);
    }
    let thread = bare::current_thread().ok_or(status::BAD_STATE)?;
    process.charge_thread().map_err(|_| status::SHOULD_WAIT)?;
    // The handle first, so that no thread runs behind a refusal: it is made and
    // not launched until its handle has a number.
    let mut answer = Err(status::NO_MEMORY);
    let made = thread
        .process_of()
        .spawn_with(entry, stack, argument, |object| {
            answer = insert_new(process, Object::Thread(Arc::clone(object)), Rights::THREAD);
            answer.is_ok()
        });
    if made.is_err() {
        process.uncharge_thread();
    }
    answer
}

/// `thread_self`: `() -> thread`.
fn thread_self(process: &Process) -> Result<usize, Errno> {
    supported()?;
    let thread = bare::current_thread().ok_or(status::BAD_STATE)?;
    let object = Arc::clone(thread.object());
    insert_new(process, Object::Thread(object), Rights::THREAD)
}

/// `thread_exit`: `(status, whole_process)`. Never returns but refused.
fn thread_exit(status_word: u64, whole: u64) -> Result<usize, Errno> {
    supported()?;
    if bare::current_thread().is_none() {
        return Err(status::BAD_STATE);
    }
    arch::disable_interrupts();
    bare::leave_current((whole != 0).then_some(status_word as u32 as i32))
}

/// `restricted_kick`: `(thread) -> 0`.
fn restricted_kick(process: &Process, thread: Handle) -> Result<usize, Errno> {
    supported()?;
    let thread = process.with_handles(|table| {
        let (object, rights) = table.get(thread).map_err(table_error)?;
        let Object::Thread(thread) = object else {
            return Err(status::WRONG_TYPE);
        };
        if !rights.contains(Rights::SIGNAL) {
            return Err(status::ACCESS_DENIED);
        }
        Ok(Arc::clone(thread))
    })?;
    if thread.has_ended() || !thread.in_region() {
        return Err(status::BAD_STATE);
    }
    let task = thread.task().ok_or(status::BAD_STATE)?;
    sched::work::notify(&task, sched::work::KICK);
    Ok(0)
}

/// `handle_to_shared`: `(private, rights, options) -> shared`.
fn handle_to_shared(
    process: &Process,
    private: Handle,
    rights: u64,
    options: u64,
) -> Result<usize, Errno> {
    supported()?;
    let region = process.region().ok_or(status::BAD_STATE)?;
    if options & !(HANDLE_KEEP as u64) != 0 {
        return Err(status::INVALID_ARGS);
    }
    let keep = options & HANDLE_KEEP as u64 != 0;
    let requested = Requested::from_register(rights).ok_or(status::INVALID_ARGS)?;
    let (object, given) = process.with_private_handles(|table| {
        let (object, held) = table.get(private).map_err(table_error)?;
        if !held.contains(Rights::TRANSFER) || (keep && !held.contains(Rights::DUPLICATE)) {
            return Err(status::ACCESS_DENIED);
        }
        let given = requested.resolve(held).ok_or(status::INVALID_ARGS)?;
        if keep {
            Ok((object.clone(), given))
        } else {
            table
                .remove(private)
                .map(|(object, _)| (object, given))
                .map_err(table_error)
        }
    })?;
    // FALLIBLE: the handle table's insert hands the object back.
    match region.with_handles(|table| table.insert(object, given)) {
        Ok(placed) => Ok(returned(placed)),
        Err(object) => {
            object::dispose([object]);
            Err(status::NO_HANDLES)
        }
    }
}

/// `handle_to_private`: `(shared, rights) -> private`.
fn handle_to_private(process: &Process, shared: Handle, rights: u64) -> Result<usize, Errno> {
    supported()?;
    let region = process.region().ok_or(status::BAD_STATE)?;
    let requested = Requested::from_register(rights).ok_or(status::INVALID_ARGS)?;
    let (object, given) = region.with_handles(|table| {
        let (_, held) = table.get(shared).map_err(table_error)?;
        if !held.contains(Rights::TRANSFER) {
            return Err(status::ACCESS_DENIED);
        }
        let given = requested.resolve(held).ok_or(status::INVALID_ARGS)?;
        table
            .remove(shared)
            .map(|(object, _)| (object, given))
            .map_err(table_error)
    })?;
    // FALLIBLE: the handle table's insert hands the object back.
    match process.with_private_handles(|table| table.insert(object, given)) {
        Ok(placed) => Ok(returned(placed)),
        Err(object) => {
            object::dispose([object]);
            Err(status::NO_HANDLES)
        }
    }
}

/// `restricted_context_read`: `(out) -> 0`.
fn context_read(process: &Process, out: u64) -> Result<usize, Errno> {
    of_a_region(process)?;
    let bytes = arch::restricted_context_bytes()?;
    uaccess::copy_to_user(process.own_space(), out, &bytes).map_err(fault)?;
    Ok(0)
}

/// `restricted_vectors_read`: `(out, length) -> bytes`.
fn vectors_read(process: &Process, out: u64, length: u64) -> Result<usize, Errno> {
    of_a_region(process)?;
    if usize::try_from(length).map_or(true, |length| length < VECTORS_BYTES) {
        return Err(status::INVALID_ARGS);
    }
    let bytes = arch::restricted_vector_bytes()?;
    uaccess::copy_to_user(process.own_space(), out, &bytes).map_err(fault)?;
    Ok(VECTORS_BYTES)
}

/// `restricted_vectors_write`: `(in, length) -> 0`.
fn vectors_write(process: &Process, at: u64, length: u64) -> Result<usize, Errno> {
    of_a_region(process)?;
    if usize::try_from(length) != Ok(VECTORS_BYTES) {
        return Err(status::INVALID_ARGS);
    }
    let mut image = [0_u8; VECTORS_BYTES];
    uaccess::copy_from_user(process.own_space(), at, &mut image).map_err(fault)?;
    arch::restricted_set_vector_bytes(&image)?;
    Ok(0)
}

/// `restricted_copy_window`: `(start, end, landing) -> 0`.
fn copy_window(process: &Process, start: u64, end: u64, landing: u64) -> Result<usize, Errno> {
    supported()?;
    let region = process.region().ok_or(status::BAD_STATE)?;
    let end_of_region = ferrix_bootinfo::USER_VIRT_END;
    let inside = |address: u64| (SHARED_BASE..end_of_region).contains(&address);
    // The landing is an address `IRETQ` will load: `USER_VIRT_END` itself is not
    // canonical, and would stop the kernel at the next fault in the window.
    if start >= end || !inside(start) || end > end_of_region || !inside(landing) {
        return Err(status::INVALID_ARGS);
    }
    region.set_window(start, end, landing);
    Ok(0)
}
