//! `devmgr`'s admission rule (`docs/DMA-GATE.md`): every combination of
//! `device_isolation`'s bits, with and without the opt-out, on AArch64 and
//! elsewhere, and every line.

extern crate std;

use std::format;

use crate::admission::{self, Admit, DMA_TRANSLATED, INTERRUPTS_ISOLATED, NO_DMA, OptOut};

/// Verifies: L.device.59
#[test]
fn every_bus_master_needs_both_bits_and_aarch64_translation_alone() {
    for bits in 0..8_u64 {
        for aarch64 in [false, true] {
            for opt_out in [false, true] {
                let decided = admission::admit(Some(bits), aarch64, opt_out, 0x0800);
                let isolated = if aarch64 {
                    bits & DMA_TRANSLATED != 0
                } else {
                    bits & (DMA_TRANSLATED | INTERRUPTS_ISOLATED)
                        == DMA_TRANSLATED | INTERRUPTS_ISOLATED
                };
                let no_dma = bits & NO_DMA != 0;
                match decided {
                    Admit::Start(admitted) => {
                        assert!(
                            isolated || no_dma || opt_out,
                            "{bits:#x} {aarch64} {opt_out}"
                        );
                        assert_eq!(admitted.unisolated(), !isolated && !no_dma);
                    }
                    Admit::Refused { bits: seen, .. } => {
                        assert!(!isolated && !no_dma && !opt_out);
                        assert_eq!(seen, bits);
                    }
                    Admit::Unread => panic!("read"),
                }
            }
        }
    }
    assert_eq!(
        admission::admit(None, false, true, 0x0800),
        Admit::Unread,
        "unread refuses, opt-out or not"
    );
}

/// Verifies: L.device.59
#[test]
fn every_line() {
    let line = |decided: Admit| decided.line("net", 0x0800).map(|line| format!("{line}"));
    assert_eq!(
        line(admission::admit(Some(0x3), false, false, 0x0800)),
        None
    );
    assert_eq!(
        line(admission::admit(Some(NO_DMA), false, false, 0x0800)),
        None
    );
    assert_eq!(
        line(admission::admit(Some(0x2), false, false, 0x0800)).unwrap(),
        "devmgr   net 08:00.0 not started: its DMA is not translated (device_isolation 0x2)"
    );
    assert_eq!(
        line(admission::admit(Some(0x1), false, false, 0x0800)).unwrap(),
        "devmgr   net 08:00.0 not started: its interrupts are not isolated (device_isolation 0x1)"
    );
    assert_eq!(
        line(admission::admit(Some(0x0), false, true, 0x0800)).unwrap(),
        "devmgr   UNISOLATED: net 08:00.0 started under ferrix.unisolated: its DMA is not \
         translated or its interrupts are not isolated, and its driver is trusted with all of \
         memory"
    );
    assert_eq!(
        line(admission::admit(None, false, false, 0x0800)).unwrap(),
        "devmgr   net 08:00.0 not started: its device_isolation could not be read"
    );
}

/// The bits are the ABI's.
#[test]
fn the_bits_are_the_abis() {
    use ferrix_native_abi::types;
    assert_eq!(DMA_TRANSLATED, types::DEVICE_ISOLATION_DMA_TRANSLATED);
    assert_eq!(INTERRUPTS_ISOLATED, types::DEVICE_ISOLATION_INTERRUPTS);
    assert_eq!(NO_DMA, types::DEVICE_ISOLATION_NO_DMA);
}

/// An admission starts the one device it was made for, at its location,
/// and no other (ledger 4153 (1)).
///
/// Verifies: L.device.59
#[test]
fn an_admission_admits_its_own_device_alone() {
    for (bits, opt_out) in [(0x3, false), (NO_DMA, false), (0x0, true)] {
        let Admit::Start(admitted) = admission::admit(Some(bits), false, opt_out, 0x0800) else {
            panic!("{bits:#x} {opt_out} was not admitted");
        };
        assert!(admitted.admits(0x0800));
        assert!(!admitted.admits(0x0808));
        assert!(!admitted.admits(0));
    }
}

/// The opt-out is the bare word alone: a value, beside the bare word or
/// not, gives none (ledger 3906 (2), 4153 (2)).
///
/// Verifies: L.device.58
#[test]
fn the_opt_out_is_the_bare_word_alone() {
    assert_eq!(admission::opt_out(false, false), OptOut::Absent);
    assert_eq!(admission::opt_out(true, false), OptOut::On);
    assert_eq!(admission::opt_out(false, true), OptOut::Ignored);
    assert_eq!(admission::opt_out(true, true), OptOut::Ignored);
}
