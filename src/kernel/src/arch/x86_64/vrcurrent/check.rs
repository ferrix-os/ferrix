//! Stage 3's checks of `ferrix.vrcurrent`'s parts that run on every boot,
//! none of which writes a register the processor implements: the word's
//! plan and clamp (`L.x86_64.190`), the guard's verdict (`L.x86_64.191`),
//! the checked write's fixup (`L.x86_64.192`), and the value written and
//! the steps (`L.x86_64.193`), on fixed values.

use core::arch::x86_64::__cpuid;

use super::cpu;
use super::logic::{self, Plan, Refusal, Temperatures, Trip};
use crate::console::println;

/// An index in no range Intel, AMD, KVM or Hyper-V defines, below the
/// hypervisors' 0x4000_0000: KVM refuses a write to it with `#GP`.
const ABSENT: u32 = 0x3fff_ffff;
/// `CPUID.1:ECX[31]`: running under a hypervisor.
const CPUID_HYPERVISOR: u32 = 1 << 31;
/// `KVMKVMKVM\0\0\0`, leaf `4000_0000h`'s EBX, ECX and EDX under KVM.
const KVM_SIGNATURE: [u32; 3] = [
    u32::from_le_bytes(*b"KVMK"),
    u32::from_le_bytes(*b"VMKV"),
    u32::from_le_bytes(*b"M\0\0\0"),
];

/// Stage 3's checks.
///
/// # Errors
///
/// The first that failed.
pub(in crate::arch::x86_64) fn run() -> Result<(), &'static str> {
    plans_parsed()?;
    guarded()?;
    composed()?;
    checked_write()
}

/// A plan of `amps` steps every `seconds`.
fn plan(amps: &[u32], seconds: u32) -> Plan {
    let mut plan = Plan {
        amps: [0; logic::MAX_STEPS],
        count: 0,
        seconds,
    };
    for (slot, &value) in plan.amps.iter_mut().zip(amps) {
        *slot = value;
        plan.count += 1;
    }
    plan
}

/// The word's grammar and the 25..32 A clamp, the processor gate, the
/// register's refusals, and the plan's packing.
///
/// Verifies: `L.x86_64.190`
fn plans_parsed() -> Result<(), &'static str> {
    if logic::parse("27,29,32@60") != Ok(plan(&[27, 29, 32], 60))
        || logic::parse("25@1") != Ok(plan(&[25], 1))
        || logic::parse("32,32,32,32,32,32,32,32@3600") != Ok(plan(&[32; 8], 3600))
    {
        return Err("a ferrix.vrcurrent word did not give its plan");
    }
    for (word, why) in [
        ("24@60", Refusal::OutOfRange(24)),
        ("27,33@60", Refusal::OutOfRange(33)),
        ("27,0@60", Refusal::OutOfRange(0)),
    ] {
        if logic::parse(word) != Err(why) {
            return Err("a ferrix.vrcurrent step outside 25..32 A was not refused");
        }
    }
    for word in [
        "",
        "27",
        "27,29",
        "@60",
        "27@",
        "27@0",
        "27@3601",
        "27@00060",
        "27,,29@60",
        "027@60",
        "27.5@60",
        " 27@60",
        "27@60s",
        "-27@60",
        "27,27,27,27,27,27,27,27,27@60",
    ] {
        if logic::parse(word) != Err(Refusal::Malformed) {
            return Err("a malformed ferrix.vrcurrent word was not refused");
        }
    }
    // N150: family 6, model 0xBE (extended 0xB, base 0xE), stepping 0.
    let n150 = 0x000b_06e0;
    if !logic::authorised(true, n150)
        || logic::authorised(false, n150)
        || logic::authorised(true, 0x000b_06f0)
        || logic::authorised(true, 0x000a_06e0)
        || logic::authorised(true, 0x000b_0fe0)
    {
        return Err("the ferrix.vrcurrent processor gate let another processor through");
    }
    // The register's refusals: locked, no firmware limit, a step more than
    // 7 A above it (24 A firmware, 32 A asked).
    let steps = plan(&[27, 29, 32], 60);
    if logic::admit(&steps, 0xc8) != Ok(())
        || logic::admit(&steps, 0x8000_00c8) != Err(Refusal::Locked)
        || logic::admit(&steps, 0x1_0000_0000) != Err(Refusal::NoFirmwareLimit)
        || logic::admit(&steps, 0xc0) != Err(Refusal::BeyondReach(32))
    {
        return Err("a ferrix.vrcurrent register refusal was not made");
    }
    let word = logic::pack(&steps);
    if logic::unpack(word) != Some(plan(&[27, 29, 32], 60))
        || logic::unpack(logic::pack(&plan(&[32; 8], 3600))) != Some(plan(&[32; 8], 3600))
        || logic::unpack(0).is_some()
    {
        return Err("a ferrix.vrcurrent plan did not survive its packing");
    }
    Ok(())
}

/// A thermal status word: `below` degrees under `TjMax`, valid, with
/// `bits` set.
const fn status(below: u64, bits: u64) -> u64 {
    (1 << 31) | (below << 16) | bits
}

/// The guard's verdict: 89 C passes and 90 C trips on either sensor; each
/// of bits 0 to 5 trips on either register; the core's invalid readout
/// trips; `TjMax` is believed only from 80 to 110 C.
///
/// Verifies: `L.x86_64.191`
fn guarded() -> Result<(), &'static str> {
    let cool = logic::verdict(100, status(65, 0), status(63, 0));
    if cool
        != Ok(Temperatures {
            core: 35,
            package: 37,
        })
    {
        return Err("the ferrix.vrcurrent guard did not pass a cool reading");
    }
    if logic::verdict(100, status(11, 0), status(11, 0)).is_err() {
        return Err("the ferrix.vrcurrent guard tripped at 89 C");
    }
    if logic::verdict(100, status(10, 0), status(11, 0)) != Err(Trip::CoreHot(90))
        || logic::verdict(100, status(11, 0), status(10, 0)) != Err(Trip::PackageHot(90))
        || logic::verdict(100, status(0, 0), status(0, 0)) != Err(Trip::CoreHot(100))
    {
        return Err("the ferrix.vrcurrent guard did not trip at 90 C");
    }
    for bit in 0..6 {
        if logic::verdict(100, status(65, 1 << bit), status(65, 0)) != Err(Trip::CoreBit(bit))
            || logic::verdict(100, status(65, 0), status(65, 1 << bit))
                != Err(Trip::PackageBit(bit))
        {
            return Err("the ferrix.vrcurrent guard did not trip on a thermal or PROCHOT bit");
        }
    }
    // Bits 6 to 15 (thresholds, power limitation) do not trip.
    if logic::verdict(100, status(65, 0xffc0), status(65, 0xffc0)).is_err() {
        return Err("the ferrix.vrcurrent guard tripped on a bit it does not guard");
    }
    if logic::verdict(100, status(65, 0) & !(1 << 31), status(65, 0)) != Err(Trip::CoreInvalid) {
        return Err("the ferrix.vrcurrent guard did not trip on an invalid readout");
    }
    if logic::tjmax_believed(0)
        || logic::tjmax_believed(79)
        || logic::tjmax_believed(111)
        || !logic::tjmax_believed(80)
        || !logic::tjmax_believed(105)
        || !logic::tjmax_believed(110)
    {
        return Err("the ferrix.vrcurrent guard believed a TjMax outside 80 to 110 C");
    }
    Ok(())
}

/// Only bits 12:0 change, to the step in 1/8 A; the step in force over
/// time.
///
/// Verifies: `L.x86_64.193`
fn composed() -> Result<(), &'static str> {
    // The N150's firmware value, 200 eighths, with every other bit set
    // but the lock.
    let firmware = 0xffff_ffff_7fff_e0c8_u64;
    if logic::compose(firmware, 27) != 0xffff_ffff_7fff_e0d8
        || logic::compose(0xc8, 32) != 0x100
        || logic::compose(0, 25) != 200
    {
        return Err("a ferrix.vrcurrent value changed bits other than 12:0");
    }
    let steps = plan(&[27, 29, 32], 60);
    for (elapsed, step) in [(0, 0), (59, 0), (60, 1), (119, 1), (120, 2), (100_000, 2)] {
        if logic::step_at(&steps, elapsed) != step {
            return Err("a ferrix.vrcurrent step came at the wrong second");
        }
    }
    if steps.step(0) != 27 || steps.step(2) != 32 || steps.step(9) != 32 {
        return Err("a ferrix.vrcurrent step read the wrong amperes");
    }
    Ok(())
}

/// Under KVM, a checked write of [`ABSENT`] is refused with exactly one
/// `#GP` counted; under another hypervisor (TCG ignores it) the line says
/// which path ran; on bare metal nothing is written.
///
/// Verifies: `L.x86_64.192`
fn checked_write() -> Result<(), &'static str> {
    if __cpuid(1).ecx & CPUID_HYPERVISOR == 0 {
        println!("  cpu      checked MSR write: not under a hypervisor, not exercised");
        return Ok(());
    }
    let leaf = __cpuid(0x4000_0000);
    let kvm = [leaf.ebx, leaf.ecx, leaf.edx] == KVM_SIGNATURE;
    let before = cpu::checked_writes_refused();
    // SAFETY: (SYSREG) under a hypervisor only: ABSENT is no register any
    // hypervisor or processor defines, so the write is refused or ignored.
    let written = unsafe { cpu::write_msr_checked(ABSENT, 0) };
    let refused = cpu::checked_writes_refused() - before;
    match (written, refused) {
        (false, 1) => {
            println!("  cpu      checked MSR write: #GP at {ABSENT:#x} resumed at its fixup");
            Ok(())
        }
        (true, 0) if !kvm => {
            println!(
                "  cpu      checked MSR write: {ABSENT:#x} taken without #GP here; fixup not exercised"
            );
            Ok(())
        }
        (true, 0) => Err("KVM took a write of an absent MSR without #GP"),
        _ => Err("a checked MSR write's result disagreed with its #GP count"),
    }
}
