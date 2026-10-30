//! `ferrix.msrlog`: a read-only log of the power-limit, limit-reason,
//! performance and thermal MSRs, for finding out why a processor's
//! graphics runs below the clock asked of it (the N150's GT held at 450 MHz
//! under "pl4"). Ring 3 cannot read an MSR, so the kernel prints them.
//!
//! Off unless the boot word is given (`L.x86_64.187`): without it nothing
//! here reads a register, starts a task or prints a line. With it, a task
//! pinned to the boot processor reads the registers once after the scheduler
//! starts and again every `ferrix.msrlog.period` seconds (5 by default, 0 for
//! once), each through [`cpu::read_msr_checked`] so a register this
//! processor does not implement prints `absent (#GP)` instead of stopping
//! the kernel (`L.x86_64.188`). It writes no register. Every line starts
//! `msr:`. The decoding is [`decode`]'s, pure (`L.x86_64.189`).

pub(super) mod check;
pub(super) mod decode;

use core::arch::x86_64::__cpuid;

use ferrix_bootinfo::BootView;
use ferrix_sched::{CpuSet, NICE_0_WEIGHT};

use super::cpu;
use crate::console::println;
use decode::{Limit, Reasons, Units};

/// The boot word that turns the log on (`L.x86_64.187`).
const FLAG: &str = "ferrix.msrlog";
/// Seconds between reads; 0 reads once. Without it, [`DEFAULT_PERIOD`].
const PERIOD_OPTION: &str = "ferrix.msrlog.period";
/// Seconds between reads where `ferrix.msrlog.period` names none, or names
/// one that is not understood.
pub(super) const DEFAULT_PERIOD: u64 = 5;
/// The longest period understood, an hour.
const MAX_PERIOD: u64 = 3600;

/// `MSR_RAPL_POWER_UNIT`.
const RAPL_POWER_UNIT: u32 = 0x606;
/// `MSR_PKG_POWER_LIMIT`: PL1 and PL2.
const PKG_POWER_LIMIT: u32 = 0x610;
/// `MSR_PKG_POWER_INFO`: TDP and the package's power range.
const PKG_POWER_INFO: u32 = 0x614;
/// `MSR_VR_CURRENT_CONFIG`: the VR current limit (PL4), in 1/8 A.
const VR_CURRENT_CONFIG: u32 = 0x601;
/// `MSR_PLATFORM_POWER_LIMIT`: the platform (Psys) PL1 and PL2.
const PLATFORM_POWER_LIMIT: u32 = 0x65c;
/// `MSR_CORE_PERF_LIMIT_REASONS`.
const CORE_PERF_LIMIT_REASONS: u32 = 0x64f;
/// `MSR_GRAPHICS_PERF_LIMIT_REASONS`.
const GRAPHICS_PERF_LIMIT_REASONS: u32 = 0x6b0;
/// `IA32_PERF_STATUS`: the ratio and voltage now.
const PERF_STATUS: u32 = 0x198;
/// `IA32_THERM_STATUS`: this core's.
const THERM_STATUS: u32 = 0x19c;
/// `MSR_TEMPERATURE_TARGET`: `TjMax` in bits 23:16.
const TEMPERATURE_TARGET: u32 = 0x1a2;
/// `IA32_PACKAGE_THERM_STATUS`.
const PACKAGE_THERM_STATUS: u32 = 0x1b1;

/// What `ferrix.msrlog.period` chose: seconds between reads, 0 for once,
/// or `None` for a value not understood (said, and the default taken).
pub(super) fn choose_period(value: Option<&str>) -> Option<u64> {
    let Some(value) = value else {
        return Some(DEFAULT_PERIOD);
    };
    let digits = value.as_bytes();
    if digits.is_empty() || digits.len() > 4 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let seconds = digits
        .iter()
        .fold(0u64, |number, digit| number * 10 + u64::from(digit - b'0'));
    (seconds <= MAX_PERIOD).then_some(seconds)
}

/// Whether `CPUID` names Intel, whose MSR layouts these are; said when not.
fn intel() -> bool {
    let vendor = __cpuid(0);
    [vendor.ebx, vendor.edx, vendor.ecx]
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .eq(b"GenuineIntel".iter().copied())
}

/// Start the log if `ferrix.msrlog` is on the command line; otherwise do
/// nothing at all. Called once from `kmain`, on the boot processor, after
/// the scheduler has started.
pub(crate) fn start(view: &BootView<'_>) {
    if !view.flag(FLAG) {
        return;
    }
    let asked = view.option(PERIOD_OPTION);
    let period = choose_period(asked).unwrap_or_else(|| {
        println!(
            "msr: {PERIOD_OPTION}={} is not understood; every {DEFAULT_PERIOD} s",
            asked.unwrap_or("")
        );
        DEFAULT_PERIOD
    });
    if !intel() {
        // Read anyway: every read is checked, and a register another
        // vendor lacks prints absent. The decoding is Intel's.
        println!("msr: not an Intel processor; the fields below are decoded as Intel's");
    }
    let Some(here) = crate::smp::this_cpu().map(|cpu| cpu.logical) else {
        println!("msr: no processor record; nothing read");
        return;
    };
    let mut only = CpuSet::empty();
    // NOALLOC: a CpuSet is a fixed bitmap; insert sets a bit.
    if only.insert(here).is_err() {
        println!("msr: processor {here} out of range; nothing read");
        return;
    }
    let started = crate::sched::spawn_on("msrlog", run, period as usize, NICE_0_WEIGHT, here, only);
    match started {
        Ok(_) => println!("msr: logging on processor {here}, every {period} s (0: once)"),
        Err(why) => println!("msr: no task: {why}"),
    }
}

/// The task: read and print, then sleep `period` seconds; once if 0.
fn run(period: usize) {
    let mut round = 0u64;
    loop {
        log_once(round);
        if period == 0 {
            return;
        }
        round += 1;
        crate::sched::sleep_for((period as u64).saturating_mul(1_000_000_000));
    }
}

/// One register: its raw value, or that the processor refused it.
fn read(msr: u32, name: &str) -> Option<u64> {
    let value = cpu::read_msr_checked(msr);
    match value {
        Some(raw) => println!("msr: {msr:#05x} {name} {raw:#018x}"),
        None => println!("msr: {msr:#05x} {name} absent (#GP)"),
    }
    value
}

/// One read of every register, and the decoded fields.
fn log_once(round: u64) {
    println!("msr: round {round}");
    let units = read(RAPL_POWER_UNIT, "RAPL_POWER_UNIT").map(Units::from_raw);
    if let Some(units) = units {
        println!(
            "msr:   units: power 1/{} W, energy 1/{} J, time 1/{} s",
            1u64 << units.power,
            1u64 << units.energy,
            1u64 << units.time
        );
    }
    let units = units.unwrap_or(Units::DEFAULT);

    if let Some(raw) = read(PKG_POWER_LIMIT, "PKG_POWER_LIMIT") {
        let (pl1, pl2) = decode::package_limits(raw);
        say_limit("PL1", pl1, units);
        say_limit("PL2", pl2, units);
        println!("msr:   package limits locked: {}", decode::bit(raw, 63));
    }
    if let Some(raw) = read(PKG_POWER_INFO, "PKG_POWER_INFO") {
        let info = decode::power_info(raw);
        println!(
            "msr:   TDP {}, min {}, max {}",
            Milliwatts(units.milliwatts(info.tdp)),
            Milliwatts(units.milliwatts(info.min)),
            Milliwatts(units.milliwatts(info.max))
        );
    }
    if let Some(raw) = read(VR_CURRENT_CONFIG, "VR_CURRENT_CONFIG") {
        let (milliamps, locked) = decode::vr_current(raw);
        println!(
            "msr:   VR current limit {}.{:03} A, locked {locked}",
            milliamps / 1000,
            milliamps % 1000
        );
    }
    if let Some(raw) = read(PLATFORM_POWER_LIMIT, "PLATFORM_POWER_LIMIT") {
        let (pl1, pl2) = decode::platform_limits(raw);
        say_limit("Psys PL1", pl1, units);
        say_limit("Psys PL2", pl2, units);
        println!("msr:   platform limits locked: {}", decode::bit(raw, 63));
    }
    if let Some(raw) = read(CORE_PERF_LIMIT_REASONS, "CORE_PERF_LIMIT_REASONS") {
        say_reasons(decode::reasons(raw));
    }
    if let Some(raw) = read(GRAPHICS_PERF_LIMIT_REASONS, "GRAPHICS_PERF_LIMIT_REASONS") {
        say_reasons(decode::reasons(raw));
    }
    if let Some(raw) = read(PERF_STATUS, "PERF_STATUS") {
        let (ratio, microvolts) = decode::perf_status(raw);
        println!("msr:   ratio {ratio}, core voltage {microvolts} uV");
    }
    let tjmax = read(TEMPERATURE_TARGET, "TEMPERATURE_TARGET").map(decode::tjmax);
    for (msr, name) in [
        (THERM_STATUS, "THERM_STATUS"),
        (PACKAGE_THERM_STATUS, "PACKAGE_THERM_STATUS"),
    ] {
        if let Some(raw) = read(msr, name) {
            let thermal = decode::thermal(raw);
            let below = thermal.below_tjmax;
            let tjmax = tjmax.unwrap_or(0);
            println!(
                "msr:   {below} C below TjMax {tjmax} C = {} C (valid {}); throttling {}, logged {}",
                i64::from(tjmax) - i64::from(below),
                thermal.valid,
                thermal.status,
                thermal.log
            );
        }
    }
}

/// A PL line: the limit in watts, its enable and clamp bits, its window.
fn say_limit(name: &str, limit: Limit, units: Units) {
    println!(
        "msr:   {name} {} enabled {} clamp {} window {} ms",
        Milliwatts(units.milliwatts(limit.power)),
        limit.enabled,
        limit.clamp,
        units.window_millis(limit.window)
    );
}

/// The reasons now and the sticky log, by name.
fn say_reasons(reasons: Reasons) {
    println!("msr:   limited now: {}", ReasonNames(reasons.now));
    println!("msr:   limited since cleared: {}", ReasonNames(reasons.log));
}

/// A reason word printed as its named bits, space-separated; `other` for a
/// bit without a name, `none` for no bit.
struct ReasonNames(u16);

impl core::fmt::Display for ReasonNames {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut first = true;
        let mut word = |f: &mut core::fmt::Formatter<'_>, name: &str| {
            let separator = if first { "" } else { " " };
            first = false;
            write!(f, "{separator}{name}")
        };
        for (bit, name) in decode::REASON_NAMES {
            if self.0 & (1 << bit) != 0 {
                word(f, name)?;
            }
        }
        if self.0 & !decode::NAMED_REASONS != 0 {
            word(f, "other")?;
        }
        if self.0 == 0 {
            word(f, "none")?;
        }
        Ok(())
    }
}

/// Milliwatts printed as watts with three decimals.
struct Milliwatts(u64);

impl core::fmt::Display for Milliwatts {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{:03} W", self.0 / 1000, self.0 % 1000)
    }
}
