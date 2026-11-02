//! The native calls' refusals that stage 9's check does not make: a handle of
//! the wrong kind, one without the right a call needs, one named twice in a
//! message, a table with no room, a send whose cycle check would walk too far,
//! a packet whose buffer faults, a copy through a VMO a device reads past the
//! caches, a clock asked for badly, the kernel log asked for by a device
//! that may not read it, a bootstrap handle or a message's handles with no
//! room or no memory to be placed in, and a write with no memory for its
//! message, which keeps the handles it carried (F-42). And the job limits a
//! program sets, read back as it set them.
//!
//! Each is a sentence `docs/NATIVE-ABI.md` promises a program: which status
//! it gets, and that a refused call leaves what it was given where it was.
//! Driven through [`native::dispatch`] from one process of the check's own,
//! as stage 9's check drives the calls that succeed.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::{Rights, SAME_RIGHTS};
use ferrix_native_abi::status;
use ferrix_native_abi::types::{
    self, CHANNEL_MAX_HANDLES, JOB_CPU_WEIGHT, JOB_MEMORY, JOB_OBJECTS, JOB_TASKS, UNLIMITED,
};

use crate::arch;
use crate::object::check::{SCRATCH, Side, reg};
use crate::object::job::{self, Job};
use crate::object::process::Host as _;
use crate::object::{HANDLE_LIMIT, Object};
use crate::syscall::image::{self, Shape};
use crate::syscall::native;

/// Where each call's user memory is, in the side's scratch region.
const PAIR: u64 = SCRATCH + 0x100;
/// A packet going in, and one coming out.
const PACKET: u64 = SCRATCH + 0x120;
/// A deadline long past.
const DEADLINE: u64 = SCRATCH + 0x160;
/// A message's bytes, and where a read puts them.
const BYTES: u64 = SCRATCH + 0x180;
/// The handles a message carries, going out or coming in.
const HANDLES: u64 = SCRATCH + 0x200;
/// What a read reports it delivered.
const ACTUAL: u64 = SCRATCH + 0x400;
/// A user address nothing maps: the scratch region is two pages.
const UNMAPPED: u64 = SCRATCH + 0x10_0000;
/// Where a child's image is staged, past the scratch region.
const IMAGE_AT: u64 = 0x6000_0000;
/// A child's name, and where it is staged.
const CHILD_NAME: &[u8] = b"unstarted";
const NAME_AT: u64 = SCRATCH + 0x480;
/// A VMO offset for a write.
const OFFSET: u64 = SCRATCH + 0x4C0;
/// A limit handed to `job_set_limit`.
const LIMIT_AT: u64 = SCRATCH + 0x500;
/// What `job_get_quota` writes: used, limit and refused.
const QUOTA_AT: u64 = SCRATCH + 0x520;
/// How many handles a message carries into a table with no memory to grow:
/// more than a table that has held one handle has room for without growing.
const CARRIED: usize = 8;
/// `audit_read`'s answer, and the records it copies.
const AUDIT_ANSWER: u64 = SCRATCH + 0x540;
const AUDIT_RECORDS: u64 = SCRATCH + 0x1000;
/// How many records the check asks `audit_read` for.
const AUDIT_ASKED: u64 = 16;

/// What the checks did, for the boot log.
#[derive(Debug, Default)]
pub struct Report {
    /// Calls refused with the status the ABI names.
    pub refusals: u32,
    /// Handles opened to fill a table.
    pub filled: usize,
    /// Endpoints a refused send would have made its check walk.
    pub walked: usize,
}

/// Run every check.
///
/// # Errors
///
/// The first property that did not hold, as a sentence.
pub fn run() -> Result<Report, &'static str> {
    let mut report = Report::default();
    let side = Side::new()?;
    let outcome = checks(&side, &mut report);
    side.close_everything();
    outcome.map(|()| report)
}

/// The checks, on one process, which [`run`] empties afterwards whatever
/// they found.
fn checks(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    the_wrong_kind_of_handle_is_refused(side, report)?;
    a_handle_without_the_right_is_refused(side, report)?;
    a_handle_named_twice_is_refused(side, report)?;
    a_packet_whose_buffer_faults_is_kept(side, report)?;
    a_copy_past_the_caches_is_refused(side, report)?;
    a_clock_asked_for_badly_is_refused(side, report)?;
    a_log_reader_its_binding_does_not_allow_is_refused(side, report)?;
    a_send_that_would_walk_too_far_is_refused(side, report)?;
    a_full_table_refuses_and_loses_nothing(side, report)?;
    a_table_that_cannot_grow_refuses_for_memory(report)?;
    a_bootstrap_without_transfer_is_refused(side, report)?;
    a_job_reads_back_its_limits(side, report)?;
    an_idle_job_refuses_a_weight(side, report)?;
    a_bootstrap_without_memory_is_kept(report)?;
    a_delivery_without_memory_is_kept(side, report)?;
    a_write_without_memory_keeps_its_handles(side, report)?;
    the_audit_record_is_read_by_its_handle_alone(side, report)?;
    a_processor_leaves_service_through_its_token_alone(side, report)?;
    memory_is_scrubbed_through_its_token_alone(side, report)?;
    Ok(())
}

/// Require `result` to be exactly the refusal `wanted`.
fn refused(
    result: Result<usize, Errno>,
    wanted: Errno,
    what: &'static str,
    report: &mut Report,
) -> Result<(), &'static str> {
    if result == Err(wanted) {
        report.refusals += 1;
        Ok(())
    } else {
        Err(what)
    }
}

/// A channel in `side`: both ends.
fn channel(side: &Side) -> Result<(Handle, Handle), &'static str> {
    let _ = side
        .call(nr::CHANNEL_CREATE, &[PAIR])
        .map_err(|_| "channel_create failed")?;
    Ok((Handle(side.get_u32(PAIR)?), Handle(side.get_u32(PAIR + 4)?)))
}

/// A one-page VMO in `side`.
fn vmo(side: &Side) -> Result<Handle, &'static str> {
    side.handle(nr::VMO_CREATE, &[PAGE_SIZE], "vmo_create failed")
}

/// Stage `handles` at [`HANDLES`].
fn put_handles(side: &Side, handles: &[Handle]) -> Result<(), &'static str> {
    let words: Vec<u8> = handles.iter().flat_map(|h| h.0.to_ne_bytes()).collect();
    side.put(HANDLES, &words)
}

/// Every call that names an object of one kind refuses a handle to another
/// with `WRONG_TYPE`, whatever rights it carries: a VMO handle where a
/// channel, a process, an interrupt, a pin, an I/O mapping or a port belongs.
fn the_wrong_kind_of_handle_is_refused(
    side: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    let memory = vmo(side)?;
    let port = side.handle(nr::PORT_CREATE, &[], "port_create failed")?;
    side.put(PACKET, &[0; 32])?;
    side.put(DEADLINE, &1_u64.to_ne_bytes())?;
    let wrong = reg(memory);
    let calls: [(usize, [u64; 5], &'static str); 9] = [
        (
            nr::CHANNEL_WRITE,
            [wrong, BYTES, 0, HANDLES, 0],
            "a VMO was written as a channel",
        ),
        (
            nr::PROCESS_START,
            [wrong, 0, 0, 0, 0],
            "a VMO was started as a process",
        ),
        (
            nr::INTERRUPT_ACK,
            [wrong, 0, 0, 0, 0],
            "a VMO was acknowledged as an interrupt",
        ),
        (
            nr::INTERRUPT_BIND,
            [wrong, reg(port), PACKET, 0, 0],
            "a VMO was bound as an interrupt",
        ),
        (
            nr::VMO_PIN_ADDRESSES,
            [wrong, PACKET, 1, 0, 0],
            "a VMO was read as a pin",
        ),
        (
            nr::IO_MAPPING_MAP,
            [wrong, 0, 0, 0, 0],
            "a VMO was mapped as an I/O mapping",
        ),
        (
            nr::PORT_QUEUE,
            [wrong, PACKET, 0, 0, 0],
            "a packet was queued on a VMO",
        ),
        (
            nr::PORT_WAIT,
            [wrong, DEADLINE, PACKET, 0, 0],
            "a VMO was waited on as a port",
        ),
        (
            nr::DEVICE_CLOCK,
            [wrong, 1, 0, 0, 0],
            "a VMO was clocked as a device",
        ),
    ];
    for (number, args, what) in calls {
        refused(side.call(number, &args), status::WRONG_TYPE, what, report)?;
    }
    Ok(())
}

/// A handle that lacks the right a call needs is refused with
/// `ACCESS_DENIED`, the object untouched: a channel end duplicated with
/// `READ` alone cannot be written.
fn a_handle_without_the_right_is_refused(
    side: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    let (end, _other) = channel(side)?;
    let read_only = side.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(end), u64::from(Rights::READ.0)],
        "handle_duplicate with fewer rights failed",
    )?;
    side.put(BYTES, b"x")?;
    refused(
        side.call(nr::CHANNEL_WRITE, &[reg(read_only), BYTES, 1, HANDLES, 0]),
        status::ACCESS_DENIED,
        "a channel end without WRITE was written",
        report,
    )
}

/// A message naming one handle twice is refused with `INVALID_ARGS`, and the
/// handle stays open in the sender.
fn a_handle_named_twice_is_refused(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    let (end, _other) = channel(side)?;
    let carried = vmo(side)?;
    put_handles(side, &[carried, carried])?;
    refused(
        side.call(nr::CHANNEL_WRITE, &[reg(end), BYTES, 0, HANDLES, 2]),
        status::INVALID_ARGS,
        "a message naming one handle twice was sent",
        report,
    )?;
    if side
        .call(nr::VMO_GET_SIZE, &[reg(carried), PACKET])
        .is_err()
    {
        return Err("a refused message took the handle it named twice");
    }
    Ok(())
}

/// A packet taken off a port for a buffer that faults is put back, and the
/// next wait with a good buffer gets it.
fn a_packet_whose_buffer_faults_is_kept(
    side: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    let port = side.handle(nr::PORT_CREATE, &[], "port_create failed")?;
    let key = 0x5EED_u64;
    let mut packet = [0_u8; 32];
    if let Some(slot) = packet.get_mut(..8) {
        slot.copy_from_slice(&key.to_ne_bytes());
    }
    side.put(PACKET, &packet)?;
    side.put(DEADLINE, &1_u64.to_ne_bytes())?;
    let _ = side
        .call(nr::PORT_QUEUE, &[reg(port), PACKET])
        .map_err(|_| "port_queue failed")?;
    refused(
        side.call(nr::PORT_WAIT, &[reg(port), DEADLINE, UNMAPPED]),
        status::FAULT,
        "a packet was delivered to a buffer nothing maps",
        report,
    )?;
    side.put(PACKET, &[0; 32])?;
    let _ = side
        .call(nr::PORT_WAIT, &[reg(port), DEADLINE, PACKET])
        .map_err(|_| "a packet whose delivery faulted was lost")?;
    let got = side.get(PACKET, 8)?;
    if got.as_slice() != key.to_ne_bytes().as_slice() {
        return Err("a packet put back came out as another");
    }
    Ok(())
}

/// A VMO a device reads past the caches is not copied through the kernel's
/// cached view: `vmo_read` and `vmo_write` answer `BAD_STATE`.
fn a_copy_past_the_caches_is_refused(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    let handle = vmo(side)?;
    let object = side
        .process
        .with_handles(|table| table.get(handle).map(|(object, _)| object.clone()))
        .map_err(|_| "a VMO just made is not in its table")?;
    let Object::Vmo(memory) = object else {
        return Err("vmo_create made something other than a VMO");
    };
    if !memory.make_coherent() {
        return Err("a VMO nothing maps could not be made coherent");
    }
    side.put(PACKET, &0_u64.to_ne_bytes())?;
    for number in [nr::VMO_READ, nr::VMO_WRITE] {
        refused(
            side.call(number, &[reg(handle), BYTES, 8, PACKET]),
            status::BAD_STATE,
            "a VMO a device reads past the caches was copied through them",
            report,
        )?;
    }
    Ok(())
}

/// `device_clock` refuses an option it does not know and a rate of zero
/// before it looks at the handle, and a device whose board keeps no clock for
/// it with `WRONG_TYPE`.
///
/// Verifies: L.device.13
fn a_clock_asked_for_badly_is_refused(
    side: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    refused(
        side.call(nr::DEVICE_CLOCK, &[0, 1, 1 << 7]),
        status::INVALID_ARGS,
        "a clock was set with an option nobody defined",
        report,
    )?;
    refused(
        side.call(nr::DEVICE_CLOCK, &[0, 0, 0]),
        status::INVALID_ARGS,
        "a clock was asked for zero hertz",
        report,
    )?;
    let Some(node) = crate::device::devices().first() else {
        return Ok(());
    };
    if crate::discovery::board::board_clock(node, 1, false).is_some() {
        // A board that keeps this device's clock: not the refusal checked here.
        return Ok(());
    }
    let device = side
        .process
        .with_handles(|table| table.insert(Object::Device(node.clone()), Rights::DEVICE))
        .map_err(|_| "no room for a device handle")?;
    refused(
        side.call(nr::DEVICE_CLOCK, &[reg(device), 1, 0]),
        status::WRONG_TYPE,
        "a clock was read from a device whose board keeps none for it",
        report,
    )
}

/// `log_control_create` refuses a device whose binding may not carry the
/// kernel log off the machine with `ACCESS_DENIED`, even with `MANAGE` on it:
/// only the Pixel 7's USB device controller may, and no machine the boot
/// tests run on has one. What a device that may read gets is the log core's
/// own check (`logctl/check.rs`).
///
/// Verifies: L.device.12
fn a_log_reader_its_binding_does_not_allow_is_refused(
    side: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    let Some(node) = crate::device::devices()
        .iter()
        .find(|node| !node.reads_log())
    else {
        return Ok(());
    };
    let device = side
        .process
        .with_handles(|table| table.insert(Object::Device(node.clone()), Rights::DEVICE))
        .map_err(|_| "no room for a device handle")?;
    refused(
        side.call(nr::LOG_CONTROL_CREATE, &[reg(device)]),
        status::ACCESS_DENIED,
        "the kernel log was given to a device whose binding may not read it",
        report,
    )
}

/// A send whose cycle check would have to walk more queued endpoints than
/// the bound is refused as `TOO_BIG`, and what it would have carried stays
/// with the sender.
///
/// One channel's queue is filled with the ends of more channels than the
/// walk visits; sending that channel's reading end anywhere then asks the
/// walk to visit all of them.
///
/// Verifies: L.object.26
fn a_send_that_would_walk_too_far_is_refused(
    side: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    const WALK_BOUND: usize = 1024;
    let (full, feeder) = channel(side)?;
    let mut queued = 0;
    while queued <= WALK_BOUND {
        let mut batch = Vec::new();
        while batch.len() + 2 <= CHANNEL_MAX_HANDLES && queued + batch.len() <= WALK_BOUND {
            let (first, second) = channel(side)?;
            batch.push(first);
            batch.push(second);
        }
        put_handles(side, &batch)?;
        let _ = side
            .call(
                nr::CHANNEL_WRITE,
                &[reg(feeder), BYTES, 0, HANDLES, batch.len() as u64],
            )
            .map_err(|_| "could not queue endpoints for the walk")?;
        queued += batch.len();
    }
    let (out, _elsewhere) = channel(side)?;
    put_handles(side, &[full])?;
    refused(
        side.call(nr::CHANNEL_WRITE, &[reg(out), BYTES, 0, HANDLES, 1]),
        status::TOO_BIG,
        "a send was made whose cycle check could not finish",
        report,
    )?;
    if side.call(nr::CHANNEL_READ, &[reg(full), BYTES, 0, HANDLES, 0, ACTUAL])
        == Err(status::BAD_HANDLE)
    {
        return Err("a send refused as too big took the end it carried");
    }
    report.walked = queued;
    Ok(())
}

/// A table with no room refuses a new handle with `NO_HANDLES` -- from a
/// duplicate, from a call that makes an object, from a read that would
/// deliver one, and from `process_bootstrap` -- and neither the message nor
/// the bootstrap handle that could not be delivered is lost.
///
/// Verifies: L.object.82, L.object.92, H.OBJ.7
fn a_full_table_refuses_and_loses_nothing(
    side: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    let (inbox, sender) = channel(side)?;
    let carried = vmo(side)?;
    put_handles(side, &[carried])?;
    let _ = side
        .call(nr::CHANNEL_WRITE, &[reg(sender), BYTES, 0, HANDLES, 1])
        .map_err(|_| "could not send a handle to read into a full table")?;

    let mut filled = 0;
    loop {
        match side.call(nr::HANDLE_DUPLICATE, &[reg(sender), u64::from(SAME_RIGHTS)]) {
            Ok(_) => filled += 1,
            Err(status::NO_HANDLES) => break,
            Err(_) => return Err("a duplicate into a table with room failed"),
        }
        if filled > HANDLE_LIMIT {
            return Err("a handle table took more handles than its limit");
        }
    }
    report.refusals += 1;
    refused(
        side.call(nr::VMO_CREATE, &[PAGE_SIZE]),
        status::NO_HANDLES,
        "an object was made into a full table",
        report,
    )?;
    refused(
        side.call(
            nr::CHANNEL_READ,
            &[
                reg(inbox),
                BYTES,
                0,
                HANDLES,
                CHANNEL_MAX_HANDLES as u64,
                ACTUAL,
            ],
        ),
        status::NO_HANDLES,
        "a handle was delivered into a full table",
        report,
    )?;
    let bootstrap = Job::new_root().map_err(|_| "no memory for a bootstrap")?;
    side.process
        .with_bootstrap(|slot| slot.give(Object::Job(bootstrap), Rights::JOB))
        .map_err(|_| "the check's process refused a bootstrap")?;
    refused(
        side.call(nr::PROCESS_BOOTSTRAP, &[]),
        status::NO_HANDLES,
        "a bootstrap handle was placed in a full table",
        report,
    )?;

    // Room again, and the message is still there to read.
    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(sender)])
        .map_err(|_| "could not close a handle of a full table")?;
    let _ = side
        .call(
            nr::CHANNEL_READ,
            &[
                reg(inbox),
                BYTES,
                0,
                HANDLES,
                CHANNEL_MAX_HANDLES as u64,
                ACTUAL,
            ],
        )
        .map_err(|_| "a message refused for a full table was lost")?;
    // And room again for the bootstrap, which was put back for this call.
    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(inbox)])
        .map_err(|_| "could not close a handle of a full table")?;
    bootstrapped(side, "a bootstrap refused for a full table was lost")?;
    report.filled = filled;
    // Room for the checks after this one.
    side.close_everything();
    Ok(())
}

/// A process whose handle table has never held a handle must grow it for its
/// first, and with no memory to grow it a call that makes an object answers
/// `NO_MEMORY` and frees the object it made, whichever of the call's
/// allocations was the one refused (finding F-23).
///
/// Each attempt is a fresh process, so the table's growth is among the few
/// allocations the call makes, and every one of them in turn is failed.
fn a_table_that_cannot_grow_refuses_for_memory(report: &mut Report) -> Result<(), &'static str> {
    let me = crate::sched::current_id().ok_or("the checking task is not running")?;
    let mut refused = 0;
    for number in [nr::VMO_CREATE, nr::CHANNEL_CREATE] {
        for period in 1..=8 {
            let fresh = Side::new()?;
            crate::fallible::inject(me, period);
            let made = fresh.call(
                number,
                &[if number == nr::VMO_CREATE {
                    PAGE_SIZE
                } else {
                    PAIR
                }],
            );
            let _ = crate::fallible::stop_injecting();
            fresh.close_everything();
            match made {
                Ok(_) => {}
                Err(status::NO_MEMORY) => refused += 1,
                Err(_) => {
                    return Err("a call without memory answered something other than NO_MEMORY");
                }
            }
        }
    }
    if refused == 0 {
        return Err("no call was refused for memory with every allocation failing in turn");
    }
    report.refusals += refused;
    Ok(())
}

/// `process_start` moves the bootstrap handle into the child only if it
/// carries `TRANSFER`: without it the start is refused with
/// `ACCESS_DENIED` before anything moves, and the handle stays the
/// caller's.
fn a_bootstrap_without_transfer_is_refused(
    side: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    if arch::USER_ARGUMENT_PROGRAM.is_empty() {
        return Ok(());
    }
    let job = Job::new_root().map_err(|_| "no memory for a child's job")?;
    let root = side
        .process
        .with_handles(|table| table.insert(Object::Job(job), Rights::JOB))
        .map_err(|_| "no room for a child's job")?;
    let class = if size_of::<usize>() == 8 {
        ferrix_elf::Class::Elf64
    } else {
        ferrix_elf::Class::Elf32
    };
    let file = image::build_with(
        class,
        arch::ARCH.elf_machine(),
        Shape::Good,
        arch::USER_ARGUMENT_PROGRAM,
    );
    let len = file.len() as u64;
    let _ = side
        .process
        .space()
        .map_anonymous(
            IMAGE_AT,
            len.div_ceil(PAGE_SIZE) * PAGE_SIZE,
            ferrix_vma::VmaFlags::READ_WRITE,
        )
        .map_err(|_| "no room for a child's image")?;
    side.put(IMAGE_AT, &file)?;
    let image = side.handle(
        nr::VMO_CREATE,
        &[len],
        "vmo_create for a child's image failed",
    )?;
    side.put(OFFSET, &0_u64.to_ne_bytes())?;
    let _ = side
        .call(nr::VMO_WRITE, &[reg(image), IMAGE_AT, len, OFFSET])
        .map_err(|_| "a child's image could not be written")?;
    side.put(NAME_AT, CHILD_NAME)?;
    let child = side.handle(
        nr::PROCESS_CREATE,
        &[reg(root), reg(image), NAME_AT, CHILD_NAME.len() as u64],
        "process_create failed",
    )?;
    let (bootstrap, _other) = channel(side)?;
    let kept = side.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(bootstrap), u64::from((Rights::READ | Rights::WRITE).0)],
        "handle_duplicate without TRANSFER failed",
    )?;
    refused(
        side.call(nr::PROCESS_START, &[reg(child), reg(kept)]),
        status::ACCESS_DENIED,
        "a child was started with a bootstrap handle that may not be transferred",
        report,
    )?;
    if side.call(nr::HANDLE_CLOSE, &[reg(kept)]).is_err() {
        return Err("a refused start took the bootstrap handle it was given");
    }
    Ok(())
}

/// `process_bootstrap` in `side`, required to place a handle: the bootstrap
/// a refused call put back.
fn bootstrapped(side: &Side, what: &'static str) -> Result<(), &'static str> {
    let placed = side.handle(nr::PROCESS_BOOTSTRAP, &[], what)?;
    if placed == Handle::INVALID {
        return Err(what);
    }
    Ok(())
}

/// `job_set_limit` on `job` for `resource`, to `limit`.
fn set_limit(side: &Side, job: Handle, resource: u64, limit: u64) -> Result<usize, Errno> {
    side.put(LIMIT_AT, &limit.to_ne_bytes())
        .map_err(|_| status::FAULT)?;
    side.call(nr::JOB_SET_LIMIT, &[reg(job), resource, LIMIT_AT])
}

/// `job_get_quota` on `job` for `resource`: used, limit and refused.
fn quota(side: &Side, job: Handle, resource: u64) -> Result<[u64; 3], &'static str> {
    let _ = side
        .call(nr::JOB_GET_QUOTA, &[reg(job), resource, QUOTA_AT])
        .map_err(|_| "job_get_quota failed")?;
    let bytes = side.get(QUOTA_AT, 24)?;
    let mut words = [0_u64; 3];
    for (word, chunk) in words.iter_mut().zip(bytes.chunks_exact(8)) {
        *word = u64::from_ne_bytes(chunk.try_into().map_err(|_| "a short quota")?);
    }
    Ok(words)
}

/// `job_get_quota` answers what `job_set_limit` set: a memory limit in whole
/// pages, rounded down, and a processor weight as given, one outside 1 to
/// 10,000 refused with `INVALID_ARGS`. The tree's root is charged nothing: it
/// reads as unlimited, with the default weight, and refuses a limit with
/// `BAD_STATE`. A quota written where nothing is mapped is `FAULT`.
fn a_job_reads_back_its_limits(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    let made = Job::new_root().map_err(|_| "no memory for a job")?;
    let job = side
        .process
        .with_handles(|table| table.insert(Object::Job(made), Rights::JOB))
        .map_err(|_| "no room for a job")?;
    let root = side
        .process
        .with_handles(|table| table.insert(Object::Job(Arc::clone(job::root())), Rights::JOB))
        .map_err(|_| "no room for the tree's root")?;

    let _ = set_limit(side, job, JOB_MEMORY, 3 * PAGE_SIZE + 1)
        .map_err(|_| "job_set_limit refused a memory limit")?;
    if quota(side, job, JOB_MEMORY)?[1] != 3 * PAGE_SIZE {
        return Err("a memory limit did not read back rounded down to whole pages");
    }
    let _ = set_limit(side, job, JOB_CPU_WEIGHT, 250)
        .map_err(|_| "job_set_limit refused a processor weight")?;
    if quota(side, job, JOB_CPU_WEIGHT)? != [0, 250, 0] {
        return Err("a processor weight did not read back as set");
    }
    for weight in [0, 10_001, u64::MAX] {
        refused(
            set_limit(side, job, JOB_CPU_WEIGHT, weight),
            status::INVALID_ARGS,
            "a processor weight outside 1 to 10,000 was accepted",
            report,
        )?;
    }

    refused(
        set_limit(side, root, JOB_TASKS, 10),
        status::BAD_STATE,
        "the tree's root took a limit",
        report,
    )?;
    if quota(side, root, JOB_OBJECTS)? != [0, UNLIMITED, 0] {
        return Err("the tree's root did not read as unlimited");
    }
    if quota(side, root, JOB_CPU_WEIGHT)? != [0, 100, 0] {
        return Err("the tree's root did not read as the default weight");
    }
    refused(
        side.call(nr::JOB_GET_QUOTA, &[reg(job), JOB_MEMORY, UNMAPPED]),
        status::FAULT,
        "a quota was written where nothing is mapped",
        report,
    )?;

    for handle in [job, root] {
        let _ = side
            .call(nr::HANDLE_CLOSE, &[reg(handle)])
            .map_err(|_| "could not close a job handle")?;
    }
    Ok(())
}

/// A job with `cpu.idle` set refuses a processor weight with `BAD_STATE` and
/// keeps the one it had; let off, it takes one (`docs/CGROUPS.md` §12).
///
/// Verifies: L.object.187
fn an_idle_job_refuses_a_weight(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    let made = Job::new_root().map_err(|_| "no memory for a job")?;
    let idle = Arc::clone(&made);
    let job = side
        .process
        .with_handles(|table| table.insert(Object::Job(made), Rights::JOB))
        .map_err(|_| "no room for a job")?;
    let _ = set_limit(side, job, JOB_CPU_WEIGHT, 250)
        .map_err(|_| "job_set_limit refused a processor weight")?;
    if !idle.set_cpu_idle(true) {
        return Err("a job's cpu.idle could not be set");
    }
    refused(
        set_limit(side, job, JOB_CPU_WEIGHT, 400),
        status::BAD_STATE,
        "an idle job took a processor weight",
        report,
    )?;
    if quota(side, job, JOB_CPU_WEIGHT)? != [0, 250, 0] {
        return Err("a weight refused to an idle job changed the one it had");
    }
    let _ = idle.set_cpu_idle(false);
    let _ = set_limit(side, job, JOB_CPU_WEIGHT, 400)
        .map_err(|_| "a job let off cpu.idle refused a processor weight")?;
    if quota(side, job, JOB_CPU_WEIGHT)? != [0, 400, 0] {
        return Err("a job let off cpu.idle did not take a processor weight");
    }
    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(job)])
        .map_err(|_| "could not close a job handle")?;
    Ok(())
}

/// A `process_bootstrap` whose table cannot grow for the handle is refused
/// with `NO_MEMORY`, and the bootstrap is put back for the next call, which
/// is given it.
///
/// Each attempt is a fresh process, whose table has never held a handle and
/// must grow for its first; each of the call's first allocations in turn is
/// failed.
fn a_bootstrap_without_memory_is_kept(report: &mut Report) -> Result<(), &'static str> {
    let me = crate::sched::current_id().ok_or("the checking task is not running")?;
    let mut kept = 0;
    for nth in 1..=4 {
        let fresh = Side::new()?;
        let bootstrap = Job::new_root().map_err(|_| "no memory for a bootstrap")?;
        fresh
            .process
            .with_bootstrap(|slot| slot.give(Object::Job(bootstrap), Rights::JOB))
            .map_err(|_| "a fresh process refused a bootstrap")?;
        crate::fallible::inject_once(me, nth, false);
        let taken = fresh.call(nr::PROCESS_BOOTSTRAP, &[]);
        let _ = crate::fallible::stop_injecting();
        let outcome = match taken {
            Ok(_) => Ok(()),
            Err(status::NO_MEMORY) => {
                kept += 1;
                bootstrapped(&fresh, "a bootstrap refused for memory was lost")
            }
            Err(_) => Err("a bootstrap without memory answered something other than NO_MEMORY"),
        };
        fresh.close_everything();
        outcome?;
    }
    if kept == 0 {
        return Err("no bootstrap was refused for memory with its first allocations failing");
    }
    report.refusals += kept;
    Ok(())
}

/// A read that would deliver a message's handles into a table with no memory
/// to grow for them is refused with `NO_MEMORY`, and the message stays
/// queued, handles and all, for the next read.
///
/// The reading end is moved into a fresh process, so the table holds that
/// one handle and must grow for the [`CARRIED`] the message brings; each of
/// the read's first allocations in turn is failed.
fn a_delivery_without_memory_is_kept(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    let me = crate::sched::current_id().ok_or("the checking task is not running")?;
    let read = |at: &Side, inbox: Handle| {
        at.call(
            nr::CHANNEL_READ,
            &[
                reg(inbox),
                BYTES,
                0,
                HANDLES,
                CHANNEL_MAX_HANDLES as u64,
                ACTUAL,
            ],
        )
    };
    let mut kept = 0;
    for nth in 1..=4 {
        let (inbox, sender) = channel(side)?;
        let mut carried = Vec::new();
        for _ in 0..CARRIED {
            carried.push(vmo(side)?);
        }
        put_handles(side, &carried)?;
        let _ = side
            .call(
                nr::CHANNEL_WRITE,
                &[reg(sender), BYTES, 0, HANDLES, CARRIED as u64],
            )
            .map_err(|_| "could not send handles to read without memory")?;
        let (object, rights) = side
            .process
            .with_handles(|table| table.remove(inbox))
            .map_err(|_| "could not take the reading end out")?;
        let fresh = Side::new()?;
        let inbox = fresh
            .process
            .with_handles(|table| table.insert(object, rights))
            .map_err(|_| "no room for the reading end")?;

        crate::fallible::inject_once(me, nth, false);
        let first = read(&fresh, inbox);
        let _ = crate::fallible::stop_injecting();
        let outcome = match first {
            Ok(_) => Ok(()),
            Err(status::NO_MEMORY) => {
                kept += 1;
                match read(&fresh, inbox) {
                    Ok(_) if fresh.get_u32(ACTUAL + 4) == Ok(CARRIED as u32) => Ok(()),
                    _ => Err("a message refused for memory was not delivered whole after"),
                }
            }
            Err(_) => Err("a read without memory answered something other than NO_MEMORY"),
        };
        fresh.close_everything();
        let _ = side.call(nr::HANDLE_CLOSE, &[reg(sender)]);
        outcome?;
    }
    if kept == 0 {
        return Err("no read was refused for memory with its first allocations failing");
    }
    report.refusals += kept;
    Ok(())
}

/// A write the peer's queue has no memory to take is refused with
/// `NO_MEMORY` and leaves the handles it carried in the sender's table, and
/// nothing queued: the handles leave a process only if the write succeeds
/// (`channel_write`'s promise).
///
/// Each attempt writes into a fresh channel, whose queue has never held a
/// message and must grow for its first, carrying a handle; each of the
/// write's first allocations in turn is failed.
fn a_write_without_memory_keeps_its_handles(
    side: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    let me = crate::sched::current_id().ok_or("the checking task is not running")?;
    let mut kept = 0;
    for nth in 1..=4 {
        let (inbox, sender) = channel(side)?;
        let carried = vmo(side)?;
        put_handles(side, &[carried])?;
        crate::fallible::inject_once(me, nth, false);
        let wrote = side.call(nr::CHANNEL_WRITE, &[reg(sender), BYTES, 0, HANDLES, 1]);
        let _ = crate::fallible::stop_injecting();
        let outcome = match wrote {
            // The handle went with the message; closing the inbox closes it.
            Ok(_) => Ok(()),
            Err(status::NO_MEMORY) => {
                kept += 1;
                let queued = side.call(
                    nr::CHANNEL_READ,
                    &[
                        reg(inbox),
                        BYTES,
                        0,
                        HANDLES,
                        CHANNEL_MAX_HANDLES as u64,
                        ACTUAL,
                    ],
                );
                if queued != Err(status::SHOULD_WAIT) {
                    Err("a write refused for memory queued its message")
                } else if side.call(nr::HANDLE_CLOSE, &[reg(carried)]).is_err() {
                    Err("a write refused for memory took the handle it carried")
                } else {
                    Ok(())
                }
            }
            Err(_) => Err("a write without memory answered something other than NO_MEMORY"),
        };
        for end in [inbox, sender] {
            let _ = side.call(nr::HANDLE_CLOSE, &[reg(end)]);
        }
        outcome?;
    }
    if kept == 0 {
        return Err("no write was refused for memory with its first allocations failing");
    }
    report.refusals += kept;
    Ok(())
}

/// Stage `audit_read`'s answer with `from` in its second word.
fn stage_answer(side: &Side, from: u64) -> Result<(), &'static str> {
    let mut words = [0_u8; types::AUDIT_ANSWER_WORDS * 8];
    if let Some(slot) = words.get_mut(8..16) {
        slot.copy_from_slice(&from.to_ne_bytes());
    }
    side.put(AUDIT_ANSWER, &words)
}

/// `audit_read`'s answer: copied, next, lost, and the audit id.
fn answered(side: &Side) -> Result<(u64, u64, u64, u128), &'static str> {
    let bytes = side.get(AUDIT_ANSWER, types::AUDIT_ANSWER_WORDS * 8)?;
    let word = |at: usize| {
        bytes
            .get(at * 8..at * 8 + 8)
            .and_then(|word| <[u8; 8]>::try_from(word).ok())
            .map_or(0, u64::from_ne_bytes)
    };
    Ok((
        word(0),
        word(1),
        word(2),
        u128::from(word(3)) | (u128::from(word(4)) << 64),
    ))
}

/// The audit record's handle (`docs/certification/AUDIT.md` §4), with the
/// rights pid 1 is given it with: it reads the pinned boot records, the start-up record first with the
/// id the kernel holds; a count of zero copies nothing and leaves the cursor
/// where it was; a handle without `READ`, another object's handle and a
/// ring that does not exist are refused; a read into a buffer nothing maps
/// is refused and counts nothing as read; and the handle, holding neither
/// `DUPLICATE` nor `TRANSFER`, can be neither copied nor sent.
///
/// Verifies: H.AUD.9
fn the_audit_record_is_read_by_its_handle_alone(
    side: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    let core = side.process.core();
    let reader = native::insert_new(core, Object::Audit, crate::init::AUDIT_RIGHTS)
        .ok()
        .and_then(|value| u32::try_from(value).ok())
        .map(Handle)
        .ok_or("could not give the check an audit handle")?;
    let read = |handle: Handle, which: u64, count: u64| {
        side.call(
            nr::AUDIT_READ,
            &[reg(handle), which, AUDIT_RECORDS, count, AUDIT_ANSWER],
        )
    };
    stage_answer(side, 0)?;
    if read(reader, types::AUDIT_BOOT, AUDIT_ASKED) != Ok(0) {
        return Err("audit_read on the audit record's handle was refused");
    }
    let (copied, next, lost, id) = answered(side)?;
    let first = side.get(AUDIT_RECORDS, ferrix_audit::RECORD_BYTES)?;
    let first = <[u8; ferrix_audit::RECORD_BYTES]>::try_from(first.as_slice())
        .map(|bytes| ferrix_audit::Record::from_bytes(&bytes))
        .map_err(|_| "a short read")?;
    let started = first
        .start_fields()
        .is_some_and(|(started, _, _)| started == id);
    if copied == 0 || next != copied || lost != 0 || id != crate::audit::id() || !started {
        return Err(
            "audit_read did not answer the boot records, the start-up record first with the kernel's id",
        );
    }
    // A count of zero: nothing copied, the cursor where it was, the buffer
    // as it was left.
    side.put(AUDIT_RECORDS, &[0xA5; ferrix_audit::RECORD_BYTES])?;
    stage_answer(side, 5)?;
    if read(reader, types::AUDIT_HIGH, 0) != Ok(0) {
        return Err("audit_read of no records was refused");
    }
    let (copied, next, _, _) = answered(side)?;
    let untouched = side
        .get(AUDIT_RECORDS, ferrix_audit::RECORD_BYTES)?
        .iter()
        .all(|&byte| byte == 0xA5);
    if copied != 0 || next != 5 || !untouched {
        return Err("audit_read of no records copied some, or moved the cursor");
    }
    refused(
        read(reader, 7, 1),
        status::INVALID_ARGS,
        "audit_read of a ring that does not exist was answered",
        report,
    )?;
    // A read whose buffer nothing maps is refused, and leaves how far the
    // reader has read where it was: the records were never handed over.
    let through = crate::audit::read_through_now();
    stage_answer(side, 0)?;
    refused(
        side.call(
            nr::AUDIT_READ,
            &[reg(reader), types::AUDIT_HIGH, UNMAPPED, 1, AUDIT_ANSWER],
        ),
        status::FAULT,
        "audit_read into a buffer nothing maps was answered",
        report,
    )?;
    if crate::audit::read_through_now() != through {
        return Err("a read that could not copy its records out counted them as read");
    }
    let waiter = native::insert_new(core, Object::Audit, Rights::WAIT)
        .ok()
        .and_then(|value| u32::try_from(value).ok())
        .map(Handle)
        .ok_or("could not give the check a second audit handle")?;
    refused(
        read(waiter, types::AUDIT_BOOT, 1),
        status::ACCESS_DENIED,
        "audit_read on a handle without READ was answered",
        report,
    )?;
    let (end, _other) = channel(side)?;
    refused(
        read(end, types::AUDIT_BOOT, 1),
        status::WRONG_TYPE,
        "audit_read on a channel end was answered",
        report,
    )?;
    refused(
        side.call(nr::HANDLE_DUPLICATE, &[reg(reader), u64::from(SAME_RIGHTS)]),
        status::ACCESS_DENIED,
        "the audit record's handle was duplicated",
        report,
    )?;
    put_handles(side, &[reader])?;
    side.put(BYTES, b"x")?;
    refused(
        side.call(nr::CHANNEL_WRITE, &[reg(end), BYTES, 1, HANDLES, 1]),
        status::ACCESS_DENIED,
        "the audit record's handle was sent",
        report,
    )
}

/// The processor service token (`docs/CORE-QUARANTINE.md` §4): pid 1's
/// rights pinned, `MANAGE`, `DUPLICATE` and `TRANSFER`; `cpu_service`
/// refusing a processor past the count, a flag other than 0 or 1, a handle
/// without `MANAGE` and another object's handle. On x86-64 with two
/// processors or more: the boot processor refused, processor 1 taken out
/// and brought back, an ask for what already is answered without a record,
/// and the three records read back -- the refusal with its number, the
/// change out and the change back, each naming processor 1 or 0 and lode's
/// door; a park whose deadline has passed undone, the processor left in
/// service; and a processor with a program pinned to it refused. Elsewhere
/// the call is refused as unsupported.
///
/// Verifies: L.sched.105, L.smp.35, L.smp.36, L.smp.37, L.smp.38
fn a_processor_leaves_service_through_its_token_alone(
    side: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    if crate::init::CPU_SERVICE_RIGHTS != Rights::MANAGE | Rights::DUPLICATE | Rights::TRANSFER {
        return Err("pid 1's processor service token is not MANAGE, DUPLICATE and TRANSFER");
    }
    let core = side.process.core();
    let token = native::insert_new(core, Object::CpuService, crate::init::CPU_SERVICE_RIGHTS)
        .ok()
        .and_then(|value| u32::try_from(value).ok())
        .map(Handle)
        .ok_or("could not give the check a processor service token")?;
    let waiter = native::insert_new(core, Object::CpuService, Rights::WAIT)
        .ok()
        .and_then(|value| u32::try_from(value).ok())
        .map(Handle)
        .ok_or("could not give the check a second processor service token")?;
    let ask =
        |handle: Handle, cpu: u64, flag: u64| side.call(nr::CPU_SERVICE, &[reg(handle), cpu, flag]);
    let count = crate::smp::count() as u64;
    refused(
        ask(token, count, 0),
        status::INVALID_ARGS,
        "cpu_service past the processor count was answered",
        report,
    )?;
    refused(
        ask(token, 0, 2),
        status::INVALID_ARGS,
        "cpu_service with a flag other than 0 or 1 was answered",
        report,
    )?;
    refused(
        ask(waiter, 0, 1),
        status::ACCESS_DENIED,
        "cpu_service through a handle without MANAGE was answered",
        report,
    )?;
    let (end, _other) = channel(side)?;
    refused(
        ask(end, 0, 1),
        status::WRONG_TYPE,
        "cpu_service through a channel end was answered",
        report,
    )?;
    if count < 2 {
        return Ok(());
    }
    if !arch::CAN_MOVE_INTERRUPTS {
        return refused(
            ask(token, 1, 0),
            status::WRONG_TYPE,
            "cpu_service took a processor out where interrupts cannot be moved",
            report,
        );
    }
    processor_one_out_and_back(side, report, token)
}

/// [`a_processor_leaves_service_through_its_token_alone`]'s second half, on
/// x86-64 with two processors or more: the boot processor refused, processor
/// 1 out and back with an ask for what already is between, the three audit
/// records read back, the deadline's undo, and the pinned refusal.
fn processor_one_out_and_back(
    side: &Side,
    report: &mut Report,
    token: Handle,
) -> Result<(), &'static str> {
    use ferrix_audit::{CPU_SERVICE, Outcome, cpu_service, target};

    let in_service = |cpu: usize| {
        crate::smp::topology()
            .and_then(|topology| topology.cpus().get(cpu))
            .is_some_and(crate::smp::PerCpu::in_service)
    };
    let ask =
        |handle: Handle, cpu: u64, flag: u64| side.call(nr::CPU_SERVICE, &[reg(handle), cpu, flag]);
    let mark = crate::audit::high_next();
    refused(
        ask(token, 0, 0),
        status::BAD_STATE,
        "cpu_service took the boot processor out",
        report,
    )?;
    // A message processor 1 cannot have moved -- in compatibility format,
    // where no unit remaps (test-nvrm's unremapped boot) -- refuses it, the
    // processor left in service and the refusal recorded with its number,
    // 3 (L.iommu.104).
    let first = ask(token, 1, 0);
    let stayed = first == Err(status::BAD_STATE) && in_service(1);
    if !stayed {
        if first != Ok(0) || in_service(1) {
            return Err("cpu_service did not take processor 1 out of service");
        }
        if ask(token, 1, 0) != Ok(0) {
            return Err("cpu_service refused to leave processor 1 out of service as it was");
        }
        if ask(token, 1, 1) != Ok(0) || !in_service(1) {
            return Err("cpu_service did not bring processor 1 back into service");
        }
    }
    let mut records = [ferrix_audit::Record::EMPTY; 16];
    let read = crate::audit::read(crate::audit::Which::High, mark, &mut records);
    let kept: Vec<&ferrix_audit::Record> = records
        .get(..read.copied)
        .unwrap_or_default()
        .iter()
        .filter(|record| record.is(CPU_SERVICE))
        .collect();
    let lode = cpu_service::LODE;
    let shaped = |record: &ferrix_audit::Record, outcome: Outcome, status: i16, cpu, way| {
        record.outcome == outcome as u16
            && record.status == status
            && record.target_kind == target::CPU
            && record.target() == cpu
            && record.detail == [way, lode, 0]
            && record.pid == side.process.pid()
    };
    let as_said = match kept.as_slice() {
        [refusal, held] if stayed => {
            shaped(refusal, Outcome::Refused, 4, 0, cpu_service::OUT)
                && shaped(held, Outcome::Refused, 3, 1, cpu_service::OUT)
        }
        [refusal, out, back] if !stayed => {
            shaped(refusal, Outcome::Refused, 4, 0, cpu_service::OUT)
                && shaped(out, Outcome::Done, 0, 1, cpu_service::OUT)
                && shaped(back, Outcome::Done, 0, 1, cpu_service::BACK)
        }
        _ => false,
    };
    if !as_said {
        return Err(
            "cpu_service's audit records were not the refusals and changes made, each as made",
        );
    }
    if stayed {
        // Nothing to park: the undo needs a processor that can go.
        report.refusals += 1;
        return Ok(());
    }
    the_undo_and_the_pinned_refusal(report)
}

/// The deadline's undo and the pinned refusal, on processor 1, which can
/// go out of service.
fn the_undo_and_the_pinned_refusal(report: &mut Report) -> Result<(), &'static str> {
    use crate::sched::{self, ServiceRefusal};

    let in_service = |cpu: usize| {
        crate::smp::topology()
            .and_then(|topology| topology.cpus().get(cpu))
            .is_some_and(crate::smp::PerCpu::in_service)
    };
    // The deadline's undo: asked of nobody, with its deadline already past,
    // the park is cancelled by compare-exchange and the processor put back.
    // A processor something else woke in the microseconds between parks
    // first: it is brought back and the undo tried again.
    let mut undone = false;
    for _ in 0..5 {
        match sched::take_out_without_asking(1, crate::timer::now_nanos()) {
            Err(ServiceRefusal::TimedOut) => {
                undone = in_service(1);
                break;
            }
            Ok(()) => {
                let back = crate::smp::service::set_in_service(
                    1,
                    true,
                    crate::smp::service::Door::Lode,
                    crate::audit::Subject::KERNEL,
                );
                if back.is_err() {
                    return Err("a processor parked by the undo's check did not come back");
                }
            }
            Err(_) => break,
        }
    }
    if !undone {
        return Err("a park past its deadline was not undone, the processor left in service");
    }
    if sched::refusal_while_pinned(1) != Err(ServiceRefusal::Pinned) {
        return Err("a processor with a program pinned to it alone was not refused");
    }
    report.refusals += 2;
    Ok(())
}

/// The memory scrub token (`docs/RAM-SCRUBBER.md` §4): pid 1's rights pinned,
/// `MANAGE`, `DUPLICATE` and `TRANSFER`; `memory_scrub` refusing a budget of 0
/// and of 65, a handle without `MANAGE` and another object's handle; and a
/// batch of one answered `WRONG_TYPE` where the scrubber stands down, or with
/// at most one frame tested and the answer adding up where it runs (the
/// gate's override).
///
/// Verifies: L.object.252, L.syscall.24, L.mm.108
fn memory_is_scrubbed_through_its_token_alone(
    side: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    if crate::init::MEMORY_SCRUB_RIGHTS != Rights::MANAGE | Rights::DUPLICATE | Rights::TRANSFER {
        return Err("pid 1's memory scrub token is not MANAGE, DUPLICATE and TRANSFER");
    }
    let core = side.process.core();
    let token = native::insert_new(core, Object::MemoryScrub, crate::init::MEMORY_SCRUB_RIGHTS)
        .ok()
        .and_then(|value| u32::try_from(value).ok())
        .map(Handle)
        .ok_or("could not give the check a memory scrub token")?;
    let waiter = native::insert_new(core, Object::MemoryScrub, Rights::WAIT)
        .ok()
        .and_then(|value| u32::try_from(value).ok())
        .map(Handle)
        .ok_or("could not give the check a second memory scrub token")?;
    let ask =
        |handle: Handle, budget: u64| side.call(nr::MEMORY_SCRUB, &[reg(handle), budget, SCRATCH]);
    refused(
        ask(token, 0),
        status::INVALID_ARGS,
        "memory_scrub with a budget of 0 was answered",
        report,
    )?;
    refused(
        ask(token, 65),
        status::INVALID_ARGS,
        "memory_scrub with a budget past 64 was answered",
        report,
    )?;
    refused(
        ask(waiter, 1),
        status::ACCESS_DENIED,
        "memory_scrub through a handle without MANAGE was answered",
        report,
    )?;
    let (end, _other) = channel(side)?;
    refused(
        ask(end, 1),
        status::WRONG_TYPE,
        "memory_scrub through a channel end was answered",
        report,
    )?;
    match ask(token, 1) {
        Err(status::WRONG_TYPE) if !crate::mm::scrub::standing().reason().starts_with("runs") => {
            report.refusals += 1;
            Ok(())
        }
        Ok(0) => {
            let words = side.get(SCRATCH, types::MEMORY_SCRUB_ANSWER_WORDS * 8)?;
            let word = |at: usize| {
                words
                    .get(at * 8..at * 8 + 8)
                    .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
                    .map_or(u64::MAX, u64::from_ne_bytes)
            };
            let (tested, suspects, bad, retired, held) =
                (word(0), word(1), word(2), word(3), word(4));
            if tested > 1 || suspects + bad > tested || retired + held > bad {
                return Err("memory_scrub's answer did not add up");
            }
            Ok(())
        }
        _ => Err("memory_scrub answered neither a batch nor the scrubber's stand-down"),
    }
}
