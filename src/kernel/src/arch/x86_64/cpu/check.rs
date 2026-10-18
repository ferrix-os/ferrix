//! The boot's checks of the hardware P-state request (`L.x86_64.177`): the
//! request composed from capabilities described rather than read, and the
//! count of processors asked against the processors online.
//!
//! A processor whose request did not read back as written says so on its
//! boot line and is counted apart ([`hwp_refused`]); these checks never
//! fail on that, since a performance feature must not stop a boot. They fail
//! on a request composed wrongly and on a count that disagrees with the
//! processors that have the feature. The preference the request carries is
//! `ferrix.epp`'s (`L.x86_64.182`): its values chosen from a table, and the
//! one in force compared with the command line's.

use super::{
    Preference, choose_preference, has_hwp, hwp_processors, hwp_refused, hwp_request, preference,
};

/// Stage 3's checks of the hardware P-state request.
///
/// # Errors
///
/// The first that failed.
pub(in crate::arch::x86_64) fn run() -> Result<(), &'static str> {
    requests_composed()?;
    nothing_counted_without_hwp()?;
    preferences_chosen()
}

/// The request from made-up `IA32_HWP_CAPABILITIES`: an N150-like core
/// with Highest 36, Guaranteed 12, `Most_Efficient` 8 and Lowest 1 is asked
/// for levels 1 to 36, Desired 0, with the preference in bits 31:24 only
/// where it takes one, performance (0) and `balance_performance` (0x80)
/// alike; the capabilities' reserved upper half reaches no field.
///
/// Verifies: `L.x86_64.177`
fn requests_composed() -> Result<(), &'static str> {
    let n150 = 0x0108_0c24;
    let (value, request) = hwp_request(n150, Some(0x80));
    if value != 0x8000_2401 || request.lowest != 1 || request.highest != 36 {
        return Err("HWP's request is not the whole range of its capabilities with EPP 0x80");
    }
    let (value, request) = hwp_request(n150, Some(0));
    if value != 0x2401 || request.epp != Some(0) {
        return Err("HWP's request is not the whole range of its capabilities with EPP 0");
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

/// `ferrix.epp`'s values, each with the choice it must give: no option is
/// the default, performance; the four names their Linux values; one to
/// three digits their number up to 255; and everything else, a sign, a
/// `0x`, a fourth digit, a number above 255, an empty value, another case
/// or a space, not understood and so the default.
///
/// Verifies: `L.x86_64.182`
fn preferences_chosen() -> Result<(), &'static str> {
    if choose_preference(None) != Preference::Default || Preference::Default.epp() != 0 {
        return Err("the preference without ferrix.epp is not performance");
    }
    if Preference::NotUnderstood.epp() != 0 {
        return Err("the preference for a ferrix.epp not understood is not performance");
    }
    let understood: [(&str, u8); 9] = [
        ("performance", 0x00),
        ("balance_performance", 0x80),
        ("balance_power", 0xc0),
        ("power", 0xff),
        ("0", 0),
        ("00", 0),
        ("128", 128),
        ("255", 255),
        ("007", 7),
    ];
    for (value, epp) in understood {
        if choose_preference(Some(value)) != Preference::Asked(epp) {
            return Err("a ferrix.epp name or number did not choose its preference");
        }
    }
    let refused = [
        "256",
        "999",
        "1000",
        "0255",
        "-1",
        "+1",
        "0x80",
        "",
        "Performance",
        "performance ",
        " 1",
        "default",
    ];
    for value in refused {
        if choose_preference(Some(value)) != Preference::NotUnderstood {
            return Err("a ferrix.epp value outside the four names and 0 to 255 was not refused");
        }
    }
    Ok(())
}

/// The preference every processor asks for is the one `ferrix.epp` on this
/// boot's command line chooses, read here by the caller from the command
/// line itself rather than from anything `read_preference` kept: so a boot
/// whose option was never read fails here whenever it named another
/// preference than the default.
///
/// Verifies: `L.x86_64.182`
///
/// # Errors
///
/// The preference in force is not the command line's.
pub(crate) fn check_preference_read(value: Option<&str>) -> Result<(), &'static str> {
    if preference() != choose_preference(value).epp() {
        return Err("the preference every processor asks for is not the one ferrix.epp chose");
    }
    Ok(())
}
