//! A native process runs as the process that made it (`docs/AUTH.md` §7,
//! P0).
//!
//! The escalation it closes, made as a delegated service would make it: root
//! makes `/check-p` and `chown`s it and its `cgroup.procs` to uid 1000, as
//! init does for a `Delegate=yes` unit. A process running as uid 1000 in
//! every id opens the directory, is given `MANAGE` on its job by
//! `job_for_cgroup`, writes an image into a VMO and makes a process from it
//! with `process_create` in that job. The child's ids must be its creator's,
//! all of them, before it starts. Started, it asks `getuid`
//! ([`arch::USER_GETUID_PROGRAM`]) and exits with the answer, which must be
//! 1000's low byte, 232: a child made as root exits 0. The cgroup must then
//! empty, and is removed.

use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_elf::Class;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::{Rights, SAME_RIGHTS};
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::PROCESS_EXITED;

use super::{Checked, Harness, delegation_check, native_check};
use crate::arch;
use crate::object::Object;
use crate::object::check::{SCRATCH, Side, reg};
use crate::syscall::credentials::{Credentials, Ids};
use crate::syscall::image::{self, Shape};
use crate::syscall::process::Process;

/// The user the service runs as.
const SERVICE: u32 = 1000;

/// What the child exits with when `getuid` answers [`SERVICE`]: its low
/// byte, as an exit status carries it.
const SERVICE_STATUS: u32 = SERVICE & 0xFF;

/// Where the child's image is staged in the creator's memory, past the
/// scratch region.
const IMAGE_AT: u64 = 0x6000_0000;
/// The child's name, and where it is staged.
const CHILD_NAME: &[u8] = b"as-its-creator";
const NAME_AT: u64 = SCRATCH + 0x500;
/// A VMO offset for the image's write.
const OFFSET: u64 = SCRATCH + 0x540;
/// A wait's deadline, and the signals it observed.
const DEADLINE: u64 = SCRATCH + 0x580;
const OBSERVED: u64 = SCRATCH + 0x588;
/// Where `process_status` writes.
const STATUS_AT: u64 = SCRATCH + 0x5C0;

/// How long the child has to run and its cgroup to empty: time for a loaded
/// host running the emulator.
const PATIENCE_NANOS: u64 = 30_000_000_000;

/// The check. Answers what the child exited with: the low byte of what
/// `getuid` answered it.
///
/// # Errors
///
/// The first thing that was not as P0 has it, by name.
pub(super) fn run(harness: &mut Harness) -> Checked<u32> {
    harness
        .mkdir(b"/check-p")
        .map_err(|_| "mkdir of the creator check's cgroup failed")?;
    harness.report.made += 1;
    delegation_check::delegate(harness, b"/check-p")?;
    delegation_check::delegate(harness, b"/check-p/cgroup.procs")?;
    let side = Side::new()?;
    let outcome = made_as_its_creator(harness, &side);
    side.close_everything();
    drop(side);
    let removed = harness.rmdir(b"/check-p");
    let answered = outcome?;
    removed.map_err(|_| "the creator check's cgroup did not empty")?;
    Ok(answered)
}

/// A service's ids: `id` in every role, and its own group alone.
fn service() -> Credentials {
    let ids = Ids {
        real: SERVICE,
        effective: SERVICE,
        saved: SERVICE,
        filesystem: SERVICE,
    };
    Credentials {
        user: ids,
        group: ids,
        groups: Vec::from([SERVICE]),
        ..Credentials::root()
    }
}

/// The body of [`run`], as the service in `side`.
fn made_as_its_creator(harness: &Harness, side: &Side) -> Checked<u32> {
    let creator = service();
    side.process
        .with_credentials(|held| *held = creator.clone());
    let dirfd = native_check::descriptor(harness, side, b"/check-p")?;
    let job = side.handle(
        nr::JOB_FOR_CGROUP,
        &[dirfd, u64::from(SAME_RIGHTS)],
        "job_for_cgroup refused uid 1000 its own delegated cgroup",
    )?;
    let manages = side.process.with_handles(|table| {
        matches!(table.get(job), Ok((Object::Job(_), rights)) if rights.contains(Rights::MANAGE))
    });
    if !manages {
        return Err("job_for_cgroup gave uid 1000 no MANAGE on its delegated cgroup");
    }

    let class = if size_of::<usize>() == 8 {
        Class::Elf64
    } else {
        Class::Elf32
    };
    let file = image::build_with(
        class,
        arch::ARCH.elf_machine(),
        Shape::Good,
        arch::USER_GETUID_PROGRAM,
    );
    let child = create(side, job, &file)?;
    let made = child_of(side, child)?;
    if made.with_credentials(|held| held.clone()) != creator {
        crate::console::println!(
            "  creator  uid {} made a native process whose ids are {:?}",
            SERVICE,
            made.with_credentials(|held| held.clone())
        );
        return Err("process_create gave a child other ids than its creator's");
    }
    drop(made);

    let _ = side
        .run(child, 0)
        .map_err(|_| "the creator check's child would not start")?;
    wait(
        side,
        child,
        Signals::TERMINATED,
        "the creator check's child never ended",
    )?;
    side.put(STATUS_AT, &[0xA5; 8])?;
    if side.call(nr::PROCESS_STATUS, &[reg(child), STATUS_AT]) != Ok(0) {
        return Err("process_status of the creator check's child was refused");
    }
    let (state, value) = (side.get_u32(STATUS_AT)?, side.get_u32(STATUS_AT + 4)?);
    if (state, value) != (PROCESS_EXITED, SERVICE_STATUS) {
        crate::console::println!(
            "  creator  a native process uid {SERVICE} made ended in state {state} with {value}, \
             not exited with {SERVICE_STATUS}"
        );
        return Err("a native process made by uid 1000 did not answer getuid with 1000");
    }
    wait(
        side,
        job,
        Signals::EMPTY,
        "the creator check's cgroup never emptied",
    )?;
    Ok(value)
}

/// A process made from `file` in `job` by `side`, unstarted.
fn create(side: &Side, job: Handle, file: &[u8]) -> Checked<Handle> {
    let len = file.len() as u64;
    let _ = side
        .process
        .space()
        .map_anonymous(
            IMAGE_AT,
            len.div_ceil(PAGE_SIZE) * PAGE_SIZE,
            ferrix_vma::VmaFlags::READ_WRITE,
        )
        .map_err(|_| "no room for the creator check's image")?;
    side.put(IMAGE_AT, file)?;
    let vmo = side.handle(
        nr::VMO_CREATE,
        &[len],
        "vmo_create for the creator check's image failed",
    )?;
    side.put(OFFSET, &0_u64.to_ne_bytes())?;
    let _ = side
        .call(nr::VMO_WRITE, &[reg(vmo), IMAGE_AT, len, OFFSET])
        .map_err(|_| "the creator check's image could not be written")?;
    side.put(NAME_AT, CHILD_NAME)?;
    side.built(
        job,
        vmo,
        NAME_AT,
        CHILD_NAME.len() as u64,
        "process_make by uid 1000 in its delegated cgroup was refused",
    )
}

/// The process `handle` in `side` names.
fn child_of(side: &Side, handle: Handle) -> Checked<alloc::sync::Arc<Process>> {
    side.process
        .with_handles(|table| match table.get(handle) {
            Ok((Object::Process(child), _)) => child.control().and_then(|c| c.process()),
            _ => None,
        })
        .ok_or("process_create answered a handle that names no process of this personality")
}

/// Wait on `handle` for `signals`, as `object_wait_one` waits.
fn wait(side: &Side, handle: Handle, signals: Signals, what: &'static str) -> Checked<()> {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    side.put(DEADLINE, &deadline.to_ne_bytes())?;
    side.call(
        nr::OBJECT_WAIT_ONE,
        &[reg(handle), u64::from(signals.0), DEADLINE, OBSERVED],
    )
    .map(|_| ())
    .map_err(|_| what)
}
