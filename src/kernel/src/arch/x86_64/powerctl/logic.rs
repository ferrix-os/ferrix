//! The power core's decisions, pure functions of their inputs and no
//! register's (`docs/POWERCTL.md` §5, §6): the model table and its gate,
//! the package count, the bounds of a VR current request, the value a
//! write composes, the backstop's verdict, the raise floor and the lease.
//! Stage 3 checks each on fixed values (`super::check`).

/// `MSR_VR_CURRENT_CONFIG` bits 12:0: the current limit in 1/8 A.
pub(in crate::arch::x86_64) const VR_FIELD: u64 = 0x1fff;
/// `MSR_VR_CURRENT_CONFIG` bit 31: the lock.
pub(in crate::arch::x86_64) const VR_LOCK: u64 = 1 << 31;
/// The thermal status bits any one of which trips the backstop: 0 thermal
/// status, 1 its log, 2 PROCHOT# or FORCEPR#, 3 its log, 4 critical
/// temperature, 5 its log.
const TRIP_BITS: u64 = 0x3f;
/// `IA32_THERM_STATUS` bit 31: the core's readout is valid.
const VALID: u64 = 1 << 31;
/// The `TjMax` believed; outside it (QEMU's TCG answers 0) the probe
/// refuses, since no temperature could be computed.
pub(in crate::arch::x86_64) const TJMAX_RANGE: core::ops::RangeInclusive<u64> = 80..=110;
/// The lease: no call on the claim for this long ends it (§6.5).
pub(in crate::arch::x86_64) const LEASE_NANOS: u64 = 5_000_000_000;
/// The least time between two raises, whichever claim made them (§5.4).
pub(in crate::arch::x86_64) const RAISE_NANOS: u64 = 1_000_000_000;

/// One row of the model table: a processor the core serves and the bounds
/// every request is held to. Constants: no word, driver or call moves them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::arch::x86_64) struct Row {
    /// `CPUID.1:EAX` family and model, extended model included.
    pub(in crate::arch::x86_64) family: u32,
    pub(in crate::arch::x86_64) model: u32,
    /// The VR current limit's absolute ceiling, eighths of an ampere.
    pub(in crate::arch::x86_64) ceiling: u64,
    /// The most it may be above firmware's, eighths.
    pub(in crate::arch::x86_64) raise: u64,
    /// The backstop's temperature, degrees Celsius.
    pub(in crate::arch::x86_64) hot: u64,
}

/// The model table (§5.3): the N150, Intel family 6 model 0xBE (Alder
/// Lake-N), single-package. The ceiling is the one the customer authorised
/// for `ferrix.vrcurrent`, 32 A and at most 7 A above firmware's, until
/// the customer's words for more are on record (ledger 3130, condition 1).
pub(in crate::arch::x86_64) const TABLE: [Row; 1] = [Row {
    family: 6,
    model: 0xbe,
    ceiling: 32 * 8,
    raise: 7 * 8,
    hot: 90,
}];

/// The row for `CPUID.0`'s vendor (`intel`) and `CPUID.1:EAX`, if any.
pub(in crate::arch::x86_64) fn row(intel: bool, signature: u32) -> Option<&'static Row> {
    let family = (signature >> 8) & 0xf;
    let model = ((signature >> 4) & 0xf) | ((signature >> 12) & 0xf0);
    TABLE
        .iter()
        .find(|row| intel && row.family == family && row.model == model)
}

/// How many packages processors with x2APIC ids `ids` span, a package's id
/// being the id shifted right by `shift`: 0, 1, or 2 for two or more.
pub(in crate::arch::x86_64) fn packages(shift: u32, mut ids: impl Iterator<Item = u64>) -> usize {
    let Some(first) = ids.next() else { return 0 };
    let package = |id: u64| id.checked_shr(shift).unwrap_or(0);
    if ids.any(|id| package(id) != package(first)) {
        2
    } else {
        1
    }
}

/// Why the probe refuses a node, from what it found: a row of the table,
/// the package count, 0x601 and `TjMax`; `None` to publish.
pub(in crate::arch::x86_64) fn refusal(
    row: bool,
    packages: Option<usize>,
    vr: Option<u64>,
    tjmax: Option<u64>,
) -> Option<&'static str> {
    if !row {
        return Some("not an authorised processor");
    }
    match packages {
        Some(1) => {}
        Some(_) => return Some("not exactly one package"),
        None => return Some("the package count is unknown"),
    }
    match vr {
        None => return Some("VR_CURRENT_CONFIG absent (#GP)"),
        Some(vr) if vr & VR_LOCK != 0 || vr & VR_FIELD == 0 => {
            return Some("VR_CURRENT_CONFIG locked or 0");
        }
        Some(_) => {}
    }
    if !tjmax.is_some_and(|tjmax| TJMAX_RANGE.contains(&tjmax)) {
        return Some("TjMax absent or outside 80 to 110 C");
    }
    None
}

/// The highest limit the row allows over firmware's `firmware` eighths.
pub(in crate::arch::x86_64) const fn highest(row: &Row, firmware: u64) -> u64 {
    let raised = firmware.saturating_add(row.raise);
    if raised < row.ceiling {
        raised
    } else {
        row.ceiling
    }
}

/// Whether `eighths` is within the row over firmware's 0x601: from
/// firmware's own limit up to [`highest`].
pub(in crate::arch::x86_64) const fn admitted(row: &Row, firmware: u64, eighths: u64) -> bool {
    let lowest = firmware & VR_FIELD;
    eighths >= lowest && eighths <= highest(row, lowest)
}

/// `now` with bits 12:0 replaced by `eighths`, every other bit as it was.
pub(in crate::arch::x86_64) const fn compose(now: u64, eighths: u64) -> u64 {
    (now & !VR_FIELD) | (eighths & VR_FIELD)
}

/// Why the backstop restores: the core's readout invalid, a trip bit in
/// the core's or the package's register, or a temperature at the row's
/// limit, in degrees.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::arch::x86_64) enum Trip {
    Invalid,
    CoreBit(u32),
    PackageBit(u32),
    Hot(u64),
}

/// The backstop's verdict on `IA32_THERM_STATUS` (`core`) and
/// `IA32_PACKAGE_THERM_STATUS` (`package`) under `tjmax` at `hot` degrees.
///
/// # Errors
///
/// The first [`Trip`].
pub(in crate::arch::x86_64) const fn verdict(
    tjmax: u64,
    hot: u64,
    core: u64,
    package: u64,
) -> Result<(), Trip> {
    if core & VALID == 0 {
        return Err(Trip::Invalid);
    }
    if core & TRIP_BITS != 0 {
        return Err(Trip::CoreBit((core & TRIP_BITS).trailing_zeros()));
    }
    if package & TRIP_BITS != 0 {
        return Err(Trip::PackageBit((package & TRIP_BITS).trailing_zeros()));
    }
    // The hotter readout is the one nearer TjMax (bits 22:16, degrees
    // below it).
    let (core_below, package_below) = ((core >> 16) & 0x7f, (package >> 16) & 0x7f);
    let below = if core_below < package_below {
        core_below
    } else {
        package_below
    };
    let celsius = tjmax.saturating_sub(below);
    if celsius >= hot {
        Err(Trip::Hot(celsius))
    } else {
        Ok(())
    }
}

/// Whether a raise may be written at `now`, the last at `last`.
pub(in crate::arch::x86_64) const fn raise_due(last: Option<u64>, now: u64) -> bool {
    match last {
        None => true,
        Some(last) => now.saturating_sub(last) >= RAISE_NANOS,
    }
}

/// Whether a lease renewed at `renewed` has lapsed at `now`.
pub(in crate::arch::x86_64) const fn lapsed(renewed: u64, now: u64) -> bool {
    now.saturating_sub(renewed) >= LEASE_NANOS
}
