//! The memory type an x86-64 processor's MTRRs give a physical range, from
//! the registers' values: what the kernel says beside the firmware
//! framebuffer it lends (`docs/DISPLAY.md` §8). It decides nothing: a page
//! the PAT maps write-combining is write-combining under every MTRR type.
//!
//! The encodings are Linux's `uapi/asm/mtrr.h` (`MTRR_TYPE_UNCACHABLE` 0,
//! `WRCOMB` 1, `WRTHROUGH` 4, `WRPROT` 5, `WRBACK` 6); the precedence of
//! overlapping variable ranges is the SDM's (Vol. 3A, "MTRR precedences"):
//! one uncacheable range makes the page uncacheable, write-through and
//! write-back make it write-through, ranges of one type give that type, and
//! any other pair is undefined.

/// Uncacheable.
pub const UNCACHEABLE: u8 = 0x00;
/// Write-through.
pub const WRITE_THROUGH: u8 = 0x04;
/// Write-back.
pub const WRITE_BACK: u8 = 0x06;

/// `MTRRdefType.E`: the MTRRs are on.
const ENABLE: u64 = 1 << 11;
/// `MTRRphysMask.V`: the variable range is in use.
const VALID: u64 = 1 << 11;
/// A page.
const PAGE: u64 = 4096;

/// The type `default` (`IA32_MTRR_DEF_TYPE`) and the variable ranges
/// `ranges` (each `IA32_MTRR_PHYSBASEn`, `IA32_MTRR_PHYSMASKn`) give every
/// page of `phys..phys + len`, when it is one type for all of them.
///
/// `None` when the range is empty, wraps, starts below 1 MiB (where the
/// fixed ranges, not given here, decide), holds pages of different types,
/// or overlapping ranges over a page combine to a type the SDM leaves
/// undefined. With the MTRRs off, everything is uncacheable.
#[must_use]
pub fn range_type(default: u64, ranges: &[(u64, u64)], phys: u64, len: u64) -> Option<u8> {
    let end = phys.checked_add(len)?;
    if len == 0 || phys < 0x10_0000 {
        return None;
    }
    if default & ENABLE == 0 {
        return Some(UNCACHEABLE);
    }
    let mut found: Option<u8> = None;
    let mut page = phys & !(PAGE - 1);
    while page < end {
        let kind = page_type(default, ranges, page)?;
        if found.is_some_and(|seen| seen != kind) {
            return None;
        }
        found = Some(kind);
        page = page.checked_add(PAGE)?;
    }
    found
}

/// The type of the page at `page`, or `None` where the SDM leaves it
/// undefined.
fn page_type(default: u64, ranges: &[(u64, u64)], page: u64) -> Option<u8> {
    let mut here: Option<u8> = None;
    for &(base, mask) in ranges {
        let bits = mask & !(PAGE - 1);
        if mask & VALID == 0 || page & bits != base & bits {
            continue;
        }
        let kind = (base & 0xFF) as u8;
        here = Some(match here {
            None => kind,
            Some(seen) if seen == kind => kind,
            Some(seen) if seen == UNCACHEABLE || kind == UNCACHEABLE => UNCACHEABLE,
            Some(seen)
                if (seen == WRITE_THROUGH && kind == WRITE_BACK)
                    || (seen == WRITE_BACK && kind == WRITE_THROUGH) =>
            {
                WRITE_THROUGH
            }
            Some(_) => return None,
        });
    }
    Some(here.unwrap_or((default & 0xFF) as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MTRRs on, write-back by default: OVMF's choice.
    const ON_WB: u64 = ENABLE | WRITE_BACK as u64;
    /// A 36-bit mask's valid range of `size` bytes.
    const fn mask(size: u64) -> u64 {
        (!(size - 1) & 0xF_FFFF_F000) | VALID
    }
    const FB: u64 = 0xC000_0000;

    #[test]
    fn the_default_where_no_range_covers() {
        assert_eq!(range_type(ON_WB, &[], FB, 0x30_0000), Some(WRITE_BACK));
    }

    #[test]
    fn an_uncacheable_hole_over_the_framebuffer() {
        // The 32-bit PCI hole, 0x8000_0000..0x1_0000_0000, uncacheable.
        let hole = [(0x8000_0000 | u64::from(UNCACHEABLE), mask(0x8000_0000))];
        assert_eq!(range_type(ON_WB, &hole, FB, 0x30_0000), Some(UNCACHEABLE));
    }

    #[test]
    fn overlaps_follow_the_precedence() {
        let wb = (FB | u64::from(WRITE_BACK), mask(0x40_0000));
        let wt = (FB | u64::from(WRITE_THROUGH), mask(0x40_0000));
        let uc = (FB | u64::from(UNCACHEABLE), mask(0x40_0000));
        let wc = (FB | 0x01, mask(0x40_0000));
        assert_eq!(range_type(ON_WB, &[wb, wt], FB, 4096), Some(WRITE_THROUGH));
        assert_eq!(range_type(ON_WB, &[wc, uc], FB, 4096), Some(UNCACHEABLE));
        assert_eq!(range_type(ON_WB, &[wc, wb], FB, 4096), None);
    }

    #[test]
    fn an_unused_range_and_mtrrs_off() {
        let unused = (FB | u64::from(UNCACHEABLE), mask(0x40_0000) & !VALID);
        assert_eq!(range_type(ON_WB, &[unused], FB, 4096), Some(WRITE_BACK));
        assert_eq!(
            range_type(WRITE_BACK as u64, &[], FB, 4096),
            Some(UNCACHEABLE)
        );
    }

    #[test]
    fn mixed_low_empty_and_wrapping_ranges_have_no_type() {
        // The first 2 MiB uncacheable, the rest by default.
        let half = [(FB | u64::from(UNCACHEABLE), mask(0x20_0000))];
        assert_eq!(range_type(ON_WB, &half, FB, 0x40_0000), None);
        assert_eq!(range_type(ON_WB, &[], 0xA_0000, 4096), None);
        assert_eq!(range_type(ON_WB, &[], FB, 0), None);
        assert_eq!(range_type(ON_WB, &[], u64::MAX - 4095, 8192), None);
    }
}
