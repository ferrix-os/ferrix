//! The self-check of jobs, quotas and a process's core with kernel objects
//! alone: `Job`, `Quota`, and a core `Process` over an empty `AddressSpace`, no
//! personality and no file system. Its caller is
//! `syscall::rehome_check`, and it builds in the core's crate, so
//! that boundary itself says it names no load (`docs/microkernel/B5.md`, class
//! A).
//!
//! The requirements here were verified before only from the personality's
//! checks, by way of cgroupfs and programs, which leave ring 0 with the load.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals;
use ferrix_vma::VmaFlags;

use crate::object::Object;
use crate::object::job::{Budget, Job, JobError};
use crate::object::port::{Observer, Port};
use crate::object::process::{self, Bootstrap, GiveRefused, Host, Process};
use crate::object::quota::{self, Counter, Pending, Quota, Resource};
use crate::user::space::{Access, AddressSpace};

/// How many cases one run makes: the boot line's count.
const CASES: u64 = 19;

/// A nanosecond figure of a millisecond.
const MS: u64 = 1_000_000;

/// Run them, and answer how many cases they are. `Err` names the first thing
/// that was not true.
///
/// Called by `syscall::rehome_check`.
///
/// # Errors
///
/// The first thing that was not so, by name.
// CHECKS-ONLY: syscall/rehome_check.rs
pub fn run() -> Result<u64, &'static str> {
    check_limits_and_usage()?;
    check_the_weight()?;
    check_the_kernel_charge()?;
    check_a_busy_child_stays()?;
    check_a_removed_job_takes_nothing()?;
    check_empty()?;
    check_oom_counts()?;
    check_a_bootstrap_is_given_once()?;
    check_exit_reports_once_ended()?;
    check_a_charge_reaches_the_tree()?;
    check_throttling()?;
    check_a_slot_says_its_job_went()?;
    check_a_table_is_charged_as_a_table()?;
    check_a_burst_is_carried()?;
    check_an_idle_job_weighs_little()?;
    check_a_seal_holds()?;
    check_may_start_in()?;
    check_sealed_tree()?;
    check_a_reaped_pid_finds_nothing()?;
    Ok(CASES)
}

/// A job under a fresh root, or the check's failure.
fn fresh() -> Result<(Arc<Job>, Arc<Job>), &'static str> {
    let root = Job::new_root().map_err(|_| "no root job for the core's object check")?;
    let child = root
        .new_child()
        .map_err(|_| "no child job for the core's object check")?;
    Ok((root, child))
}

/// A process's core in `job`, over an empty space, numbered zero.
fn member(job: &Arc<Job>) -> Result<Process, &'static str> {
    let space = AddressSpace::new().map_err(|_| "no space for a member")?;
    Process::new(space, 0, Arc::clone(job)).map_err(|_| "no memory for a member")
}

/// `set_limit` is what the next charge is judged against, and `usage` says the
/// use, the limit and the refusals.
///
/// Verifies: L.object.52
fn check_limits_and_usage() -> Result<(), &'static str> {
    let (_root, job) = fresh()?;
    let empty = job
        .usage(Resource::Objects)
        .ok_or("a job has no usage to read")?;
    if empty.used != 0 || empty.refused != 0 {
        return Err("an empty job's usage reads something used or refused");
    }
    if !job.set_limit(Resource::Objects, 2) {
        return Err("a job refused a limit");
    }
    let slot = job.quota_index();
    if quota::charge(slot, Resource::Objects, 2).is_err() {
        return Err("a charge within the limit just set was refused");
    }
    if quota::charge(slot, Resource::Objects, 1).is_ok() {
        return Err("a charge past the limit just set was taken");
    }
    let usage = job
        .usage(Resource::Objects)
        .ok_or("a job has no usage to read")?;
    quota::uncharge(slot, Resource::Objects, 2);
    if usage.used != 2 {
        return Err("usage did not report the use");
    }
    if usage.limit != 2 {
        return Err("usage did not report the limit");
    }
    if usage.refused != 1 {
        return Err("usage did not report the refusals");
    }
    Ok(())
}

/// `set_cpu_weight` makes the job's weight the value written.
///
/// Verifies: L.object.61
fn check_the_weight() -> Result<(), &'static str> {
    let (_root, job) = fresh()?;
    if !job.set_cpu_weight(300) || job.cpu_weight() != 300 {
        return Err("a job's weight is not the value written");
    }
    Ok(())
}

/// Kernel heap is charged against the memory limit and counted again as kernel
/// memory, both or neither, and given back from both.
///
/// Verifies: L.object.56
fn check_the_kernel_charge() -> Result<(), &'static str> {
    let (_root, job) = fresh()?;
    let slot = job.quota_index();
    let _ = job.set_limit(Resource::Memory, 32 * 1024);
    let both = |memory: u64, kernel: u64| {
        job.usage(Resource::Memory)
            .is_some_and(|u| u.used == memory)
            && job
                .usage(Resource::Kernel)
                .is_some_and(|u| u.used == kernel)
    };
    quota::charge_kernel(slot, 4096).map_err(|_| "kernel heap within the limit was refused")?;
    if !both(4096, 4096) {
        return Err("charge_kernel did not charge both memory and kernel memory");
    }
    if quota::charge_kernel(slot, 64 * 1024).is_ok() {
        quota::uncharge_kernel(slot, 64 * 1024);
        quota::uncharge_kernel(slot, 4096);
        return Err("charge_kernel took heap past the memory limit");
    }
    if !both(4096, 4096) {
        quota::uncharge_kernel(slot, 4096);
        return Err("a refused charge_kernel charged one of the two");
    }
    quota::uncharge_kernel(slot, 4096);
    if !both(0, 0) {
        return Err("uncharge_kernel did not give back both");
    }
    Ok(())
}

/// A named child holding a live anonymous job is not removed.
///
/// Verifies: L.object.68
fn check_a_busy_child_stays() -> Result<(), &'static str> {
    let (_root, job) = fresh()?;
    let named = job
        .new_named_child("named", Budget::Parents)
        .map_err(|_| "no named child to remove")?;
    let inner = named
        .new_child()
        .map_err(|_| "no anonymous job inside a named one")?;
    if job.remove_named_child("named").map(|_| ()) != Err(JobError::Busy) {
        return Err("a named child holding a live anonymous job was removed");
    }
    drop(inner);
    if job.remove_named_child("named").is_err() {
        return Err("a named child with nothing left in it was not removed");
    }
    Ok(())
}

/// A removed job takes no process and no child.
///
/// Verifies: L.object.69
fn check_a_removed_job_takes_nothing() -> Result<(), &'static str> {
    let (_root, job) = fresh()?;
    let named = job
        .new_named_child("gone", Budget::Parents)
        .map_err(|_| "no named child to remove")?;
    let _ = job
        .remove_named_child("gone")
        .map_err(|_| "an empty named child was not removed")?;
    let process = member(&job)?;
    if process.move_to(&named).is_ok() {
        return Err("a removed job took a process");
    }
    if !Arc::ptr_eq(&process.job(), &job) {
        return Err("a refused move left its process elsewhere");
    }
    if named.new_named_child("inside", Budget::Parents).is_ok() {
        return Err("a removed job took a child");
    }
    if named.admits() != Err(JobError::Removed) {
        return Err("a removed job says it admits a process");
    }
    Ok(())
}

/// EMPTY exactly while not populated, and a registration fires at once if
/// empty and otherwise as it empties.
///
/// Verifies: L.object.74
fn check_empty() -> Result<(), &'static str> {
    let (_root, job) = fresh()?;
    if !job.signals().intersects(Signals::EMPTY) {
        return Err("an empty job does not assert EMPTY");
    }
    let port = Port::new().map_err(|_| "no port to observe a job on")?;
    let observe = || {
        Observer::new(&port, 7, Signals::EMPTY)
            .map_err(|_| "no room on the port")
            .and_then(|observer| {
                job.observe(observer)
                    .map_err(|_| "the job refused a registration")
            })
    };
    observe()?;
    if port.take().is_none() {
        return Err("a registration for EMPTY on an empty job did not fire at once");
    }
    let first = member(&job)?;
    let last = member(&job)?;
    if job.signals().intersects(Signals::EMPTY) {
        return Err("a job with members asserts EMPTY");
    }
    observe()?;
    if port.take().is_some() {
        return Err("a registration for EMPTY fired on a populated job");
    }
    first.leave_job();
    if port.take().is_some() {
        return Err("a registration for EMPTY fired while a member was still live");
    }
    last.leave_job();
    if port.take().is_none() {
        return Err("the last member's leaving fired no EMPTY registration");
    }
    if !job.signals().intersects(Signals::EMPTY) {
        return Err("a job its last member left does not assert EMPTY");
    }
    Ok(())
}

/// `count_oom` counts in the job and every job above it and no other, and wakes
/// whatever polls their `memory.events`.
///
/// Verifies: L.object.80
fn check_oom_counts() -> Result<(), &'static str> {
    let (root, job) = fresh()?;
    let sibling = root
        .new_child()
        .map_err(|_| "no sibling for the OOM counts")?;
    let (wakes_job, wakes_root) = (job.memory_events().wakes(), root.memory_events().wakes());
    let sibling_wakes = sibling.memory_events().wakes();
    job.count_oom(true);
    job.count_oom(false);
    if job.oom_counts() != (1, 1) {
        return Err("count_oom did not count in the job");
    }
    if root.oom_counts() != (1, 1) {
        return Err("count_oom did not count in the job above");
    }
    if sibling.oom_counts() != (0, 0) {
        return Err("count_oom counted in a sibling");
    }
    if job.memory_events().wakes() == wakes_job || root.memory_events().wakes() == wakes_root {
        return Err("count_oom did not wake what polls memory.events");
    }
    if sibling.memory_events().wakes() != sibling_wakes {
        return Err("count_oom woke what polls a sibling's memory.events");
    }
    Ok(())
}

/// One object, given once and handed over once.
///
/// Verifies: L.object.89
fn check_a_bootstrap_is_given_once() -> Result<(), &'static str> {
    let object = || {
        Port::new()
            .map(Object::Port)
            .map_err(|_| "no object to give")
    };
    let mut slot = Bootstrap::Open;
    if slot.refusal().is_some() || slot.take().is_some() {
        return Err("an open bootstrap slot refused, or handed something over");
    }
    if slot.give(object()?, Rights::READ).is_err() {
        return Err("the first give to a bootstrap slot was refused");
    }
    if !matches!(
        slot.give(object()?, Rights::READ),
        Err((GiveRefused::Given, _))
    ) {
        return Err("a second give before the take was not refused");
    }
    if !matches!(slot.take(), Some((Object::Port(_), Rights::READ))) {
        return Err("a bootstrap's first take did not hand over what was given");
    }
    if slot.take().is_some() {
        return Err("a bootstrap's second take handed something over");
    }
    if !matches!(
        slot.give(object()?, Rights::READ),
        Err((GiveRefused::Given, _))
    ) {
        return Err("a give after the take was not refused");
    }
    Ok(())
}

/// An `Exit` reports nothing before its end, and its status and signal after.
///
/// Verifies: L.object.94
fn check_exit_reports_once_ended() -> Result<(), &'static str> {
    let (_root, job) = fresh()?;
    let signalled = member(&job)?;
    let exit = signalled.exit();
    if exit.status().is_some() || exit.signal().is_some() {
        return Err("an end record reported something before its end");
    }
    signalled.end_record(143, 15);
    if exit.status() != Some(143) {
        return Err("an end record did not report its status once ended");
    }
    if exit.signal() != Some(15) {
        return Err("an end record did not report the signal that ended it");
    }
    let plain = member(&job)?;
    plain.end_record(9, 0);
    if plain.exit().status() != Some(9) || plain.exit().signal().is_some() {
        return Err("an end record of a plain exit reported a signal");
    }
    Ok(())
}

/// The time a task ran goes to its job, every job above it and the machine, as
/// user or system time by how the slice ended, by the time it is read.
fn check_a_charge_reaches_the_tree() -> Result<(), &'static str> {
    let parent = Quota::new(None).map_err(|_| "no quota slot")?;
    let child = Quota::new(Some(&parent)).map_err(|_| "no quota slot")?;
    let machine = quota::machine_cpu();
    let mut pending = Pending::new();
    let now = crate::timer::now_nanos();
    quota::charge_cpu(&mut pending, child.index(), now, 1_000, true);
    quota::charge_cpu(&mut pending, child.index(), now, 20, false);
    quota::settle(&mut pending);
    let (child_user, child_system) = child.cpu_times();
    let (parent_user, parent_system) = parent.cpu_times();
    let after = quota::machine_cpu();
    quota::leaving(&mut pending);
    if (child_user, child_system) != (1_000, 20) {
        return Err("charge_cpu did not charge user and system time to the task's job");
    }
    if (parent_user, parent_system) != (1_000, 20) {
        return Err("charge_cpu did not charge the job above it");
    }
    if after.0 < machine.0 + 1_000 || after.1 < machine.1 + 20 {
        return Err("charge_cpu did not charge the machine");
    }
    Ok(())
}

/// A job whose subtree has used its quota is throttled until the period ends.
fn check_throttling() -> Result<(), &'static str> {
    let parent = Quota::new(None).map_err(|_| "no quota slot")?;
    let child = Quota::new(Some(&parent)).map_err(|_| "no quota slot")?;
    parent.set_bandwidth(20 * MS, 100 * MS);
    let mut pending = Pending::new();
    let now = crate::timer::now_nanos();
    let outcome = (|| {
        quota::charge_cpu(&mut pending, child.index(), now, 5 * MS, true);
        if quota::runtime_left(child.index(), now + 1) != Some(15 * MS) {
            return Err("runtime_left did not read the quota less what was used");
        }
        if quota::throttled_until(child.index(), now + 1).is_some() {
            return Err("a job within its cpu.max quota was reported throttled");
        }
        quota::charge_cpu(&mut pending, child.index(), now + 2, 15 * MS, true);
        let until = quota::throttled_until(child.index(), now + 3);
        if until != Some(now + 100 * MS) {
            return Err(
                "a job's subtree that used its quota was not throttled to the period's end",
            );
        }
        if parent.counted(Counter::Throttled) != 1 {
            return Err("a throttled period was not counted");
        }
        if quota::throttled_until(child.index(), now + 100 * MS).is_some() {
            return Err("a job was still throttled in the next period");
        }
        Ok(())
    })();
    quota::leaving(&mut pending);
    parent.set_bandwidth(quota::UNLIMITED, quota::DEFAULT_PERIOD_NS);
    outcome
}

/// `job_ended` is true from the moment the job goes, though a task holds the
/// slot, and false once the slot is claimed anew.
///
/// Verifies: L.object.110
fn check_a_slot_says_its_job_went() -> Result<(), &'static str> {
    let job = Quota::new(None).map_err(|_| "no quota slot")?;
    let index = job.index();
    if quota::job_ended(index) {
        return Err("job_ended was true of a live job");
    }
    quota::hold_group(index);
    drop(job);
    let ended = quota::job_ended(index);
    quota::release_group(index);
    if !ended {
        return Err("job_ended was false of a slot a task holds once its job went");
    }

    // The slot goes round: claimed anew, it says so.
    for _ in 0..4096 {
        let quota = Quota::new(None).map_err(|_| "no quota slot")?;
        if quota.index() == index {
            if quota::job_ended(index) {
                return Err("job_ended was still true of a slot claimed anew");
            }
            return Ok(());
        }
    }
    Err("a released quota slot was not claimed anew in 4096 claims")
}

/// A program's page table is charged as memory and as a table, and a frame's
/// free gives back a table's two charges and a page's one.
///
/// Verifies: L.object.182
fn check_a_table_is_charged_as_a_table() -> Result<(), &'static str> {
    const PAGES: u64 = 4;
    let (_root, job) = fresh()?;
    let slot = job.quota_index();
    let used = |resource| job.usage(resource).map_or(u64::MAX, |usage| usage.used);
    crate::sched::set_current_group(slot);
    let outcome = (|| {
        let space = AddressSpace::new().map_err(|_| "no space to charge tables to")?;
        let at = space
            .map_anywhere(None, PAGES * PAGE_SIZE, VmaFlags::READ_WRITE)
            .map_err(|_| "no region to fault")?;
        let before = used(Resource::Memory);
        for page in 0..PAGES {
            space
                .fault(at + page * PAGE_SIZE, Access::WRITE)
                .map_err(|_| "a fault in a job failed")?;
        }
        let tables = used(Resource::Tables);
        let memory = used(Resource::Memory);
        if tables < PAGE_SIZE {
            return Err("a program's page table was not counted as a table");
        }
        if memory < before + PAGES * PAGE_SIZE + tables {
            return Err("a program's page table was not charged as memory beside its pages");
        }
        space
            .unmap(at, PAGES * PAGE_SIZE)
            .map_err(|_| "the faulted region would not unmap")?;
        drop(space);
        Ok(())
    })();
    crate::sched::set_current_group(quota::NONE);
    outcome?;
    if used(Resource::Tables) != 0 {
        return Err("a table's free did not give back its table charge");
    }
    if used(Resource::Memory) != 0 {
        return Err("a frame's free did not give back its memory charge");
    }
    Ok(())
}

/// The burst's carry, charged at chosen instants, on a slot of its own.
///
/// Verifies: L.object.184
fn check_a_burst_is_carried() -> Result<(), &'static str> {
    quota::check_burst_carry(crate::timer::now_nanos())
}

/// A job set idle has the entity weight 3, and its own again once let off.
///
/// Verifies: L.object.185
fn check_an_idle_job_weighs_little() -> Result<(), &'static str> {
    quota::check_idle_weight()
}

/// A started process moves out of or into a sealed job by no move, and stays.
///
/// Verifies: H.OBJ.22
fn check_a_seal_holds() -> Result<(), &'static str> {
    let root = Job::new_root().map_err(|_| "no root job")?;
    let sealed = root
        .new_child_as(false, true)
        .map_err(|_| "no sealed job")?;
    let inner = sealed
        .new_child()
        .map_err(|_| "no job inside a sealed one")?;
    let open = root.new_child().map_err(|_| "no open job")?;
    let inside = member(&sealed)?;
    let outside = member(&open)?;
    if inside.move_to(&open) != Err(JobError::Sealed) {
        return Err("a started process moved out of a sealed job");
    }
    if inside.move_to(&inner) != Err(JobError::Sealed) {
        return Err("a started process moved within a sealed job's tree");
    }
    if outside.move_to(&sealed) != Err(JobError::Sealed) {
        return Err("a started process moved into a sealed job");
    }
    if !Arc::ptr_eq(&inside.job(), &sealed) || !Arc::ptr_eq(&outside.job(), &open) {
        return Err("a refused move left its process elsewhere");
    }
    Ok(())
}

/// A sealed job's members start only within its tree, and into a sealed job
/// from outside only native `process_create` starts.
///
/// Verifies: L.object.249, H.OBJ.22
fn check_may_start_in() -> Result<(), &'static str> {
    let root = Job::new_root().map_err(|_| "no root job")?;
    let sealed = root
        .new_child_as(false, true)
        .map_err(|_| "no sealed job")?;
    let inner = sealed
        .new_child()
        .map_err(|_| "no job inside a sealed one")?;
    let open = root.new_child().map_err(|_| "no open job")?;
    if sealed.may_start_in(&open, true) || sealed.may_start_in(&open, false) {
        return Err("a sealed member may start a process outside its tree");
    }
    if !sealed.may_start_in(&inner, false) || !sealed.may_start_in(&sealed, false) {
        return Err("a sealed member may not start a process inside its own tree");
    }
    if open.may_start_in(&sealed, false) {
        return Err("a start into a sealed job from outside it was allowed, not native");
    }
    if !open.may_start_in(&sealed, true) {
        return Err("native process_create's start into a sealed job was refused");
    }
    Ok(())
}

/// A job made inside a sealed job is sealed, and the job it was made under is
/// not unsealed or sealed by it.
///
/// Verifies: H.OBJ.22
fn check_sealed_tree() -> Result<(), &'static str> {
    let root = Job::new_root().map_err(|_| "no root job")?;
    let sealed = root
        .new_child_as(false, true)
        .map_err(|_| "no sealed job")?;
    let plain = sealed
        .new_child()
        .map_err(|_| "no job inside a sealed one")?;
    let named = sealed
        .new_named_child("inner", Budget::Parents)
        .map_err(|_| "no named job inside a sealed one")?;
    if !sealed.sealed() {
        return Err("a job made with the seal does not read as sealed");
    }
    if !plain.sealed() || !named.sealed() {
        return Err("a job made inside a sealed job does not read as sealed");
    }
    if root.sealed() {
        return Err("the job a sealed job was made in reads as sealed");
    }
    Ok(())
}

/// A host of the check's own: a core process and what a personality would keep
/// beside it, which is nothing. For the process table's lookup alone.
#[derive(Debug)]
struct Listed {
    /// The core process.
    core: Arc<Process>,
    /// Calls that reached it, none being expected.
    calls: AtomicU64,
}

impl Host for Listed {
    fn core(&self) -> &Process {
        &self.core
    }

    fn core_arc(&self) -> &Arc<Process> {
        &self.core
    }

    fn kill(&self, _status: i32) {
        let _ = self.calls.fetch_add(1, Ordering::Relaxed);
    }

    fn thread_starting(&self) {
        let _ = self.calls.fetch_add(1, Ordering::Relaxed);
    }

    fn thread_gone(&self, _ended: bool) {
        let _ = self.calls.fetch_add(1, Ordering::Relaxed);
    }

    fn wait_interrupted(&self) -> bool {
        false
    }
}

/// Once unlisted a pid finds nothing and `listed` leaves it out; `live` still
/// lists it, and the number stays taken until the process is dropped.
///
/// Verifies: L.object.181
fn check_a_reaped_pid_finds_nothing() -> Result<(), &'static str> {
    let (_root, job) = fresh()?;
    let pid = process::allocate().ok_or("no pid for the table's case")?;
    let space = AddressSpace::new().map_err(|_| "no space for the table's case")?;
    let core = Process::new(space, pid, job).map_err(|_| "no memory for the table's case")?;
    let core = crate::fallible::try_arc(core).map_err(|_| "no memory for the table's case")?;
    let host = crate::fallible::try_arc(Listed {
        core,
        calls: AtomicU64::new(0),
    })
    .map_err(|_| "no memory for the table's case")?;
    let dynamic: Arc<dyn Host> = Arc::clone(&host) as Arc<dyn Host>;
    process::name(pid, Arc::downgrade(&dynamic)).map_err(|_| "the pid would not name its host")?;
    let lists = |all: bool| {
        let found = if all {
            process::live()
        } else {
            process::listed()
        };
        found.map(|hosts| hosts.iter().any(|host| host.core().pid() == pid))
    };
    if process::find(pid).is_none() || lists(false) != Ok(true) {
        return Err("a process named by its pid was not found before its reap");
    }
    process::unlist(host.as_ref());
    if process::find(pid).is_some() {
        return Err("find answered a pid after its process was unlisted");
    }
    if lists(false) != Ok(false) {
        return Err("listed kept a process after it was unlisted");
    }
    if lists(true) != Ok(true) {
        return Err("live left out a process not yet dropped");
    }
    if process::is_free(pid) {
        return Err("an unlisted process's pid was free before it was dropped");
    }
    drop(dynamic);
    // The host's own reference is the core's last: a process gives its number
    // back as it is dropped.
    drop(host);
    if !process::is_free(pid) {
        return Err("a dropped process's pid was not free");
    }
    Ok(())
}
