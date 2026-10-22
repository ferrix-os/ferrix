//! What the kernel's raw readings mean (`power_read`'s words,
//! `ferrix_native_abi::types::POWER_READ_*`). Layouts as Linux's
//! `msr-index.h` and `intel_rapl_common.c`, and turbostat's names for the
//! limit reasons, as `ferrix.msrlog` prints them.

use core::fmt;

use ferrix_native_abi::types::{
    POWER_READ_CEILING, POWER_READ_FIRMWARE, POWER_READ_FIRST, POWER_READ_FLAGS,
    POWER_READ_PRESENT, POWER_READ_VR, POWER_READ_WORDS,
};

/// The read-only registers' words, from [`POWER_READ_FIRST`], in order:
/// `MSR_TEMPERATURE_TARGET`.
pub const TEMPERATURE_TARGET: usize = POWER_READ_FIRST;
/// `IA32_THERM_STATUS`, the boot processor's core.
pub const THERM_STATUS: usize = POWER_READ_FIRST + 1;
/// `IA32_PACKAGE_THERM_STATUS`.
pub const PACKAGE_THERM_STATUS: usize = POWER_READ_FIRST + 2;
/// `IA32_PERF_STATUS`.
pub const PERF_STATUS: usize = POWER_READ_FIRST + 3;
/// `CORE_PERF_LIMIT_REASONS`.
pub const CORE_LIMIT_REASONS: usize = POWER_READ_FIRST + 4;
/// `GRAPHICS_PERF_LIMIT_REASONS`.
pub const GRAPHICS_LIMIT_REASONS: usize = POWER_READ_FIRST + 5;
/// `PKG_ENERGY_STATUS`.
pub const PACKAGE_ENERGY: usize = POWER_READ_FIRST + 6;
/// `RAPL_POWER_UNIT`.
pub const POWER_UNIT: usize = POWER_READ_FIRST + 7;
/// The package power limit register.
pub const PACKAGE_LIMIT: usize = POWER_READ_FIRST + 8;

/// turbostat's names for the limit-reason bits. Bit 8, "amps/pl4", is the
/// current limit the VR current register sets.
pub const REASON_NAMES: [(u32, &str); 11] = [
    (0, "prochot"),
    (1, "thermal"),
    (4, "graphics"),
    (5, "auto-hwp"),
    (6, "vr-thermal"),
    (8, "amps/pl4"),
    (9, "domain-power"),
    (10, "pkg-pl1"),
    (11, "pkg-pl2"),
    (12, "multi-core-turbo"),
    (13, "transitions"),
];

/// One `power_read`, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// The boot processor's core, degrees Celsius, if its readout is valid.
    pub core: Option<i64>,
    /// The package, degrees Celsius.
    pub package: Option<i64>,
    /// The boot processor's ratio (`IA32_PERF_STATUS` bits 15:8).
    pub ratio: Option<u64>,
    /// The core's limit reasons now (bits 15:0).
    pub core_reasons: Option<u16>,
    /// The graphics' limit reasons now.
    pub graphics_reasons: Option<u16>,
    /// The package energy counter, raw, and its unit's exponent.
    pub energy: Option<(u32, u32)>,
    /// The VR current limit in force, eighths of an ampere.
    pub vr_eighths: Option<u64>,
    /// Firmware's VR current limit, eighths of an ampere.
    pub firmware_eighths: u64,
    /// The highest the kernel admits, eighths.
    pub ceiling_eighths: u64,
    /// The kernel latched its power core off until reboot.
    pub latched: bool,
    /// The kernel left the register to firmware, which changed it.
    pub forgotten: bool,
}

/// Word `index` of `words`, if word [`POWER_READ_PRESENT`] says it was read.
fn present(words: &[u64; POWER_READ_WORDS], index: usize) -> Option<u64> {
    let mask = words.get(POWER_READ_PRESENT).copied().unwrap_or(0);
    let bit = u32::try_from(index).ok()?;
    if (mask >> bit) & 1 == 1 {
        words.get(index).copied()
    } else {
        None
    }
}

/// `TjMax` less a thermal status register's readout (bits 22:16).
#[must_use]
pub const fn celsius(tjmax: u64, raw: u64) -> i64 {
    tjmax as i64 - ((raw >> 16) & 0x7f) as i64
}

/// `power_read`'s words, decoded.
#[must_use]
pub fn reading(words: &[u64; POWER_READ_WORDS]) -> Reading {
    let tjmax = present(words, TEMPERATURE_TARGET).map(|raw| (raw >> 16) & 0xff);
    let core = tjmax
        .zip(present(words, THERM_STATUS))
        .and_then(|(tjmax, raw)| ((raw >> 31) & 1 == 1).then(|| celsius(tjmax, raw)));
    let package = tjmax
        .zip(present(words, PACKAGE_THERM_STATUS))
        .map(|(tjmax, raw)| celsius(tjmax, raw));
    let unit = present(words, POWER_UNIT).map(|raw| ((raw >> 8) & 0x1f) as u32);
    Reading {
        core,
        package,
        ratio: present(words, PERF_STATUS).map(|raw| (raw >> 8) & 0xff),
        core_reasons: present(words, CORE_LIMIT_REASONS).map(|raw| (raw & 0xffff) as u16),
        graphics_reasons: present(words, GRAPHICS_LIMIT_REASONS).map(|raw| (raw & 0xffff) as u16),
        energy: present(words, PACKAGE_ENERGY)
            .zip(unit)
            .map(|(raw, unit)| ((raw & 0xffff_ffff) as u32, unit)),
        vr_eighths: present(words, POWER_READ_VR).map(|raw| raw & 0x1fff),
        firmware_eighths: words.get(POWER_READ_FIRMWARE).copied().unwrap_or(0) & 0x1fff,
        ceiling_eighths: words.get(POWER_READ_CEILING).copied().unwrap_or(0),
        latched: words.get(POWER_READ_FLAGS).copied().unwrap_or(0) & 1 == 1,
        forgotten: words.get(POWER_READ_FLAGS).copied().unwrap_or(0) & 2 == 2,
    }
}

impl Reading {
    /// The hotter of the two temperatures read, if either was.
    #[must_use]
    pub fn hottest(&self) -> Option<i64> {
        match (self.core, self.package) {
            (Some(core), Some(package)) => Some(core.max(package)),
            (one, other) => one.or(other),
        }
    }
}

/// The package's power in milliwatts between two energy readings
/// `elapsed_ms` apart, the 32-bit counter's wrap allowed once; `None` for no
/// time or a unit past 31.
#[must_use]
pub fn milliwatts(before: (u32, u32), after: (u32, u32), elapsed_ms: u64) -> Option<u64> {
    if elapsed_ms == 0 || before.1 != after.1 || after.1 > 31 {
        return None;
    }
    let counts = u64::from(after.0.wrapping_sub(before.0));
    // counts / 2^unit joules over elapsed_ms: mW = counts * 10^6 / 2^unit / ms.
    Some(((counts * 1_000_000) >> after.1) / elapsed_ms)
}

/// The names of the reason bits set in `bits`, in bit order, into `out`;
/// how many were written.
pub fn reason_names<'a>(bits: u16, out: &mut [&'a str]) -> usize
where
    'static: 'a,
{
    let mut written = 0;
    for &(bit, name) in &REASON_NAMES {
        if (bits >> bit) & 1 == 1
            && let Some(slot) = out.get_mut(written)
        {
            *slot = name;
            written += 1;
        }
    }
    written
}

/// A current in eighths of an ampere, as amperes with three decimals, as
/// `ferrix.vrcurrent` printed them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Amps(pub u64);

impl fmt::Display for Amps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:03}", self.0 / 8, self.0 % 8 * 125)
    }
}
