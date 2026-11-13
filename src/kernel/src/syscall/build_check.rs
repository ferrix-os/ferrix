//! Stage 9's `build`: a process made empty and run where its creator says
//! (`docs/microkernel/K15.md` §6), through the native dispatcher, as the object
//! checks drive it. What a real starter does end to end is the boot's driver
//! starts, by `ferrix-process-builder` under `devmgr`.

use alloc::sync::Arc;

use ferrix_bootinfo::{PAGE_SIZE, USER_VIRT_END};
use ferrix_errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::{IMAGE_RIGHTS, Rights};
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{
    JOB_MEASURED, JOB_MEMORY, JOB_SEALED, MAP_EXECUTE, MAP_NO_PAGER, MAP_NONE, MAP_READ, MAP_WRITE,
    VMO_SEAL,
};

use crate::arch;
use crate::fs::SetIds;
use crate::object::Object;
use crate::object::check::{SCRATCH, Side, reg};
use crate::object::job::Job;

/// A child's name.
const NAME_AT: u64 = SCRATCH + 0x200;
/// What it says.
const NAME: &[u8] = b"built";
/// A VMO offset.
const OFFSET_AT: u64 = SCRATCH + 0x300;
/// A deadline.
const DEADLINE_AT: u64 = SCRATCH + 0x308;
/// What a wait saw.
const OBSERVED_AT: u64 = SCRATCH + 0x310;
/// Bytes staged, one page.
const BYTES_AT: u64 = SCRATCH + PAGE_SIZE;
/// Where the child's code goes in its space.
const TEXT_AT: u64 = 0x40_0000;
/// The top of the stack it is given, its page number in the exit code.
const STACK_TOP: u64 = 0x7A_0000;
/// How long a child may take to end.
const PATIENCE_NANOS: u64 = 20_000_000_000;

/// What the check found, for the boot line.
#[derive(Debug, Default, Clone, Copy)]
pub struct Report {
    /// Processes made empty and run.
    pub run: u32,
    /// Refusals, each as specified.
    pub refusals: u32,
}

/// `answer` is exactly `status`, counted.
fn refused(
    answer: Result<usize, Errno>,
    want: Errno,
    what: &'static str,
    report: &mut Report,
) -> Result<(), &'static str> {
    if answer.err() != Some(want) {
        return Err(what);
    }
    report.refusals += 1;
    Ok(())
}

/// A process made in `job`, empty.
fn make(side: &Side, job: Handle) -> Result<Handle, &'static str> {
    side.handle(
        nr::PROCESS_MAKE,
        &[reg(job), NAME_AT, NAME.len() as u64],
        "build self-check failed: process_make refused a good call",
    )
}

/// A space handle on `child` with every right.
fn space_of(side: &Side, child: Handle) -> Result<Handle, &'static str> {
    side.handle(
        nr::PROCESS_SPACE,
        &[reg(child), u64::from(Rights::SPACE.0)],
        "build self-check failed: process_space refused the creator",
    )
}

/// A VMO that holds `code`, as an image: executable, not writable.
fn image_of(side: &Side, code: &[u8]) -> Result<Handle, &'static str> {
    let what = "build self-check failed: an image VMO could not be made";
    let vmo = side.handle(nr::VMO_CREATE, &[PAGE_SIZE], what)?;
    side.put(BYTES_AT, code)?;
    side.put(OFFSET_AT, &0_u64.to_ne_bytes())?;
    let _ = side
        .call(
            nr::VMO_WRITE,
            &[reg(vmo), BYTES_AT, code.len() as u64, OFFSET_AT],
        )
        .map_err(|_| what)?;
    side.handle(nr::VMO_EXECUTABLE, &[reg(vmo), 0], what)
}

/// `code` mapped read-execute and a stack allocated below [`STACK_TOP`], as the
/// library does it.
fn map_program(side: &Side, space: Handle, code: &[u8]) -> Result<(), &'static str> {
    let what = "build self-check failed: a program could not be mapped by handle";
    let vmo = image_of(side, code)?;
    side.put(OFFSET_AT, &0_u64.to_ne_bytes())?;
    let _ = side
        .call(
            nr::SPACE_MAP,
            &[
                reg(space),
                reg(vmo),
                TEXT_AT,
                PAGE_SIZE,
                u64::from(MAP_READ | MAP_EXECUTE | MAP_NO_PAGER),
                OFFSET_AT,
            ],
        )
        .map_err(|_| what)?;
    let _ = side
        .call(
            nr::SPACE_ALLOCATE,
            &[
                reg(space),
                STACK_TOP - 2 * PAGE_SIZE,
                2 * PAGE_SIZE,
                u64::from(MAP_READ | MAP_WRITE),
            ],
        )
        .map_err(|_| what)?;
    let _ = side.call(nr::HANDLE_CLOSE, &[reg(vmo)]);
    Ok(())
}

/// The exit status of `child`, once it has ended.
fn ended(side: &Side, child: Handle, what: &'static str) -> Result<i32, &'static str> {
    let now = crate::timer::now_nanos();
    side.put(DEADLINE_AT, &(now + PATIENCE_NANOS).to_ne_bytes())?;
    let _ = side
        .call(
            nr::OBJECT_WAIT_ONE,
            &[
                reg(child),
                u64::from(Signals::TERMINATED.0),
                DEADLINE_AT,
                OBSERVED_AT,
            ],
        )
        .map_err(|_| what)?;
    side.process
        .with_handles(|table| match table.get(child) {
            Ok((Object::Process(made), _)) => made.exit_status(),
            _ => None,
        })
        .ok_or(what)
}

/// L.object.290: made empty, in its job, with `process_create`'s job rules.
///
/// Verifies: L.object.290
fn making(side: &Side, root: Handle, report: &mut Report) -> Result<Handle, &'static str> {
    side.put(NAME_AT, NAME)?;
    let child = make(side, root)?;
    let host = side
        .process
        .with_handles(|table| match table.get(child) {
            Ok((Object::Process(made), rights)) if rights == Rights::PROCESS_CREATED => {
                made.control().and_then(|control| control.host())
            }
            _ => None,
        })
        .ok_or("build self-check failed: process_make did not answer PROCESS_CREATED")?;
    if host
        .core()
        .own_space()
        .with_regions(|regions| regions.count())
        != 0
    {
        return Err("build self-check failed: a made process's space is not empty");
    }
    let plain = side.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(root), u64::from(Rights::WAIT.0)],
        "build self-check failed: a job handle could not be narrowed",
    );
    if let Ok(plain) = plain {
        refused(
            side.call(nr::PROCESS_MAKE, &[reg(plain), NAME_AT, NAME.len() as u64]),
            status::ACCESS_DENIED,
            "build self-check failed: process_make without MANAGE was not refused",
            report,
        )?;
    }
    refused(
        side.call(
            nr::PROCESS_MAKE,
            &[reg(root), NAME_AT, nr::PROCESS_NAME_MAX as u64 + 1],
        ),
        status::INVALID_ARGS,
        "build self-check failed: process_make took a name past its bound",
        report,
    )?;
    // A sealed member stays in its tree, as for `process_create`.
    let sealed = side.handle(
        nr::JOB_CREATE,
        &[reg(root), JOB_SEALED],
        "build self-check failed: no sealed job",
    )?;
    let member = Side::new()?;
    let (sealed_job, outside) = side.process.with_handles(|table| {
        let job_of = |handle| match table.get(handle) {
            Ok((Object::Job(job), _)) => Some(Arc::clone(job)),
            _ => None,
        };
        (job_of(sealed), job_of(root))
    });
    let (sealed_job, outside): (Arc<Job>, Arc<Job>) = sealed_job
        .zip(outside)
        .ok_or("build self-check failed: the check's jobs are gone")?;
    member
        .process
        .move_new_to(&sealed_job)
        .map_err(|_| "build self-check failed: a sealed job refused a member")?;
    let theirs = member
        .process
        .with_handles(|table| table.insert(Object::Job(outside), Rights::JOB))
        .map_err(|_| "build self-check failed: no room for a job handle")?;
    member.put(NAME_AT, NAME)?;
    refused(
        member.call(nr::PROCESS_MAKE, &[reg(theirs), NAME_AT, NAME.len() as u64]),
        status::ACCESS_DENIED,
        "build self-check failed: a sealed member made a process outside its tree",
        report,
    )?;
    member.close_everything();
    crate::syscall::process::kill(&member.process, crate::object::job::KILLED_STATUS);
    Ok(child)
}

/// L.object.290: a process made in a measured job has a measured space, so what its
/// maker maps to run must be sealed, and a sealed map is recorded.
///
/// Verifies: L.object.290
fn made_measured(side: &Side, root: Handle) -> Result<(), &'static str> {
    let job = side.handle(
        nr::JOB_CREATE,
        &[reg(root), JOB_MEASURED],
        "build self-check failed: no measured job",
    )?;
    let child = make(side, job)?;
    let space = space_of(side, child)?;
    side.put(OFFSET_AT, &0_u64.to_ne_bytes())?;
    let map = |vmo: Handle| {
        side.call(
            nr::SPACE_MAP,
            &[
                reg(space),
                reg(vmo),
                TEXT_AT,
                PAGE_SIZE,
                u64::from(MAP_READ | MAP_EXECUTE),
                OFFSET_AT,
            ],
        )
    };
    let plain = side.handle(
        nr::VMO_CREATE,
        &[PAGE_SIZE],
        "build self-check failed: vmo_create",
    )?;
    let plain = side.handle(
        nr::VMO_EXECUTABLE,
        &[reg(plain), 0],
        "build self-check failed: vmo_executable",
    )?;
    if map(plain) != Err(status::BAD_STATE) {
        return Err(
            "build self-check failed: a process made in a measured job mapped an unsealed image",
        );
    }
    let sealed = side.handle(
        nr::VMO_CREATE,
        &[PAGE_SIZE],
        "build self-check failed: vmo_create",
    )?;
    let sealed = side.handle(
        nr::VMO_EXECUTABLE,
        &[reg(sealed), VMO_SEAL],
        "build self-check failed: vmo_executable with a seal",
    )?;
    if map(sealed).is_err() {
        return Err(
            "build self-check failed: a process made in a measured job refused a sealed image",
        );
    }
    for handle in [sealed, plain, space, child, job] {
        let _ = side.call(nr::HANDLE_CLOSE, &[reg(handle)]);
    }
    Ok(())
}

/// L.object.291, L.object.292: who may run it, and where.
///
/// Verifies: L.object.291
/// Verifies: L.object.292
fn running_is_judged(side: &Side, child: Handle, report: &mut Report) -> Result<(), &'static str> {
    // The bound's own odd neighbour: the Thumb bit cannot carry an address across it.
    let bad_entries = [
        USER_VIRT_END,
        USER_VIRT_END | 1,
        u64::MAX,
        USER_VIRT_END + PAGE_SIZE,
        1_u64 << 63,
    ];
    for entry in bad_entries {
        refused(
            side.call(nr::PROCESS_RUN, &[reg(child), entry, STACK_TOP, 0]),
            status::INVALID_ARGS,
            "build self-check failed: process_run took an entry outside user space",
            report,
        )?;
    }
    for stack in [USER_VIRT_END + 16, u64::MAX] {
        refused(
            side.call(nr::PROCESS_RUN, &[reg(child), TEXT_AT, stack, 0]),
            status::INVALID_ARGS,
            "build self-check failed: process_run took a stack outside user space",
            report,
        )?;
    }
    let watcher = side.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(child), u64::from(Rights::WAIT.0)],
        "build self-check failed: a process handle could not be narrowed",
    )?;
    refused(
        side.call(nr::PROCESS_RUN, &[reg(watcher), TEXT_AT, STACK_TOP, 0]),
        status::ACCESS_DENIED,
        "build self-check failed: process_run without MANAGE was not refused",
        report,
    )?;
    let _ = side.call(nr::HANDLE_CLOSE, &[reg(watcher)]);
    // A closed bootstrap is refused and gives the start back: the run below takes it.
    refused(
        side.call(
            nr::PROCESS_RUN,
            &[reg(child), TEXT_AT, STACK_TOP, 0x7FFF_FFF0],
        ),
        status::BAD_HANDLE,
        "build self-check failed: process_run took a bootstrap that is no handle",
        report,
    )?;
    Ok(())
}

/// L.object.293, L.object.296: it enters there, on that stack, and nothing of its
/// space is reachable through its handle after.
///
/// Verifies: L.object.293
/// Verifies: L.object.296
fn it_enters_where_it_is_told(
    side: &Side,
    root: Handle,
    child: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let space = space_of(side, child)?;
    map_program(side, space, arch::programs_check::USER_STACK_PROGRAM)?;
    let _ = side.call(nr::HANDLE_CLOSE, &[reg(space)]);
    running_is_judged(side, child, report)?;
    let _ = side
        .call(nr::PROCESS_RUN, &[reg(child), TEXT_AT, STACK_TOP, 0])
        .map_err(|_| "build self-check failed: a mapped program would not run")?;
    refused(
        side.call(nr::PROCESS_RUN, &[reg(child), TEXT_AT, STACK_TOP, 0]),
        status::BAD_STATE,
        "build self-check failed: a process was run twice",
        report,
    )?;
    refused(
        side.call(nr::PROCESS_SPACE, &[reg(child), u64::from(Rights::SPACE.0)]),
        status::BAD_STATE,
        "build self-check failed: process_space answered a process that has started",
        report,
    )?;
    let code = ended(
        side,
        child,
        "build self-check failed: the program never ended",
    )?;
    let want = ((STACK_TOP >> 12) & 0xFF) as i32;
    if code != want {
        crate::console::println!("  build    the program ended with {code}, not {want}");
        return Err("build self-check failed: the program did not enter on the stack it was given");
    }
    report.run += 1;

    // The bootstrap's value arrives in the first argument register.
    let second = make(side, root)?;
    let space = space_of(side, second)?;
    map_program(side, space, arch::USER_ARGUMENT_PROGRAM)?;
    let _ = side.call(nr::HANDLE_CLOSE, &[reg(space)]);
    let far = side.handle(nr::PORT_CREATE, &[], "build self-check failed: no port")?;
    let _ = side
        .call(
            nr::PROCESS_RUN,
            &[reg(second), TEXT_AT, STACK_TOP, reg(far)],
        )
        .map_err(|_| "build self-check failed: a program with a bootstrap would not run")?;
    let code = ended(
        side,
        second,
        "build self-check failed: the program never ended",
    )?;
    if code <= 0 {
        return Err("build self-check failed: the bootstrap's value did not arrive");
    }
    report.run += 1;
    Ok(())
}

/// L.object.294: an empty space, or an entry in nothing, ends the child by a
/// fault in its own job; the kernel and a sibling go on.
///
/// Verifies: L.object.294
fn an_empty_space_only_ends_the_child(
    side: &Side,
    root: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let child = make(side, root)?;
    let _ = side
        .call(nr::PROCESS_RUN, &[reg(child), TEXT_AT, STACK_TOP, 0])
        .map_err(|_| "build self-check failed: an empty process would not run")?;
    let code = ended(
        side,
        child,
        "build self-check failed: an empty process never ended",
    )?;
    if code == 0 {
        return Err("build self-check failed: an empty process ended well");
    }
    report.run += 1;

    // Mapped text is the VMO's, and its tables come at run time: a job with no
    // room left builds a process and then loses it at the first fault, in that
    // job, and no one else notices (ledger 5726 (9)).
    let tight = side.handle(
        nr::JOB_CREATE,
        &[reg(root), 0],
        "build self-check failed: no job for the limit's case",
    )?;
    let child = make(side, tight)?;
    let space = space_of(side, child)?;
    map_program(side, space, arch::USER_ARGUMENT_PROGRAM)?;
    let _ = side.call(nr::HANDLE_CLOSE, &[reg(space)]);
    side.put(OFFSET_AT, &1_u64.to_ne_bytes())?;
    let _ = side
        .call(nr::JOB_SET_LIMIT, &[reg(tight), JOB_MEMORY, OFFSET_AT])
        .map_err(|_| "build self-check failed: the limit could not be set")?;
    let _ = side
        .call(nr::PROCESS_RUN, &[reg(child), TEXT_AT, STACK_TOP, 0])
        .map_err(|_| "build self-check failed: a process in a full job would not run")?;
    let code = ended(
        side,
        child,
        "build self-check failed: a process in a full job never ended",
    )?;
    if code == 0 {
        return Err("build self-check failed: a process in a job with no room ran to its exit");
    }
    report.run += 1;
    Ok(())
}

/// L.object.297: protection 0 is a guard, and only `space_allocate` takes it.
///
/// Verifies: L.object.297
fn a_guard(side: &Side, root: Handle, report: &mut Report) -> Result<(), &'static str> {
    let child = make(side, root)?;
    let space = space_of(side, child)?;
    let _ = side
        .call(
            nr::SPACE_ALLOCATE,
            &[reg(space), TEXT_AT, PAGE_SIZE, u64::from(MAP_NONE)],
        )
        .map_err(|_| "build self-check failed: space_allocate refused a guard")?;
    refused(
        side.call(nr::SPACE_READ, &[reg(space), TEXT_AT, BYTES_AT, 16]),
        status::FAULT,
        "build self-check failed: a guard was read",
        report,
    )?;
    refused(
        side.call(
            nr::SPACE_PROTECT,
            &[reg(space), TEXT_AT, PAGE_SIZE, u64::from(MAP_NONE)],
        ),
        status::INVALID_ARGS,
        "build self-check failed: space_protect took protection 0",
        report,
    )?;
    let vmo = side.handle(
        nr::VMO_CREATE,
        &[PAGE_SIZE],
        "build self-check failed: no VMO",
    )?;
    side.put(OFFSET_AT, &0_u64.to_ne_bytes())?;
    refused(
        side.call(
            nr::SPACE_MAP,
            &[
                reg(space),
                reg(vmo),
                TEXT_AT + 2 * PAGE_SIZE,
                PAGE_SIZE,
                u64::from(MAP_NONE),
                OFFSET_AT,
            ],
        ),
        status::INVALID_ARGS,
        "build self-check failed: space_map took protection 0",
        report,
    )?;
    let _ = side.call(nr::HANDLE_CLOSE, &[reg(space)]);
    let _ = side.call(nr::HANDLE_CLOSE, &[reg(child)]);
    Ok(())
}

/// L.object.309: while a handle names a process's space, `execve` raises no ids
/// from a set-id file: whoever holds it could write into the raised program.
///
/// Verifies: L.object.309
fn a_kept_handle_raises_no_ids(side: &Side, root: Handle) -> Result<(), &'static str> {
    let child = make(side, root)?;
    let space = space_of(side, child)?;
    let made = side
        .process
        .with_handles(|table| match table.get(child) {
            Ok((Object::Process(made), _)) => made.control().and_then(|held| held.process()),
            _ => None,
        })
        .ok_or("build self-check failed: the made process is gone")?;
    let raised = SetIds {
        uid: Some(0),
        gid: Some(0),
    };
    let may = crate::syscall::exec::ids_it_may_raise(&made, raised);
    if may != SetIds::NONE {
        return Err("build self-check failed: execve would raise ids under a kept space handle");
    }
    let _ = side.call(nr::HANDLE_CLOSE, &[reg(space)]);
    let may = crate::syscall::exec::ids_it_may_raise(&made, raised);
    if may != raised {
        return Err("build self-check failed: a closed space handle still held set-id bits off");
    }
    let _ = side.call(nr::HANDLE_CLOSE, &[reg(child)]);
    Ok(())
}

/// L.object.299: `MAP_NO_PAGER` refuses a pager-backed VMO. L.object.306: an image's
/// handle is what the requirement says, as the kernel hands `devmgr` one.
///
/// Verifies: L.object.299
/// Verifies: L.object.306
fn no_pager_and_the_image_rights(
    side: &Side,
    root: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let child = make(side, root)?;
    let space = space_of(side, child)?;
    let port = side.handle(nr::PORT_CREATE, &[], "build self-check failed: no port")?;
    side.put(DEADLINE_AT, &0_u64.to_ne_bytes())?;
    let pager = side.handle(
        nr::PAGER_CREATE,
        &[reg(port), DEADLINE_AT, 0],
        "build self-check failed: no pager",
    )?;
    let spec = [1_u64, 1, 0];
    let mut bytes = [0_u8; 24];
    for (out, word) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(spec) {
        *out = word.to_ne_bytes();
    }
    side.put(BYTES_AT, &bytes)?;
    let paged = side.handle(
        nr::PAGER_CREATE_VMO,
        &[reg(pager), BYTES_AT],
        "build self-check failed: no pager VMO",
    )?;
    side.put(OFFSET_AT, &0_u64.to_ne_bytes())?;
    refused(
        side.call(
            nr::SPACE_MAP,
            &[
                reg(space),
                reg(paged),
                TEXT_AT,
                PAGE_SIZE,
                u64::from(MAP_READ | MAP_NO_PAGER),
                OFFSET_AT,
            ],
        ),
        status::WRONG_TYPE,
        "build self-check failed: space_map with MAP_NO_PAGER took a pager-backed VMO",
        report,
    )?;
    let plain = side.handle(
        nr::VMO_CREATE,
        &[PAGE_SIZE],
        "build self-check failed: no VMO",
    )?;
    let _ = side
        .call(
            nr::SPACE_MAP,
            &[
                reg(space),
                reg(plain),
                TEXT_AT,
                PAGE_SIZE,
                u64::from(MAP_READ | MAP_NO_PAGER),
                OFFSET_AT,
            ],
        )
        .map_err(|_| "build self-check failed: MAP_NO_PAGER refused an anonymous VMO")?;
    if IMAGE_RIGHTS.contains(Rights::WRITE)
        || IMAGE_RIGHTS.contains(Rights::DUPLICATE)
        || !IMAGE_RIGHTS.contains(Rights::EXECUTE)
        || crate::discovery::first_image_check::handed_image_rights() != IMAGE_RIGHTS
    {
        return Err("build self-check failed: an image's handle carries the wrong rights");
    }
    for handle in [paged, pager, port, plain, space, child] {
        let _ = side.call(nr::HANDLE_CLOSE, &[reg(handle)]);
    }
    Ok(())
}

/// Every case, on a process of the check's own.
///
/// Verifies: H.OBJ.27
///
/// # Errors
///
/// The first case that did not hold.
pub fn run() -> Result<Report, &'static str> {
    let mut report = Report::default();
    let side = Side::new()?;
    let root_job = Job::new_root().map_err(|_| "build self-check failed: no job")?;
    let root = side
        .process
        .with_handles(|table| table.insert(Object::Job(root_job), Rights::JOB))
        .map_err(|_| "build self-check failed: no room for the check's job")?;
    let child = making(&side, root, &mut report)?;
    made_measured(&side, root)?;
    it_enters_where_it_is_told(&side, root, child, &mut report)?;
    an_empty_space_only_ends_the_child(&side, root, &mut report)?;
    a_guard(&side, root, &mut report)?;
    a_kept_handle_raises_no_ids(&side, root)?;
    no_pager_and_the_image_rights(&side, root, &mut report)?;
    side.close_everything();
    crate::syscall::process::kill(&side.process, crate::object::job::KILLED_STATUS);
    Ok(report)
}
