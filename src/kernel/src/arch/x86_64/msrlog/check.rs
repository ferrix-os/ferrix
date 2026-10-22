//! Stage 3's checks of `ferrix.msrlog`'s parts that run on every boot: the
//! checked MSR read and its `#GP` fixup (`L.x86_64.188`), the decoder on
//! fixed values (`L.x86_64.189`) and the period chooser (`L.x86_64.187`).

use super::decode::{self, Limit, Units};
use super::{choose_period, cpu};
use crate::console::println;

/// `IA32_APIC_BASE`: every x86-64 processor has it, so a checked read of it
/// must succeed and agree with the unchecked one.
const IA32_APIC_BASE: u32 = 0x1b;
/// An index in no range Intel, AMD, KVM or Hyper-V defines: a processor or
/// hypervisor that refuses unknown registers refuses it with `#GP`.
const ABSENT: u32 = 0x3fff_ffff;

/// Stage 3's checks.
///
/// # Errors
///
/// The first that failed.
pub(in crate::arch::x86_64) fn run() -> Result<(), &'static str> {
    checked_read()?;
    decoded()?;
    periods_chosen()
}

/// A checked read of a register that exists gives the unchecked read's
/// value with no fixup counted; one of [`ABSENT`] either comes back `None`
/// with exactly one fixup counted, or, on a machine that answers unknown
/// registers with 0 instead of `#GP` (QEMU's TCG, KVM with
/// `ignore_msrs`), comes back with no fixup counted. The boot line says
/// which path ran; the `x86_64:kvm` gate row requires the `#GP` one.
///
/// Verifies: `L.x86_64.188`
fn checked_read() -> Result<(), &'static str> {
    let before = cpu::checked_reads_refused();
    // SAFETY: (SYSREG) `IA32_APIC_BASE` exists on every 64-bit x86.
    let unchecked = unsafe { cpu::read_msr(IA32_APIC_BASE) };
    if cpu::read_msr_checked(IA32_APIC_BASE) != Some(unchecked) {
        return Err("a checked read of IA32_APIC_BASE disagreed with rdmsr");
    }
    if cpu::checked_reads_refused() != before {
        return Err("a checked read of an MSR that exists counted a #GP");
    }
    match cpu::read_msr_checked(ABSENT) {
        None if cpu::checked_reads_refused() == before + 1 => {
            println!("  cpu      checked MSR read: #GP at {ABSENT:#x} resumed at its fixup");
            Ok(())
        }
        None => Err("a checked read came back absent without its fixup running"),
        Some(value) if cpu::checked_reads_refused() == before => {
            println!(
                "  cpu      checked MSR read: {ABSENT:#x} read {value:#x} without #GP here; fixup not exercised"
            );
            Ok(())
        }
        Some(_) => Err("a checked read returned a value after its fixup ran"),
    }
}

/// The decoder on fixed values.
///
/// Verifies: `L.x86_64.189`
fn decoded() -> Result<(), &'static str> {
    // 1/8 W, 1/16384 J, 1/1024 s: the common Intel units.
    let units = Units::from_raw(0x000a_0e03);
    if units != Units::DEFAULT {
        return Err("RAPL units decoded wrongly");
    }
    if units.milliwatts(48) != 6000 || units.milliwatts(1) != 125 {
        return Err("power units converted to milliwatts wrongly");
    }
    // Y = 10, Z = 3: 2^10 * 1.75 / 1024 s = 1.75 s; Y = 0, Z = 0: 1/1024 s.
    if units.window_millis((3 << 5) | 10) != 1750 || units.window_millis(0) != 0 {
        return Err("a time window decoded wrongly");
    }
    // PL1 48 enabled, clamp off, window 0x6a; PL2 200 enabled with clamp,
    // window 0x01; locked.
    let raw = (1 << 63)
        | (0x01 << 49)
        | (1 << 48)
        | (1 << 47)
        | (200 << 32)
        | (0x6a << 17)
        | (1 << 15)
        | 48;
    let (pl1, pl2) = decode::package_limits(raw);
    let want1 = Limit {
        power: 48,
        enabled: true,
        clamp: false,
        window: 0x6a,
    };
    let want2 = Limit {
        power: 200,
        enabled: true,
        clamp: true,
        window: 0x01,
    };
    if pl1 != want1 || pl2 != want2 || !decode::bit(raw, 63) {
        return Err("MSR_PKG_POWER_LIMIT decoded wrongly");
    }
    // Psys: 17-bit fields, PL1 0x1ffff with clamp, PL2 1 enabled.
    let raw = (1 << 49) | (1 << 32) | (1 << 18) | 0x1_ffff;
    let (pl1, pl2) = decode::platform_limits(raw);
    if pl1
        != (Limit {
            power: 0x1_ffff,
            enabled: false,
            clamp: true,
            window: 0,
        })
        || pl2
            != (Limit {
                power: 1,
                enabled: true,
                clamp: false,
                window: 0,
            })
    {
        return Err("MSR_PLATFORM_POWER_LIMIT decoded wrongly");
    }
    // 200 eighths is the N150's 25 A; 0x1fff the field's largest, 1023.875 A.
    if decode::vr_current(0x8000_e0c8) != (25_000, true)
        || decode::vr_current(0x1fff) != (1_023_875, false)
    {
        return Err("MSR_VR_CURRENT_CONFIG decoded wrongly");
    }
    let info = decode::power_info((0x7fff_u64 << 32) | (0x18 << 16) | 0x30 | (1 << 15));
    if (info.tdp, info.min, info.max) != (0x30, 0x18, 0x7fff) {
        return Err("MSR_PKG_POWER_INFO decoded wrongly");
    }
    let reasons = decode::reasons(0x0100_0900);
    if (reasons.now, reasons.log) != (0x0900, 0x0100)
        || decode::NAMED_REASONS
            != decode::REASON_NAMES
                .iter()
                .fold(0, |mask, (bit, _)| mask | (1 << bit))
    {
        return Err("a limit-reasons word decoded wrongly");
    }
    // Ratio 0x1c; 0x1800 / 8192 V = 0.75 V.
    if decode::perf_status((0x1800 << 32) | 0x1c00) != (0x1c, 750_000) {
        return Err("IA32_PERF_STATUS decoded wrongly");
    }
    if decode::tjmax(0x0064_0000) != 100 {
        return Err("TjMax decoded wrongly");
    }
    let thermal = decode::thermal((1 << 31) | (0x7f << 16) | 0b10);
    if (
        thermal.status,
        thermal.log,
        thermal.below_tjmax,
        thermal.valid,
    ) != (false, true, 0x7f, true)
    {
        return Err("a thermal status decoded wrongly");
    }
    Ok(())
}

/// `ferrix.msrlog.period`: none is 5; 0 to 3600 of one to four digits are
/// their number; anything else is not understood.
///
/// Verifies: `L.x86_64.187`
fn periods_chosen() -> Result<(), &'static str> {
    let understood = [
        (None, 5),
        (Some("0"), 0),
        (Some("1"), 1),
        (Some("0005"), 5),
        (Some("3600"), 3600),
    ];
    for (value, seconds) in understood {
        if choose_period(value) != Some(seconds) {
            return Err("a ferrix.msrlog.period did not choose its seconds");
        }
    }
    for value in ["", "3601", "10000", "-1", "+1", "0x5", " 5", "5s"] {
        if choose_period(Some(value)).is_some() {
            return Err("a ferrix.msrlog.period outside 0 to 3600 was not refused");
        }
    }
    Ok(())
}
