//! The fields of the registers `ferrix.msrlog` prints (`L.x86_64.189`):
//! pure functions of a raw value, checked on fixed values at boot
//! (`super::check`). Layouts as Linux's `intel_rapl_common.c` and
//! `msr-index.h`, and turbostat's names for the limit reasons.

/// `MSR_RAPL_POWER_UNIT`: each unit is `1 / 2^n` of a watt, joule, second.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::arch::x86_64) struct Units {
    /// Bits 3:0.
    pub(in crate::arch::x86_64) power: u32,
    /// Bits 12:8.
    pub(in crate::arch::x86_64) energy: u32,
    /// Bits 19:16.
    pub(in crate::arch::x86_64) time: u32,
}

impl Units {
    /// The units most Intel parts report (1/8 W, 1/16384 J, 1/1024 s),
    /// used to decode the limits where `MSR_RAPL_POWER_UNIT` is absent.
    pub(in crate::arch::x86_64) const DEFAULT: Self = Self {
        power: 3,
        energy: 14,
        time: 10,
    };

    /// The units in a raw `MSR_RAPL_POWER_UNIT`.
    pub(in crate::arch::x86_64) const fn from_raw(raw: u64) -> Self {
        Self {
            power: (raw & 0xf) as u32,
            energy: ((raw >> 8) & 0x1f) as u32,
            time: ((raw >> 16) & 0xf) as u32,
        }
    }

    /// `count` power units in milliwatts, rounded down.
    pub(in crate::arch::x86_64) const fn milliwatts(self, count: u64) -> u64 {
        count.saturating_mul(1000) >> self.power
    }

    /// A time window field (bits 4:0 `Y`, 6:5 `Z`) in milliseconds:
    /// `2^Y * (1 + Z/4)` time units, rounded down.
    pub(in crate::arch::x86_64) const fn window_millis(self, window: u8) -> u64 {
        let y = (window & 0x1f) as u32;
        let z = ((window >> 5) & 0x3) as u64;
        ((1000 * (4 + z)) << y) >> (self.time + 2)
    }
}

/// One power limit: its value in power units, enable and clamp bits, and
/// its 7-bit time window field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::arch::x86_64) struct Limit {
    pub(in crate::arch::x86_64) power: u64,
    pub(in crate::arch::x86_64) enabled: bool,
    pub(in crate::arch::x86_64) clamp: bool,
    pub(in crate::arch::x86_64) window: u8,
}

/// Bit `n` of `raw`.
pub(in crate::arch::x86_64) const fn bit(raw: u64, n: u32) -> bool {
    (raw >> n) & 1 == 1
}

/// A limit whose `width`-bit power field starts at `shift`, its enable
/// bit right above it, then clamp, then the window.
const fn limit(raw: u64, shift: u32, width: u32) -> Limit {
    let field = raw >> shift;
    Limit {
        power: field & ((1 << width) - 1),
        enabled: bit(field, width),
        clamp: bit(field, width + 1),
        window: ((field >> (width + 2)) & 0x7f) as u8,
    }
}

/// `MSR_PKG_POWER_LIMIT`: PL1 in bits 14:0 (enable 15, clamp 16, window
/// 23:17) and PL2 the same from bit 32. Bit 63 is the lock.
pub(in crate::arch::x86_64) const fn package_limits(raw: u64) -> (Limit, Limit) {
    (limit(raw, 0, 15), limit(raw, 32, 15))
}

/// `MSR_PLATFORM_POWER_LIMIT`: Psys PL1 in bits 16:0 (enable 17, clamp 18,
/// window 25:19) and PL2 the same from bit 32. Bit 63 is the lock.
pub(in crate::arch::x86_64) const fn platform_limits(raw: u64) -> (Limit, Limit) {
    (limit(raw, 0, 17), limit(raw, 32, 17))
}

/// `MSR_VR_CURRENT_CONFIG`: the VR current limit, bits 12:0 in 1/8 A, in
/// milliamperes, and the lock in bit 31.
pub(in crate::arch::x86_64) const fn vr_current(raw: u64) -> (u64, bool) {
    ((raw & 0x1fff) * 125, bit(raw, 31))
}

/// `MSR_PKG_POWER_INFO`: TDP, minimum and maximum, in power units.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::arch::x86_64) struct PowerInfo {
    pub(in crate::arch::x86_64) tdp: u64,
    pub(in crate::arch::x86_64) min: u64,
    pub(in crate::arch::x86_64) max: u64,
}

/// TDP bits 14:0, minimum 30:16, maximum 46:32.
pub(in crate::arch::x86_64) const fn power_info(raw: u64) -> PowerInfo {
    PowerInfo {
        tdp: raw & 0x7fff,
        min: (raw >> 16) & 0x7fff,
        max: (raw >> 32) & 0x7fff,
    }
}

/// A limit-reasons word: the reasons now (bits 15:0) and the sticky log
/// of them (bits 31:16).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::arch::x86_64) struct Reasons {
    pub(in crate::arch::x86_64) now: u16,
    pub(in crate::arch::x86_64) log: u16,
}

/// Split a limit-reasons register.
pub(in crate::arch::x86_64) const fn reasons(raw: u64) -> Reasons {
    Reasons {
        now: (raw & 0xffff) as u16,
        log: ((raw >> 16) & 0xffff) as u16,
    }
}

/// turbostat's names for the reason bits (`MSR_CORE_PERF_LIMIT_REASONS`,
/// `MSR_GRAPHICS_PERF_LIMIT_REASONS`). Bit 8, "amps", is the current/peak
/// limit that i915's `GT0_PERF_LIMIT_REASONS` calls PL4.
pub(in crate::arch::x86_64) const REASON_NAMES: [(u32, &str); 11] = [
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

/// The bits [`REASON_NAMES`] names: 0, 1, 4-6 and 8-13 (the boot check
/// folds the table and compares).
pub(in crate::arch::x86_64) const NAMED_REASONS: u16 = 0x3f73;

/// `IA32_PERF_STATUS`: the ratio (bits 15:8) and the core voltage (bits
/// 47:32 in 1/8192 V) in microvolts.
pub(in crate::arch::x86_64) const fn perf_status(raw: u64) -> (u64, u64) {
    let ratio = (raw >> 8) & 0xff;
    let voltage = (raw >> 32) & 0xffff;
    (ratio, voltage * 1_000_000 / 8192)
}

/// `MSR_TEMPERATURE_TARGET`: `TjMax`, bits 23:16, in degrees Celsius.
pub(in crate::arch::x86_64) const fn tjmax(raw: u64) -> u32 {
    ((raw >> 16) & 0xff) as u32
}

/// `IA32_THERM_STATUS` and `IA32_PACKAGE_THERM_STATUS`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::arch::x86_64) struct Thermal {
    /// Bit 0: throttling now.
    pub(in crate::arch::x86_64) status: bool,
    /// Bit 1: throttled since cleared.
    pub(in crate::arch::x86_64) log: bool,
    /// Bits 22:16, degrees below `TjMax`.
    pub(in crate::arch::x86_64) below_tjmax: u32,
    /// Bit 31: the reading is valid (the core register's; printed for
    /// the package's too, which may not set it).
    pub(in crate::arch::x86_64) valid: bool,
}

/// Split a thermal status register.
pub(in crate::arch::x86_64) const fn thermal(raw: u64) -> Thermal {
    Thermal {
        status: bit(raw, 0),
        log: bit(raw, 1),
        below_tjmax: ((raw >> 16) & 0x7f) as u32,
        valid: bit(raw, 31),
    }
}
