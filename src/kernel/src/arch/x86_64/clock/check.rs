//! The boot's checks of the clock's choice (`docs/CLOCK.md` §8): the
//! decisions and the arithmetic from machines described rather than run on,
//! then this machine's own facts against the counter it was given.

use core::sync::atomic::Ordering;

use super::{
    HpetFacts, HpetReason, KVM_FEATURE_CLOCKSOURCE_STABLE, KVM_FEATURE_CLOCKSOURCE2, KVM_SIGNATURE,
    KvmFacts, PVCLOCK_TSC_STABLE, Source, TIME_PAGE_TRIES, TimeInfo, choose, counter_hz,
    counter_is_tsc, hpet_reason, invariant_tsc, kvm_facts, kvm_time_page, kvm_time_page_registered,
    kvm_word, pvclock_hz, read_time_page,
};

/// The multiplier and shift nazuna's time page showed on 2026-10-10 (a
/// Ryzen 9 under KVM, `-cpu host`), saying stable.
const NAZUNA: TimeInfo = TimeInfo {
    mul: NAZUNA_MUL,
    shift: NAZUNA_SHIFT,
    flags: PVCLOCK_TSC_STABLE,
};
/// [`NAZUNA`]'s multiplier.
const NAZUNA_MUL: u32 = 0xe8b9_ded1;
/// [`NAZUNA`]'s shift.
const NAZUNA_SHIFT: i8 = -2;
/// The rate [`NAZUNA`] states, worked out apart from [`pvclock_hz`]:
/// `floor(10^9 * 2^34 / 3,904,495,313)`, 4,400.023 `MHz`.
const NAZUNA_HZ: u64 = 4_400_023_000;

/// Every check of the clock's choice, in the order of their requirements.
///
/// # Errors
///
/// The first that failed.
pub(in crate::arch::x86_64) fn run() -> Result<(), &'static str> {
    // The rate first: the table of sources is written in its terms.
    pvclock_rates()?;
    sources_decided()?;
    time_page_given_back()?;
    time_page_read_whole()?;
    reason_only_for_the_hpet()?;
    counter_matches_machine()
}

/// A machine as [`choose`] is told of it, and the counter it must get.
type Machine = (bool, Result<u64, HpetReason>, Option<HpetFacts>, Source);

/// KVM vouching in full, at [`NAZUNA`]'s rate.
const fn vouching() -> KvmFacts {
    KvmFacts {
        hypervisor: true,
        signature: KVM_SIGNATURE,
        features: KVM_FEATURE_CLOCKSOURCE2 | KVM_FEATURE_CLOCKSOURCE_STABLE,
        page: Some(NAZUNA),
    }
}

/// Each machine [`choose`] can be shown, with the counter it must get: the
/// five rows of `docs/CLOCK.md` §3, an invariant TSC whose measurement did
/// not come off, and KVM's word refused for each single condition of row 2
/// that can fail, every one of which falls through to the HPET with its own
/// reason.
///
/// # Errors
///
/// The first machine given a counter it must not get.
/// Verifies: `L.x86_64.166`
fn sources_decided() -> Result<(), &'static str> {
    let wide = |measured| {
        Some(HpetFacts {
            wide: true,
            measured,
        })
    };
    let narrow = |measured| {
        Some(HpetFacts {
            wide: false,
            measured,
        })
    };
    let with = |change: fn(&mut KvmFacts)| {
        let mut facts = vouching();
        change(&mut facts);
        kvm_word(&facts)
    };
    let yes = kvm_word(&vouching());
    if yes != Ok(NAZUNA_HZ) {
        return Err("KVM vouching in full was not taken at its time page's rate");
    }
    let no = Err(HpetReason::NoHypervisor);

    let rows: [Machine; 9] = [
        // Row 1, and KVM's word not looked at with the bit.
        (true, yes, wide(Some(7)), Source::TscByHpet(7)),
        (true, no, narrow(Some(7)), Source::TscByHpet(7)),
        // Row 2, with either kind of HPET and with none.
        (false, yes, wide(None), Source::TscByKvm(NAZUNA_HZ)),
        (false, yes, narrow(None), Source::TscByKvm(NAZUNA_HZ)),
        (false, yes, None, Source::TscByKvm(NAZUNA_HZ)),
        // Row 3, with the bit and without.
        (true, no, None, Source::TscByPit),
        (false, no, None, Source::TscByPit),
        // Row 5.
        (false, no, narrow(Some(9)), Source::TscByNarrowHpet(9)),
        // An invariant TSC that could not be measured: the HPET, saying so,
        // whatever KVM would have said.
        (
            true,
            yes,
            wide(None),
            Source::Hpet(HpetReason::MeasurementFailed),
        ),
    ];
    for (invariant, kvm, hpet, want) in rows {
        if choose(invariant, kvm, hpet) != Ok(want) {
            return Err("a machine described was not given the counter docs/CLOCK.md gives it");
        }
    }
    if choose(false, no, narrow(None)).is_ok() || choose(true, no, narrow(None)).is_ok() {
        return Err("a 32-bit HPET the TSC could not be measured against was handed out");
    }

    // Row 4: each condition of row 2 failing alone, and the HPET for it.
    let refusals: [(Result<u64, HpetReason>, HpetReason); 7] = [
        (with(|f| f.hypervisor = false), HpetReason::NoHypervisor),
        (
            with(|f| f.signature[2] = u32::from_le_bytes(*b"GTCG")),
            HpetReason::OtherHypervisor,
        ),
        (
            with(|f| f.features &= !KVM_FEATURE_CLOCKSOURCE2),
            HpetReason::KvmNoClock,
        ),
        (
            with(|f| f.features &= !KVM_FEATURE_CLOCKSOURCE_STABLE),
            HpetReason::KvmNoStableBit,
        ),
        (with(|f| f.page = None), HpetReason::KvmPageUnread),
        (
            with(|f| f.page = Some(TimeInfo { flags: 0, ..NAZUNA })),
            HpetReason::KvmFlagClear,
        ),
        (
            with(|f| f.page = Some(TimeInfo { mul: 0, ..NAZUNA })),
            HpetReason::KvmRateRefused,
        ),
    ];
    for (word, reason) in refusals {
        if word != Err(reason) {
            return Err("KVM's word was not refused for the one condition that failed");
        }
        if choose(false, word, wide(None)) != Ok(Source::Hpet(reason)) {
            return Err("a machine KVM does not vouch for was not given the HPET with its reason");
        }
    }
    Ok(())
}

/// [`pvclock_hz`] against rates worked out by hand, [`NAZUNA`]'s among them,
/// and each refusal: a zero multiplier, the shifts either side of -31 and
/// 31, and rates either side of 1 `MHz` and of 2^40 Hz.
///
/// # Errors
///
/// The first pair given another rate, or a refused one given any.
/// Verifies: `L.x86_64.167`
fn pvclock_rates() -> Result<(), &'static str> {
    let rates: [(u32, i8, Option<u64>); 15] = [
        (NAZUNA_MUL, NAZUNA_SHIFT, Some(NAZUNA_HZ)),
        // 2^31 with no shift is one nanosecond for two ticks: 2 GHz.
        (0x8000_0000, 0, Some(2_000_000_000)),
        // The same multiplier with a shift of -1 halves the ticks first: 4 GHz.
        (0x8000_0000, -1, Some(4_000_000_000)),
        // And with a shift of 1 doubles them: 1 GHz.
        (0x8000_0000, 1, Some(1_000_000_000)),
        // floor(10^9 * 2^32 / 3) = 1,431,655,765,333,333,333: above 2^40.
        (3, 0, None),
        (0, 0, None),
        // -32 and 32 are no shifts at all.
        (0x8000_0000, -32, None),
        (0x8000_0000, 32, None),
        // -31 and 31 are shifts, and with 2^31 their rates are out of range:
        // 2 * 10^9 * 2^31 and 2 * 10^9 / 2^31.
        (0x8000_0000, -31, None),
        (0x8000_0000, 31, None),
        // And in range with multipliers that suit them: 2^32 - 1 with -31
        // is over 2^40 still, so 31 with 1 is the one that fits: 10^9 * 2^32
        // / 2^31 = 2 * 10^9.
        (1, 31, Some(2_000_000_000)),
        // 2^31 with a shift of 11: 2 * 10^9 / 2048 = 976,562: under 1 MHz.
        (0x8000_0000, 11, None),
        // 2^31 with a shift of 10: 2 * 10^9 / 1024 = 1,953,125.
        (0x8000_0000, 10, Some(1_953_125)),
        // 10^9 with a shift of -8: 2^32 * 2^8 = 2^40 exactly, the last rate
        // believed; one less in the multiplier is above it.
        (1_000_000_000, -8, Some(1 << 40)),
        (999_999_999, -8, None),
    ];
    for (mul, shift, want) in rates {
        if pvclock_hz(mul, shift) != want {
            return Err("a time page's multiplier and shift were given another rate than theirs");
        }
    }
    Ok(())
}

/// [`read_time_page`] against pages that are scripted: one literal 32-byte
/// image decoded, a version odd and then even, a version that changes
/// across the read, a page never filled, and one that never settles, which
/// is given up on after [`TIME_PAGE_TRIES`] looks.
///
/// # Errors
///
/// The first page read otherwise.
/// Verifies: `L.x86_64.169`
fn time_page_read_whole() -> Result<(), &'static str> {
    // struct pvclock_vcpu_time_info, byte for byte: version 2, pad, a
    // timestamp, a system time, tsc_to_system_mul 0x1234_5678 at byte 24,
    // tsc_shift -3 (0xfd) at byte 28, flags 0x01 at byte 29, two pad bytes
    // that are not zero so that they cannot pass for either.
    let image: [u8; 32] = [
        2, 0, 0, 0, 0xaa, 0xaa, 0xaa, 0xaa, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
        0x78, 0x56, 0x34, 0x12, 0xfd, 0x01, 0xee, 0xee,
    ];
    let mut words = [0u32; 8];
    for (word, bytes) in words.iter_mut().zip(image.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*bytes);
    }
    let word_of = |n: usize| words.get(n).copied().unwrap_or(0);
    let want = TimeInfo {
        mul: 0x1234_5678,
        shift: -3,
        flags: 0x01,
    };
    if read_time_page(word_of) != Some(want) {
        return Err(
            "a time page's 32 bytes were not decoded as pvclock_vcpu_time_info lays them out",
        );
    }

    // The version reads 3 (being written) once, then 4.
    let mut looks = 0u32;
    let odd_then_even = read_time_page(|n| {
        if n == 0 {
            looks += 1;
            if looks == 1 { 3 } else { 4 }
        } else {
            word_of(n)
        }
    });
    if odd_then_even != Some(want) || looks != 3 {
        return Err("a time page being written was not read again once it was whole");
    }

    // The version changes across the first read: 2 before, 4 after and from
    // then on, and the multiplier changes with it. The first read's
    // multiplier must not be the answer.
    let mut looks = 0u32;
    let changing = read_time_page(|n| match n {
        0 => {
            looks += 1;
            if looks == 1 { 2 } else { 4 }
        }
        6 if looks == 1 => 0xdead_beef,
        _ => word_of(n),
    });
    if changing != Some(want) || looks != 4 {
        return Err("a time page written during the read was answered from the torn read");
    }

    // Never filled: the version stays zero.
    let mut looks = 0u32;
    let unfilled = read_time_page(|n| {
        if n == 0 {
            looks += 1;
            0
        } else {
            word_of(n)
        }
    });
    if unfilled.is_some() || looks != TIME_PAGE_TRIES {
        return Err("a time page never filled was taken for one, or looked at without end");
    }

    // Never settles: every look at the version is a new even number.
    let mut version = 0u32;
    let mut looks = 0u32;
    let restless = read_time_page(|n| {
        if n == 0 {
            looks += 1;
            version += 2;
            version
        } else {
            word_of(n)
        }
    });
    if restless.is_some() || looks != 2 * TIME_PAGE_TRIES {
        return Err("a time page that never settles was answered, or looked at without end");
    }
    Ok(())
}

/// [`hpet_reason`] answers a reason exactly when the counter is the HPET:
/// the boot's second `clock` line is printed from it, so it is neither left
/// out for an HPET nor printed for a TSC.
///
/// # Errors
///
/// Which way the two disagree.
/// Verifies: `L.x86_64.170`
fn reason_only_for_the_hpet() -> Result<(), &'static str> {
    let hpet = super::HPET.load(Ordering::Relaxed) != 0;
    match (hpet, hpet_reason()) {
        (true, None) => Err("the HPET is the counter and the boot was given no reason to say so"),
        (false, Some(_)) => Err("the counter is the TSC and the boot was told it is the HPET"),
        _ => Ok(()),
    }
}

/// Under KVM with a paravirtual clock, whichever counter was chosen: no
/// time page is registered after bring-up; none is registered after one
/// more read either, whether or not that read answered a page; and where it
/// did, the rate the page states agrees with
/// the counter's where the counter is the TSC and the page says stable --
/// within a third, which a formula wrong by a factor of two cannot pass and
/// a measurement over ten milliseconds of an emulated HPET does not miss.
/// Elsewhere it says that it did not run.
///
/// # Errors
///
/// What was left registered, unread or disagreeing.
/// Verifies: `L.x86_64.168`
fn time_page_given_back() -> Result<(), &'static str> {
    use core::arch::x86_64::__cpuid;
    // The facts, not the counter chosen: every boot under KVM runs this.
    let hypervisor = __cpuid(1).ecx & super::CPUID_HYPERVISOR != 0;
    let kvm = hypervisor && {
        let first = __cpuid(super::HYPERVISOR_LEAF);
        [first.ebx, first.ecx, first.edx] == KVM_SIGNATURE
            && (first.eax == 0 || first.eax >= super::KVM_FEATURE_LEAF)
            && __cpuid(super::KVM_FEATURE_LEAF).eax & KVM_FEATURE_CLOCKSOURCE2 != 0
    };
    if !kvm {
        crate::console::println!(
            "  clock    no KVM paravirtual clock here: its time page's registration and \
             give-back were not run"
        );
        return Ok(());
    }
    if kvm_time_page_registered() != 0 {
        return Err("bring-up left a time page registered with KVM");
    }
    let read = kvm_time_page();
    if kvm_time_page_registered() != 0 {
        return Err("a time page was left registered with KVM after it was read");
    }
    // A page KVM never filled vouches for nothing and is given back like any
    // other: that machine is on the HPET with its reason, and is not failed
    // here for it.
    let Some(page) = read else {
        crate::console::println!(
            "  clock    KVM's time page could not be read whole; it was given back all the same"
        );
        return Ok(());
    };
    let stable = page.flags & PVCLOCK_TSC_STABLE != 0;
    let stated = pvclock_hz(page.mul, page.shift);
    if stable && counter_is_tsc() {
        let Some(stated) = stated else {
            return Err("KVM's time page says stable and states a rate no TSC has");
        };
        let counted = counter_hz();
        if stated > counted + counted / 3 || stated < counted - counted / 3 {
            return Err("the rate KVM's time page states is not the TSC's rate the counter has");
        }
    }
    crate::console::println!(
        "  clock    KVM's time page read and given back: multiplier {:#x}, shift {}, {}, {} Hz \
         stated against the counter's {}",
        page.mul,
        page.shift,
        if stable { "stable" } else { "not stable" },
        stated.unwrap_or(0),
        counter_hz(),
    );
    Ok(())
}

/// This machine against the counter it was given: where KVM vouches in full
/// and the processor reports no invariant TSC, the counter is the TSC; and
/// where the processor reports one, the HPET is the counter only for a
/// measurement that did not come off.
///
/// # Errors
///
/// The HPET in use on a machine that had a TSC to trust.
/// Verifies: `L.x86_64.171`
fn counter_matches_machine() -> Result<(), &'static str> {
    let reason = hpet_reason();
    if invariant_tsc() {
        return match reason {
            None | Some(HpetReason::MeasurementFailed) => Ok(()),
            Some(_) => Err(
                "the HPET is the counter although the TSC is invariant and was not found \
                 unmeasurable",
            ),
        };
    }
    if kvm_word(&kvm_facts()).is_ok() && reason.is_some() {
        return Err("the HPET is the counter although KVM vouches for the TSC");
    }
    Ok(())
}
