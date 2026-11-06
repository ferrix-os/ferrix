//! A delegated cgroup's own limits stay its delegator's.
//!
//! Root makes `/check-l`, enables `memory`, `pids` and `cpu` above it, sets
//! its `memory.max` to 16 MiB, and `chown`s the directory and its
//! `cgroup.procs` to uid 1000, as init delegates a `Delegate=yes` unit. The
//! delegatee, uid 1000 in every id, is given its job by `job_for_cgroup` with
//! `MANAGE` and without `SET_LIMIT`, since it may write `cgroup.procs` and not
//! the limit files. Then, each refused with `ACCESS_DENIED`: asking
//! `job_for_cgroup` for `SET_LIMIT`, `job_set_limit` on the job for memory,
//! tasks and processor weight, a duplicate asked with `SET_LIMIT`, and
//! `job_set_limit` through a duplicate asked with the same rights -- a right
//! is only ever dropped, so a duplicate cannot regain one. Through the files,
//! an open for writing of its own `memory.max`, `pids.max` and `cpu.weight`
//! is refused `EACCES`, and `memory.max` still reads 16 MiB afterwards.
//!
//! What it may limit is what it makes: a job it makes with `job_create`
//! inside its own carries `SET_LIMIT`, and a limit set on that is accepted.
//! Such a limit may exceed the delegated job's as a number, and binds nothing
//! beyond it: every job's use is charged to each of its ancestors
//! (`object::quota`).
//!
//! `docs/CGROUPS.md` §5.

use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::{Rights, SAME_RIGHTS};
use ferrix_native_abi::status;
use ferrix_native_abi::types::{JOB_CPU_WEIGHT, JOB_MEMORY, JOB_TASKS, UNLIMITED};
use ferrix_vfs::{Access, OpenFlags};

use super::{Checked, Harness, delegation_check, native_check};
use crate::object::Object;
use crate::syscall::credentials::{Credentials, Ids};
use crate::syscall::side_check::{SCRATCH, Side, reg};

/// The user the cgroup is delegated to.
const DELEGATE: u32 = 1000;

/// The memory limit root sets on the delegated cgroup.
const ROOTS_LIMIT: &[u8] = b"16777216\n";

/// A limit the delegatee sets on a job it made: 1 MiB.
const ITS_OWN: u64 = 1 << 20;

/// Where a limit handed to `job_set_limit` is staged.
const LIMIT_AT: u64 = SCRATCH + 0x600;

/// What the delegatee's handle to its own job carries.
const DELEGATED: Rights =
    Rights(Rights::DUPLICATE.0 | Rights::TRANSFER.0 | Rights::WAIT.0 | Rights::MANAGE.0);

/// The check. Answers how many of the delegatee's attempts on its own
/// limits were refused.
///
/// # Errors
///
/// The first thing that was not as `docs/CGROUPS.md` §5 has it, by name.
pub(super) fn run(harness: &mut Harness) -> Checked<u32> {
    let _ = harness
        .write(b"/cgroup.subtree_control", b"+memory +pids +cpu\n")
        .map_err(|_| "the root refused to enable the limits check's controllers")?;
    harness
        .mkdir(b"/check-l")
        .map_err(|_| "mkdir of the limits check's cgroup failed")?;
    harness.report.made += 1;
    let outcome = delegated(harness);
    let removed = harness.rmdir(b"/check-l");
    let disabled = harness.write(b"/cgroup.subtree_control", b"-memory -pids -cpu\n");
    let refused = outcome?;
    removed.map_err(|_| "the limits check's cgroup did not empty")?;
    let _ = disabled.map_err(|_| "the root refused to disable the limits check's controllers")?;
    Ok(refused)
}

/// Set root's limit, delegate the cgroup, and try it as the delegatee.
fn delegated(harness: &mut Harness) -> Checked<u32> {
    let _ = harness
        .write(b"/check-l/memory.max", ROOTS_LIMIT)
        .map_err(|_| "root could not set the limits check's memory.max")?;
    delegation_check::delegate(harness, b"/check-l")?;
    delegation_check::delegate(harness, b"/check-l/cgroup.procs")?;
    let side = Side::new()?;
    side.process.with_credentials(|held| *held = delegatee());
    let outcome = natively(harness, &side);
    side.close_everything();
    let mut refused = outcome?;
    refused += through_the_files(harness)?;
    if !harness.reads(b"/check-l/memory.max", ROOTS_LIMIT) {
        return Err("the delegated cgroup's memory.max changed under its delegatee");
    }
    Ok(refused)
}

/// Uid 1000 in every id, and its own group alone.
fn delegatee() -> Credentials {
    let ids = Ids {
        real: DELEGATE,
        effective: DELEGATE,
        saved: DELEGATE,
        filesystem: DELEGATE,
    };
    Credentials {
        user: ids,
        group: ids,
        groups: Vec::from([DELEGATE]),
        ..Credentials::root()
    }
}

/// The rights `handle` carries in `side`, if it names a job.
fn job_rights(side: &Side, handle: Handle) -> Option<Rights> {
    side.process.with_handles(|table| match table.get(handle) {
        Ok((Object::Job(_), rights)) => Some(rights),
        _ => None,
    })
}

/// `job_set_limit` on `job` for `resource`, to `limit`.
fn set_limit(side: &Side, job: Handle, resource: u64, limit: u64) -> Result<usize, Errno> {
    side.put(LIMIT_AT, &limit.to_ne_bytes())
        .map_err(|_| status::FAULT)?;
    side.call(nr::JOB_SET_LIMIT, &[reg(job), resource, LIMIT_AT])
}

/// Require `got` to be `ACCESS_DENIED`.
fn denied(got: Result<usize, Errno>, what: &'static str) -> Checked<u32> {
    if got == Err(status::ACCESS_DENIED) {
        Ok(1)
    } else {
        Err(what)
    }
}

/// The native side, as the delegatee in `side`. Answers the refusals.
fn natively(harness: &Harness, side: &Side) -> Checked<u32> {
    let dirfd = native_check::descriptor(harness, side, b"/check-l")?;
    let asked = side.call(
        nr::JOB_FOR_CGROUP,
        &[dirfd, u64::from((Rights::MANAGE | Rights::SET_LIMIT).0)],
    );
    let mut refused = denied(
        asked,
        "job_for_cgroup gave SET_LIMIT to a delegatee that may not write the limit files",
    )?;
    let job = side.handle(
        nr::JOB_FOR_CGROUP,
        &[dirfd, u64::from(SAME_RIGHTS)],
        "job_for_cgroup refused uid 1000 its own delegated cgroup",
    )?;
    if job_rights(side, job) != Some(DELEGATED) {
        crate::console::println!(
            "  limits   the delegatee's handle to its own job carries {:?}",
            job_rights(side, job)
        );
        return Err("job_for_cgroup gave the delegatee other rights than MANAGE without SET_LIMIT");
    }
    for (resource, limit, what) in [
        (
            JOB_MEMORY,
            UNLIMITED,
            "a delegatee raised its own cgroup's memory limit with job_set_limit",
        ),
        (
            JOB_TASKS,
            UNLIMITED,
            "a delegatee raised its own cgroup's task limit with job_set_limit",
        ),
        (
            JOB_CPU_WEIGHT,
            10_000,
            "a delegatee raised its own cgroup's processor weight with job_set_limit",
        ),
    ] {
        refused += denied(set_limit(side, job, resource, limit), what)?;
    }
    refused += denied(
        side.call(
            nr::HANDLE_DUPLICATE,
            &[reg(job), u64::from((DELEGATED | Rights::SET_LIMIT).0)],
        ),
        "a duplicate of a job handle gained SET_LIMIT",
    )?;
    let same = side.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(job), u64::from(SAME_RIGHTS)],
        "a duplicate of the delegatee's job handle was refused",
    )?;
    refused += denied(
        set_limit(side, same, JOB_MEMORY, UNLIMITED),
        "a duplicate of the delegatee's job handle set its limit",
    )?;

    let made = side.handle(
        nr::JOB_CREATE,
        &[reg(job)],
        "the delegatee could not make a job inside its own",
    )?;
    if !job_rights(side, made).is_some_and(|rights| rights.contains(Rights::SET_LIMIT)) {
        return Err("a job the delegatee made does not carry SET_LIMIT");
    }
    if set_limit(side, made, JOB_MEMORY, ITS_OWN) != Ok(0) {
        return Err("the delegatee could not limit a job it made");
    }
    Ok(refused)
}

/// The files: an open for writing of each limit file of its own cgroup, as
/// the delegatee, is `EACCES`. Answers the refusals.
fn through_the_files(harness: &Harness) -> Checked<u32> {
    let mut user = harness.ns.context();
    user.who = Access::user(DELEGATE, DELEGATE);
    let write = OpenFlags {
        write: true,
        ..OpenFlags::default()
    };
    let mut refused = 0;
    for tail in [
        &b"/check-l/memory.max"[..],
        b"/check-l/pids.max",
        b"/check-l/cpu.weight",
    ] {
        let opened = harness
            .ns
            .open(&user, None, &Harness::path(tail), &write, 0);
        if opened.err() != Some(Errno::EACCES) {
            return Err("a delegatee opened one of its own cgroup's limit files for writing");
        }
        refused += 1;
    }
    Ok(refused)
}
