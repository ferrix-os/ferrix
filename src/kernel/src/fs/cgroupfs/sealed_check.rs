//! The sealed job (`H.OBJ.22`, `L.object.248`-`251`; `docs/INTEL-GPU.md`
//! §12.11 S1, F-92): a job made with `job_create`'s `JOB_SEALED` keeps what
//! is in it, takes nothing in but by native `process_create`, and its
//! members start nothing outside it, whoever asks.
//!
//! A check process, making its calls through the native dispatch as devmgr
//! would, makes a sealed job inside a cgroup of its own, one more inside
//! that without asking, and a cgroup by `mkdir` inside it: both read as
//! sealed. A process is put in the sealed job by `process_create`'s way.
//! Root's writes of its pid into the parent's `cgroup.procs` (out), into
//! the inner job's (within the tree) and of an outside process's pid into
//! the sealed job's (in) are refused `EPERM`, and every process is still
//! where it was. A `CLONE_INTO_CGROUP` start by a member of the sealed job
//! into an unsealed cgroup is refused, and into the inner job allowed; one
//! by an outside process into the sealed job is refused. A second check
//! process put in the sealed job is refused `process_create` into an
//! outside job with `ACCESS_DENIED`, and gets past that refusal into its own
//! inner job. An unknown `job_create` option is still `INVALID_ARGS`. That
//! `drivers.slice` is sealed is the sysfs check's, which runs once devmgr
//! has.

use alloc::format;
use alloc::sync::Arc;

use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::SAME_RIGHTS;
use ferrix_native_abi::status;
use ferrix_native_abi::types::JOB_SEALED;
use ferrix_vfs::OpenFlags;

use super::native_check::{descriptor, held};
use super::{Checked, Harness};
use crate::object::check::{SCRATCH, Side, reg};
use crate::object::job::{Job, KILLED_STATUS};
use crate::syscall::process;

/// Where the second check process keeps a name for `process_create`.
const NAME_AT: u64 = SCRATCH + 0x600;

/// The jobs the check makes: the cgroup's, the sealed one and its inner.
struct Jobs {
    cgroup: Handle,
    sealed: Arc<Job>,
    sealed_handle: Handle,
    inner: Arc<Job>,
}

/// The check; its refusals are counted with the others.
///
/// # Errors
///
/// The first thing that was not as `H.OBJ.22` has it, by name.
pub(super) fn run(harness: &mut Harness) -> Checked<()> {
    harness
        .mkdir(b"/check-s")
        .map_err(|_| "mkdir of the sealed check's cgroup failed")?;
    harness.report.made += 1;
    let side = Side::new()?;
    let jobs = make(harness, &side)?;
    check_moves(harness, &jobs)?;
    check_clones(harness, &side, &jobs)?;
    check_process_create(harness, &side, &jobs)?;
    drop(jobs);
    side.close_everything();
    drop(side);
    harness
        .rmdir(b"/check-s")
        .map_err(|_| "the sealed check's cgroup did not empty")
}

/// The sealed job, the job made inside it without asking, and a cgroup made
/// inside it by `mkdir`, each read as sealed; an unknown option refused.
///
/// Verifies: L.object.250
fn make(harness: &mut Harness, side: &Side) -> Checked<Jobs> {
    let dirfd = descriptor(harness, side, b"/check-s")?;
    let cgroup = side.handle(
        nr::JOB_FOR_CGROUP,
        &[dirfd, u64::from(SAME_RIGHTS)],
        "job_for_cgroup refused root the sealed check's cgroup",
    )?;
    let unknown = side.call(nr::JOB_CREATE, &[reg(cgroup), JOB_SEALED << 1]);
    harness.refused(
        unknown.err(),
        status::INVALID_ARGS,
        "job_create took an option no kernel defines",
    )?;
    let sealed_handle = side.handle(
        nr::JOB_CREATE,
        &[reg(cgroup), JOB_SEALED],
        "job_create with JOB_SEALED failed",
    )?;
    let (sealed, _) = held(side, sealed_handle)?;
    let inner_handle = side.handle(
        nr::JOB_CREATE,
        &[reg(sealed_handle), 0],
        "job_create inside a sealed job failed",
    )?;
    let (inner, _) = held(side, inner_handle)?;
    let (outside, _) = held(side, cgroup)?;
    if !sealed.sealed() || !inner.sealed() || outside.sealed() {
        return Err("JOB_SEALED, or a job made inside a sealed one, did not seal exactly");
    }
    let made = format!("/check-s/job-{}/sub", sealed.id());
    harness
        .mkdir(made.as_bytes())
        .map_err(|_| "mkdir inside a sealed job failed")?;
    let child = sealed
        .children()
        .map_err(|_| "no memory to list a sealed job's children")?
        .into_iter()
        .find(|job| job.name() == Some("sub"));
    if !child.is_some_and(|job| job.sealed()) {
        return Err("a cgroup made by mkdir inside a sealed job is not sealed");
    }
    harness
        .rmdir(made.as_bytes())
        .map_err(|_| "rmdir of a cgroup inside a sealed job failed")?;
    Ok(Jobs {
        cgroup,
        sealed,
        sealed_handle,
        inner,
    })
}

/// A started process moved by root out of, within and into a sealed tree:
/// each refused `EPERM`, each process left where it was.
///
/// Verifies: H.OBJ.22, L.object.248
fn check_moves(harness: &mut Harness, jobs: &Jobs) -> Checked<()> {
    let member = process::new_for_check().map_err(|_| "could not make a member")?;
    member
        .move_new_to(&jobs.sealed)
        .map_err(|_| "a sealed job refused process_create's way in")?;
    let outsider = process::new_for_check().map_err(|_| "could not make an outsider")?;
    let pid = format!("{}\n", member.pid());
    let other = format!("{}\n", outsider.pid());
    let inner = format!(
        "/check-s/job-{}/job-{}/cgroup.procs",
        jobs.sealed.id(),
        jobs.inner.id()
    );
    let into = format!("/check-s/job-{}/cgroup.procs", jobs.sealed.id());
    for (tail, who, what) in [
        (
            &b"/check-s/cgroup.procs"[..],
            &pid,
            "root's write moved a process out of a sealed job",
        ),
        (
            inner.as_bytes(),
            &pid,
            "root's write moved a process within a sealed tree",
        ),
        (
            into.as_bytes(),
            &other,
            "root's write moved a process into a sealed job",
        ),
    ] {
        let wrote = harness.write(tail, who.as_bytes());
        harness.refused(wrote.err(), Errno::EPERM, what)?;
    }
    if !Arc::ptr_eq(&member.job(), &jobs.sealed) || Arc::ptr_eq(&outsider.job(), &jobs.sealed) {
        return Err("a refused move moved a process all the same");
    }
    process::kill(&member, KILLED_STATUS);
    drop((member, outsider));
    Ok(())
}

/// `CLONE_INTO_CGROUP`'s rule, through the predicate `clone3` asks: a
/// sealed member starts only within its tree, and nobody starts in a
/// sealed job from outside it.
///
/// Verifies: L.object.249
fn check_clones(harness: &mut Harness, side: &Side, jobs: &Jobs) -> Checked<()> {
    let (outside, _) = held(side, jobs.cgroup)?;
    if jobs.sealed.may_start_in(&outside, false) {
        return Err("a sealed member may start a process outside its tree");
    }
    if !jobs.sealed.may_start_in(&jobs.inner, false) {
        return Err("a sealed member may not start a process inside its own tree");
    }
    if outside.may_start_in(&jobs.sealed, false) {
        return Err("a process outside may clone into a sealed job");
    }
    let directory = format!("/check-s/job-{}", jobs.sealed.id());
    let flags = OpenFlags {
        path: true,
        directory: true,
        ..OpenFlags::default()
    };
    let file = harness
        .ns
        .open(
            &harness.ctx,
            None,
            &Harness::path(directory.as_bytes()),
            &flags,
            0,
        )
        .map_err(|_| "the sealed job's directory did not open")?;
    let to = super::clone_target(&file, &side.process, &outside)
        .map_err(|_| "clone_target refused root a cgroup it may write")?;
    if !Arc::ptr_eq(&to, &jobs.sealed) {
        return Err("clone_target answered another job");
    }
    harness.report.refusals += 3;
    Ok(())
}

/// A member of the sealed job asks `process_create` for a process outside
/// its tree, refused `ACCESS_DENIED` before anything is made, and one in
/// its inner job, which gets past that refusal to the image it was given.
///
/// Verifies: L.object.251
fn check_process_create(harness: &mut Harness, side: &Side, jobs: &Jobs) -> Checked<()> {
    let member = Side::new()?;
    member
        .process
        .move_new_to(&jobs.sealed)
        .map_err(|_| "a sealed job refused a check process")?;
    let mine = give(side, &member, jobs.sealed_handle)?;
    let theirs = give(side, &member, jobs.cgroup)?;
    let inner = member.handle(nr::JOB_CREATE, &[reg(mine), 0], "a member made no job")?;
    let image = member.handle(nr::VMO_CREATE, &[4096], "a member made no VMO")?;
    member.put(NAME_AT, b"sealed")?;
    let outside = member.call(nr::PROCESS_CREATE, &[reg(theirs), reg(image), NAME_AT, 6]);
    harness.refused(
        outside.err(),
        status::ACCESS_DENIED,
        "a sealed member's process_create outside its tree was not ACCESS_DENIED",
    )?;
    let within = member.call(nr::PROCESS_CREATE, &[reg(inner), reg(image), NAME_AT, 6]);
    if within == Err(status::ACCESS_DENIED) {
        return Err("a sealed member's process_create inside its tree was refused as outside");
    }
    member.close_everything();
    process::kill(&member.process, KILLED_STATUS);
    drop(member);
    Ok(())
}

/// A duplicate of `side`'s `handle`, put in `member`'s table.
fn give(side: &Side, member: &Side, handle: Handle) -> Checked<Handle> {
    let (job, rights) = held(side, handle)?;
    member
        .process
        .with_handles(|table| table.insert(crate::object::Object::Job(job), rights))
        .map_err(|_| "a member's table had no room")
}
