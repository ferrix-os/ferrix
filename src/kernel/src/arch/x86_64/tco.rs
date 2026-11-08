//! The chipset's TCO watchdog timer: the hardware half of `/dev/watchdog`
//! (`docs/POSTMORTEM.md` §6.11, `crate::watchdog`).
//!
//! Intel's platform controller hubs since the ICH have a timer that counts
//! down in 0.6 s ticks from `TCO_TMR` and, reaching zero a second time
//! without a reload, resets the machine -- with every processor halted and
//! interrupts masked, which is the hang no software of the kernel's can end.
//! This module finds it, holds it halted until ring 3 arms it, reloads it,
//! and halts it again. When to arm it, for how long and what must be alive
//! for it to be fed are ring 3's (`crate::watchdog`, init's
//! `ferrix.init.watchdog=`).
//!
//! Where it is, read from PCI configuration and never assumed, for the two
//! identities verified against their documents (§6.1, §6.10):
//!
//! * **8086:54a3**, the Alder Lake-N SMBus function at 00:1f.4 (the N150 box):
//!   TCO version 6. Its I/O base is `TCOBASE` (+0x50, bits 15:5), live only
//!   with `TCOCTL`'s `TCO_BASE_EN` (+0x54, bit 8); `NO_REBOOT` is `TCO1_CNT`
//!   bit 0, written with `NMI_NOW` masked (Linux's `update_no_reboot_bit_cnt`).
//! * **8086:2918**, the ICH9 LPC bridge at 00:1f.0 (QEMU's q35): TCO version
//!   2, at the ACPI I/O base (`PMBASE`, +0x40, bits 15:7, live with
//!   `ACPI_CNTL` bit 7) plus 0x60; `NO_REBOOT` is bit 5 of the chipset's
//!   `GCS` at the root complex base (`RCBA`, +0xF0, enabled by bit 0) plus
//!   0x3410.
//!
//! Anything else is no TCO, said in one line. A timer firmware locked
//! (`TCO_LOCK`) or whose `NO_REBOOT` does not clear is refused and left as
//! found.
//!
//! [`panic_backstop`] is the one entry the panic path makes: port writes and
//! one memory write, no lock, no allocation.

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

use super::cpu;
use crate::device;
use crate::mmio::Mmio;

/// The registers, as offsets from the TCO's I/O base (Linux's `iTCO_wdt.c`
/// for versions 2 and later).
const TCO_RLD: u16 = 0x00;
const TCO1_STS: u16 = 0x04;
const TCO2_STS: u16 = 0x06;
const TCO1_CNT: u16 = 0x08;
const TCO_TMR: u16 = 0x12;

/// `TCO1_STS`: the first expiry happened (write 1 to clear).
const TIMEOUT: u16 = 1 << 3;
/// `TCO2_STS`: the second expiry, the one that resets, happened; it survives
/// the reset it caused (write 1 to clear).
const SECOND_TO_STS: u16 = 1 << 1;
/// `TCO2_STS`: set with `SECOND_TO_STS` on version 2 (write 1 to clear).
const BOOT_STS: u16 = 1 << 2;
/// `TCO1_CNT`: the timer is halted.
const TMR_HLT: u16 = 1 << 11;
/// `TCO1_CNT`: firmware locked the halt bit until the next reset.
const TCO_LOCK: u16 = 1 << 12;
/// `TCO1_CNT`: reads back inverted after a write of 1; masked on every write.
const NMI_NOW: u16 = 1 << 8;
/// `TCO1_CNT`, version 6: the second expiry does not reset.
const NO_REBOOT_CNT: u16 = 1 << 0;
/// `GCS`, version 2: the second expiry does not reset.
const NO_REBOOT_GCS: u32 = 1 << 5;
/// `TCO_TMR`'s count: 10 bits.
const TMR_MASK: u16 = 0x3ff;

/// One tick of the timer, in milliseconds.
const TICK_MS: u64 = 600;
/// The fewest ticks the timer takes ("values of 0h-3h are ignored") and the
/// most its 10 bits hold.
const MIN_TICKS: u16 = 4;
const MAX_TICKS: u16 = TMR_MASK;

/// The ticks the panic path sets ([`panic_backstop`]): 21 ticks, 12.6 s to
/// the first expiry and 25.2 s to the reset -- after `ferrix.panic=reset`'s
/// own reset (its report and 10 s), and before anybody would reach for the
/// plug.
const PANIC_TICKS: u16 = 21;

/// The identities this module knows, by vendor and device.
const ADL_N_SMBUS: (u16, u16) = (0x8086, 0x54a3);
const ICH9_LPC: (u16, u16) = (0x8086, 0x2918);

/// The PCI location each must be found at: bus 0, device 0x1f, function 4
/// and 0, as `DeviceNode::describe` encodes it.
const SMBUS_LOCATION: u32 = (0x1f << 3) | 4;
const LPC_LOCATION: u32 = 0x1f << 3;

/// The version 6 function's registers.
const TCOBASE: u64 = 0x50;
const TCOCTL: u64 = 0x54;
const TCOCTL_EN: u32 = 1 << 8;
const TCOBASE_MASK: u32 = 0xffe0;
/// The ICH9 bridge's registers.
const PMBASE: u64 = 0x40;
const ACPI_CNTL: u64 = 0x44;
const ACPI_EN: u32 = 1 << 7;
const PMBASE_MASK: u32 = 0xff80;
const TCO_IN_PMBASE: u16 = 0x60;
const RCBA: u64 = 0xf0;
const RCBA_EN: u32 = 1 << 0;
const RCBA_MASK: u32 = 0xffff_c000;
const GCS: u64 = 0x3410;

/// The TCO's I/O base once [`claim`] has taken it; 0 for none.
static BASE: AtomicU16 = AtomicU16::new(0);
/// The ICH9's `GCS` register, mapped; 0 on version 6, whose `NO_REBOOT` is a
/// TCO register.
static GCS_VIRT: AtomicU64 = AtomicU64::new(0);
/// Whether ring 3's open has the timer counting.
static ARMED: AtomicBool = AtomicBool::new(false);
/// Whether [`claim`] found `SECOND_TO_STS`: the timer reset the boot before.
static PREVIOUS_RESET: AtomicBool = AtomicBool::new(false);

/// What [`claim`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claimed {
    /// The timer is this kernel's, halted; `previous_reset` says whether it
    /// had reset the boot before this one.
    Taken {
        /// The I/O base.
        base: u16,
        /// The TCO's version, 2 or 6.
        version: u8,
        /// `SECOND_TO_STS` was set.
        previous_reset: bool,
    },
    /// No device of a known identity.
    Absent,
    /// A device, refused for the reason given; left as found.
    Refused(&'static str),
}

/// Find the timer, take it, and leave it halted with its status cleared.
/// Once, at boot, after the device nodes are published.
pub fn claim() -> Claimed {
    let found = device::devices().iter().find_map(|node| {
        let info = node.describe();
        let id = (info.vendor_id, info.device_id);
        if id == ADL_N_SMBUS && info.location == SMBUS_LOCATION {
            let base = node.config_read(TCOBASE, 4).ok()?;
            let control = node.config_read(TCOCTL, 4).ok()?;
            if control & TCOCTL_EN == 0 {
                return Some(Err(
                    "TCOCTL's TCO_BASE_EN is clear: the TCO's ports are off",
                ));
            }
            let base = u16::try_from(base & TCOBASE_MASK).unwrap_or(0);
            Some(Ok((base, 6_u8, 0_u64)))
        } else if id == ICH9_LPC && info.location == LPC_LOCATION {
            let pmbase = node.config_read(PMBASE, 4).ok()?;
            let control = node.config_read(ACPI_CNTL, 4).ok()?;
            let rcba = node.config_read(RCBA, 4).ok()?;
            if control & ACPI_EN == 0 {
                return Some(Err("ACPI_CNTL's ACPI_EN is clear: the PM ports are off"));
            }
            if rcba & RCBA_EN == 0 {
                return Some(Err(
                    "the root complex base is not enabled, so NO_REBOOT is out of reach",
                ));
            }
            let base = u16::try_from(pmbase & PMBASE_MASK)
                .unwrap_or(0)
                .saturating_add(TCO_IN_PMBASE);
            let gcs = u64::from(rcba & RCBA_MASK) + GCS;
            Some(Ok((base, 2, gcs)))
        } else {
            None
        }
    });
    let (base, version, gcs) = match found {
        None => return Claimed::Absent,
        Some(Err(why)) => return Claimed::Refused(why),
        Some(Ok(found)) => found,
    };
    if base == 0 || base == TCO_IN_PMBASE {
        return Claimed::Refused("its I/O base reads as zero");
    }
    if gcs != 0 {
        match crate::vmap::map_device(gcs, 4) {
            Ok(virt) => GCS_VIRT.store(virt, Ordering::Relaxed),
            Err(_) => return Claimed::Refused("GCS could not be mapped"),
        }
    }
    let control = read(base, TCO1_CNT);
    if control & TCO_LOCK != 0 {
        GCS_VIRT.store(0, Ordering::Relaxed);
        return Claimed::Refused(if control & TMR_HLT == 0 {
            "firmware locked it running"
        } else {
            "firmware locked it halted"
        });
    }
    let second = read(base, TCO2_STS) & SECOND_TO_STS != 0;
    // Halted first, so that clearing NO_REBOOT to test it cannot let a
    // running count reset the machine.
    write(base, TCO1_CNT, (control & !NMI_NOW) | TMR_HLT);
    write(base, TCO1_STS, TIMEOUT);
    write(base, TCO2_STS, SECOND_TO_STS | BOOT_STS);
    if !set_no_reboot(base, false) {
        let _ = set_no_reboot(base, true);
        GCS_VIRT.store(0, Ordering::Relaxed);
        return Claimed::Refused("its NO_REBOOT does not clear: firmware holds the reset off");
    }
    let _ = set_no_reboot(base, true);
    PREVIOUS_RESET.store(second, Ordering::Relaxed);
    BASE.store(base, Ordering::Release);
    Claimed::Taken {
        base,
        version,
        previous_reset: second,
    }
}

/// Whether [`claim`] took a timer.
pub fn claimed() -> bool {
    BASE.load(Ordering::Acquire) != 0
}

/// Whether the timer reset the boot before this one.
pub fn previous_reset() -> bool {
    PREVIOUS_RESET.load(Ordering::Relaxed)
}

/// The ticks for a reset `seconds` after the last reload: two expiries of
/// the count, so half of it each, rounded up and held to what the register
/// takes.
pub fn ticks_for(seconds: u32) -> u16 {
    let half_ms = u64::from(seconds).saturating_mul(1000).div_ceil(2);
    let ticks = half_ms.div_ceil(TICK_MS);
    u16::try_from(ticks)
        .unwrap_or(MAX_TICKS)
        .clamp(MIN_TICKS, MAX_TICKS)
}

/// The seconds a count of `ticks` resets after: two expiries.
pub fn seconds_for(ticks: u16) -> u64 {
    u64::from(ticks) * 2 * TICK_MS / 1000
}

/// Arm the timer to reset the machine `seconds` after the last reload, and
/// reload it. What ring 3's open and a new timeout come to.
///
/// # Errors
///
/// No timer claimed, or a register that did not take its value: the timer is
/// halted again.
pub fn arm(seconds: u32) -> Result<u16, &'static str> {
    let base = BASE.load(Ordering::Acquire);
    if base == 0 {
        return Err("no TCO timer was claimed");
    }
    let ticks = ticks_for(seconds);
    let timer = read(base, TCO_TMR);
    write(base, TCO_TMR, (timer & !TMR_MASK) | ticks);
    if read(base, TCO_TMR) & TMR_MASK != ticks {
        halt_at(base);
        return Err("TCO_TMR did not take the count");
    }
    if !set_no_reboot(base, false) {
        halt_at(base);
        return Err("NO_REBOOT did not clear");
    }
    write(base, TCO1_STS, TIMEOUT);
    write(base, TCO_RLD, 1);
    let control = read(base, TCO1_CNT) & !NMI_NOW;
    write(base, TCO1_CNT, control & !TMR_HLT);
    if read(base, TCO1_CNT) & TMR_HLT != 0 {
        halt_at(base);
        return Err("TCO_TMR_HLT did not clear");
    }
    ARMED.store(true, Ordering::Release);
    Ok(ticks)
}

/// Reload an armed timer: the count starts again from `TCO_TMR`, and a first
/// expiry already passed is forgotten, so the next reset is a whole count
/// away.
pub fn feed() {
    let base = BASE.load(Ordering::Acquire);
    if base != 0 && ARMED.load(Ordering::Acquire) {
        write(base, TCO1_STS, TIMEOUT);
        write(base, TCO_RLD, 1);
    }
}

/// Halt the timer and hold its reset off: a disarm on purpose, and every
/// stop on purpose (`reboot(2)`, a power-off), so that the next system -- the
/// box's Linux, say -- is not reset by a count it never started.
pub fn halt() {
    let base = BASE.load(Ordering::Acquire);
    if base != 0 {
        halt_at(base);
    }
}

/// [`halt`] at `base`.
fn halt_at(base: u16) {
    ARMED.store(false, Ordering::Release);
    let control = read(base, TCO1_CNT) & !NMI_NOW;
    write(base, TCO1_CNT, control | TMR_HLT);
    let _ = set_no_reboot(base, true);
}

/// Whether the timer is counting for ring 3.
pub fn armed() -> bool {
    ARMED.load(Ordering::Acquire)
}

/// The panic path's: make sure a claimed timer resets the machine
/// [`PANIC_TICKS`] twice over from now, whatever the rest of the report
/// does -- armed or not, and sooner than ring 3's count would. Nothing after
/// this point feeds it. Port writes and, on version 2, one memory write; no
/// lock, no allocation, nothing read but `TCO1_CNT` and `TCO_TMR`.
pub fn panic_backstop() -> bool {
    let base = BASE.load(Ordering::Acquire);
    if base == 0 {
        return false;
    }
    let timer = read(base, TCO_TMR);
    write(base, TCO_TMR, (timer & !TMR_MASK) | PANIC_TICKS);
    let _ = set_no_reboot(base, false);
    write(base, TCO1_STS, TIMEOUT);
    write(base, TCO_RLD, 1);
    let control = read(base, TCO1_CNT) & !NMI_NOW;
    write(base, TCO1_CNT, control & !TMR_HLT);
    ARMED.store(true, Ordering::Release);
    true
}

/// The seconds [`panic_backstop`] leaves the machine.
pub const fn panic_seconds() -> u64 {
    PANIC_TICKS as u64 * 2 * TICK_MS / 1000
}

/// Set or clear `NO_REBOOT`, and whether it reads back as set.
fn set_no_reboot(base: u16, on: bool) -> bool {
    let gcs = GCS_VIRT.load(Ordering::Relaxed);
    if gcs == 0 {
        let control = read(base, TCO1_CNT) & !NMI_NOW;
        let wanted = if on {
            control | NO_REBOOT_CNT
        } else {
            control & !NO_REBOOT_CNT
        };
        write(base, TCO1_CNT, wanted);
        (read(base, TCO1_CNT) & NO_REBOOT_CNT != 0) == on
    } else {
        let registers = Mmio::at(gcs);
        let value = registers.read32(0);
        let wanted = if on {
            value | NO_REBOOT_GCS
        } else {
            value & !NO_REBOOT_GCS
        };
        registers.write32(0, wanted);
        (registers.read32(0) & NO_REBOOT_GCS != 0) == on
    }
}

/// Read the 16-bit TCO register at `offset`.
fn read(base: u16, offset: u16) -> u16 {
    // SAFETY: (DEVICE) `base` is the TCO's I/O base as the chipset's own
    // configuration names it (`claim`), and reading a TCO register changes
    // nothing.
    unsafe { cpu::inw(base.wrapping_add(offset)) }
}

/// Write the 16-bit TCO register at `offset`.
fn write(base: u16, offset: u16, value: u16) {
    // SAFETY: (DEVICE) `base` is the TCO's I/O base as the chipset's own
    // configuration names it (`claim`); every write here is one of the
    // timer's documented controls, and halting, arming or reloading the
    // watchdog is the point.
    unsafe { cpu::outw(base.wrapping_add(offset), value) }
}
