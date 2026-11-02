//! Taking a processor out of service and bringing it back
//! (`docs/CORE-QUARANTINE.md` §3, §4): the one function both doors end in,
//! root's write to `cpuN/online` and lode's `CPU_SERVICE` call, with the
//! same checks behind each.
//!
//! A processor out of service is still online: it answers shootdowns and
//! grace periods from its park (`smp::PerCpu::is_online`), but no task and
//! no interrupt is placed on it (`sched::in_service`).

use ferrix_sync::{Once, SleepLock};

use crate::audit;
use crate::sched::{self, ServiceRefusal};
use crate::sync::SchedParker;

/// How long a processor is given to park, or to come back: long enough for
/// its running task's slice to end and its queue to be moved.
const DEADLINE_NANOS: u64 = 2_000_000_000;

/// Who asked: the door, as the audit record and the kernel line name it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Door {
    /// Root, through `cpuN/online`.
    Root,
    /// lode, through `CPU_SERVICE`.
    Lode,
}

impl Door {
    /// Its number in a `CPU_SERVICE` record.
    const fn number(self) -> u32 {
        match self {
            Door::Root => ferrix_audit::cpu_service::ROOT,
            Door::Lode => ferrix_audit::cpu_service::LODE,
        }
    }

    /// Its name on the kernel's line.
    const fn name(self) -> &'static str {
        match self {
            Door::Root => "root",
            Door::Lode => "lode",
        }
    }
}

/// Why a processor's service was not changed (`L.smp.36`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// No such processor.
    NoSuchProcessor,
    /// This architecture cannot move interrupts off a processor yet.
    Unsupported,
    /// A message is aimed at the processor and cannot be moved.
    Interrupts,
    /// The scheduler's refusal.
    Scheduler(ServiceRefusal),
}

impl Refusal {
    /// Its number in a refused record's status, never 0.
    const fn number(self) -> i16 {
        match self {
            Refusal::NoSuchProcessor => 1,
            Refusal::Unsupported => 2,
            Refusal::Interrupts => 3,
            Refusal::Scheduler(ServiceRefusal::BootProcessor) => 4,
            Refusal::Scheduler(ServiceRefusal::Last) => 5,
            Refusal::Scheduler(ServiceRefusal::Pinned) => 6,
            Refusal::Scheduler(ServiceRefusal::NoStealing) => 7,
            Refusal::Scheduler(ServiceRefusal::NotThere) => 8,
            Refusal::Scheduler(ServiceRefusal::TimedOut) => 9,
        }
    }

    /// What the kernel's line says.
    const fn reason(self) -> &'static str {
        match self {
            Refusal::NoSuchProcessor => "no such processor",
            Refusal::Unsupported => "this architecture cannot move its interrupts",
            Refusal::Interrupts => "an interrupt aimed at it cannot be moved",
            Refusal::Scheduler(ServiceRefusal::BootProcessor) => "it is the boot processor",
            Refusal::Scheduler(ServiceRefusal::Last) => "it is the last in service",
            Refusal::Scheduler(ServiceRefusal::Pinned) => "a program is pinned to it",
            Refusal::Scheduler(ServiceRefusal::NoStealing) => "its domain does not move work",
            Refusal::Scheduler(ServiceRefusal::NotThere) => "it is changing already",
            Refusal::Scheduler(ServiceRefusal::TimedOut) => "it did not answer in time",
        }
    }
}

/// The one change at a time: held across the park's wait, so a sleeping
/// lock (`L.smp.35`).
static CHANGING: Once<SleepLock<()>> = Once::new();

/// Take processor `cpu` out of service, or with `wanted` bring it back,
/// asked by `subject` through `door`; a processor already as asked is left
/// as it is, unrecorded. Every change and every refusal is recorded in the
/// audit store and said on the console (`docs/CORE-QUARANTINE.md` §4).
///
/// # Errors
///
/// The [`Refusal`]; the processor is then as it was.
pub fn set_in_service(
    cpu: usize,
    wanted: bool,
    door: Door,
    subject: audit::Subject,
) -> Result<(), Refusal> {
    if cpu >= super::count() {
        return Err(Refusal::NoSuchProcessor);
    }
    let lock = CHANGING.call_once(|| SleepLock::new((), &SchedParker));
    let _one = lock.lock();
    if sched::in_service(cpu) == wanted {
        return Ok(());
    }
    let done = if wanted {
        bring_back(cpu)
    } else {
        take_out(cpu)
    };
    let status = done.err().map_or(0, Refusal::number);
    audit::cpu_service(subject, cpu, wanted, door.number(), status);
    let by = door.name();
    match (done, wanted) {
        (Ok(()), false) => crate::println!("cpu: {cpu} out of service (by {by})"),
        (Ok(()), true) => crate::println!("cpu: {cpu} back in service (by {by})"),
        (Err(why), false) => {
            crate::println!("cpu: {cpu} stays in service (by {by}): {}", why.reason());
        }
        (Err(why), true) => {
            crate::println!(
                "cpu: {cpu} stays out of service (by {by}): {}",
                why.reason()
            );
        }
    }
    done
}

/// Take `cpu` out: refused while a message that cannot be moved is aimed
/// at it, before anything changes; once it has parked -- so no message can
/// be made aimed at it any more -- every message aimed at it moved to the
/// boot processor, and if one cannot be, the processor put back.
///
/// The design moves the messages before the park (§3.2 step 2); after it,
/// one made on the processor while it was parking is moved too, where
/// before it would be missed.
fn take_out(cpu: usize) -> Result<(), Refusal> {
    let id = hardware_id(cpu)?;
    let boot = u32::try_from(hardware_id(0)?).map_err(|_| Refusal::Interrupts)?;
    // The scheduler's refusals first -- the boot processor, the last one, a
    // pinned program -- then the messages: a refusal names the most basic
    // reason there is.
    sched::service_refusal(cpu).map_err(Refusal::Scheduler)?;
    if immovable_aimed_at(id)? {
        return Err(Refusal::Interrupts);
    }
    let deadline = crate::timer::now_nanos().saturating_add(DEADLINE_NANOS);
    sched::take_out_of_service(cpu, deadline).map_err(Refusal::Scheduler)?;
    match move_messages(id, boot) {
        Ok(0) => Ok(()),
        Ok(moved) => {
            crate::println!("cpu: {cpu}: {moved} of its interrupts moved to the boot processor");
            Ok(())
        }
        Err(why) => {
            crate::println!("cpu: {cpu}'s interrupts not moved: {why}");
            let deadline = crate::timer::now_nanos().saturating_add(DEADLINE_NANOS);
            sched::put_back_in_service(cpu, deadline).map_err(Refusal::Scheduler)?;
            Err(Refusal::Interrupts)
        }
    }
}

/// Processor `cpu`'s hardware identifier.
fn hardware_id(cpu: usize) -> Result<u64, Refusal> {
    super::record(cpu)
        .map(|record| record.hardware_id)
        .ok_or(Refusal::NoSuchProcessor)
}

/// Bring `cpu` back.
fn bring_back(cpu: usize) -> Result<(), Refusal> {
    let deadline = crate::timer::now_nanos().saturating_add(DEADLINE_NANOS);
    sched::put_back_in_service(cpu, deadline).map_err(Refusal::Scheduler)
}

/// Whether a message that cannot be moved is aimed at the processor whose
/// hardware identifier is `id`; refused as unsupported where the
/// architecture cannot move interrupts at all.
fn immovable_aimed_at(id: u64) -> Result<bool, Refusal> {
    if !crate::arch::CAN_MOVE_INTERRUPTS {
        return Err(Refusal::Unsupported);
    }
    Ok(crate::arch::immovable_aimed_at(id))
}

/// Move every message aimed at the processor `from` to the local APIC `to`.
fn move_messages(from: u64, to: u32) -> Result<usize, &'static str> {
    crate::arch::move_messages(from, to)
}
