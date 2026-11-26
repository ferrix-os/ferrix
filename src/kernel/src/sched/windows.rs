//! `schedule=windows`'s frame on this kernel (`docs/microkernel/K9.md` §4, ledger 7460): the
//! kernel's default frame, set and sealed by the boot authority before any first process, with
//! the bodies the boot check calls. No native number reaches them (WN.1): no process holds the
//! root job.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_sched_windows::Placed;
use ferrix_sched_windows::frame::{self, FRAME_CPUS, PARTITIONS, Part, Refusal, Row, Window};
use ferrix_sync::{IrqControl, SpinLock};

use super::queue::CpuQueue;
use super::{Task, in_service, kick, queue_of};
use crate::arch;
use crate::object::job::Job;

/// The default frame's major frame: 10 ms on each processor in service.
pub(crate) const MAJOR_NS: u64 = 10_000_000;
/// Partition 0's window in it: the kernel's threads, 1 ms.
pub(crate) const KERNEL_NS: u64 = 1_000_000;
/// The partition every first job is made in.
pub const FIRST: u8 = 1;

/// The level a task of `weight`'s nice takes: the servers 0-9, a program 10-39 (WN.10). The
/// body is the pure crate's, so its host test reads the levels this kernel gives (WO.3).
pub(crate) fn level_of(weight: u32, program: bool) -> u8 {
    ferrix_sched_windows::level_of(weight, program)
}

impl Placed for Task {
    fn level(&self) -> u8 {
        self.level.load(Ordering::Relaxed)
    }

    fn partition(&self) -> u8 {
        self.partition.load(Ordering::Relaxed)
    }
}

/// Why the frame refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// The frame's own refusal.
    Frame(Refusal),
    /// The boot's frame is sealed: no window, no seal, no frame taken off (L.sched.160).
    Sealed,
    /// The job may not take the partition (L.sched.162).
    Job,
}

impl Refused {
    /// What it says.
    pub const fn says(self) -> &'static str {
        match self {
            Refused::Frame(why) => why.says(),
            Refused::Sealed => "the frame is sealed",
            Refused::Job => "the job may not take the partition",
        }
    }
}

/// A frame being set.
pub(crate) struct Plan {
    rows: SpinLock<[Row; FRAME_CPUS]>,
}

impl Plan {
    /// No windows.
    pub(crate) const fn new() -> Plan {
        Plan {
            rows: SpinLock::new([Row::empty(); FRAME_CPUS]),
        }
    }
}

/// The boot's frame, and whether it is sealed.
static BOOT: Plan = Plan::new();
static SEALED: AtomicBool = AtomicBool::new(false);
/// The partitions the sealed frame names, bit `n` for partition `n`.
static NAMED: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// Whether the boot's frame is sealed.
pub(crate) fn sealed() -> bool {
    SEALED.load(Ordering::Acquire)
}

/// Make `job`, a child of the root with no process or child, partition `partition`: before
/// the seal any but 0, after it only one the sealed frame names. A job's byte, not a window:
/// the frame is unchanged (the boot authority makes its first jobs after its seal).
///
/// # Errors
///
/// [`Refused::Sealed`] after the seal for a partition the frame does not name;
/// [`Refused::Frame`] for no such partition; [`Refused::Job`].
pub fn assign(job: &Arc<Job>, partition: u8) -> Result<(), Refused> {
    if sealed() && NAMED.load(Ordering::Acquire) & (1_u8 << (partition & 7)) == 0 {
        return Err(Refused::Sealed);
    }
    if usize::from(partition) >= PARTITIONS || partition == 0 {
        return Err(Refused::Frame(Refusal::Partition));
    }
    job.set_partition(partition).map_err(|_| Refused::Job)
}

/// Give `cpu` a window in `plan`: the body the boot authority and the check share.
///
/// # Errors
///
/// [`Refused::Sealed`] for the boot's after its seal; the frame's own refusals.
pub(crate) fn set_window(plan: &Plan, cpu: usize, window: Window) -> Result<(), Refused> {
    if core::ptr::eq(plan, &BOOT) && sealed() {
        return Err(Refused::Sealed);
    }
    frame::set_window(&mut *plan.rows.lock(), cpu, window).map_err(Refused::Frame)
}

/// [`set_window`] on the boot's frame: refused once it is sealed.
///
/// # Errors
///
/// As [`set_window`].
pub(crate) fn set_window_boot(cpu: usize, window: Window) -> Result<(), Refused> {
    set_window(&BOOT, cpu, window)
}

/// Whether `plan` would seal, `major_ns` long, for the processors in service: the seal's own
/// refusals, for the check, with nothing installed.
///
/// # Errors
///
/// The frame's refusals.
pub(crate) fn check_seal(plan: &Plan, major_ns: u64) -> Result<(), Refused> {
    check_rows(plan, major_ns)
}

/// The processors in service now, as one word.
fn in_service_word() -> u64 {
    (0..crate::smp::count().min(FRAME_CPUS))
        .filter(|&cpu| in_service(cpu))
        .fold(0, |word, cpu| word | (1_u64 << cpu))
}

/// Check `plan` would seal for the processors in service, `major_ns` long, and record the
/// partitions the boot's names. Read under its lock, never copied: a frame is too large for a
/// kernel stack.
///
/// # Errors
///
/// The frame's refusals.
fn check_rows(plan: &Plan, major_ns: u64) -> Result<(), Refused> {
    let rows = plan.rows.lock();
    frame::seal(&*rows, major_ns, in_service_word()).map_err(Refused::Frame)?;
    if core::ptr::eq(plan, &BOOT) {
        let named = rows
            .iter()
            .flat_map(Row::windows)
            .fold(0_u8, |named, window| {
                named | (1_u8 << (window.partition & 7))
            });
        NAMED.store(named, Ordering::Release);
    }
    Ok(())
}

/// `cpu`'s part of a sealed `plan`, if it has windows.
fn part_of(plan: &Plan, cpu: usize, major_ns: u64, epoch: u64) -> Option<Part> {
    let row = plan.rows.lock().get(cpu).copied()?;
    Part::new(row, major_ns, epoch)
}

impl Plan {
    /// Take every window out: the check's scratch frame, between its cases.
    pub(crate) fn clear(&self) {
        for row in self.rows.lock().iter_mut() {
            *row = Row::empty();
        }
    }
}

fn on_queue<R>(cpu: usize, body: impl FnOnce(&mut CpuQueue) -> R) -> Option<R> {
    let lock = queue_of(cpu)?;
    let saved = <arch::Irq as IrqControl>::disable();
    let answer = body(&mut lock.lock());
    <arch::Irq as IrqControl>::restore(saved);
    Some(answer)
}

/// Seal the boot's frame, `major_ns` long, starting at the next millisecond, and install it on
/// every processor in service (H.SCHED.27).
///
/// # Errors
///
/// [`Refused::Sealed`] for a second seal; the frame's refusals.
pub(crate) fn seal(major_ns: u64) -> Result<(), Refused> {
    if sealed() {
        return Err(Refused::Sealed);
    }
    let epoch = crate::timer::now_nanos().saturating_add(1_000_000);
    check_rows(&BOOT, major_ns)?;
    if SEALED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(Refused::Sealed);
    }
    for cpu in 0..crate::smp::count().min(FRAME_CPUS) {
        if let Some(part) = part_of(&BOOT, cpu, major_ns, epoch) {
            let _ = on_queue(cpu, |queue| {
                // A check's frame never outlives its check: none is on here.
                let _ = queue.fair.uninstall();
                queue.fair.install(part)
            });
            kick(cpu);
        }
    }
    Ok(())
}

/// Whether every processor in service reads a row of the sealed frame, none the merged set
/// (WN.7, L.sched.160): after the seal, for the boot's after-seal check.
pub(crate) fn every_row_installed() -> bool {
    (0..crate::smp::count().min(FRAME_CPUS))
        .filter(|&cpu| in_service(cpu))
        .all(|cpu| on_queue(cpu, |queue| queue.fair.part().is_some()) == Some(true))
}

/// The kernel's default frame (WN.2): partition 0 from 0 to 1 ms and partition 1 to 10 ms on
/// every processor in service, sealed.
///
/// # Errors
///
/// As [`set_window`] and [`seal`].
pub fn seal_default() -> Result<(), Refused> {
    for cpu in (0..crate::smp::count().min(FRAME_CPUS)).filter(|&cpu| in_service(cpu)) {
        set_window(
            &BOOT,
            cpu,
            Window {
                partition: 0,
                offset_ns: 0,
                length_ns: KERNEL_NS,
            },
        )?;
        set_window(
            &BOOT,
            cpu,
            Window {
                partition: FIRST,
                offset_ns: KERNEL_NS,
                length_ns: MAJOR_NS - KERNEL_NS,
            },
        )?;
    }
    seal(MAJOR_NS)
}

/// Install a check's own sealed `plan` on `cpu`, before the boot's seal (WN.7).
///
/// # Errors
///
/// [`Refused::Sealed`] after the boot's seal or with a frame already there; the frame's
/// refusals.
pub(crate) fn install_check(
    plan: &Plan,
    cpu: usize,
    major_ns: u64,
    epoch: u64,
) -> Result<(), Refused> {
    if sealed() {
        return Err(Refused::Sealed);
    }
    check_rows(plan, major_ns)?;
    let part = part_of(plan, cpu, major_ns, epoch).ok_or(Refused::Frame(Refusal::OutOfService))?;
    if on_queue(cpu, |queue| queue.fair.install(part)) != Some(true) {
        return Err(Refused::Sealed);
    }
    kick(cpu);
    Ok(())
}

/// Take a check's frame off `cpu`: refused once the boot's frame is sealed (L.sched.160).
///
/// # Errors
///
/// [`Refused::Sealed`].
pub(crate) fn uninstall_check(cpu: usize) -> Result<(), Refused> {
    if sealed() {
        return Err(Refused::Sealed);
    }
    let _ = on_queue(cpu, |queue| queue.fair.uninstall());
    kick(cpu);
    Ok(())
}

/// Whether `cpu` may come back into service: after the seal, only with its row (L.sched.170).
pub(crate) fn may_return(cpu: usize) -> bool {
    let sealed = sealed();
    on_queue(cpu, |queue| queue.fair.may_return(sealed)).unwrap_or(!sealed)
}

/// The worst overrun and the windows ended running, over every processor (L.sched.167).
pub(crate) fn overruns() -> (u64, u64) {
    (0..crate::smp::count()).fold((0, 0), |(worst, ended), cpu| {
        on_queue(cpu, |queue| {
            (
                worst.max(queue.fair.worst_overrun_ns()),
                ended + queue.fair.windows_ended(),
            )
        })
        .unwrap_or((worst, ended))
    })
}

/// Wakes into the open window's partition that kicked their processor (L.sched.164's other half).
static KICKED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Count a wake that kicked, its partition's window open (WX.6).
pub(crate) fn note_kicked() {
    let _ = KICKED.fetch_add(1, Ordering::Relaxed);
}

/// The wakes [`note_kicked`] counted, for the boot check.
pub(crate) fn kicked() -> u64 {
    KICKED.load(Ordering::Relaxed)
}

/// Wakes that would have kicked a processor but for the window: a task of another partition.
static UNKICKED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Count a wake not kicked for its window (L.sched.164).
pub(crate) fn note_unkicked() {
    let _ = UNKICKED.fetch_add(1, Ordering::Relaxed);
}

/// Wakes not kicked for their window so far.
pub(crate) fn unkicked() -> u64 {
    UNKICKED.load(Ordering::Relaxed)
}

/// Whether `queue`'s window admits `task`'s partition at this instant, the wake's (ledger 7639
/// WT.1): what the wake's kick, the pull and `place_on_idle` decide by (L.sched.164, 166).
pub(crate) fn admits(queue: &CpuQueue, task: &Task) -> bool {
    queue.fair.admits_at(
        task.partition.load(Ordering::Relaxed),
        crate::timer::now_nanos(),
    )
}

/// The pull's decision under windows: only into a window that admits the task (WT.1).
pub(crate) fn pull_decides(decide: bool, queue: &CpuQueue, task: &Task) -> bool {
    decide && admits(queue, task)
}

/// A wake's kick through the window (K9 §4, L.sched.164): a task of another partition than the
/// window's at the wake's instant only queues, no kick, no preemption, its window's own timer
/// finds it; one into the open window's partition kicks as before (W.4, ledger 7540 WX.6). Each
/// kick leaving the filter is counted, so that WN.4 tells a kick sent from none decided (ledger
/// 7639 WT.4). One helper, called once, so the wake's body keeps its shape (ledger 7649 WQ.2).
pub(crate) fn filter_kick(queue: &CpuQueue, task: &Task, kick_cpu: Option<usize>) -> Option<usize> {
    let mut kick_cpu = kick_cpu;
    if !admits(queue, task) {
        if kick_cpu.is_some() {
            note_unkicked();
        }
        kick_cpu = None;
    }
    if kick_cpu.is_some() {
        note_kicked();
    }
    kick_cpu
}

/// Whether a hand-over is declined for the window (K9 §4, L.sched.165): across partitions, or to
/// a peer the window may not run at this instant, the long way, since a hand-over never moves
/// time from one partition to another. Counted when declined.
pub(crate) fn declines_hand_over(caller: &Task, peer: &Task, queue: &CpuQueue) -> bool {
    let declined = caller.partition.load(Ordering::Relaxed)
        != peer.partition.load(Ordering::Relaxed)
        || !admits(queue, peer);
    if declined {
        note_declined();
    }
    declined
}

/// Hand-overs declined across partitions.
static DECLINED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Count a hand-over declined for its partitions (L.sched.165).
pub(crate) fn note_declined() {
    let _ = DECLINED.fetch_add(1, Ordering::Relaxed);
}

/// Hand-overs declined across partitions so far.
pub(crate) fn declined() -> u64 {
    DECLINED.load(Ordering::Relaxed)
}

/// Put the check's blocked kernel thread `task` in `partition`, before the boot's seal: a task
/// on no queue is read by no class, so its byte may change (L.sched.163 holds of every other).
#[cfg(ferrix_checks = "on")]
pub(crate) fn rehome_for_check(task: &Task, partition: u8) -> Result<(), Refused> {
    if sealed() {
        return Err(Refused::Sealed);
    }
    if !task.is_blocked() || task.is_queued() || task.core_process().is_some() {
        return Err(Refused::Job);
    }
    task.partition.store(partition, Ordering::Release);
    Ok(())
}
