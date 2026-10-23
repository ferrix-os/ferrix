//! Stage 13's native side of a cgroup, landing G5 (`docs/CGROUPS.md` §5): a
//! native service manager holds the job behind a cgroup and waits for it to
//! empty, as a Linux one waits on `cgroup.events`.
//!
//! A check process, making its calls through the native dispatch as a program
//! would, opens a cgroup's directory and asks `job_for_cgroup` for its job:
//! as root it gets `WAIT` and `MANAGE`, as uid 1000, who may only read the
//! directory's `cgroup.procs`, `WAIT` and not `MANAGE`. A descriptor not
//! open, and one of a directory that is not a cgroup, are refused.
//!
//! Then `EMPTY`. The job asserts it while nothing is in it, stops when two
//! members are moved in, and a port registration for it stays quiet through
//! the first member's release and fires at the last's, at once, with
//! `cgroup.events` already saying `populated 0`. A registration made while
//! the job is empty fires as it is made. A native job made inside the
//! cgroup's job shows as `job-<id>` beside it, keeps the cgroup from being
//! removed, and goes when its handle is closed.

use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::{Rights, SAME_RIGHTS};
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types::PACKET_SIGNAL;
use ferrix_vfs::OpenFlags;

use super::{Checked, Harness};
use crate::object::Object;
use crate::object::check::{SCRATCH, Side, reg};
use crate::object::job::{Job, KILLED_STATUS};
use crate::syscall::process::{self, Process};

/// A wait's deadline, in the check process's scratch region; clear of what
/// the stage 9 checks keep there, since each has a process of its own.
const DEADLINE: u64 = SCRATCH + 0x400;
/// The signals a wait observed.
const OBSERVED: u64 = SCRATCH + 0x408;
/// A registration's key.
const KEY: u64 = SCRATCH + 0x410;
/// A packet taken from the port.
const PACKET_AT: u64 = SCRATCH + 0x420;

/// The key the `EMPTY` registration carries.
const EMPTY_KEY: u64 = 0xE3_0070;

/// The user the check asks as when it may only read `cgroup.procs`.
const READER: u32 = 1000;

/// What a populated cgroup's `cgroup.events` says.
const EMPTY_EVENTS: &[u8] = b"populated 0\nfrozen 0\n";

/// The check. Answers how many native waits for `EMPTY` the populated flip
/// fired: the registration made before the flip, and the one made after.
///
/// # Errors
///
/// The first thing that was not as `docs/CGROUPS.md` §5 has it, by name.
pub(super) fn run(harness: &mut Harness) -> Checked<u32> {
    harness
        .mkdir(b"/check-n")
        .map_err(|_| "mkdir of the native check's cgroup failed")?;
    harness.report.made += 1;
    let side = Side::new()?;
    let dirfd = descriptor(harness, &side, b"/check-n")?;
    let job = check_job_for_cgroup(harness, &side, dirfd)?;
    let fired = check_empty(harness, &side, job)?;
    check_a_native_child_is_shown(harness, &side, job)?;
    side.close_everything();
    drop(side);
    harness
        .rmdir(b"/check-n")
        .map_err(|_| "the native check's cgroup did not empty")?;
    Ok(fired)
}

/// A descriptor in `side`'s table on the directory at `tail` beneath the
/// mount, opened `O_PATH`, as a service manager holds a cgroup it made.
pub(super) fn descriptor(harness: &Harness, side: &Side, tail: &[u8]) -> Checked<u64> {
    descriptor_at(harness, side, &Harness::path(tail))
}

/// [`descriptor`], for a whole path.
fn descriptor_at(harness: &Harness, side: &Side, whole: &[u8]) -> Checked<u64> {
    let flags = OpenFlags {
        path: true,
        directory: true,
        ..OpenFlags::default()
    };
    let file = harness
        .ns
        .open(&harness.ctx, None, whole, &flags, 0)
        .map_err(|_| "the native check could not open a directory")?;
    let fd = side
        .process
        .files()
        .lock()
        .insert(file, false)
        .map_err(|_| "the native check's process had no room for a descriptor")?;
    u64::try_from(fd).map_err(|_| "a descriptor was negative")
}

/// The rights the handle `handle` of `side` carries, and its job.
pub(super) fn held(side: &Side, handle: Handle) -> Checked<(Arc<Job>, Rights)> {
    side.process.with_handles(|table| match table.get(handle) {
        Ok((Object::Job(job), rights)) => Ok((Arc::clone(job), rights)),
        _ => Err("job_for_cgroup answered a handle that names no job"),
    })
}

/// Require `got` to be refused with `wanted`, and count it.
fn refused(
    harness: &mut Harness,
    got: Result<usize, Errno>,
    wanted: Errno,
    what: &'static str,
) -> Checked<()> {
    harness.refused(got.err(), wanted, what)
}

/// `job_for_cgroup` answers the job behind the directory, with the rights
/// its `cgroup.procs` allows the caller, and refuses what is not a cgroup.
/// Answers the root's handle, which carries every right.
fn check_job_for_cgroup(harness: &mut Harness, side: &Side, dirfd: u64) -> Checked<Handle> {
    let same = u64::from(SAME_RIGHTS);
    let not_open = side.call(nr::JOB_FOR_CGROUP, &[99, same]);
    refused(
        harness,
        not_open,
        status::BAD_HANDLE,
        "job_for_cgroup of a descriptor not open was not BAD_HANDLE",
    )?;
    let tmp = descriptor_at(harness, side, b"/tmp")?;
    let not_a_cgroup = side.call(nr::JOB_FOR_CGROUP, &[tmp, same]);
    refused(
        harness,
        not_a_cgroup,
        status::WRONG_TYPE,
        "job_for_cgroup of a directory that is not a cgroup was not WRONG_TYPE",
    )?;
    let unknown = side.call(nr::JOB_FOR_CGROUP, &[dirfd, 1 << 20]);
    refused(
        harness,
        unknown,
        status::INVALID_ARGS,
        "job_for_cgroup took a right no kernel defines",
    )?;

    let job = side.handle(
        nr::JOB_FOR_CGROUP,
        &[dirfd, same],
        "job_for_cgroup refused root a cgroup it made",
    )?;
    let (found, rights) = held(side, job)?;
    if found.name() != Some("check-n") {
        return Err("job_for_cgroup answered a job other than the cgroup's");
    }
    if rights != Rights::JOB {
        return Err("job_for_cgroup gave root other rights than a job's own");
    }

    // As uid 1000, who may read root's `cgroup.procs` (0644) and not write
    // it: a handle to wait on, and none to kill with.
    let before = side.process.with_credentials(|credentials| {
        let before = credentials.clone();
        credentials.user.filesystem = READER;
        credentials.group.filesystem = READER;
        credentials.groups = Vec::new();
        before
    });
    let manage = u64::from(Rights::MANAGE.0);
    let asked_to_manage = side.call(nr::JOB_FOR_CGROUP, &[dirfd, manage]);
    let reader = side.handle(
        nr::JOB_FOR_CGROUP,
        &[dirfd, same],
        "job_for_cgroup refused a user who may read cgroup.procs",
    );
    side.process
        .with_credentials(|credentials| *credentials = before);
    refused(
        harness,
        asked_to_manage,
        status::ACCESS_DENIED,
        "job_for_cgroup gave MANAGE to a user who may not write cgroup.procs",
    )?;
    let (_, rights) = held(side, reader?)?;
    if rights != Rights::DUPLICATE | Rights::TRANSFER | Rights::WAIT {
        return Err("job_for_cgroup gave a reader of cgroup.procs other rights than WAIT's");
    }
    Ok(job)
}

/// Wait on `job` for `EMPTY` until now, as `object_wait_one` does: whether
/// it is asserted, and what else was.
fn empty_now(side: &Side, job: Handle) -> Checked<(bool, Signals)> {
    side.put(DEADLINE, &crate::timer::now_nanos().to_ne_bytes())?;
    let waited = side.call(
        nr::OBJECT_WAIT_ONE,
        &[reg(job), u64::from(Signals::EMPTY.0), DEADLINE, OBSERVED],
    );
    let observed = Signals(side.get_u32(OBSERVED)?);
    match waited {
        Ok(_) => Ok((true, observed)),
        Err(status::TIMED_OUT) => Ok((false, observed)),
        Err(_) => Err("object_wait_one on a cgroup's job failed"),
    }
}

/// A packet taken from `port` without waiting: its key, kind and signals,
/// or `None` when there is none.
fn packet_now(side: &Side, port: Handle) -> Checked<Option<(u64, u32, u32)>> {
    side.put(DEADLINE, &crate::timer::now_nanos().to_ne_bytes())?;
    match side.call(nr::PORT_WAIT, &[reg(port), DEADLINE, PACKET_AT]) {
        Ok(_) => {}
        Err(status::TIMED_OUT) => return Ok(None),
        Err(_) => return Err("port_wait on the EMPTY registration's port failed"),
    }
    let bytes = side.get(PACKET_AT, 16)?;
    let key = bytes
        .get(..8)
        .and_then(|slice| <[u8; 8]>::try_from(slice).ok())
        .map(u64::from_ne_bytes);
    let half = |at: usize| {
        bytes
            .get(at..at + 4)
            .and_then(|slice| <[u8; 4]>::try_from(slice).ok())
            .map(u32::from_ne_bytes)
    };
    match (key, half(8), half(12)) {
        (Some(key), Some(kind), Some(signals)) => Ok(Some((key, kind, signals))),
        _ => Err("a packet read back short"),
    }
}

/// Register for `EMPTY` on `job` through `port`.
fn register_for_empty(side: &Side, job: Handle, port: Handle) -> Checked<()> {
    side.put(KEY, &EMPTY_KEY.to_ne_bytes())?;
    side.call(
        nr::OBJECT_WAIT_ASYNC,
        &[reg(job), reg(port), u64::from(Signals::EMPTY.0), KEY],
    )
    .map(|_| ())
    .map_err(|_| "object_wait_async for EMPTY on a cgroup's job failed")
}

/// Whether a packet is the `EMPTY` registration's.
fn is_empty_packet(packet: Option<(u64, u32, u32)>) -> bool {
    packet == Some((EMPTY_KEY, PACKET_SIGNAL, Signals::EMPTY.0))
}

/// Move a new process into `/check-n` by its pid.
fn member(harness: &Harness) -> Checked<Arc<Process>> {
    let process =
        process::new_for_check().map_err(|_| "could not make a process for the EMPTY check")?;
    let listed = format!("{}\n", process.pid());
    let _ = harness
        .write(b"/check-n/cgroup.procs", listed.as_bytes())
        .map_err(|_| "writing a pid to cgroup.procs failed")?;
    Ok(process)
}

/// `EMPTY` is a level: asserted while the job is empty, not while it has a
/// member; a registration fires at the last release and not the first, with
/// the populated flip; one made while empty fires at once. Answers how many
/// registrations fired.
///
/// Verifies: L.object.74
fn check_empty(harness: &mut Harness, side: &Side, job: Handle) -> Checked<u32> {
    match empty_now(side, job)? {
        (true, observed) if !observed.intersects(Signals::TERMINATED) => {}
        (true, _) => return Err("an empty cgroup's job says it was terminated"),
        (false, _) => return Err("a cgroup nothing was put in does not assert EMPTY"),
    }
    let first = member(harness)?;
    let last = member(harness)?;
    if empty_now(side, job)?.0 {
        return Err("a cgroup with two members still asserts EMPTY");
    }

    let port = side.handle(nr::PORT_CREATE, &[], "port_create failed")?;
    register_for_empty(side, job, port)?;
    if packet_now(side, port)?.is_some() {
        return Err("a registration for EMPTY fired on a populated cgroup");
    }
    process::kill(&first, KILLED_STATUS);
    if packet_now(side, port)?.is_some() {
        return Err("a registration for EMPTY fired while a member was still live");
    }
    process::kill(&last, KILLED_STATUS);
    // At once: the release that emptied the job fired it, not a later look.
    let packet = packet_now(side, port)?;
    if !is_empty_packet(packet) {
        return Err("the last member's release fired no EMPTY packet with its key");
    }
    if !harness.reads(b"/check-n/cgroup.events", EMPTY_EVENTS) {
        return Err("EMPTY fired and cgroup.events does not say populated 0");
    }
    if !empty_now(side, job)?.0 {
        return Err("an emptied cgroup's job does not assert EMPTY");
    }
    register_for_empty(side, job, port)?;
    if !is_empty_packet(packet_now(side, port)?) {
        return Err("a registration for EMPTY on an empty cgroup did not fire at once");
    }
    drop((first, last));
    Ok(2)
}

/// A job native `job_create` makes inside a cgroup's job is shown beside it
/// as `job-<id>`, keeps the cgroup from being removed, is itself refused
/// `rmdir`, and goes when its handle is closed.
///
/// Verifies: L.object.68
fn check_a_native_child_is_shown(harness: &mut Harness, side: &Side, job: Handle) -> Checked<()> {
    let child = side.handle(
        nr::JOB_CREATE,
        &[reg(job)],
        "job_create inside a cgroup's job failed",
    )?;
    let (made, _) = held(side, child)?;
    let tail = format!("/check-n/job-{}", made.id());
    drop(made);
    if !harness.exists(tail.as_bytes()) {
        return Err("a native job made inside a cgroup is not shown as job-<id>");
    }
    let busy = harness.rmdir(b"/check-n");
    harness.refused(
        busy.err(),
        Errno::EBUSY,
        "rmdir of a cgroup holding a native job was not EBUSY",
    )?;
    let anonymous = harness.rmdir(tail.as_bytes());
    harness.refused(
        anonymous.err(),
        Errno::EBUSY,
        "rmdir of a native job's directory was not EBUSY",
    )?;
    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(child)])
        .map_err(|_| "handle_close of a native job failed")?;
    if harness.exists(tail.as_bytes()) {
        return Err("a native job is still shown after its last handle closed");
    }
    Ok(())
}
