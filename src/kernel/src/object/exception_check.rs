//! Stage 9's check of exception channels (`docs/microkernel/K4.md` §5):
//! attaching and detaching, and the faults a supervisor hears and decides.
//!
//! Every call goes through the native dispatch from a check process, as a
//! program makes it, so the decoding, the rights and the refusals are those
//! a program meets. The faults are real ones: hand-encoded programs, made
//! with `process_create` and started with `process_start`, that store
//! through a null pointer or take a breakpoint in user mode, since only an
//! instruction a program executes takes the trap path under test. The check
//! process is their supervisor.

use alloc::sync::Arc;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_elf::Class;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::exception::{
    FLAG_PC_AT_TRAP, FLAG_WRITE, HAND_ON, KILL, KIND_PAGE_FAULT, REPORT_BYTES, RESUME, Registers,
    Report as Report64, STATE_READ, STATE_WRITE, Words,
};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::{Rights, SAME_RIGHTS};
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types::EXCEPTION_TIMEOUT_MAX_MS;
use ferrix_vma::VmaFlags;

use crate::arch;
use crate::audit;
use crate::object::check::{SCRATCH, Side};
use crate::object::exception;
use crate::object::job::{self, Job};
use crate::object::process::ProcessRef;
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
/// How long the check waits for a report or a program's end: three times
/// the longest wait a fault can make (`EXCEPTION_TIMEOUT_MAX_MS`), so that a
/// sabotage that makes a case wait out its timeout is caught by that case's
/// own bound, not by this one (the consultant's condition 7).
const PATIENCE_NANOS: u64 = 30_000_000_000;
/// The generous upper bound on case e's default action after its 5 ms
/// timeout, for a loaded TCG host: well under the 10 s a lost level timeout
/// would take.
const EXPIRY_SLACK_NANOS: u64 = 5_000_000_000;

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
    /// Faults reported to the check and decided by it, or left to expire.
    pub(crate) faults: u32,
    /// Register sets a write refused, every one of them forged.
    pub(crate) forged: u32,
}

/// Run every case of §5 but o, which is cgroupfs's (`fs::cgroupfs`'s check).
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
    let vmo = image_vmo(&side, arch::USER_ARGUMENT_PROGRAM, IMAGE_AT)?;
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
    faults(&side, root_handle, &mut report)?;

    side.close_everything();
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

/// A VMO holding an image of `code`, staged at `at`, and the name to make it
/// with, staged.
fn image_vmo(side: &Side, code: &[u8], at: u64) -> Result<Handle, &'static str> {
    let class = if size_of::<usize>() == 8 {
        Class::Elf64
    } else {
        Class::Elf32
    };
    let file = image::build_with(class, arch::ARCH.elf_machine(), image::Shape::Good, code);
    let len = file.len() as u64;
    let _ = side
        .process
        .space()
        .map_anonymous(
            at,
            len.div_ceil(PAGE_SIZE) * PAGE_SIZE,
            VmaFlags::READ_WRITE,
        )
        .map_err(|_| "could not map room for an image")?;
    side.put(at, &file)?;
    let vmo = side.handle(nr::VMO_CREATE, &[len], "vmo_create for an image failed")?;
    side.put(OFFSET_AT, &0_u64.to_ne_bytes())?;
    let _ = side
        .call(nr::VMO_WRITE, &[reg(vmo), at, len, OFFSET_AT])
        .map_err(|_| "writing an image into a VMO failed")?;
    side.put(NAME_AT, NAME)?;
    Ok(vmo)
}

/// A handle as a register.
fn reg(handle: Handle) -> u64 {
    u64::from(handle.0)
}

/// Where a report's bytes are read to.
const REPORT_AT: u64 = SCRATCH + 0x900;
/// Where a report's one handle is read to.
const HANDLE_AT: u64 = SCRATCH + 0x980;
/// `channel_read`'s actual sizes.
const ACTUAL_AT: u64 = SCRATCH + 0x988;
/// `process_status`'s answer.
const STATUS_AT: u64 = SCRATCH + 0x998;
/// Registers read and written.
const REGISTERS_AT: u64 = SCRATCH + 0xA00;
/// Bytes in this architecture's register set.
const REGISTER_BYTES: usize = <Registers as Words>::BYTES;
/// The longest a case lets a channel wait, so that a case that must be
/// answered before the deadline has seconds to spare on a loaded host
/// (the consultant's condition 7).
const LONG_MS: u64 = EXCEPTION_TIMEOUT_MAX_MS;
/// The expiry case's timeout.
const SHORT_MS: u64 = 5;
/// `process_status`'s states.
const EXITED: u32 = ferrix_native_abi::types::PROCESS_EXITED;
/// See [`EXITED`].
const KILLED: u32 = ferrix_native_abi::types::PROCESS_KILLED;
/// `SIGSEGV`, the default action's signal for a store through null.
const SIGSEGV: u32 = ferrix_linux_abi::types::SIGSEGV;
/// `SIGKILL`, a job kill's.
const SIGKILL: u32 = 9;

/// The faulting cases, a to n but g, h's call refusals with them.
///
/// Verifies: H.TRAP.20, L.trap.9, L.trap.10, L.trap.11, L.trap.12, L.trap.13
/// Verifies: L.object.243, L.object.244, L.object.245
/// Verifies: L.x86_64.186, L.aarch64.66, L.armv7a.28
fn faults(side: &Side, root: Handle, report: &mut Report) -> Result<(), &'static str> {
    let fault = image_vmo(side, arch::USER_FAULT_PROGRAM, IMAGE_AT + 0x10_0000)?;
    let breakpoint = image_vmo(
        side,
        arch::exception_check::BREAKPOINT_PROGRAM,
        IMAGE_AT + 0x20_0000,
    )?;
    resume_with_written_registers(side, root, fault, report)?;
    kill_verdict(side, root, fault, report)?;
    hand_on(side, root, fault, report)?;
    expiry(side, root, fault, report)?;
    self_supervised(side, root, fault, report)?;
    closed_undecided(side, root, fault, report)?;
    breakpoint_case(side, root, breakpoint, report)?;
    supervisor_dies(side, root, fault, report)?;
    repeats(side, root, fault, report)?;
    unsupervised(side, root, fault)?;
    no_raise_under_a_supervisor(side, root, fault)?;
    raised_is_never_offered(side)?;
    close(side, fault)?;
    close(side, breakpoint)?;
    Ok(())
}

/// A fresh job inside `root`, from `job_create`, and a process made in it
/// from `image`, not started.
fn made(side: &Side, root: Handle, image: Handle) -> Result<(Handle, Handle), &'static str> {
    let job = side.handle(nr::JOB_CREATE, &[reg(root), 0], "job_create failed")?;
    side.put(NAME_AT, NAME)?;
    let process = side.handle(
        nr::PROCESS_CREATE,
        &[reg(job), reg(image), NAME_AT, NAME.len() as u64],
        "process_create failed",
    )?;
    Ok((job, process))
}

/// `process_start`, with `bootstrap` or none.
fn start(side: &Side, process: Handle, bootstrap: Option<Handle>) -> Result<(), &'static str> {
    side.call(nr::PROCESS_START, &[reg(process), bootstrap.map_or(0, reg)])
        .map(|_| ())
        .map_err(|_| "process_start failed")
}

/// Wait for any of `signals` on `handle`, for at most `patience` nanoseconds.
fn wait(side: &Side, handle: Handle, signals: Signals, patience: u64) -> Result<(), Errno> {
    let deadline = crate::timer::now_nanos().saturating_add(patience);
    side.put(DEADLINE_AT, &deadline.to_ne_bytes())
        .map_err(|_| status::FAULT)?;
    side.call(
        nr::OBJECT_WAIT_ONE,
        &[reg(handle), u64::from(signals.0), DEADLINE_AT, OBSERVED_AT],
    )
    .map(|_| ())
}

/// The next report on `channel`, and the exception it carries.
fn next_report(side: &Side, channel: Handle) -> Result<(Report64, Handle), &'static str> {
    wait(side, channel, Signals::READABLE, PATIENCE_NANOS)
        .map_err(|_| "no report reached a supervisor within the check's patience")?;
    let _ = side
        .call(
            nr::CHANNEL_READ,
            &[
                reg(channel),
                REPORT_AT,
                REPORT_BYTES as u64,
                HANDLE_AT,
                1,
                ACTUAL_AT,
            ],
        )
        .map_err(|_| "a report could not be read off its channel")?;
    let bytes = side.get(REPORT_AT, REPORT_BYTES)?;
    let report = Report64::from_bytes(&bytes).ok_or("a report was not 64 bytes")?;
    let exception = Handle(side.get_u32(HANDLE_AT)?);
    Ok((report, exception))
}

/// How `process` ended, once it has, within the check's patience.
fn ended(side: &Side, process: Handle) -> Result<(u32, u32), &'static str> {
    wait(side, process, Signals::TERMINATED, PATIENCE_NANOS)
        .map_err(|_| "a supervised program never ended")?;
    let _ = side
        .call(nr::PROCESS_STATUS, &[reg(process), STATUS_AT])
        .map_err(|_| "process_status failed")?;
    Ok((side.get_u32(STATUS_AT)?, side.get_u32(STATUS_AT + 4)?))
}

/// The exception's registers.
fn registers(side: &Side, exception: Handle) -> Result<Registers, Errno> {
    let _ = side.call(
        nr::EXCEPTION_STATE,
        &[
            reg(exception),
            STATE_READ,
            REGISTERS_AT,
            REGISTER_BYTES as u64,
        ],
    )?;
    let bytes = side
        .get(REGISTERS_AT, REGISTER_BYTES)
        .map_err(|_| status::FAULT)?;
    Registers::read(&bytes).ok_or(status::FAULT)
}

/// Write `registers` to the exception.
fn write(side: &Side, exception: Handle, registers: &Registers) -> Result<(), Errno> {
    let mut bytes = [0_u8; REGISTER_BYTES];
    registers.write(&mut bytes);
    side.put(REGISTERS_AT, &bytes).map_err(|_| status::FAULT)?;
    side.call(
        nr::EXCEPTION_STATE,
        &[
            reg(exception),
            STATE_WRITE,
            REGISTERS_AT,
            REGISTER_BYTES as u64,
        ],
    )
    .map(|_| ())
}

/// `exception_resolve`.
fn resolve(side: &Side, exception: Handle, verdict: u64) -> Result<(), Errno> {
    side.call(nr::EXCEPTION_RESOLVE, &[reg(exception), verdict])
        .map(|_| ())
}

/// The program counter of `USER_FAULT_PROGRAM`'s exit, given its fault's.
fn exit_of(fault_pc: u64) -> u64 {
    fault_pc - arch::exception_check::FAULT_AT + arch::exception_check::FAULT_EXIT_AT
}

/// Cases a, b and h's exception refusals: a store through null reported at
/// level 0 as a write page fault at 0, its pc at the store; the exception
/// carries `EXCEPTION` and cannot be duplicated; every forged register set
/// is refused and changes nothing, every set a program may hold is taken;
/// then the pc moved to the program's exit with status 42, resumed, the
/// program exits 42 with no signal, and one `EXCEPTION_CONTEXT` names both
/// pcs.
fn resume_with_written_registers(
    side: &Side,
    root: Handle,
    image: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let (job, process) = made(side, root, image)?;
    let channel = attach(side, process, LONG_MS).map_err(|_| "case a: attach failed")?;
    start(side, process, None)?;
    let (fault, exception) = next_report(side, channel)?;
    report.faults += 1;
    if fault.kind != KIND_PAGE_FAULT
        || fault.level != 0
        || fault.address != 0
        || fault.flags & FLAG_WRITE == 0
        || fault.flags & FLAG_PC_AT_TRAP == 0
        || fault.repeat != 0
        || fault.job != job_of(side, job)?.id()
    {
        return Err("case a: a store through null was not reported as one at level 0");
    }
    if rights_of(side, exception)? != Rights::EXCEPTION {
        return Err("case a: an exception's handle did not carry EXCEPTION");
    }
    expect(
        side.call(
            nr::HANDLE_DUPLICATE,
            &[reg(exception), u64::from(SAME_RIGHTS)],
        ),
        status::ACCESS_DENIED,
        "case h: an exception was duplicated",
        report,
    )?;
    let base = registers(side, exception).map_err(|_| "case a: exception_state's read failed")?;
    if arch::exception_check::pc(&base) != fault.pc {
        return Err("case a: the registers read do not hold the reported pc");
    }
    refuse_bad_calls(side, exception, channel, report)?;
    for (forged, _what) in arch::exception_check::forgeries(&base) {
        if write(side, exception, &forged) != Err(status::INVALID_ARGS) {
            crate::console::println!("  k4       a forged register set was taken: {_what}");
            return Err("case b: a forged register set was not refused");
        }
        if registers(side, exception) != Ok(base) {
            return Err("case b: a refused register set changed the registers");
        }
        report.forged += 1;
    }
    for (allowed, _what) in arch::exception_check::accepted(&base) {
        if write(side, exception, &allowed).is_err() {
            crate::console::println!(
                "  k4       a register set a program may hold was refused: {_what}"
            );
            return Err("case b: a register set a program may hold was refused");
        }
        if registers(side, exception) != Ok(allowed) {
            return Err("case b: a register set taken did not read back as written");
        }
    }
    let mut exit = base;
    arch::exception_check::exit_with(&mut exit, exit_of(fault.pc), 42);
    write(side, exception, &exit).map_err(|_| "case a: the exit's registers were refused")?;
    resolve(side, exception, RESUME).map_err(|_| "case a: resume failed")?;
    expect(
        resolve(side, exception, KILL),
        status::BAD_STATE,
        "case h: a second verdict was taken",
        report,
    )?;
    expect(
        registers(side, exception),
        status::BAD_STATE,
        "case h: a decided exception's registers were read",
        report,
    )?;
    if ended(side, process)? != (EXITED, 42) {
        return Err("case a: a program resumed at its exit with 42 did not exit 42");
    }
    let record = last_record(audit::EXCEPTION_CONTEXT)
        .ok_or("case a: a resume with written registers was not audited")?;
    if record.target() != u64::from(fault.pid)
        || record.detail[0] != fault.pc as u32
        || record.detail[1] != exit_of(fault.pc) as u32
    {
        return Err("case a: EXCEPTION_CONTEXT does not name the process and both pcs");
    }
    for handle in [exception, channel, process, job] {
        close(side, handle)?;
    }
    Ok(())
}

/// Case h's refusals of the two calls on an exception: an unknown op, a
/// wrong length, an unknown verdict, and a handle that is not an exception.
fn refuse_bad_calls(
    side: &Side,
    exception: Handle,
    channel: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    expect(
        side.call(
            nr::EXCEPTION_STATE,
            &[reg(exception), 2, REGISTERS_AT, REGISTER_BYTES as u64],
        ),
        status::INVALID_ARGS,
        "case h: an unknown exception_state op was taken",
        report,
    )?;
    expect(
        side.call(
            nr::EXCEPTION_STATE,
            &[
                reg(exception),
                STATE_READ,
                REGISTERS_AT,
                REGISTER_BYTES as u64 - 1,
            ],
        ),
        status::INVALID_ARGS,
        "case h: a register buffer of the wrong length was taken",
        report,
    )?;
    expect(
        resolve(side, exception, 3),
        status::INVALID_ARGS,
        "case h: an unknown verdict was taken",
        report,
    )?;
    expect(
        resolve(side, channel, RESUME),
        status::WRONG_TYPE,
        "case h: a channel was resolved as an exception",
        report,
    )?;
    Ok(())
}

/// Case c: `KILL` ends the process as a job kill does, audited.
fn kill_verdict(
    side: &Side,
    root: Handle,
    image: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let (job, process) = made(side, root, image)?;
    let channel = attach(side, process, LONG_MS).map_err(|_| "case c: attach failed")?;
    start(side, process, None)?;
    let (fault, exception) = next_report(side, channel)?;
    report.faults += 1;
    resolve(side, exception, KILL).map_err(|_| "case c: kill failed")?;
    if ended(side, process)? != (KILLED, SIGKILL) {
        return Err("case c: KILL did not end the process as a job kill does");
    }
    let record =
        last_record(audit::EXCEPTION_KILLED).ok_or("case c: a KILL verdict was not audited")?;
    if record.target() != u64::from(fault.pid) || record.detail[..2] != [1, 0] {
        return Err("case c: EXCEPTION_KILLED does not name the process, the verdict and level 0");
    }
    for handle in [exception, channel, process, job] {
        close(side, handle)?;
    }
    Ok(())
}

/// Case d: the process's channel hands on, and its job's channel gets the
/// same fault at level 1, resumes it at the exit, and the program exits 97.
fn hand_on(
    side: &Side,
    root: Handle,
    image: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let (job, process) = made(side, root, image)?;
    let near =
        attach(side, process, LONG_MS).map_err(|_| "case d: attach to the process failed")?;
    let far = attach(side, job, LONG_MS).map_err(|_| "case d: attach to the job failed")?;
    start(side, process, None)?;
    let (first, handed) = next_report(side, near)?;
    if first.level != 0 {
        return Err("case d: the process's own channel did not hear the fault first");
    }
    resolve(side, handed, HAND_ON).map_err(|_| "case d: hand on failed")?;
    let (second, exception) = next_report(side, far)?;
    report.faults += 2;
    if second.level != 1 || second.pc != first.pc || second.tid != first.tid {
        return Err("case d: the job's channel did not get the same fault at level 1");
    }
    let mut exit = registers(side, exception).map_err(|_| "case d: read failed")?;
    arch::exception_check::exit_with(&mut exit, exit_of(second.pc), 97);
    write(side, exception, &exit).map_err(|_| "case d: write failed")?;
    resolve(side, exception, RESUME).map_err(|_| "case d: resume failed")?;
    if ended(side, process)? != (EXITED, 97) {
        return Err(
            "case d: a fault handed on and resumed by the job's supervisor did not exit 97",
        );
    }
    for handle in [handed, exception, near, far, process, job] {
        close(side, handle)?;
    }
    Ok(())
}

/// Case e: a channel nobody reads, timeout 5 ms. The program ends as an
/// unsupervised one does, with `SIGSEGV`, no sooner than the timeout after
/// it started and within the check's patience after that; one `EXPIRED`
/// record; the exception, read off the channel afterwards, refuses both
/// calls with `TIMED_OUT`.
fn expiry(
    side: &Side,
    root: Handle,
    image: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let (job, process) = made(side, root, image)?;
    let channel = attach(side, process, SHORT_MS).map_err(|_| "case e: attach failed")?;
    let began = crate::timer::now_nanos();
    start(side, process, None)?;
    let end = ended(side, process)?;
    let took = crate::timer::now_nanos().saturating_sub(began);
    if end != (KILLED, SIGSEGV) {
        return Err("case e: an unanswered fault did not take the default action");
    }
    if took < SHORT_MS * 1_000_000 {
        return Err("case e: the default action came before the supervisor's timeout");
    }
    if took > SHORT_MS * 1_000_000 + EXPIRY_SLACK_NANOS {
        return Err("case e: the default action came long after the supervisor's timeout");
    }
    let (fault, exception) = next_report(side, channel)?;
    report.faults += 1;
    let record = last_record(audit::EXCEPTION_KILLED).ok_or("case e: an expiry was not audited")?;
    if record.target() != u64::from(fault.pid) || record.detail[..2] != [2, 0] {
        return Err("case e: EXCEPTION_KILLED does not name the process, the expiry and level 0");
    }
    expect(
        resolve(side, exception, RESUME),
        status::TIMED_OUT,
        "case e: an expired exception took a verdict",
        report,
    )?;
    expect(
        registers(side, exception),
        status::TIMED_OUT,
        "case e: an expired exception's registers were read",
        report,
    )?;
    for handle in [exception, channel, process, job] {
        close(side, handle)?;
    }
    Ok(())
}

/// Case e2: the only reader of the job's channel is the faulting program
/// itself, given the read end as its bootstrap. Nobody answers; the wait
/// expires and the default action ends it with `SIGSEGV`.
fn self_supervised(
    side: &Side,
    root: Handle,
    image: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let (job, process) = made(side, root, image)?;
    let channel = attach(side, job, SHORT_MS).map_err(|_| "case e2: attach failed")?;
    start(side, process, Some(channel))?;
    if ended(side, process)? != (KILLED, SIGSEGV) {
        return Err("case e2: a program that alone could answer its own fault was not ended");
    }
    report.faults += 1;
    for handle in [process, job] {
        close(side, handle)?;
    }
    Ok(())
}

/// Case f: the supervisor closes the exception undecided; with no level
/// left, the program ends with `SIGSEGV`, long before the 10 s timeout.
fn closed_undecided(
    side: &Side,
    root: Handle,
    image: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let (job, process) = made(side, root, image)?;
    let channel = attach(side, process, LONG_MS).map_err(|_| "case f: attach failed")?;
    start(side, process, None)?;
    let (_, exception) = next_report(side, channel)?;
    report.faults += 1;
    let closed = crate::timer::now_nanos();
    close(side, exception)?;
    if ended(side, process)? != (KILLED, SIGSEGV) {
        return Err("case f: a fault closed undecided did not take the default action");
    }
    if crate::timer::now_nanos().saturating_sub(closed) >= LONG_MS * 1_000_000 {
        return Err("case f: a fault closed undecided waited out its timeout");
    }
    for handle in [channel, process, job] {
        close(side, handle)?;
    }
    Ok(())
}

/// Case i: a breakpoint instruction, reported as the architecture takes it
/// (a breakpoint on the Arm pair; on x86-64 the general protection fault
/// its DPL 0 gate makes of a program's `int3`), its pc at the instruction;
/// moved past and resumed, the program exits 97.
fn breakpoint_case(
    side: &Side,
    root: Handle,
    image: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let (job, process) = made(side, root, image)?;
    let channel = attach(side, process, LONG_MS).map_err(|_| "case i: attach failed")?;
    start(side, process, None)?;
    let (fault, exception) = next_report(side, channel)?;
    report.faults += 1;
    let at_trap = fault.flags & FLAG_PC_AT_TRAP != 0;
    if fault.kind != arch::exception_check::BREAKPOINT_KIND
        || arch::exception_check::BREAKPOINT_CODE.is_some_and(|code| code != fault.code)
        || at_trap != arch::exception_check::BREAKPOINT_PC_AT_TRAP
    {
        crate::console::println!(
            "  k4       a breakpoint reported as kind {} code {:#x} flags {:#x}",
            fault.kind,
            fault.code,
            fault.flags
        );
        return Err("case i: a breakpoint was not reported as one, its pc where it is left");
    }
    let mut past = registers(side, exception).map_err(|_| "case i: read failed")?;
    arch::exception_check::set_pc(&mut past, fault.pc + arch::exception_check::BREAKPOINT_SKIP);
    write(side, exception, &past).map_err(|_| "case i: write failed")?;
    resolve(side, exception, RESUME).map_err(|_| "case i: resume failed")?;
    if ended(side, process)? != (EXITED, 97) {
        return Err("case i: a program resumed past its breakpoint did not exit 97");
    }
    for handle in [exception, channel, process, job] {
        close(side, handle)?;
    }
    Ok(())
}

/// Case k: the process level's exception is moved to a helper process, and
/// the helper dies holding it. The fault goes on to the job's channel at
/// level 1 at once, not after the process level's 10 s timeout.
fn supervisor_dies(
    side: &Side,
    root: Handle,
    image: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let (job, process) = made(side, root, image)?;
    let near =
        attach(side, process, LONG_MS).map_err(|_| "case k: attach to the process failed")?;
    let far = attach(side, job, LONG_MS).map_err(|_| "case k: attach to the job failed")?;
    start(side, process, None)?;
    let (_, held) = next_report(side, near)?;
    let helper = Side::new()?;
    let moved = side
        .process
        .with_handles(|table| table.remove(held))
        .map_err(|_| "case k: the exception was not in the check's table")?;
    if let Err(object) = helper
        .process
        .with_handles(|table| table.insert(moved.0, moved.1))
    {
        object::dispose([object]);
        return Err("case k: no room in the helper's table");
    }
    let died = crate::timer::now_nanos();
    crate::syscall::process::kill(&helper.process, job::KILLED_STATUS);
    helper.close_everything();
    drop(helper);
    let (second, exception) = next_report(side, far)?;
    report.faults += 2;
    if second.level != 1 {
        return Err("case k: a dead supervisor's fault did not reach the job's channel");
    }
    if crate::timer::now_nanos().saturating_sub(died) >= LONG_MS * 1_000_000 {
        return Err("case k: a dead supervisor's fault waited out its timeout");
    }
    resolve(side, exception, KILL).map_err(|_| "case k: kill failed")?;
    if ended(side, process)?.0 != KILLED {
        return Err("case k: KILL did not end the process");
    }
    for handle in [exception, near, far, process, job] {
        close(side, handle)?;
    }
    Ok(())
}

/// Case l: resumed unchanged twice, the store faults again, `repeat` 0, 1,
/// then 2.
fn repeats(
    side: &Side,
    root: Handle,
    image: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let (job, process) = made(side, root, image)?;
    let channel = attach(side, process, LONG_MS).map_err(|_| "case l: attach failed")?;
    start(side, process, None)?;
    for expected in 0..3 {
        let (fault, exception) = next_report(side, channel)?;
        report.faults += 1;
        if fault.repeat != expected {
            return Err("case l: repeat did not count a fault resumed unchanged");
        }
        let verdict = if expected == 2 { KILL } else { RESUME };
        resolve(side, exception, verdict).map_err(|_| "case l: a verdict failed")?;
        close(side, exception)?;
    }
    if ended(side, process)?.0 != KILLED {
        return Err("case l: KILL did not end the process");
    }
    for handle in [channel, process, job] {
        close(side, handle)?;
    }
    Ok(())
}

/// Case m: with no channel anywhere, a fault reports nothing, skips
/// nothing, and ends the program with `SIGSEGV` as before.
fn unsupervised(side: &Side, root: Handle, image: Handle) -> Result<(), &'static str> {
    let (reported, skipped) = (exception::reported(), exception::skipped());
    let (job, process) = made(side, root, image)?;
    start(side, process, None)?;
    if ended(side, process)? != (KILLED, SIGSEGV) {
        return Err("case m: an unsupervised fault did not end the program with SIGSEGV");
    }
    if exception::reported() != reported || exception::skipped() != skipped {
        return Err("case m: an unsupervised fault reported or skipped a level");
    }
    for handle in [process, job] {
        close(side, handle)?;
    }
    Ok(())
}

/// Case n: what `execve` lets a set-id file give a process
/// (`syscall::exec::ids_it_may_raise`) is nothing while a channel watches
/// its job, and the file's ids once that channel is closed.
fn no_raise_under_a_supervisor(
    side: &Side,
    root: Handle,
    image: Handle,
) -> Result<(), &'static str> {
    let (job, process) = made(side, root, image)?;
    let host = side.process.with_handles(|table| match table.get(process) {
        Ok((Object::Process(made), _)) => made.control().and_then(|control| control.host()),
        _ => None,
    });
    let made = host
        .and_then(object::process::downcast::<crate::syscall::process::Process>)
        .ok_or("case n: the made process is not the personality's")?;
    let root_ids = crate::fs::SetIds {
        uid: Some(0),
        gid: Some(0),
    };
    let channel = attach(side, job, LONG_MS).map_err(|_| "case n: attach failed")?;
    if crate::syscall::exec::ids_it_may_raise(&made, root_ids) != crate::fs::SetIds::NONE {
        return Err("case n: a set-id file raised a process a supervisor watches");
    }
    close(side, channel)?;
    if crate::syscall::exec::ids_it_may_raise(&made, root_ids) != root_ids {
        return Err("case n: a set-id file did not raise a process no supervisor watches");
    }
    drop(made);
    for handle in [process, job] {
        close(side, handle)?;
    }
    Ok(())
}

/// The newest record of `event` the high-value ring holds.
fn last_record(event: audit::Event) -> Option<audit::Record> {
    let mut records = [audit::Record::EMPTY; 16];
    let mut from = 0;
    let mut last = None;
    loop {
        let read = audit::read(audit::Which::High, from, &mut records);
        if read.copied == 0 {
            return last;
        }
        if let Some(found) = records
            .iter()
            .take(read.copied)
            .rev()
            .find(|record| record.is(event))
        {
            last = Some(*found);
        }
        from = read.next;
    }
}

/// Cases p, q and r (the consultant's condition 1, ledgers 2973 and
/// 3011): a program loaded with a set-id file's ids, unsupervised, is
/// marked; so are a fork of it and a native child it launches. Each, given
/// a supervisor afterwards through a handle carrying `SUPERVISE` and then
/// faulting, is reported to nobody: its fault takes the path a fault takes
/// with no supervisor, `SIGSEGV`.
fn raised_is_never_offered(side: &Side) -> Result<(), &'static str> {
    let class = if size_of::<usize>() == 8 {
        Class::Elf64
    } else {
        Class::Elf32
    };
    let file = image::build_with(
        class,
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_FAULT_PROGRAM,
    );
    let raised = crate::syscall::exec::load_executable(
        crate::syscall::exec::Executable {
            image: crate::syscall::load::Source::Bytes(&file),
            exe: b"/k4-raised",
            exec_fn: b"/k4-raised",
            set_ids: crate::fs::SetIds {
                uid: Some(1000),
                gid: Some(1000),
            },
            interpreter: None,
        },
        &[b"/k4-raised"],
        &[],
        [0x4b; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "case p: a set-id program could not be loaded")?;
    if !raised.supervisor().is_refused() {
        return Err("case p: a program loaded with a set-id file's ids was not marked");
    }
    let forked = crate::syscall::process::fork_for_check(&raised)
        .map_err(|_| "case q: a marked program could not be forked")?;
    if !forked.supervisor().is_refused() {
        return Err("case q: a fork of a marked program was not marked");
    }
    let host: &dyn object::process::Host = &*raised;
    let launched = crate::syscall::launch::load_native(Some(host), &file, b"k4-launched")
        .ok()
        .and_then(object::process::downcast::<crate::syscall::process::Process>)
        .ok_or("case r: a marked program could not launch a native child")?;
    if !launched.supervisor().is_refused() {
        return Err("case r: a native child of a marked program was not marked");
    }
    never_offered(
        side,
        raised,
        "case p: a raised program's fault was offered to a supervisor",
    )?;
    never_offered(
        side,
        launched,
        "case r: a marked creator's child's fault was offered",
    )?;
    crate::syscall::process::kill(&forked, job::KILLED_STATUS);
    drop(forked);
    Ok(())
}

/// Give `process`, loaded and not started, a supervisor through a handle
/// carrying `PROCESS_CREATED`, start it, and require its store through
/// null to end it with `SIGSEGV` with nothing reported (`what` if not).
fn never_offered(
    side: &Side,
    process: Arc<crate::syscall::process::Process>,
    what: &'static str,
) -> Result<(), &'static str> {
    let host: Arc<dyn object::process::Host> =
        Arc::clone(&process) as Arc<dyn object::process::Host>;
    let made = ProcessRef::created(host).map_err(|_| "no memory for a handle")?;
    let handle = side
        .process
        .with_handles(|table| table.insert(Object::Process(made), Rights::PROCESS_CREATED))
        .map_err(|_| "no room for a handle")?;
    let channel =
        attach(side, handle, LONG_MS).map_err(|_| "an attach to a marked process failed")?;
    let reported = exception::reported();
    let task =
        crate::syscall::process::start(&process).map_err(|_| "a marked program would not start")?;
    side.process.with_handles(|table| {
        if let Ok((Object::Process(made), _)) = table.get(handle)
            && let Some(control) = made.control()
        {
            control.started();
        }
    });
    if ended(side, handle)? != (KILLED, SIGSEGV) {
        return Err(what);
    }
    if exception::reported() != reported
        || wait(side, channel, Signals::READABLE, 1_000_000) != Err(status::TIMED_OUT)
    {
        return Err(what);
    }
    sched::wait_until_gone(&task, sched::REAPER_PATIENCE_NANOS)?;
    drop(task);
    for handle in [channel, handle] {
        close(side, handle)?;
    }
    drop(process);
    Ok(())
}
