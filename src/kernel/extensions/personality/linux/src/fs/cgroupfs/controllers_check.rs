//! The `cpu`, `memory` and `pids` controllers through cgroupfs: the files a
//! Linux program sees are the job's quota (`object::quota`, `FRU_RSA.1`).
//!
//! The root enables the three, and a child cgroup gets their files and
//! `cgroup.controllers` lists them. `pids.max`, `memory.max` and `cpu.weight`
//! take what Linux takes and refuse what it refuses, and read back as Linux
//! prints them; each write reaches the job, as a native `job_get_quota`
//! would read it. A process moved in counts in `pids.current`, and a process
//! forked there past `pids.max` is refused and counted in `pids.events`. The
//! root then disables them again, so nothing after sees them on.

use alloc::sync::Arc;

use ferrix_linux_abi::errno::Errno;

use super::{Checked, Harness};
use crate::object::job::{Job, KILLED_STATUS};
use crate::object::quota::{self, Resource};
use crate::syscall::process::{self, Process};
use crate::syscall::registry;
use crate::user::space::AddressSpace;

/// Run it. How many writes were refused as Linux refuses them.
pub(super) fn run(harness: &mut Harness, process: &Arc<Process>) -> Checked<u32> {
    let refusals = harness.report.refusals;
    let _ = harness
        .write(b"/cgroup.subtree_control", b"+pids +memory +cpu\n")
        .map_err(|_| "the root refused to enable cpu, memory and pids")?;
    harness
        .mkdir(b"/check-q")
        .map_err(|_| "mkdir of a cgroup for the controllers failed")?;
    harness.report.made += 1;
    if !harness.reads(b"/check-q/cgroup.controllers", b"cpu memory pids\n") {
        return Err("a child of the root does not list cpu, memory and pids as its controllers");
    }
    check_the_files(harness)?;
    check_a_fork_past_pids_max(harness, process)?;
    harness
        .rmdir(b"/check-q")
        .map_err(|_| "rmdir of the controllers' cgroup failed")?;
    let _ = harness
        .write(b"/cgroup.subtree_control", b"-pids -memory -cpu\n")
        .map_err(|_| "the root refused to disable cpu, memory and pids")?;
    if !harness.reads(b"/cgroup.subtree_control", b"\n") {
        return Err("the root's controllers were not all disabled again");
    }
    Ok(harness.report.refusals - refusals)
}

/// Each limit file takes, refuses and reads back as Linux's does, and the
/// job reads what was written.
///
/// Verifies: L.object.52, L.object.61
fn check_the_files(harness: &mut Harness) -> Checked<()> {
    for (file, written, read) in [
        (&b"/check-q/pids.max"[..], &b"max\n"[..], &b"max\n"[..]),
        (b"/check-q/pids.max", b"3\n", b"3\n"),
        (b"/check-q/memory.max", b"1M\n", b"1048576\n"),
        (b"/check-q/memory.max", b"max", b"max\n"),
        (b"/check-q/memory.max", b"8193", b"8192\n"),
        (b"/check-q/cpu.weight", b"300\n", b"300\n"),
    ] {
        let _ = harness
            .write(file, written)
            .map_err(|_| "a controller file refused what Linux takes")?;
        if !harness.reads(file, read) {
            return Err("a controller file does not read back what was written as Linux prints it");
        }
    }
    for (file, written, errno) in [
        (&b"/check-q/pids.max"[..], &b"-1\n"[..], Errno::EINVAL),
        (b"/check-q/pids.max", b"4194305", Errno::EINVAL),
        (b"/check-q/memory.max", b"1Q", Errno::EINVAL),
        (b"/check-q/cpu.weight", b"0", Errno::ERANGE),
        (b"/check-q/cpu.weight", b"10001", Errno::ERANGE),
        (b"/check-q/pids.current", b"1", Errno::EACCES),
    ] {
        let refused = harness.write(file, written).err();
        harness.refused(refused, errno, "a controller file took what Linux refuses")?;
    }
    let job = job_of(harness, b"check-q")?;
    let limit = |resource| job.usage(resource).map(|usage| usage.limit);
    if limit(Resource::Tasks) != Some(3) || limit(Resource::Memory) != Some(8192) {
        return Err("pids.max and memory.max did not reach the job's quota");
    }
    if job.cpu_weight() != 300 {
        return Err("cpu.weight did not reach the job's weight");
    }
    if !harness.reads(b"/check-q/pids.current", b"0\n")
        || !harness.reads(b"/check-q/memory.current", b"0\n")
        || !harness.reads(b"/check-q/pids.events", b"max 0\n")
    {
        return Err("an empty cgroup's current and events do not read zero");
    }
    let _ = job.set_limit(Resource::Memory, quota::UNLIMITED);
    Ok(())
}

/// A process moved in counts, one forked there past `pids.max` is refused
/// and counted, and it all leaves with the process.
fn check_a_fork_past_pids_max(harness: &mut Harness, process: &Arc<Process>) -> Checked<()> {
    let _ = harness
        .write(b"/check-q/pids.max", b"2\n")
        .map_err(|_| "pids.max refused a count")?;
    let member = process::fork_for_check(process).map_err(|_| "no process to move")?;
    let listed = alloc::format!("{}\n", member.pid());
    let _ = harness
        .write(b"/check-q/cgroup.procs", listed.as_bytes())
        .map_err(|_| "a move into a cgroup with pids.max failed")?;
    let mut children = alloc::vec::Vec::new();
    loop {
        let space = AddressSpace::new().map_err(|_| "no address space for a fork")?;
        let child = registry::register(
            Process::forked(&member, space, false, false).map_err(|_| "no memory for a fork")?,
        );
        if child.over_quota() {
            break;
        }
        children.push(child);
        if children.len() > 2 {
            return Err("forks went past pids.max");
        }
    }
    if children.len() != 1
        || !harness.reads(b"/check-q/pids.current", b"2\n")
        || !harness.reads(b"/check-q/pids.events", b"max 1\n")
    {
        return Err("a fork past pids.max was not refused at it, or not counted in pids.events");
    }
    for each in children.iter().chain([&member]) {
        process::kill(each, KILLED_STATUS);
    }
    drop((children, member));
    if !harness.reads(b"/check-q/pids.current", b"0\n") {
        return Err("processes gone and still counted in pids.current");
    }
    Ok(())
}

/// The job behind the cgroup `name` beneath the check's mount.
fn job_of(harness: &Harness, name: &[u8]) -> Checked<Arc<Job>> {
    let mut path = alloc::vec::Vec::from(&b"/"[..]);
    path.extend_from_slice(name);
    let file = harness
        .ns
        .open(
            &harness.ctx,
            None,
            &Harness::path(&path),
            &ferrix_vfs::OpenFlags {
                read: true,
                directory: true,
                ..ferrix_vfs::OpenFlags::default()
            },
            0,
        )
        .map_err(|_| "a cgroup directory did not open")?;
    super::directory_job(&file)
        .map(|(job, _)| job)
        .ok_or("a cgroup directory has no job behind it")
}
