//! `ferrix.vrcurrent`'s decisions, pure functions of their inputs and no
//! register's: the boot word's plan and its 25..32 A clamp
//! (`L.x86_64.190`), the value written -- only `MSR_VR_CURRENT_CONFIG` bits
//! 12:0 changed (`L.x86_64.193`) -- and the thermal guard's verdict
//! (`L.x86_64.191`). Stage 3 checks each on fixed values (`super::check`).

/// The lowest current the word may ask, in amperes: the N150's firmware
/// value. Below it the word is refused.
pub(in crate::arch::x86_64) const MIN_AMPS: u32 = 25;
/// The highest current the word may ask, in amperes (the customer's ceiling,
/// 2026-10-22). Above it the word is refused.
pub(in crate::arch::x86_64) const MAX_AMPS: u32 = 32;
/// The most steps one word may name.
pub(in crate::arch::x86_64) const MAX_STEPS: usize = 8;
/// The longest step, in seconds: an hour.
pub(in crate::arch::x86_64) const MAX_SECONDS: u32 = 3600;
/// `MSR_VR_CURRENT_CONFIG` bits 12:0: the current limit in 1/8 A.
pub(in crate::arch::x86_64) const LIMIT_MASK: u64 = 0x1fff;
/// `MSR_VR_CURRENT_CONFIG` bit 31: the lock.
pub(in crate::arch::x86_64) const LOCK_BIT: u32 = 31;
/// The guard's temperature: at or above it on either sensor, the firmware
/// value goes back.
pub(in crate::arch::x86_64) const HOT_CELSIUS: i64 = 90;
/// The `IA32_THERM_STATUS` and `IA32_PACKAGE_THERM_STATUS` bits any one of
/// which reverts: 0 thermal status, 1 its log, 2 PROCHOT# or FORCEPR#
/// event, 3 its log, 4 critical temperature, 5 its log.
pub(in crate::arch::x86_64) const TRIP_BITS: u64 = 0x3f;
/// `IA32_THERM_STATUS` bit 31: the core's readout is valid.
const READOUT_VALID: u32 = 31;
/// The lowest and highest `TjMax` believed; a register answering outside
/// (QEMU's TCG answers 0) refuses the run, since no temperature could be
/// computed from it.
pub(in crate::arch::x86_64) const TJMAX_RANGE: (u32, u32) = (80, 110);
/// The most a step may ask above the firmware's own limit, in amperes.
pub(in crate::arch::x86_64) const MAX_RAISE_AMPS: u64 = 7;
/// The one processor the run is authorised on: Intel family 6 model 0xBE,
/// Alder Lake-N (the N150), as `CPUID.1:EAX` gives family and model.
const AUTHORISED_FAMILY: u32 = 6;
/// See [`AUTHORISED_FAMILY`].
const AUTHORISED_MODEL: u32 = 0xbe;

/// The steps a word asked: amperes, in order, and the seconds each lasts
/// before the next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::arch::x86_64) struct Plan {
    /// The steps; only the first [`Plan::count`] mean anything.
    pub(in crate::arch::x86_64) amps: [u32; MAX_STEPS],
    /// How many steps, 1 to [`MAX_STEPS`].
    pub(in crate::arch::x86_64) count: usize,
    /// Seconds between steps, 1 to [`MAX_SECONDS`].
    pub(in crate::arch::x86_64) seconds: u32,
}

impl Plan {
    /// Step `index`'s amperes; the last step's past the end.
    pub(in crate::arch::x86_64) fn step(&self, index: usize) -> u32 {
        let last = self.count.saturating_sub(1);
        self.amps.get(index.min(last)).copied().unwrap_or(MIN_AMPS)
    }
}

/// Why a word, or the register it would change, was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::arch::x86_64) enum Refusal {
    /// Not `A1,A2,...@SECONDS` of decimal digits, or too many steps, or
    /// seconds outside 1 to 3600.
    Malformed,
    /// A step outside the clamp, [`MIN_AMPS`] to [`MAX_AMPS`].
    OutOfRange(u32),
    /// `MSR_VR_CURRENT_CONFIG`'s lock bit is set.
    Locked,
    /// The firmware's limit is 0, a value no step is measured from.
    NoFirmwareLimit,
    /// A step more than [`MAX_RAISE_AMPS`] above the firmware's limit.
    BeyondReach(u32),
}

/// A run of one to `width` ASCII decimal digits as a number; `None` for
/// anything else.
fn decimal(text: &str, width: usize) -> Option<u32> {
    let digits = text.as_bytes();
    if digits.is_empty() || digits.len() > width || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(
        digits
            .iter()
            .fold(0u32, |number, digit| number * 10 + u32::from(digit - b'0')),
    )
}

/// The plan in `ferrix.vrcurrent`'s value: one to [`MAX_STEPS`] amperes of
/// one or two digits, comma-separated, each within [`MIN_AMPS`] to
/// [`MAX_AMPS`]; `@`; seconds of one to four digits, 1 to [`MAX_SECONDS`].
///
/// # Errors
///
/// [`Refusal::OutOfRange`] for the first step outside the clamp,
/// [`Refusal::Malformed`] for anything else not of that form.
pub(in crate::arch::x86_64) fn parse(value: &str) -> Result<Plan, Refusal> {
    let (steps, seconds) = value.split_once('@').ok_or(Refusal::Malformed)?;
    let seconds = decimal(seconds, 4).ok_or(Refusal::Malformed)?;
    if seconds == 0 || seconds > MAX_SECONDS {
        return Err(Refusal::Malformed);
    }
    let mut plan = Plan {
        amps: [0; MAX_STEPS],
        count: 0,
        seconds,
    };
    for step in steps.split(',') {
        let amps = decimal(step, 2).ok_or(Refusal::Malformed)?;
        if !(MIN_AMPS..=MAX_AMPS).contains(&amps) {
            return Err(Refusal::OutOfRange(amps));
        }
        let slot = plan.amps.get_mut(plan.count).ok_or(Refusal::Malformed)?;
        *slot = amps;
        plan.count += 1;
    }
    Ok(plan)
}

/// `firmware` with bits 12:0 replaced by `amps` in 1/8 A and every other
/// bit as it was.
pub(in crate::arch::x86_64) const fn compose(firmware: u64, amps: u32) -> u64 {
    (firmware & !LIMIT_MASK) | ((amps as u64 * 8) & LIMIT_MASK)
}

/// The current limit in a raw `MSR_VR_CURRENT_CONFIG`, in 1/8 A.
pub(in crate::arch::x86_64) const fn eighths(raw: u64) -> u64 {
    raw & LIMIT_MASK
}

/// Whether a raw `MSR_VR_CURRENT_CONFIG` is locked.
pub(in crate::arch::x86_64) const fn locked(raw: u64) -> bool {
    (raw >> LOCK_BIT) & 1 == 1
}

/// Why the guard sends the firmware value back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::arch::x86_64) enum Trip {
    /// The core's readout is not valid (`IA32_THERM_STATUS` bit 31 clear).
    CoreInvalid,
    /// The core reads at or above [`HOT_CELSIUS`], in degrees Celsius.
    CoreHot(i64),
    /// The package reads at or above [`HOT_CELSIUS`].
    PackageHot(i64),
    /// A [`TRIP_BITS`] bit of the core's register, the lowest set.
    CoreBit(u32),
    /// A [`TRIP_BITS`] bit of the package's register.
    PackageBit(u32),
}

/// Both temperatures, in degrees Celsius.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::arch::x86_64) struct Temperatures {
    pub(in crate::arch::x86_64) core: i64,
    pub(in crate::arch::x86_64) package: i64,
}

/// A thermal status register's temperature: `TjMax` less its readout,
/// bits 22:16.
const fn celsius(tjmax: u32, raw: u64) -> i64 {
    tjmax as i64 - ((raw >> 16) & 0x7f) as i64
}

/// The guard's verdict on one reading of `IA32_THERM_STATUS` (`core`) and
/// `IA32_PACKAGE_THERM_STATUS` (`package`) under `tjmax`: the temperatures,
/// or the first reason to revert -- the core's readout invalid, a
/// [`TRIP_BITS`] bit set in the core's then the package's register, then
/// either at or above [`HOT_CELSIUS`].
///
/// # Errors
///
/// The [`Trip`].
pub(in crate::arch::x86_64) const fn verdict(
    tjmax: u32,
    core: u64,
    package: u64,
) -> Result<Temperatures, Trip> {
    if (core >> READOUT_VALID) & 1 == 0 {
        return Err(Trip::CoreInvalid);
    }
    if core & TRIP_BITS != 0 {
        return Err(Trip::CoreBit((core & TRIP_BITS).trailing_zeros()));
    }
    if package & TRIP_BITS != 0 {
        return Err(Trip::PackageBit((package & TRIP_BITS).trailing_zeros()));
    }
    let temperatures = Temperatures {
        core: celsius(tjmax, core),
        package: celsius(tjmax, package),
    };
    if temperatures.core >= HOT_CELSIUS {
        return Err(Trip::CoreHot(temperatures.core));
    }
    if temperatures.package >= HOT_CELSIUS {
        return Err(Trip::PackageHot(temperatures.package));
    }
    Ok(temperatures)
}

/// Whether a `TjMax` read is one the guard can compute from.
pub(in crate::arch::x86_64) const fn tjmax_believed(tjmax: u32) -> bool {
    tjmax >= TJMAX_RANGE.0 && tjmax <= TJMAX_RANGE.1
}

/// Whether `CPUID.0`'s vendor (`intel`) and `CPUID.1:EAX` (`signature`) name
/// the authorised processor, Intel family 6 model 0xBE, Alder Lake-N.
pub(in crate::arch::x86_64) const fn authorised(intel: bool, signature: u32) -> bool {
    let family = (signature >> 8) & 0xf;
    let model = ((signature >> 4) & 0xf) | ((signature >> 12) & 0xf0);
    intel && family == AUTHORISED_FAMILY && model == AUTHORISED_MODEL
}

/// Whether `plan` may be written over a register whose firmware value is
/// `firmware`: not locked, a limit there to measure from, and no step more
/// than [`MAX_RAISE_AMPS`] above it.
///
/// # Errors
///
/// The first [`Refusal`]: [`Refusal::Locked`], [`Refusal::NoFirmwareLimit`]
/// or [`Refusal::BeyondReach`] for the first step too high.
pub(in crate::arch::x86_64) fn admit(plan: &Plan, firmware: u64) -> Result<(), Refusal> {
    if locked(firmware) {
        return Err(Refusal::Locked);
    }
    let base = eighths(firmware);
    if base == 0 {
        return Err(Refusal::NoFirmwareLimit);
    }
    for &amps in plan.amps.iter().take(plan.count) {
        if u64::from(amps) * 8 > base + MAX_RAISE_AMPS * 8 {
            return Err(Refusal::BeyondReach(amps));
        }
    }
    Ok(())
}

/// The step in force `elapsed` whole seconds after the first write: one
/// more every [`Plan::seconds`], the last kept.
pub(in crate::arch::x86_64) const fn step_at(plan: &Plan, elapsed: u64) -> usize {
    let step = elapsed / plan.seconds as u64;
    let last = plan.count.saturating_sub(1);
    if step >= last as u64 {
        last
    } else {
        step as usize
    }
}

/// A plan as one word, for the task's argument: each step less
/// [`MIN_AMPS`] in three bits from bit 0, the count in four bits from 24,
/// the seconds in twelve from 28.
pub(in crate::arch::x86_64) fn pack(plan: &Plan) -> usize {
    let mut word = 0usize;
    for (index, &amps) in plan.amps.iter().take(plan.count).enumerate() {
        word |= ((amps.saturating_sub(MIN_AMPS) & 0x7) as usize) << (3 * index);
    }
    word | ((plan.count & 0xf) << 24) | (((plan.seconds & 0xfff) as usize) << 28)
}

/// [`pack`]'s word as the plan again; `None` for a word no plan packs to.
pub(in crate::arch::x86_64) fn unpack(word: usize) -> Option<Plan> {
    let count = (word >> 24) & 0xf;
    let seconds = ((word >> 28) & 0xfff) as u32;
    if count == 0 || count > MAX_STEPS || seconds == 0 || seconds > MAX_SECONDS {
        return None;
    }
    let mut plan = Plan {
        amps: [0; MAX_STEPS],
        count,
        seconds,
    };
    for (index, slot) in plan.amps.iter_mut().take(count).enumerate() {
        *slot = MIN_AMPS + ((word >> (3 * index)) & 0x7) as u32;
    }
    Some(plan)
}
