//! Stage 9's check of exception channels, so far: attaching and detaching
//! (`docs/microkernel/K4.md` §5, cases g, h and j).
//!
//! Every call goes through the native dispatch from a check process, as a
//! program makes it, so the decoding, the rights and the refusals are those
//! a program meets. The faults a channel hears come with K4's second
//! landing, and their cases with it.

use alloc::sync::Arc;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_elf::Class;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types::EXCEPTION_TIMEOUT_MAX_MS;
use ferrix_vma::VmaFlags;

use crate::arch;
use crate::object::check::{SCRATCH, Side};
use crate::object::job::Job;
use crate::object::quota::Resource;
use crate::object::{self, Object};
use crate::sched;
use crate::syscall::image;

/// The name a made process is given.
const NAME_AT: u64 = SCRATCH + 0x800;
/// A VMO offset, for `vmo_write`.
const OFFSET_AT: u64 = SCRATCH + 0x810;
/// A wait's deadline.
const DEADLINE_AT: u64 = SCRATCH + 0x818;
/// The signals a wait observed.
const OBSERVED_AT: u64 = SCRATCH + 0x820;
/// A byte to write, for the write the read end must refuse.
const BYTE_AT: u64 = SCRATCH + 0x828;
/// Where the image of a made process is staged: clear of the scratch region.
const IMAGE_AT: u64 = 0x6000_0000;
/// The made process's name.
const NAME: &[u8] = b"k4-supervised";
/// The timeout every attach here asks for, in milliseconds.
const TIMEOUT_MS: u64 = 1_000;
/// How long the check waits for a process's end to reach its supervisor.
const PATIENCE_NANOS: u64 = 10_000_000_000;

/// What the check measured, for the boot log.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Channels attached.
    pub(crate) attached: u32,
    /// Channels detached, by closing their read end or by their process's
    /// end.
    pub(crate) detached: u32,
    /// Calls refused with exactly the status they had to be refused with.
    pub(crate) refusals: u32,
    /// Objects one attach charged to the caller's job.
    pub(crate) charged: u64,
}

/// Run cases g, h and j.
///
/// Verifies: L.object.241, L.object.242, L.object.246
///
/// # Errors
///
/// The first case that did not hold.
pub(crate) fn run() -> Result<Report, &'static str> {
    let mut report = Report::default();
    let side = Side::new()?;
    let root = Job::new_root().map_err(|_| "no memory for the check's job")?;
    // The root job as the kernel hands one out: `Rights::JOB`, never
    // `SUPERVISE` (L.object.246).
    let root_handle = side
        .process
        .with_handles(|table| table.insert(Object::Job(Arc::clone(&root)), Rights::JOB))
        .map_err(|_| "no room for the check's job")?;
    let job = side.handle(nr::JOB_CREATE, &[reg(root_handle), 0], "job_create failed")?;
    let job_object = job_of(&side, job)?;
    if rights_of(&side, job)? != Rights::JOB_CREATED {
        return Err("job_create did not answer JOB_CREATED");
    }

    attach_and_detach_a_job(&side, job, &job_object, &mut report)?;
    let vmo = image_vmo(&side)?;
    let process = side.handle(
        nr::PROCESS_CREATE,
        &[reg(job), reg(vmo), NAME_AT, NAME.len() as u64],
        "process_create failed",
    )?;
    if rights_of(&side, process)? != Rights::PROCESS_CREATED {
        return Err("process_create did not answer PROCESS_CREATED");
    }
    refusals(&side, root_handle, process, vmo, &mut report)?;
    a_process_attached_hears_its_end(&side, job, process, &mut report)?;
    charging(&side, &root, &mut report)?;

    drop(side);
    object::dispose([Object::Job(root)]);
    Ok(report)
}

/// Case g on a job: one channel attached, a second `ALREADY_BOUND`,
/// closing the read end detaches, and a new attach is taken.
fn attach_and_detach_a_job(
    side: &Side,
    job: Handle,
    job_object: &Arc<Job>,
    report: &mut Report,
) -> Result<(), &'static str> {
    let first = attach(side, job, TIMEOUT_MS)
        .map_err(|_| "an exception channel was not attached to a job made with job_create")?;
    report.attached += 1;
    if !job_object.supervisor().is_live() {
        return Err("an attached channel is not live in its job's slot");
    }
    if rights_of(side, first)? != Rights::EXCEPTION_CHANNEL {
        return Err("an exception channel's read end did not carry EXCEPTION_CHANNEL");
    }
    expect(
        attach(side, job, TIMEOUT_MS),
        status::ALREADY_BOUND,
        "a second exception channel on one job was not ALREADY_BOUND",
        report,
    )?;
    // The read end writes nothing: the kernel holds the only writing end.
    side.put(BYTE_AT, &[0x4B])?;
    expect(
        side.call(nr::CHANNEL_WRITE, &[reg(first), BYTE_AT, 1, 0, 0]),
        status::ACCESS_DENIED,
        "an exception channel's read end was written",
        report,
    )?;
    close(side, first)?;
    if job_object.supervisor().is_live() {
        return Err("a channel whose read end closed is still live");
    }
    report.detached += 1;
    let second = attach(side, job, TIMEOUT_MS)
        .map_err(|_| "an attach after the read end closed was refused")?;
    report.attached += 1;
    close(side, second)?;
    report.detached += 1;
    Ok(())
}

/// Case h: every refusal, each leaving nothing attached.
fn refusals(
    side: &Side,
    root: Handle,
    process: Handle,
    vmo: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let narrowed = side.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(process), u64::from(Rights::PROCESS.0)],
        "duplicating a process handle without SUPERVISE failed",
    )?;
    expect(
        attach(side, narrowed, TIMEOUT_MS),
        status::ACCESS_DENIED,
        "a process handle without SUPERVISE attached a supervisor",
        report,
    )?;
    close(side, narrowed)?;
    expect(
        attach(side, root, TIMEOUT_MS),
        status::ACCESS_DENIED,
        "a job handle as the kernel hands one out (Rights::JOB) attached a supervisor",
        report,
    )?;
    for (timeout, what) in [
        (0, "a timeout of 0 was taken"),
        (
            EXCEPTION_TIMEOUT_MAX_MS + 1,
            "a timeout past the maximum was taken",
        ),
    ] {
        expect(
            attach(side, process, timeout),
            status::INVALID_ARGS,
            what,
            report,
        )?;
    }
    expect(
        side.call(nr::EXCEPTION_CHANNEL_CREATE, &[reg(process), TIMEOUT_MS, 1]),
        status::INVALID_ARGS,
        "an option bit was taken",
        report,
    )?;
    expect(
        attach(side, vmo, TIMEOUT_MS),
        status::WRONG_TYPE,
        "a VMO was taken as a process or a job",
        report,
    )?;
    expect(
        side.call(nr::EXCEPTION_CHANNEL_CREATE, &[0x7FFF_FFFF, TIMEOUT_MS, 0]),
        status::BAD_HANDLE,
        "a handle that names nothing was taken",
        report,
    )?;
    Ok(())
}

/// Case g on a process, unstarted: attached, a second `ALREADY_BOUND`; its
/// end, by a kill of its job, closes the kernel's end, so its supervisor
/// sees `PEER_CLOSED`, and an attach after it is `BAD_STATE`.
fn a_process_attached_hears_its_end(
    side: &Side,
    job: Handle,
    process: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let channel = attach(side, process, TIMEOUT_MS)
        .map_err(|_| "an exception channel was not attached to an unstarted process")?;
    report.attached += 1;
    expect(
        attach(side, process, TIMEOUT_MS),
        status::ALREADY_BOUND,
        "a second exception channel on one process was not ALREADY_BOUND",
        report,
    )?;
    let _ = side
        .call(nr::JOB_KILL, &[reg(job)])
        .map_err(|_| "job_kill failed")?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    side.put(DEADLINE_AT, &deadline.to_ne_bytes())?;
    let _ = side
        .call(
            nr::OBJECT_WAIT_ONE,
            &[
                reg(channel),
                u64::from(Signals::PEER_CLOSED.0),
                DEADLINE_AT,
                OBSERVED_AT,
            ],
        )
        .map_err(|_| "a supervisor never saw its process's end as PEER_CLOSED")?;
    report.detached += 1;
    expect(
        attach(side, process, TIMEOUT_MS),
        status::BAD_STATE,
        "an exception channel was attached to a process that has ended",
        report,
    )?;
    close(side, channel)?;
    Ok(())
}

/// Case j: an attach charges the channel pair to the caller's job, as
/// `channel_create` does, and is refused with `NO_MEMORY` at its object
/// limit, attaching nothing.
fn charging(side: &Side, tree: &Arc<Job>, report: &mut Report) -> Result<(), &'static str> {
    let job = tree
        .new_child()
        .map_err(|_| "a job refused a child for the charging case")?;
    let handle = side
        .process
        .with_handles(|table| table.insert(Object::Job(Arc::clone(&job)), Rights::JOB_CREATED))
        .map_err(|_| "no room for the charging case's job")?;
    let objects = |job: &Job| job.usage(Resource::Objects).map_or(0, |usage| usage.used);
    let before = objects(&job);
    let channel = as_task_of(&job, || attach(side, handle, TIMEOUT_MS))
        .map_err(|_| "an attach in a job with room was refused")?;
    report.attached += 1;
    let charged = objects(&job).saturating_sub(before);
    if charged != 2 {
        return Err("an attach did not charge its channel's two ends to the caller's job");
    }
    report.charged = charged;
    close(side, channel)?;
    report.detached += 1;
    let _ = job.set_limit(Resource::Objects, objects(&job));
    let refused = as_task_of(&job, || attach(side, handle, TIMEOUT_MS));
    expect(
        refused,
        status::NO_MEMORY,
        "an attach past the job's object limit was not NO_MEMORY",
        report,
    )?;
    if job.supervisor().is_live() {
        return Err("an attach refused for memory left a channel attached");
    }
    close(side, handle)?;
    Ok(())
}

/// `exception_channel_create(task, timeout_ms, 0)`.
fn attach(side: &Side, task: Handle, timeout_ms: u64) -> Result<Handle, Errno> {
    let value = side.call(nr::EXCEPTION_CHANNEL_CREATE, &[reg(task), timeout_ms, 0])?;
    u32::try_from(value)
        .map(Handle)
        .map_err(|_| status::BAD_STATE)
}

/// A call that had to be refused with `refusal`, counted when it was.
fn expect<T>(
    outcome: Result<T, Errno>,
    refusal: Errno,
    what: &'static str,
    report: &mut Report,
) -> Result<(), &'static str> {
    match outcome {
        Err(errno) if errno == refusal => {
            report.refusals += 1;
            Ok(())
        }
        _ => Err(what),
    }
}

/// Run `work` charged to `job`, as a task of it would be.
fn as_task_of<T>(job: &Job, work: impl FnOnce() -> T) -> T {
    let own = sched::running_group();
    sched::set_current_group(job.quota_index());
    let done = work();
    sched::set_current_group(own);
    done
}

/// `handle_close`.
fn close(side: &Side, handle: Handle) -> Result<(), &'static str> {
    side.call(nr::HANDLE_CLOSE, &[reg(handle)])
        .map(|_| ())
        .map_err(|_| "handle_close failed")
}

/// The rights `handle` carries in `side`'s table.
fn rights_of(side: &Side, handle: Handle) -> Result<Rights, &'static str> {
    side.process
        .with_handles(|table| table.get(handle).map(|(_, rights)| rights))
        .map_err(|_| "a handle the check made is not in its table")
}

/// The job `handle` names in `side`'s table.
fn job_of(side: &Side, handle: Handle) -> Result<Arc<Job>, &'static str> {
    side.process.with_handles(|table| match table.get(handle) {
        Ok((Object::Job(job), _)) => Ok(Arc::clone(job)),
        _ => Err("a job handle the check made names no job"),
    })
}

/// A VMO holding a program's image, and the name to make it with, staged.
fn image_vmo(side: &Side) -> Result<Handle, &'static str> {
    let class = if size_of::<usize>() == 8 {
        Class::Elf64
    } else {
        Class::Elf32
    };
    let file = image::build_with(
        class,
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_ARGUMENT_PROGRAM,
    );
    let len = file.len() as u64;
    let _ = side
        .process
        .space()
        .map_anonymous(
            IMAGE_AT,
            len.div_ceil(PAGE_SIZE) * PAGE_SIZE,
            VmaFlags::READ_WRITE,
        )
        .map_err(|_| "could not map room for an image")?;
    side.put(IMAGE_AT, &file)?;
    let vmo = side.handle(nr::VMO_CREATE, &[len], "vmo_create for an image failed")?;
    side.put(OFFSET_AT, &0_u64.to_ne_bytes())?;
    let _ = side
        .call(nr::VMO_WRITE, &[reg(vmo), IMAGE_AT, len, OFFSET_AT])
        .map_err(|_| "writing an image into a VMO failed")?;
    side.put(NAME_AT, NAME)?;
    Ok(vmo)
}

/// A handle as a register.
fn reg(handle: Handle) -> u64 {
    u64::from(handle.0)
}
