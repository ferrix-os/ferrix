//! `ferrix.vrcurrent=A1,A2,...@SECONDS`: raise the processor's VR current
//! limit (`MSR_VR_CURRENT_CONFIG` bits 12:0, in 1/8 A) in steps while a
//! thermal guard watches, for the N150 whose graphics runs held under
//! "amps/pl4" at the firmware's 25 A. The customer authorised it
//! explicitly on 2026-10-22 ("I will allow you to raise the current
//! limit"); the consultant's OK IF is ledger line 3086.
//!
//! Without the word nothing here reads, writes, starts or prints
//! (`L.x86_64.190`). With it, a task pinned to the boot processor refuses --
//! a line, nothing written -- unless the processor is Intel family 6 model
//! 0xBE, the register is present and unlocked, `TjMax` reads 80 to 110 C,
//! the core's readout is valid and the guard's first verdict is good. Then
//! it writes the first step, only bits 12:0 changed (`L.x86_64.193`),
//! through the checked write (`L.x86_64.192`), reads it back, and steps on
//! every `SECONDS`. Every write prints a line.
//!
//! The guard (`L.x86_64.191`) reads the core's and the package's thermal
//! status every second and writes the firmware's value back at 90 C on
//! either, at any thermal, PROCHOT or critical status or log bit, or at any
//! read that fails, and the run ends. An orderly power-off or reset and
//! both ends of a panic write it back too ([`restore`]). Its limits: the
//! core register read is the boot processor's only; a scheduler that stops
//! running the task stops the guard; the firmware's own PROCHOT and
//! THERMTRIP remain the hardware's protection under any limit written.

pub(super) mod check;
pub(super) mod logic;

use core::arch::x86_64::__cpuid;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_bootinfo::BootView;
use ferrix_sched::{CpuSet, NICE_0_WEIGHT};

use super::cpu;
use crate::console::println;
use logic::{Plan, Refusal, Temperatures, Trip};

/// The boot word (`L.x86_64.190`).
const OPTION: &str = "ferrix.vrcurrent";
/// `MSR_VR_CURRENT_CONFIG`: the only register this module writes.
const VR_CURRENT_CONFIG: u32 = 0x601;
/// `MSR_TEMPERATURE_TARGET`: `TjMax` in bits 23:16.
const TEMPERATURE_TARGET: u32 = 0x1a2;
/// `IA32_THERM_STATUS`: the boot processor's core.
const THERM_STATUS: u32 = 0x19c;
/// `IA32_PACKAGE_THERM_STATUS`.
const PACKAGE_THERM_STATUS: u32 = 0x1b1;
/// The guard's period: a second.
const GUARD_NANOS: u64 = 1_000_000_000;
/// Guard periods between temperature lines.
const LOG_EVERY: u64 = 5;

/// The firmware's raw `MSR_VR_CURRENT_CONFIG`, stored before any write.
static FIRMWARE: AtomicU64 = AtomicU64::new(0);
/// A raised value may be in force: set before each raise is written,
/// cleared by the one revert that writes the firmware value back.
static IN_FORCE: AtomicBool = AtomicBool::new(false);
/// The run is over; no further raise is written.
static STOPPED: AtomicBool = AtomicBool::new(false);

/// Start the run if `ferrix.vrcurrent=` is on the command line; otherwise
/// do nothing at all. Called once from `kmain`, on the boot processor, after
/// the scheduler has started.
pub(crate) fn start(view: &BootView<'_>) {
    let Some(value) = view.option(OPTION) else {
        if view.flag(OPTION) {
            println!("msr: vrcurrent: {OPTION} needs =A1,A2,...@SECONDS; nothing written");
        }
        return;
    };
    let plan = match logic::parse(value) {
        Ok(plan) => plan,
        Err(why) => {
            println!(
                "msr: vrcurrent: {OPTION}={value} refused: {}; nothing written",
                Why(why)
            );
            return;
        }
    };
    let Some(here) = crate::smp::this_cpu().map(|cpu| cpu.logical) else {
        println!("msr: vrcurrent: no processor record; nothing written");
        return;
    };
    let mut only = CpuSet::empty();
    // NOALLOC: a CpuSet is a fixed bitmap; insert sets a bit.
    if only.insert(here).is_err() {
        println!("msr: vrcurrent: processor {here} out of range; nothing written");
        return;
    }
    let started = crate::sched::spawn_on(
        "vrcurrent",
        run,
        logic::pack(&plan),
        NICE_0_WEIGHT,
        here,
        only,
    );
    if let Err(why) = started {
        println!("msr: vrcurrent: no task: {why}; nothing written");
    }
}

/// The processor's vendor is Intel, and its `CPUID.1:EAX`.
fn processor() -> (bool, u32) {
    let vendor = __cpuid(0);
    let intel = [vendor.ebx, vendor.edx, vendor.ecx]
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .eq(b"GenuineIntel".iter().copied());
    (intel, __cpuid(1).eax)
}

/// The task: every check before the first write, then the steps and the
/// guard until a revert or the last step's guard runs on for good.
fn run(word: usize) {
    let Some(plan) = logic::unpack(word) else {
        println!("msr: vrcurrent: plan lost; nothing written");
        return;
    };
    let Some((firmware, tjmax)) = ready(&plan) else {
        return;
    };
    FIRMWARE.store(firmware, Ordering::SeqCst);
    let mut step = 0;
    if !raise(firmware, firmware, plan.step(0)) {
        return;
    }
    let mut elapsed = 0u64;
    loop {
        crate::sched::sleep_for(GUARD_NANOS);
        elapsed += 1;
        let temperatures = match guard(tjmax) {
            Ok(temperatures) => temperatures,
            Err(why) => {
                revert(why);
                return;
            }
        };
        let amps = plan.step(step);
        if elapsed.is_multiple_of(LOG_EVERY) {
            println!(
                "msr: vrcurrent {amps} A, core {} C, package {} C",
                temperatures.core, temperatures.package
            );
        }
        let next = logic::step_at(&plan, elapsed);
        if next != step {
            let was = logic::compose(firmware, amps);
            step = next;
            if !raise(firmware, was, plan.step(step)) {
                return;
            }
        }
        if STOPPED.load(Ordering::SeqCst) {
            return;
        }
    }
}

/// Every check before the first write; the firmware value and `TjMax`, or
/// `None` after a line saying which refused.
fn ready(plan: &Plan) -> Option<(u64, u32)> {
    let (intel, signature) = processor();
    if !logic::authorised(intel, signature) {
        println!(
            "msr: vrcurrent refused: not the authorised processor (Intel {intel}, signature {signature:#x}); nothing written"
        );
        return None;
    }
    let Some(firmware) = cpu::read_msr_checked(VR_CURRENT_CONFIG) else {
        println!("msr: vrcurrent refused: VR_CURRENT_CONFIG absent (#GP); nothing written");
        return None;
    };
    if let Err(why) = logic::admit(plan, firmware) {
        println!(
            "msr: vrcurrent refused: {} (VR_CURRENT_CONFIG {firmware:#x}); nothing written",
            Why(why)
        );
        return None;
    }
    let tjmax = cpu::read_msr_checked(TEMPERATURE_TARGET).map(super::msrlog::decode::tjmax);
    let Some(tjmax) = tjmax.filter(|&tjmax| logic::tjmax_believed(tjmax)) else {
        println!(
            "msr: vrcurrent refused: TjMax absent or outside 80 to 110 C ({tjmax:?}); nothing written"
        );
        return None;
    };
    match guard(tjmax) {
        Ok(temperatures) => {
            println!(
                "msr: vrcurrent firmware {} A, TjMax {tjmax} C, core {} C, package {} C",
                Amps(logic::eighths(firmware)),
                temperatures.core,
                temperatures.package
            );
            Some((firmware, tjmax))
        }
        Err(why) => {
            println!("msr: vrcurrent refused: {why}; nothing written");
            None
        }
    }
}

/// Write `amps` over `firmware`'s other bits, `was` being the value in
/// force, and read it back; `false`, with the firmware value written back
/// where a raise may be in force, if the write or its read-back failed or a
/// revert came first.
fn raise(firmware: u64, was: u64, amps: u32) -> bool {
    if STOPPED.load(Ordering::SeqCst) {
        return false;
    }
    let value = logic::compose(firmware, amps);
    IN_FORCE.store(true, Ordering::SeqCst);
    // SAFETY: (SYSREG) VR_CURRENT_CONFIG on the authorised processor, its lock
    // clear, bits 12:0 within 25..32 A and 7 A of the firmware's, every other
    // bit as firmware left it; the guard reverts it (L.x86_64.191).
    if !unsafe { cpu::write_msr_checked(VR_CURRENT_CONFIG, value) } {
        println!("msr: vrcurrent write of {amps} A refused (#GP)");
        revert(Reason::WriteRefused);
        return false;
    }
    println!(
        "msr: vrcurrent set {amps} A (was {} A)",
        Amps(logic::eighths(was))
    );
    let back = cpu::read_msr_checked(VR_CURRENT_CONFIG);
    if back.map(logic::eighths) != Some(logic::eighths(value)) {
        revert(Reason::ReadBack(back));
        return false;
    }
    // A revert from another processor between the flag and the write wrote
    // the firmware value before this write: undo this one too.
    if STOPPED.load(Ordering::SeqCst) {
        IN_FORCE.store(true, Ordering::SeqCst);
        revert(Reason::Stopped);
        return false;
    }
    true
}

/// Why the firmware value went back.
#[derive(Clone, Copy)]
enum Reason {
    /// A register the guard reads was refused.
    Absent(&'static str),
    /// The guard's verdict.
    Trip(Trip),
    /// The raise's write took `#GP`.
    WriteRefused,
    /// The raise read back other bits 12:0, or nothing.
    ReadBack(Option<u64>),
    /// A revert elsewhere stopped the run while it wrote.
    Stopped,
    /// An orderly power-off or reset, or a panic.
    Exit(&'static str),
}

impl core::fmt::Display for Reason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::Absent(name) => write!(f, "{name} absent (#GP)"),
            Self::Trip(Trip::CoreInvalid) => write!(f, "core readout not valid"),
            Self::Trip(Trip::CoreHot(celsius)) => write!(f, "core {celsius} C"),
            Self::Trip(Trip::PackageHot(celsius)) => write!(f, "package {celsius} C"),
            Self::Trip(Trip::CoreBit(bit)) => write!(f, "THERM_STATUS bit {bit} set"),
            Self::Trip(Trip::PackageBit(bit)) => write!(f, "PACKAGE_THERM_STATUS bit {bit} set"),
            Self::WriteRefused => write!(f, "the write took #GP"),
            Self::ReadBack(Some(raw)) => write!(f, "read back {raw:#x}"),
            Self::ReadBack(None) => write!(f, "read back absent (#GP)"),
            Self::Stopped => write!(f, "stopped while writing"),
            Self::Exit(what) => write!(f, "{what}"),
        }
    }
}

/// One guard reading: the temperatures, or why to revert.
fn guard(tjmax: u32) -> Result<Temperatures, Reason> {
    let core = cpu::read_msr_checked(THERM_STATUS).ok_or(Reason::Absent("THERM_STATUS"))?;
    let package = cpu::read_msr_checked(PACKAGE_THERM_STATUS)
        .ok_or(Reason::Absent("PACKAGE_THERM_STATUS"))?;
    logic::verdict(tjmax, core, package).map_err(Reason::Trip)
}

/// End the run; if a raise may be in force, write the firmware value back,
/// read it back and say so. Single-shot: of every caller, on any processor,
/// only the first to clear [`IN_FORCE`] writes.
fn revert(why: Reason) {
    STOPPED.store(true, Ordering::SeqCst);
    if !IN_FORCE.swap(false, Ordering::SeqCst) {
        return;
    }
    let firmware = FIRMWARE.load(Ordering::SeqCst);
    // SAFETY: (SYSREG) the value firmware left in VR_CURRENT_CONFIG, read at
    // the run's start (L.x86_64.191).
    let written = unsafe { cpu::write_msr_checked(VR_CURRENT_CONFIG, firmware) };
    let back = cpu::read_msr_checked(VR_CURRENT_CONFIG);
    println!(
        "msr: vrcurrent reverted to {} A: {why}",
        Amps(logic::eighths(firmware))
    );
    if !written || back != Some(firmware) {
        println!("msr: vrcurrent revert NOT confirmed: written {written}, read back {back:?}");
    }
}

/// Write the firmware value back if a raise may be in force: from an
/// orderly power-off or reset and from both ends of a panic, on any
/// processor, with no lock and no allocation (`L.x86_64.191`). Nothing at
/// all where no run raised anything.
pub(crate) fn restore(what: &'static str) {
    if IN_FORCE.load(Ordering::SeqCst) {
        revert(Reason::Exit(what));
    } else {
        STOPPED.store(true, Ordering::SeqCst);
    }
}

/// A refusal in words.
struct Why(Refusal);

impl core::fmt::Display for Why {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Refusal::Malformed => write!(
                f,
                "not 1 to {} amperes of {}..{} A, @, and 1 to {} s",
                logic::MAX_STEPS,
                logic::MIN_AMPS,
                logic::MAX_AMPS,
                logic::MAX_SECONDS
            ),
            Refusal::OutOfRange(amps) => write!(
                f,
                "{amps} A is outside {}..{} A",
                logic::MIN_AMPS,
                logic::MAX_AMPS
            ),
            Refusal::Locked => write!(f, "VR_CURRENT_CONFIG is locked"),
            Refusal::NoFirmwareLimit => write!(f, "the firmware limit is 0"),
            Refusal::BeyondReach(amps) => write!(
                f,
                "{amps} A is more than {} A above the firmware limit",
                logic::MAX_RAISE_AMPS
            ),
        }
    }
}

/// A current in 1/8 A printed as amperes with three decimals.
pub(super) struct Amps(pub(super) u64);

impl core::fmt::Display for Amps {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{:03}", self.0 / 8, self.0 % 8 * 125)
    }
}
