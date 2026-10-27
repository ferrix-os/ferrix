//! The borrowed running task's check (OPAQUE-KERNEL.md §9.8, 2a): across
//! switches made by a check's tasks, and at every site the audit looks from
//! boot to here, the processor record names the task its run queue holds.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_sched_fair::NICE_0_WEIGHT;
use ferrix_sync::IrqControl;

use super::Task;
use super::borrow::{self, Site};
use crate::smp::Topology;

/// Resumptions after a switch the check's tasks must make between them.
const SWITCHES: u64 = 10_000;

/// Tasks per processor.
const TASKS_PER_CPU: usize = 2;

/// How long the check waits for its tasks.
const PATIENCE_NANOS: u64 = 60_000_000_000;

/// Resumptions after a switch so far.
static RESUMED: AtomicU64 = AtomicU64::new(0);

/// Tasks that have finished.
static FINISHED: AtomicU64 = AtomicU64::new(0);

/// Set by a task whose borrow named another task than its own.
static NOT_ITSELF: AtomicBool = AtomicBool::new(false);

/// The audit's two sites that are made once each, before any check runs:
/// the boot task's adoption and each idle task's first run. Looked at first,
/// because every check after this one finds the running task through the
/// record, and a record the boot write missed would fail the first of them
/// on its own words instead of this one's.
///
/// # Errors
///
/// The site that was never audited or saw a mismatch, as a sentence.
///
/// Verifies: L.sched.24
pub(super) fn installed(topology: &Topology) -> Result<(), &'static str> {
    let (boot, boot_bad) = borrow::audited(Site::Boot);
    let (idle, idle_bad) = borrow::audited(Site::Idle);
    if boot_bad != 0 {
        report();
        return Err("the record did not name the boot task once it was adopted");
    }
    if idle_bad != 0 {
        report();
        return Err("the record did not name an idle task as it first ran");
    }
    if boot == 0 {
        return Err("the boot task's adoption was never audited");
    }
    // Every secondary's idle task has run by now; the boot processor's may
    // not have yet, and is required at the end of stage 5 (`run`).
    if idle < (topology.online() as u64).saturating_sub(1) {
        return Err("an idle task's first run went unaudited");
    }
    Ok(())
}

/// Run it, report the audit and end it.
///
/// # Errors
///
/// The first thing that failed, as a sentence.
///
/// Verifies: L.sched.24
/// Verifies: L.sched.25
pub(super) fn run(topology: &Topology) -> Result<(), &'static str> {
    let result = the_record_names_the_running_task(topology);
    report();
    borrow::end_audit();
    result?;
    for (site, mismatch, never) in [
        (
            Site::Boot,
            "the record did not name the boot task once it was adopted",
            "the boot task's adoption was never audited",
        ),
        (
            Site::Idle,
            "the record did not name an idle task as it first ran",
            "no idle task's first run was audited",
        ),
        (
            Site::Switch,
            "the record did not name the task a switch had just installed",
            "no switch was audited",
        ),
        (
            Site::Interrupt,
            "the record did not name the queue's current task at an interrupt's exit",
            "no interrupt's exit was audited",
        ),
        (
            Site::Task,
            "the record did not name the queue's current task as a task resumed",
            "no task's resumption was audited",
        ),
    ] {
        let (audits, mismatches) = borrow::audited(site);
        if mismatches != 0 {
            return Err(mismatch);
        }
        if audits == 0 {
            return Err(never);
        }
    }
    let (idle, _) = borrow::audited(Site::Idle);
    if idle < topology.online() as u64 {
        return Err("an idle task's first run went unaudited");
    }
    if NOT_ITSELF.load(Ordering::Acquire) {
        return Err("a borrow lent a task other than the one running");
    }
    Ok(())
}

/// Print what the audit saw, site by site.
fn report() {
    let line = |site| borrow::audited(site);
    let (boot, boot_bad) = line(Site::Boot);
    let (idle, idle_bad) = line(Site::Idle);
    let (switch, switch_bad) = line(Site::Switch);
    let (interrupt, interrupt_bad) = line(Site::Interrupt);
    let (task, task_bad) = line(Site::Task);
    crate::console::println!(
        "  borrow   audits (mismatches): boot {boot} ({boot_bad}), idle {idle} ({idle_bad}), \
         switch {switch} ({switch_bad}), interrupt {interrupt} ({interrupt_bad}), task {task} \
         ({task_bad}); {} resumptions",
        RESUMED.load(Ordering::Relaxed),
    );
}

/// Two tasks a processor yield to each other until they have resumed after
/// a switch [`SWITCHES`] times between them, each audit made at resumption,
/// under the queue's lock, and each borrow compared with the task's own
/// `Arc`.
fn the_record_names_the_running_task(topology: &Topology) -> Result<(), &'static str> {
    RESUMED.store(0, Ordering::Release);
    FINISHED.store(0, Ordering::Release);
    NOT_ITSELF.store(false, Ordering::Release);
    let tasks = topology.online().saturating_mul(TASKS_PER_CPU);
    let mut running: Vec<Arc<Task>> = Vec::new();
    for index in 0..tasks {
        running.push(super::spawn(
            "check-borrow",
            yield_and_audit,
            index,
            NICE_0_WEIGHT,
        )?);
    }
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while FINISHED.load(Ordering::Acquire) < tasks as u64 {
        if crate::timer::now_nanos() >= deadline {
            return Err("a task of the borrow check never finished");
        }
        super::sleep_for(1_000_000);
    }
    for task in &running {
        super::wait_until_gone(task, super::REAPER_PATIENCE_NANOS)?;
    }
    drop(running);
    Ok(())
}

/// One task of the check.
fn yield_and_audit(_index: usize) {
    let Some(me) = super::current() else {
        let _ = FINISHED.fetch_add(1, Ordering::AcqRel);
        return;
    };
    while RESUMED.load(Ordering::Relaxed) < SWITCHES {
        let before = me.switches();
        super::yield_now();
        if me.switches() == before {
            continue;
        }
        let _ = RESUMED.fetch_add(1, Ordering::Relaxed);
        let saved = <crate::arch::Irq as IrqControl>::disable();
        if let Some(lock) = super::this_cpu().and_then(super::queue_of) {
            borrow::audit(Site::Task, lock.lock().current.as_ref());
        }
        <crate::arch::Irq as IrqControl>::restore(saved);
        let itself = super::with_current(|task| core::ptr::eq(task, Arc::as_ptr(&me)));
        if itself != Some(true) {
            NOT_ITSELF.store(true, Ordering::Release);
        }
    }
    let _ = FINISHED.fetch_add(1, Ordering::AcqRel);
}
