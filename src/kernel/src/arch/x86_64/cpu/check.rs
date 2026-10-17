//! The boot's checks of the hardware P-state request (`L.x86_64.177`): the
//! request composed from capabilities described rather than read, and the
//! count of processors asked against the processors online.
//!
//! A processor whose request did not read back as written says so on its
//! boot line and is counted apart ([`hwp_refused`]); these checks never
//! fail on that, since a performance feature must not stop a boot. They fail
//! on a request composed wrongly and on a count that disagrees with the
//! processors that have the feature.

use super::{HWP_EPP, has_hwp, hwp_processors, hwp_refused, hwp_request};

/// Stage 3's checks of the hardware P-state request.
///
/// # Errors
///
/// The first that failed.
pub(in crate::arch::x86_64) fn run() -> Result<(), &'static str> {
    requests_composed()?;
    nothing_counted_without_hwp()
}

/// The request from made-up `IA32_HWP_CAPABILITIES`: an N150-like core
/// with Highest 36, Guaranteed 12, `Most_Efficient` 8 and Lowest 1 is asked
/// for levels 1 to 36, Desired 0, with the preference in bits 31:24 only
/// where it takes one; the capabilities' reserved upper half reaches no
/// field.
///
/// Verifies: `L.x86_64.177`
fn requests_composed() -> Result<(), &'static str> {
    let n150 = 0x0108_0c24;
    let (value, request) = hwp_request(n150, Some(HWP_EPP));
    if value != 0x8000_2401 || request.lowest != 1 || request.highest != 36 {
        return Err("HWP's request is not the whole range of its capabilities with EPP 0x80");
    }
    if hwp_request(n150, None).0 != 0x2401 {
        return Err("HWP's request wrote a preference on a processor that takes none");
    }
    if hwp_request(0xffff_ffff_0108_0c24, None).0 != 0x2401 {
        return Err("HWP's request took bits from the capabilities' reserved half");
    }
    Ok(())
}

/// On the processor that runs stage 3: where `CPUID.06H:EAX[7]` is clear,
/// no processor was counted, asked or refused.
///
/// Verifies: `L.x86_64.177`
fn nothing_counted_without_hwp() -> Result<(), &'static str> {
    if !has_hwp() && (hwp_processors() != 0 || hwp_refused() != 0) {
        return Err("HWP was counted as asked on a processor without it");
    }
    Ok(())
}

/// Once the secondaries are up: where the processor running this has
/// hardware P-states, every processor online was asked and read back, but
/// for those that said otherwise on their line; where it has not, none was
/// counted. Prints the `hwp` line.
///
/// Verifies: `L.x86_64.177`
///
/// # Errors
///
/// A count that disagrees with the processors online.
pub(crate) fn check_hwp_processors() -> Result<(), &'static str> {
    let online = crate::smp::count() as u64;
    let (asked, refused) = (hwp_processors(), hwp_refused());
    if !has_hwp() {
        if asked != 0 || refused != 0 {
            return Err("HWP was counted as asked on a processor without it");
        }
        crate::println!("  hwp      no hardware P-states: none of {online} processors asked");
        return Ok(());
    }
    if asked + refused != online {
        return Err("a processor with HWP was not asked for its whole range");
    }
    crate::println!(
        "  hwp      {asked} of {online} processors asked for their whole range and read back, \
         {refused} left to firmware"
    );
    Ok(())
}
