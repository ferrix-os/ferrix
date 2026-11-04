//! Stage 9's check of criticality levels (`docs/microkernel/K17.md` §5): native
//! calls from check processes, as a program makes them, between jobs at levels 4,
//! 2 and 0.

use alloc::sync::Arc;

use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::{Rights, SAME_RIGHTS};
use ferrix_native_abi::status;
use ferrix_native_abi::types::{JOB_LEVEL_SET, JOB_LEVEL_SHIFT, MAP_READ, MAP_WRITE};

use crate::audit;
use crate::object::Object;
use crate::object::check::{SCRATCH, Side};
use crate::object::job::Job;
use crate::object::process as core_process;

/// Offset words for the VMO calls.
const OFFSET_AT: u64 = SCRATCH + 0x100;
/// Two ends of a channel.
const ENDS_AT: u64 = SCRATCH + 0x110;
/// A message's byte.
const BYTE_AT: u64 = SCRATCH + 0x120;
/// A message's handles.
const HANDLES_AT: u64 = SCRATCH + 0x130;
/// A read's record.
const ACTUAL_AT: u64 = SCRATCH + 0x150;

/// What the check measured, for the boot log.
#[derive(Debug, Default)]
pub struct Report {
    /// Refusals with the status and the audit record they had to have.
    pub refusals: u32,
    /// Handles delivered as `INVALID`.
    pub dropped: u32,
}

/// A handle as a register.
const fn reg(handle: Handle) -> u64 {
    handle.0 as u64
}

/// `job_create`'s options for `level`.
const fn at_level(level: u8) -> u64 {
    JOB_LEVEL_SET | ((level as u64) << JOB_LEVEL_SHIFT)
}

/// The sequence of the last level refusal in the audit record: it moves when one
/// is made.
fn level_refusals() -> u64 {
    let mut records = [audit::Record::EMPTY; 16];
    let mut from = 0;
    let mut last = 0;
    loop {
        let read = audit::read(audit::Which::Refusals, from, &mut records);
        if read.copied == 0 {
            return last;
        }
        for record in records.iter().take(read.copied) {
            if record.is(audit::LEVEL) {
                last = last.max(record.sequence + 1);
            }
        }
        from = read.next;
    }
}

/// Run the cases.
///
/// Verifies: H.OBJ.34, L.object.283, L.object.284, L.object.286, L.object.287
/// Verifies: L.object.288, L.object.289, L.user.185, L.user.186, L.user.187
/// Verifies: L.user.188, L.object.285
///
/// # Errors
///
/// The first case that did not hold.
pub fn run() -> Result<Report, &'static str> {
    let mut report = Report::default();
    let high = Side::new()?;
    let root = Job::new_root().map_err(|_| "no memory for the check's job")?;
    let root_handle = high
        .process
        .with_handles(|table| table.insert(Object::Job(Arc::clone(&root)), Rights::JOB_CREATED))
        .map_err(|_| "no room for the check's job")?;

    // Rule (e): at most its parent's; the option absent, its parent's.
    let level_2 = high.handle(
        nr::JOB_CREATE,
        &[reg(root_handle), at_level(2)],
        "job_create at level 2 failed",
    )?;
    let level_2_job = job_of(&high, level_2)?;
    if level_2_job.level() != 2 || !crate::object::job::level_declared(&level_2_job) {
        return Err("a job asked for level 2 is not at it, declared");
    }
    let before = level_refusals();
    if high.call(nr::JOB_CREATE, &[reg(level_2), at_level(3)]) != Err(status::ACCESS_DENIED) {
        return Err("a child above its parent's level was not refused");
    }
    if level_refusals() <= before {
        return Err("a child above its parent's level was not audited");
    }
    report.refusals += 1;
    let inherits = high.handle(
        nr::JOB_CREATE,
        &[reg(level_2), 0],
        "job_create without a level failed",
    )?;
    if job_of(&high, inherits)?.level() != 2 {
        return Err("a job with no level asked did not take its parent's");
    }
    if high.call(
        nr::JOB_CREATE,
        &[reg(level_2), JOB_LEVEL_SET | (7 << JOB_LEVEL_SHIFT)],
    ) != Err(status::INVALID_ARGS)
    {
        return Err("a level past the top was not refused");
    }
    let level_0 = high.handle(
        nr::JOB_CREATE,
        &[reg(level_2), at_level(0)],
        "job_create at level 0 failed",
    )?;
    let level_0_job = job_of(&high, level_0)?;

    // Two processes, in levels 2 and 0.
    let two = Side::new_in(&level_2_job)?;
    let zero = Side::new_in(&level_0_job)?;
    moves_across_levels(&zero, &level_2_job, &mut report)?;
    handle_door(&two, &zero, &level_2_job, &mut report)?;
    let level_1 = high.handle(
        nr::JOB_CREATE,
        &[reg(level_2), at_level(1)],
        "job_create at level 1 failed",
    )?;
    let one = Side::new_in(&job_of(&high, level_1)?)?;
    vmo_rules(&two, &one, &zero, &mut report)?;
    map_at_the_space_level(&high, &two, &zero, &mut report)?;
    region_is_one_level(&two, &zero, &level_2_job, &level_0_job, &mut report)?;
    Ok(report)
}

/// The job a handle in `side`'s table names.
fn job_of(side: &Side, handle: Handle) -> Result<Arc<Job>, &'static str> {
    side.process.with_handles(|table| match table.get(handle) {
        Ok((Object::Job(job), _)) => Ok(Arc::clone(job)),
        _ => Err("a job's handle names no job"),
    })
}

/// Rule: a move across levels is refused (`L.object.288`), and with it a process
/// never joins a speculation domain of another level, a domain being one job
/// (rule f, `L.object.285`).
fn moves_across_levels(
    zero: &Side,
    higher: &Arc<Job>,
    report: &mut Report,
) -> Result<(), &'static str> {
    if zero.process.move_to(higher).is_ok() {
        return Err("a process moved between levels");
    }
    report.refusals += 1;
    Ok(())
}

/// Send `handle` from `two` to `zero` on a fresh channel, and answer what `zero`
/// finds in its slot.
pub(super) fn sent_to_zero(two: &Side, zero: &Side, handle: Handle) -> Result<u32, &'static str> {
    let _ = two
        .call(nr::CHANNEL_CREATE, &[ENDS_AT])
        .map_err(|_| "channel_create failed")?;
    let ours = Handle(two.get_u32(ENDS_AT)?);
    let theirs = Handle(two.get_u32(ENDS_AT + 4)?);
    two.put(BYTE_AT, &[7])?;
    two.put(HANDLES_AT, &handle.0.to_ne_bytes())?;
    let _ = two
        .call(nr::CHANNEL_WRITE, &[reg(ours), BYTE_AT, 1, HANDLES_AT, 1])
        .map_err(|_| "a write of a handle failed")?;
    let end = two
        .process
        .with_handles(|table| table.remove(theirs))
        .map_err(|_| "no end to pass")?;
    let there = zero
        .process
        .with_handles(|table| table.insert(end.0, end.1))
        .map_err(|_| "no room for the end")?;
    let _ = zero
        .call(
            nr::CHANNEL_READ,
            &[reg(there), BYTE_AT, 1, HANDLES_AT, 1, ACTUAL_AT],
        )
        .map_err(|_| "a read of a refused handle failed: a refusal must not jam a channel")?;
    if zero.get(BYTE_AT, 1)?.first() != Some(&7) {
        return Err("a message with a refused handle lost its bytes");
    }
    zero.get_u32(HANDLES_AT)
}

/// Rules (a), (c), (g): a level-0 reader of a level-2 job's and process's handle
/// gets `INVALID` and the bytes, audited; a port stays allowed (`L.object.283`,
/// `L.object.286`, `L.object.289`).
fn handle_door(
    two: &Side,
    zero: &Side,
    level_2: &Arc<Job>,
    report: &mut Report,
) -> Result<(), &'static str> {
    let job = two
        .process
        .with_handles(|table| table.insert(Object::Job(Arc::clone(level_2)), Rights::JOB))
        .map_err(|_| "no room for a job")?;
    let process = two
        .process
        .with_handles(|table| {
            table.insert(
                Object::Process(core_process::ProcessRef::new(&two.process)),
                Rights::PROCESS,
            )
        })
        .map_err(|_| "no room for a process handle")?;
    let space = two.handle(nr::SPACE_CREATE, &[], "space_create failed")?;
    let thread = crate::object::bare::thread_object_at(level_2.level())
        .ok_or("no memory for a thread's handle")?;
    let thread = two
        .process
        .with_handles(|table| table.insert(Object::Thread(thread), Rights::THREAD))
        .map_err(|_| "no room for a thread's handle")?;
    let port = two.handle(nr::PORT_CREATE, &[], "port_create failed")?;
    two.put(OFFSET_AT, &1_000_000_000_u64.to_ne_bytes())?;
    let pager = two.handle(
        nr::PAGER_CREATE,
        &[reg(port), OFFSET_AT, 0],
        "pager_create failed",
    )?;
    // A level-2 pager's keeper stays with level 2: taken up, it would put that pager in a
    // lower job's own table (ledger 6575 D1).
    let keeper = two.handle(nr::PAGER_KEEP, &[reg(pager)], "pager_keep failed")?;
    let before = level_refusals();
    if sent_to_zero(two, zero, keeper)? != Handle::INVALID.0 {
        return Err("a higher level's pager keeper reached a lower level's table");
    }
    if level_refusals() <= before {
        return Err("a refused keeper was not audited");
    }
    report.dropped += 1;
    let mut held = [job, process, space, thread, pager, Handle::INVALID];
    let mut count = 5;
    if crate::arch::HAS_RESTRICTED {
        let its_job = two
            .process
            .with_handles(|table| {
                table.insert(Object::Job(Arc::clone(level_2)), Rights::JOB_CREATED)
            })
            .map_err(|_| "no room for a job")?;
        if let Some(slot) = held.get_mut(count) {
            *slot = two.handle(
                nr::REGION_CREATE,
                &[reg(its_job), ACTUAL_AT],
                "region_create failed",
            )?;
            count += 1;
        }
    }
    for &handle in held.iter().take(count) {
        let before = level_refusals();
        if sent_to_zero(two, zero, handle)? != Handle::INVALID.0 {
            return Err("a higher level's handle reached a lower level's table");
        }
        if level_refusals() <= before {
            return Err("a refused handle was not audited");
        }
        report.dropped += 1;
    }
    let port = two.handle(nr::PORT_CREATE, &[], "port_create failed")?;
    if sent_to_zero(two, zero, port)? == Handle::INVALID.0 {
        return Err("a handle with no level was refused");
    }
    Ok(())
}

/// A VMO of one page in `side`'s table.
fn vmo(side: &Side) -> Result<Handle, &'static str> {
    side.handle(nr::VMO_CREATE, &[4096], "vmo_create failed")
}

/// Map `vmo` as `protection` from `side`.
fn map(side: &Side, vmo: Handle, protection: u32) -> Result<usize, ferrix_linux_abi::errno::Errno> {
    side.put(OFFSET_AT, &0_u64.to_ne_bytes())
        .map_err(|_| status::FAULT)?;
    side.call(
        nr::VMO_MAP,
        &[reg(vmo), 0, 4096, u64::from(protection), OFFSET_AT],
    )
}

/// Rules (b), (d) and the declaration (`L.user.185` to `L.user.189`).
fn vmo_rules(two: &Side, one: &Side, zero: &Side, report: &mut Report) -> Result<(), &'static str> {
    // The row's negative control: level 2 maps its VMO; a write right to it does
    // not land at level 0, and a read right does.
    let private = vmo(two)?;
    let _ =
        map(two, private, MAP_READ | MAP_WRITE).map_err(|_| "a job could not map its own VMO")?;
    let read_only = two.handle(
        nr::HANDLE_DUPLICATE,
        &[
            reg(private),
            u64::from(Rights::READ.0 | Rights::MAP.0 | Rights::TRANSFER.0),
        ],
        "could not reduce a VMO handle",
    )?;
    let writable = two.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(private), u64::from(SAME_RIGHTS)],
        "could not duplicate a VMO handle",
    )?;
    if sent_to_zero(two, zero, writable)? != Handle::INVALID.0 {
        return Err("a lower level was given a write right to a VMO a higher level maps");
    }
    if sent_to_zero(two, zero, read_only)? == Handle::INVALID.0 {
        return Err("a lower level was refused a read right to a VMO");
    }
    report.dropped += 1;
    // The other order: level 0 made it, so holds a write right; level 2's map
    // of it is refused.
    let lower = vmo(zero)?;
    let moved = zero.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(lower), u64::from(SAME_RIGHTS)],
        "could not duplicate a VMO handle",
    )?;
    let (object, rights) = zero
        .process
        .with_handles(|table| table.remove(moved))
        .map_err(|_| "no VMO to move")?;
    let above = two
        .process
        .with_handles(|table| table.insert(object, rights))
        .map_err(|_| "no room for a VMO")?;
    if map(two, above, MAP_READ) != Err(status::ACCESS_DENIED) {
        return Err("a higher level mapped a VMO a lower level may write");
    }
    report.refusals += 1;
    // Declared for level 2, it maps there; the declaration needs `SET_LIMIT`, and
    // is once.
    let job_of_two = two
        .process
        .with_handles(|table| {
            table.insert(
                Object::Job(two.process.job()),
                Rights(Rights::JOB.0 & !Rights::SET_LIMIT.0),
            )
        })
        .map_err(|_| "no room for a job")?;
    if two.call(nr::JOB_DECLARE_BUFFER, &[reg(job_of_two), reg(above)])
        != Err(status::ACCESS_DENIED)
    {
        return Err("a declaration without SET_LIMIT was not refused");
    }
    let manager = two
        .process
        .with_handles(|table| {
            table.insert(
                Object::Job(two.process.job()),
                Rights(Rights::JOB.0 | Rights::SET_LIMIT.0),
            )
        })
        .map_err(|_| "no room for a job")?;
    let declared = vmo(zero)?;
    let moved = zero.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(declared), u64::from(SAME_RIGHTS)],
        "could not duplicate a VMO handle",
    )?;
    let (object, rights) = zero
        .process
        .with_handles(|table| table.remove(moved))
        .map_err(|_| "no VMO to move")?;
    let held = two
        .process
        .with_handles(|table| table.insert(object, rights))
        .map_err(|_| "no room for a VMO")?;
    let _ = two
        .call(nr::JOB_DECLARE_BUFFER, &[reg(manager), reg(held)])
        .map_err(|_| "a declaration with SET_LIMIT was refused")?;
    if two.call(nr::JOB_DECLARE_BUFFER, &[reg(manager), reg(held)]) != Err(status::BAD_STATE) {
        return Err("a VMO was declared twice");
    }
    let _ = map(two, held, MAP_READ)
        .map_err(|_| "a declared buffer could not be mapped at its level")?;
    report.refusals += 1;
    // The declaration is for its level, not for every level above the writer.
    let elsewhere = two.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(held), u64::from(SAME_RIGHTS)],
        "could not duplicate a VMO handle",
    )?;
    let elsewhere = carried(two, one, elsewhere)?;
    if map(one, elsewhere, MAP_READ) != Err(status::ACCESS_DENIED) {
        return Err("a declaration lifted a level it was not for");
    }
    report.refusals += 1;
    pager_rules(two, zero, manager, report)?;
    clone_keeps_its_level(two, zero, report)?;
    Ok(())
}

/// A clone keeps every mapping at the source's level, so a space of another is not
/// cloned (`L.user.186`).
fn clone_keeps_its_level(two: &Side, zero: &Side, report: &mut Report) -> Result<(), &'static str> {
    let source = zero.handle(nr::SPACE_CREATE, &[], "space_create failed")?;
    let source = carried(zero, two, source)?;
    let before = level_refusals();
    if two.call(nr::SPACE_CLONE, &[reg(source)]) != Err(status::ACCESS_DENIED) {
        return Err("a space of another level was cloned");
    }
    if level_refusals() <= before {
        return Err("a refused clone was not audited");
    }
    report.refusals += 1;
    Ok(())
}

/// Rule (d): a lower pager serves no higher map, declared or not, and is not declared
/// (`L.user.187`).
fn pager_rules(
    two: &Side,
    zero: &Side,
    manager: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let port = zero.handle(nr::PORT_CREATE, &[], "port_create failed")?;
    zero.put(OFFSET_AT, &1_000_000_000_u64.to_ne_bytes())?;
    let pager = zero.handle(
        nr::PAGER_CREATE,
        &[reg(port), OFFSET_AT, 0],
        "pager_create failed",
    )?;
    let mut spec = [0_u8; 24];
    for (place, word) in spec.chunks_exact_mut(8).zip([0x4b17_u64, 1, 0]) {
        place.copy_from_slice(&word.to_ne_bytes());
    }
    zero.put(OFFSET_AT, &spec)?;
    // Level 2 holds level 0's pager, so makes the VMO itself: the writer's mark says
    // level 2, and only the pager's says who fills it.
    let pager = carried(zero, two, pager)?;
    two.put(OFFSET_AT, &spec)?;
    let paged = two.handle(
        nr::PAGER_CREATE_VMO,
        &[reg(pager), OFFSET_AT],
        "pager_create_vmo failed",
    )?;
    if two.call(nr::JOB_DECLARE_BUFFER, &[reg(manager), reg(paged)]) != Err(status::BAD_STATE) {
        return Err("a pager's VMO was declared");
    }
    if map(two, paged, MAP_READ) != Err(status::ACCESS_DENIED) {
        return Err("a higher level mapped a VMO a lower level serves");
    }
    report.refusals += 1;
    Ok(())
}

/// Move `handle` from `from`'s table to `to`'s, past every door: what the case is
/// about comes after.
fn carried(from: &Side, to: &Side, handle: Handle) -> Result<Handle, &'static str> {
    let (object, rights) = from
        .process
        .with_handles(|table| table.remove(handle))
        .map_err(|_| "nothing to carry")?;
    to.process
        .with_handles(|table| table.insert(object, rights))
        .map_err(|_| "no room to carry a handle")
}

/// `space_map` goes by the level of the space it lands in, not the caller's: a
/// supervisor above a job builds that job's space (`L.user.186`).
fn map_at_the_space_level(
    high: &Side,
    two: &Side,
    zero: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    let space = two.handle(nr::SPACE_CREATE, &[], "space_create failed")?;
    let space = carried(two, high, space)?;
    high.put(OFFSET_AT, &0_u64.to_ne_bytes())?;
    let place = |vmo: Handle, at: u64| {
        high.call(
            nr::SPACE_MAP,
            &[
                reg(space),
                reg(vmo),
                at,
                4096,
                u64::from(MAP_READ),
                OFFSET_AT,
            ],
        )
    };
    let its_own = vmo(two)?;
    let its_own = carried(two, high, its_own)?;
    if place(its_own, 0x10_0000).is_err() {
        return Err("a map into a lower job's space took the caller's level");
    }
    let below = vmo(zero)?;
    let below = carried(zero, high, below)?;
    let before = level_refusals();
    if place(below, 0x20_0000) != Err(status::ACCESS_DENIED) {
        return Err("a map into a space above a VMO's writer was not refused");
    }
    if level_refusals() <= before {
        return Err("a refused map was not audited");
    }
    report.refusals += 1;
    Ok(())
}

/// A region's shared table crosses its processes' jobs, so a region is one level
/// (`L.object.288`): a shared process of it in a job of another level is refused,
/// though the job is inside the region's.
fn region_is_one_level(
    two: &Side,
    zero: &Side,
    level_2_job: &Arc<Job>,
    level_0_job: &Arc<Job>,
    report: &mut Report,
) -> Result<(), &'static str> {
    if !crate::arch::HAS_RESTRICTED {
        return Ok(());
    }
    let job_handle = |job: &Arc<Job>| {
        two.process
            .with_handles(|table| table.insert(Object::Job(Arc::clone(job)), Rights::JOB_CREATED))
            .map_err(|_| "no room for a job")
    };
    let its_job = job_handle(level_2_job)?;
    let lower = job_handle(level_0_job)?;
    let region = two.handle(
        nr::REGION_CREATE,
        &[reg(its_job), ACTUAL_AT],
        "region_create failed",
    )?;
    two.put(BYTE_AT, b"l")?;
    let before = level_refusals();
    if two.call(
        nr::PROCESS_CREATE_SHARED,
        &[reg(region), reg(lower), 0, BYTE_AT, 1, ACTUAL_AT],
    ) != Err(status::ACCESS_DENIED)
    {
        return Err("a region took a process of another level");
    }
    if level_refusals() <= before {
        return Err("a region's refused process was not audited");
    }
    report.refusals += 1;
    // And binds no space of another level.
    let space = zero.handle(nr::SPACE_CREATE, &[], "space_create failed")?;
    let space = carried(zero, two, space)?;
    let before = level_refusals();
    if two.call(
        nr::PROCESS_CREATE_SHARED,
        &[reg(region), reg(its_job), reg(space), BYTE_AT, 1, ACTUAL_AT],
    ) != Err(status::ACCESS_DENIED)
    {
        return Err("a shared process was bound to a space of another level");
    }
    if level_refusals() <= before {
        return Err("a refused bind was not audited");
    }
    report.refusals += 1;
    Ok(())
}
