//! The job quotas `FRU_RSA.1` claims, proved at boot (`object::quota`,
//! `docs/certification/IMPLEMENTATION.md` W-13).
//!
//! Every limit is driven through the path a program's use takes, not only
//! through the counters: tasks by making and forking processes, memory by
//! faulting a user space in, objects by making them, the processor by
//! spinning tasks in two jobs on one processor. Each check refuses at
//! exactly its limit, leaves a sibling job untouched, and ends with every
//! counter back at zero and every slot the jobs took given back.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_vma::VmaFlags;

use crate::object::job::{Job, KILLED_STATUS};
use crate::object::oom;
use crate::object::port::Port;
use crate::object::quota::{self, DEFAULT_WEIGHT, MAX_WEIGHT, MIN_WEIGHT, Resource};
use crate::sched::{self, Task};
use crate::syscall::process::{self, Process};
use crate::syscall::registry;
use crate::user::space::{Access, AddressSpace};

/// What the checks saw, for the boot line.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// The task limit a fork bomb met, and how many processes it made.
    pub(crate) tasks: u64,
    /// The memory limit a user space was faulted in to, in pages.
    pub(crate) pages: u64,
    /// How many of them were page tables.
    pub(crate) tables: u64,
    /// Bytes of kernel heap the limited job's space held beside them: its
    /// regions, charged as memory too.
    pub(crate) heap: u64,
    /// Pages a sibling committed after the limited job was refused.
    pub(crate) sibling_pages: u64,
    /// The object limit the objects met.
    pub(crate) objects: u64,
    /// The share of a processor, in tenths of a per cent, one task in its
    /// own job had against eight in another.
    pub(crate) alone_share: u64,
}

/// Run every quota check.
///
/// # Errors
///
/// Which property failed.
///
/// Verifies: L.object.53, H.QUOTA.4
pub(crate) fn run() -> Result<Report, &'static str> {
    let slots = quota::live_slots();
    let mut report = Report::default();
    {
        let tree = Job::new_root().map_err(|_| "no memory for a job")?;
        check_the_counters(&tree)?;
        report.tasks = check_a_fork_bomb_meets_its_limit(&tree)?;
        check_memory(&tree, &mut report)?;
        report.objects = check_objects(&tree)?;
        report.alone_share = check_the_processor(&tree)?;
        check_the_weight(&tree)?;
        check_an_ended_victim_is_emptied(&tree)?;
        check_a_move_settles_the_pending_charge(&tree)?;
        if Resource::ALL
            .iter()
            .any(|&resource| tree.usage(resource).is_none_or(|usage| usage.used != 0))
        {
            return Err("a job tree the checks emptied still holds something");
        }
    }
    if quota::live_slots() != slots {
        return Err("the checks' jobs are gone and their quota slots are not");
    }
    Ok(report)
}

/// A child's use is its parent's too; a limit refuses at exactly its value,
/// anywhere above; a refusal takes nothing; everything comes back.
///
/// Verifies: L.object.50, L.object.51, L.object.64
fn check_the_counters(tree: &Arc<Job>) -> Result<(), &'static str> {
    let parent = tree.new_child().map_err(|_| "a job refused a child")?;
    let child = parent.new_child().map_err(|_| "a job refused a child")?;
    for resource in Resource::ALL {
        let _ = child.set_limit(resource, 5);
        let _ = parent.set_limit(resource, 3);
        let used = |job: &Job| job.usage(resource).map_or(u64::MAX, |usage| usage.used);
        for _ in 0..3 {
            quota::charge(child.quota_index(), resource, 1)
                .map_err(|_| "a charge within every limit was refused")?;
        }
        if quota::charge(child.quota_index(), resource, 1).is_ok() {
            return Err("a charge past the parent's limit went through the child");
        }
        if used(&child) != 3 || used(&parent) != 3 || used(tree) != 3 {
            return Err("a refused charge was counted somewhere on the way up");
        }
        if parent.usage(resource).map(|usage| usage.refused) != Some(1)
            || child.usage(resource).map(|usage| usage.refused) != Some(0)
        {
            return Err("a refusal was not counted where the limit was");
        }
        let _ = parent.set_limit(resource, quota::UNLIMITED);
        quota::charge(child.quota_index(), resource, 2)
            .map_err(|_| "a charge up to the child's own limit was refused")?;
        if quota::charge(child.quota_index(), resource, 1).is_ok() {
            return Err("a charge past the child's own limit went through");
        }
        quota::uncharge(child.quota_index(), resource, 5);
        if used(&child) != 0 || used(&parent) != 0 || used(tree) != 0 {
            return Err("a job charged and uncharged the same does not read zero");
        }
    }
    Ok(())
}

/// A task limit refuses a fork loop at the limit, counting the forking
/// process, while a sibling job forks on; and a move takes its charge with
/// it.
///
/// Verifies: L.object.57, L.object.81, L.object.83, L.object.85, H.QUOTA.1
fn check_a_fork_bomb_meets_its_limit(tree: &Arc<Job>) -> Result<u64, &'static str> {
    const LIMIT: u64 = 8;
    let bomb = tree.new_child().map_err(|_| "a job refused a child")?;
    let sibling = tree.new_child().map_err(|_| "a job refused a child")?;
    let _ = bomb.set_limit(Resource::Tasks, LIMIT);
    let _ = sibling.set_limit(Resource::Tasks, LIMIT);
    let parent = member_of(&bomb)?;
    let mut children = Vec::new();
    let made = loop {
        let child = fork(&parent)?;
        if child.over_quota() {
            break children.len() as u64 + 1;
        }
        children.push(child);
        if children.len() as u64 > LIMIT {
            return Err("a fork loop went past its job's task limit");
        }
    };
    let tasks = |job: &Job| job.usage(Resource::Tasks).map_or(0, |usage| usage.used);
    if made != LIMIT || tasks(&bomb) != LIMIT {
        return Err("a fork loop was not refused at exactly its job's task limit");
    }
    if bomb.usage(Resource::Tasks).map(|usage| usage.refused) != Some(1) {
        return Err("the refused fork was not counted as pids.events counts it");
    }
    // A sibling is not held back by it.
    let other = member_of(&sibling)?;
    let forked = fork(&other)?;
    if forked.over_quota() || tasks(&sibling) != 2 {
        return Err("a job at its task limit held back its sibling");
    }
    // A move takes a process's charge with it, whatever the limit there.
    forked
        .move_to(&bomb)
        .map_err(|_| "a move into a full job was refused")?;
    if tasks(&bomb) != LIMIT + 1 || tasks(&sibling) != 1 {
        return Err("a move did not take its task charge with it");
    }
    for process in children.iter().chain([&parent, &forked, &other]) {
        process::kill(process, KILLED_STATUS);
    }
    drop(children);
    drop((parent, forked, other));
    if tasks(&bomb) != 0 || tasks(&sibling) != 0 || tasks(tree) != 0 {
        return Err("processes gone and their tasks still charged");
    }
    Ok(made)
}

/// A process of the check's own, moved into `job` as `process_create`
/// moves a new one.
fn member_of(job: &Arc<Job>) -> Result<Arc<Process>, &'static str> {
    let member = process::new_for_check().map_err(|_| "no process for the quota checks")?;
    member
        .move_new_to(job)
        .map_err(|_| "a job refused a process within its limit")?;
    Ok(member)
}

/// A fork of `parent`, as `fork` makes one, with no task.
fn fork(parent: &Arc<Process>) -> Result<Arc<Process>, &'static str> {
    let space = AddressSpace::new().map_err(|_| "no address space for a fork")?;
    let child = Process::forked(parent, space, false, false).map_err(|_| "no memory for a fork")?;
    Ok(registry::register(child))
}

/// Faulting a user space in as a task of a limited job is refused at its
/// memory limit, pages and page tables together, while a sibling's faults go
/// on; and every frame comes back as the spaces go.
///
/// Verifies: H.QUOTA.2, L.mm.8, L.object.55, L.user.99
fn check_memory(tree: &Arc<Job>, report: &mut Report) -> Result<(), &'static str> {
    const LIMIT: u64 = 48;
    const BASE: u64 = 0x40_0000;
    const BYTES: u64 = LIMIT * PAGE_SIZE;
    let hog = tree.new_child().map_err(|_| "a job refused a child")?;
    let sibling = tree.new_child().map_err(|_| "a job refused a child")?;
    let _ = hog.set_limit(Resource::Memory, BYTES);
    let used = |job: &Job, resource| job.usage(resource).map_or(0, |usage| usage.used);
    // Frames, a page each: what is charged as memory and is not heap.
    let pages = |job: &Job| {
        used(job, Resource::Memory).saturating_sub(used(job, Resource::Kernel)) / PAGE_SIZE
    };

    let (faulted, refused) = as_task_of(&hog, || fault_in(BASE, LIMIT + 8));
    let (faulted, space) = faulted?;
    // The space's regions are heap charged to the job too (F-37), so the
    // frames stop short of the limit by what they hold: refused at exactly
    // the limit means the next page would not have fitted.
    let memory = used(&hog, Resource::Memory);
    if !refused || memory > BYTES || memory + PAGE_SIZE <= BYTES || faulted >= LIMIT {
        return Err("faults were not refused at exactly their job's memory limit");
    }
    if hog.usage(Resource::Memory).map(|usage| usage.refused) == Some(0) {
        return Err("a refused fault was not counted as memory.events counts it");
    }
    report.pages = pages(&hog);
    report.tables = report.pages.saturating_sub(faulted);
    report.heap = used(&hog, Resource::Kernel);

    let (other, _) = as_task_of(&sibling, || fault_in(BASE, LIMIT));
    let (other, other_space) = other?;
    if other != LIMIT || pages(&sibling) < LIMIT {
        return Err("a job at its memory limit held back its sibling");
    }
    report.sibling_pages = other;

    drop((space, other_space));
    if pages(&hog) != 0 || pages(&sibling) != 0 || pages(tree) != 0 {
        return Err("address spaces gone and their frames still charged");
    }
    if [&hog, &sibling, tree]
        .iter()
        .any(|job| used(job, Resource::Memory) != 0 || used(job, Resource::Kernel) != 0)
    {
        return Err("address spaces gone and the heap of their regions still charged");
    }
    Ok(())
}

/// Map `count` pages at `base` in a new space and fault each in by a write,
/// until one is refused. How many went in, and the space.
fn fault_in(base: u64, count: u64) -> Result<(u64, Arc<AddressSpace>), &'static str> {
    let space = AddressSpace::new().map_err(|_| "no address space for the memory check")?;
    let _ = space
        .map_anonymous(base, count * PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "no mapping for the memory check")?;
    let faulted = (0..count)
        .take_while(|&page| space.fault(base + page * PAGE_SIZE, Access::WRITE).is_ok())
        .count() as u64;
    Ok((faulted, space))
}

/// Run `work` charged to `job`, as a task of it would be; and whether
/// `job`'s memory limit refused anything meanwhile.
fn as_task_of<T>(job: &Job, work: impl FnOnce() -> T) -> (T, bool) {
    let before = job.usage(Resource::Memory).map_or(0, |usage| usage.refused);
    let own = sched::running_group();
    sched::set_current_group(job.quota_index());
    let done = work();
    sched::set_current_group(own);
    let after = job.usage(Resource::Memory).map_or(0, |usage| usage.refused);
    (done, after != before)
}

/// An object limit refuses the object past it, a channel's two ends
/// together, and an object gone makes room.
///
/// Verifies: L.object.9, L.object.27, L.object.59, H.QUOTA.3
fn check_objects(tree: &Arc<Job>) -> Result<u64, &'static str> {
    const LIMIT: u64 = 5;
    let job = tree.new_child().map_err(|_| "a job refused a child")?;
    let _ = job.set_limit(Resource::Objects, LIMIT);
    let objects = |job: &Job| job.usage(Resource::Objects).map_or(0, |usage| usage.used);
    let (made, _) = as_task_of(&job, || {
        let mut ports = Vec::new();
        while let Ok(port) = Port::new() {
            ports.push(port);
            if ports.len() as u64 > LIMIT {
                break;
            }
        }
        let full = ports.len() as u64;
        let pair_refused = crate::object::channel::Endpoint::pair().is_err();
        let _ = ports.pop();
        let _ = ports.pop();
        let pair = crate::object::channel::Endpoint::pair().ok();
        (full, pair_refused, pair, ports)
    });
    let (full, pair_refused, pair, ports) = made;
    if full != LIMIT || !pair_refused || pair.is_none() || objects(&job) != LIMIT {
        return Err("objects were not refused at exactly their job's object limit");
    }
    drop((pair, ports));
    if objects(&job) != 0 {
        return Err("objects gone and still charged");
    }
    Ok(full)
}

/// Spinners stop when this is set.
static STOP: AtomicBool = AtomicBool::new(false);
/// Spinners running.
static SPINNING: AtomicU64 = AtomicU64::new(0);

/// A task that spins until told to stop.
fn spin(_argument: usize) {
    let _ = SPINNING.fetch_add(1, Ordering::AcqRel);
    while !STOP.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
}

/// How many tasks spin in the crowded job.
const CROWD: usize = 8;
/// How long the shares are measured over.
const WINDOW_NANOS: u64 = 400_000_000;

/// One task in its own job keeps about half a processor against eight in
/// another, where per-task shares would give it a ninth; and the jobs' loads
/// are empty once the tasks are gone. The share, in tenths of a per cent.
///
/// Verifies: L.object.60
fn check_the_processor(tree: &Arc<Job>) -> Result<u64, &'static str> {
    let crowd = tree.new_child().map_err(|_| "a job refused a child")?;
    let alone = tree.new_child().map_err(|_| "a job refused a child")?;
    let cpu = crate::smp::topology().map_or(0, |topology| topology.online().saturating_sub(1));
    STOP.store(false, Ordering::Release);
    SPINNING.store(0, Ordering::Release);
    let mut tasks: Vec<Arc<Task>> = Vec::new();
    for index in 0..=CROWD {
        let group = if index < CROWD {
            crowd.quota_index()
        } else {
            alone.quota_index()
        };
        tasks.push(sched::spawn_in_group("quota", spin, index, cpu, group)?);
    }
    let started = crate::timer::now_nanos();
    while SPINNING.load(Ordering::Acquire) <= CROWD as u64 {
        if crate::timer::now_nanos().saturating_sub(started) > 5_000_000_000 {
            STOP.store(true, Ordering::Release);
            return Err("a spinner of the processor check never started");
        }
        sched::sleep_for(1_000_000);
    }
    // Long enough for every spinner to have run under its job's share.
    sched::sleep_for(50_000_000);
    let before: Vec<u64> = tasks.iter().map(|task| task.runtime()).collect();
    sched::sleep_for(WINDOW_NANOS);
    let had: Vec<u64> = tasks
        .iter()
        .zip(&before)
        .map(|(task, before)| task.runtime().saturating_sub(*before))
        .collect();
    STOP.store(true, Ordering::Release);
    for task in &tasks {
        sched::wait_until_gone(task, 5_000_000_000)?;
    }
    drop(tasks);
    let crowded: u64 = had.iter().take(CROWD).sum();
    let lone = had.get(CROWD).copied().unwrap_or(0);
    let share = lone.saturating_mul(1000) / crowded.saturating_add(lone).max(1);
    if !(350..=650).contains(&share) {
        crate::console::println!(
            "  quota    one task alone in its job had {lone} ns against {crowded} ns for eight \
             in another: {share} per mille, where the job's weight gives about 500"
        );
        return Err("a job with many spinning tasks took more than its share from another job");
    }
    if [&crowd, &alone, tree]
        .iter()
        .any(|job| quota::load(job.quota_index()) != 0)
    {
        return Err("tasks gone and still counted in their jobs' processor load");
    }
    Ok(share)
}

/// A job's weight as its parent's load counts it: `cpu.weight` 100 is one
/// task at nice 0.
fn scaled(weight: u32) -> i64 {
    i64::from(weight) * i64::from(ferrix_sched::NICE_0_WEIGHT) / i64::from(DEFAULT_WEIGHT)
}

/// `cpu.weight` is clamped to 1 to 10,000, and setting a busy job's weight
/// changes its parent's load at once, by the difference of the two weights
/// as the scheduler scales them, while an idle job's changes nothing.
///
/// The largest, in task units, is below 2^32, which `quota::effective`'s
/// 64-bit arithmetic rests on (OPAQUE-KERNEL.md §9.8, 2f, Q7).
///
/// Verifies: L.object.62
/// Verifies: L.object.161
fn check_the_weight(tree: &Arc<Job>) -> Result<(), &'static str> {
    let parent = tree.new_child().map_err(|_| "a job refused a child")?;
    let busy = parent.new_child().map_err(|_| "a job refused a child")?;
    let _ = busy.set_cpu_weight(0);
    let low = busy.cpu_weight();
    let _ = busy.set_cpu_weight(20_000);
    let high = busy.cpu_weight();
    if low != MIN_WEIGHT || high != MAX_WEIGHT {
        return Err("cpu.weight was not clamped to 1 to 10,000");
    }
    if u32::try_from(scaled(high)).is_err() {
        return Err("the largest cpu.weight is not below 2^32 in task units");
    }
    let _ = busy.set_cpu_weight(DEFAULT_WEIGHT);
    if quota::load(parent.quota_index()) != 0 {
        return Err("an idle job's weight counted in its parent's load");
    }

    let cpu = crate::smp::topology().map_or(0, |topology| topology.online().saturating_sub(1));
    STOP.store(false, Ordering::Release);
    SPINNING.store(0, Ordering::Release);
    let task = sched::spawn_in_group("weight", spin, 0, cpu, busy.quota_index())?;
    let started = crate::timer::now_nanos();
    while SPINNING.load(Ordering::Acquire) == 0 {
        if crate::timer::now_nanos().saturating_sub(started) > 5_000_000_000 {
            STOP.store(true, Ordering::Release);
            return Err("the weight check's spinner never started");
        }
        sched::sleep_for(1_000_000);
    }
    let before = quota::load(parent.quota_index());
    let _ = busy.set_cpu_weight(4 * DEFAULT_WEIGHT);
    let after = quota::load(parent.quota_index());
    STOP.store(true, Ordering::Release);
    sched::wait_until_gone(&task, 5_000_000_000)?;
    drop(task);
    if before != scaled(DEFAULT_WEIGHT) {
        return Err("a busy job's weight was not its parent's load");
    }
    if after != scaled(4 * DEFAULT_WEIGHT) {
        return Err("a busy job's new weight did not reach its parent's load at once");
    }
    if quota::load(parent.quota_index()) != 0 || quota::load(busy.quota_index()) != 0 {
        return Err("a task gone and its job still counted in the processor load");
    }
    Ok(())
}

/// A fault at a full memory limit whose victim is still ending waits and
/// kills nobody else, and once the victim has let go of everything, the next
/// fault empties its space -- before any parent's wait -- and goes in.
///
/// Driven a step at a time through [`oom::out_of_memory`], as a fault's
/// retries drive it: the victim is a process of the check's own, kept
/// ending by a thread it counts until the check lets it go.
///
/// Verifies: L.object.103
fn check_an_ended_victim_is_emptied(tree: &Arc<Job>) -> Result<(), &'static str> {
    const VICTIM_PAGES: u64 = 32;
    const BASE: u64 = 0x40_0000;
    let job = tree.new_child().map_err(|_| "a job refused a child")?;
    let victim = member_of(&job)?;
    let faulter = member_of(&job)?;
    let used = |job: &Job| job.usage(Resource::Memory).map_or(0, |usage| usage.used);
    let (checked, _) = as_task_of(&job, || -> Result<(), &'static str> {
        let _ = victim
            .space()
            .map_anonymous(BASE, VICTIM_PAGES * PAGE_SIZE, VmaFlags::READ_WRITE)
            .map_err(|_| "no mapping for the OOM check's victim")?;
        for page in 0..VICTIM_PAGES {
            victim
                .space()
                .fault(BASE + page * PAGE_SIZE, Access::WRITE)
                .map_err(|_| "the OOM check's victim could not fault its memory in")?;
        }
        let _ = faulter
            .space()
            .map_anonymous(BASE, PAGE_SIZE, VmaFlags::READ_WRITE)
            .map_err(|_| "no mapping for the OOM check's faulter")?;
        // Full: the faulter's first frame is past the limit.
        let _ = job.set_limit(Resource::Memory, used(&job));
        let full = used(&job);
        let refused = || {
            faulter
                .space()
                .fault(BASE, Access::WRITE)
                .is_err_and(|error| oom::is_charge_refusal(&error))
        };
        let kills = oom::kills();
        victim.thread_starting();

        if !refused() || oom::out_of_memory(faulter.space()) != oom::Answer::Retry {
            return Err("a fault at a full limit did not kill and retry");
        }
        if !victim.is_terminated() || faulter.is_terminated() {
            return Err("the scoped OOM kill did not choose the process with the most resident");
        }
        if !refused() || oom::out_of_memory(faulter.space()) != oom::Answer::Retry {
            return Err("a fault finding its job's victim still ending did not wait and retry");
        }
        if oom::kills().wrapping_sub(kills) != 1 || faulter.is_terminated() {
            return Err("a fault while the victim was ending killed a second process");
        }
        if used(&job) != full {
            return Err("an ending victim's memory was given back before it let go");
        }

        victim.thread_gone(true);
        if !refused() || oom::out_of_memory(faulter.space()) != oom::Answer::Retry {
            return Err("a fault after the victim let go did not retry");
        }
        if used(&job).saturating_add(VICTIM_PAGES * PAGE_SIZE) > full {
            return Err("an ended victim's memory was not given back before its parent's wait");
        }
        faulter
            .space()
            .fault(BASE, Access::WRITE)
            .map_err(|_| "the fault after the victim's memory came back was refused")?;
        Ok(())
    });
    for process in [&victim, &faulter] {
        process::kill(process, KILLED_STATUS);
    }
    drop((victim, faulter));
    checked?;
    if used(&job) != 0 {
        return Err("the OOM check's processes gone and their memory still charged");
    }
    Ok(())
}

/// A running task that moves to another job leaves nothing of its processor
/// time kept back for the job it left (`quota::Pending`), and no hold on that
/// job's slot: the job gone, its slot is free, though the task was charged to
/// it a moment before it moved and is not charged again yet. And a read of
/// `cpu.stat` (`sched::charge_running`) counts what the queue keeps back
/// before anything else moves it: nothing switches while this runs masked.
///
/// Verifies: L.object.180
fn check_a_move_settles_the_pending_charge(tree: &Arc<Job>) -> Result<(), &'static str> {
    use ferrix_sync::IrqControl;
    let slots = quota::live_slots();
    let job = tree.new_child().map_err(|_| "a job refused a child")?;
    let index = job.quota_index();
    let own = sched::running_group();
    // Masked, so that the task stays on the processor whose queue it asks.
    let saved = <crate::arch::Irq as IrqControl>::disable();
    sched::set_current_group(index);
    let start = crate::timer::now_nanos();
    while crate::timer::now_nanos() == start {
        core::hint::spin_loop();
    }
    // Charges the running task, to `job`, and keeps it back.
    let _ = sched::current_runtime();
    let named = sched::pending_group_here();
    sched::charge_running();
    let (user, system) = job.cpu_times();
    sched::set_current_group(own);
    let after = sched::pending_group_here();
    <crate::arch::Irq as IrqControl>::restore(saved);
    drop(job);
    if named != index {
        return Err("a running task's processor time was not kept back for its job");
    }
    if user.saturating_add(system) == 0 {
        return Err("a read of cpu.stat did not count the processor time a run queue kept back");
    }
    if after == index {
        return Err("a running task that left a job left its processor time kept back for it");
    }
    if quota::live_slots() != slots {
        return Err("a job a running task left is gone and its quota slot is not");
    }
    Ok(())
}
