//! Tests for the RMRR identity rule, on the N150's RMRR and memory map and
//! on regions firmware should never give.

use super::{Identity, MOST_PAGES, PAGE, Refusal, disjoint, held, identity};

extern crate std;
use std::vec::Vec;

/// The N150 box's RMRR, from its DMAR (`box-2026-10-17/DMAR.bin`): the
/// iGPU's stolen memory, GSM and DSM.
const BOX_RMRR: (u64, u64) = (0x7C00_0000, 0x803F_FFFF);

/// The box's memory map as Linux read it from firmware -- Linux's e820, not
/// Ferrix's own loader map, which the box row in BACKLOG records --
/// (`box-2026-10-16/dmesg.txt`, the `BIOS-e820` lines): every region it
/// calls RAM -- System RAM, ACPI data and ACPI NVS -- as `(base, len)`.
/// The RMRR lies in 0x7B000000-0x803FFFFF, "device reserved".
const BOX_RAM: [(u64, u64); 9] = [
    (0x0, 0x9_E000),
    (0x9_F000, 0x1000),
    (0x10_0000, 0x3E55_C000 - 0x10_0000),
    (0x3E55_D000, 0x3E59_3000 - 0x3E55_D000),
    (0x3E59_4000, 0x7023_0000 - 0x3E59_4000),
    (0x7333_0000, 0x7341_8000 - 0x7333_0000),
    (0x7341_8000, 0x734D_8000 - 0x7341_8000),
    (0x73EF_F000, 0x1000),
    (0x1_0000_0000, 0x4_7FC0_0000 - 0x1_0000_0000),
];

/// Verifies: `L.iommu.73`
#[test]
fn the_boxs_rmrr_is_honoured_page_for_page() {
    let (base, limit) = BOX_RMRR;
    let region = identity(base, limit, BOX_RAM).unwrap();
    assert_eq!(
        region,
        Identity {
            base,
            pages: 17_408
        },
        "68 MiB of pages"
    );
    assert_eq!(region.end(), limit + 1);
    let pages: Vec<u64> = region.page_addresses().collect();
    assert_eq!(pages.len(), 17_408);
    assert_eq!(pages.first(), Some(&base));
    assert_eq!(pages.last(), Some(&(limit + 1 - PAGE)));
    assert!(pages.windows(2).all(|pair| pair[1] == pair[0] + PAGE));
    // Exactly the pages: the byte before and the byte after are not held.
    assert!(held(&[region], base) && held(&[region], limit));
    assert!(!held(&[region], base - 1) && !held(&[region], limit + 1));
}

/// Verifies: `L.iommu.74`
#[test]
fn an_rmrr_not_whole_pages_is_refused() {
    let (base, limit) = BOX_RMRR;
    assert_eq!(
        identity(base + 0x800, limit, BOX_RAM),
        Err(Refusal::Unaligned),
        "base inside a page"
    );
    assert_eq!(
        identity(base, limit - 0x800, BOX_RAM),
        Err(Refusal::Unaligned),
        "limit not a page's last byte"
    );
    assert_eq!(
        identity(base, limit + 1, BOX_RAM),
        Err(Refusal::Unaligned),
        "limit one past a page's last byte"
    );
    assert_eq!(identity(base, base - 1, BOX_RAM), Err(Refusal::Inverted));
}

/// Verifies: `L.iommu.74`
#[test]
fn an_rmrr_out_of_reach_or_too_large_is_refused() {
    let reach = 1_u64 << 39;
    assert_eq!(
        identity(reach - 2 * PAGE, reach - 1, []),
        Ok(Identity {
            base: reach - 2 * PAGE,
            pages: 2
        }),
        "up to the last page a domain reaches"
    );
    assert_eq!(
        identity(reach - PAGE, reach + PAGE - 1, []),
        Err(Refusal::OutOfReach)
    );
    assert_eq!(identity(0, u64::MAX, []), Err(Refusal::OutOfReach));
    assert_eq!(
        identity(0x1_0000_0000, 0x1_0000_0000 + MOST_PAGES * PAGE - 1, []),
        Ok(Identity {
            base: 0x1_0000_0000,
            pages: MOST_PAGES
        })
    );
    assert_eq!(
        identity(
            0x1_0000_0000,
            0x1_0000_0000 + (MOST_PAGES + 1) * PAGE - 1,
            []
        ),
        Err(Refusal::TooLarge)
    );
}

/// Verifies: `L.iommu.74`
/// Verifies: `L.iommu.75`
#[test]
fn an_rmrr_over_ram_is_refused() {
    let (base, limit) = BOX_RMRR;
    // A RAM region ending in the RMRR's first page, and one starting in its
    // last: each refuses it, naming the region.
    let first = [(base - PAGE, 2 * PAGE)];
    assert_eq!(
        identity(base, limit, first),
        Err(Refusal::OverRam {
            base: base - PAGE,
            len: 2 * PAGE
        })
    );
    let last = [(limit + 1 - PAGE, 2 * PAGE)];
    assert_eq!(
        identity(base, limit, last),
        Err(Refusal::OverRam {
            base: limit + 1 - PAGE,
            len: 2 * PAGE
        })
    );
    let around = [(0x1000_0000, 0x8000_0000)];
    assert!(matches!(
        identity(base, limit, around),
        Err(Refusal::OverRam { .. })
    ));
    // Touching, not overlapping, is allowed; so is an empty region.
    let touching = [(base - PAGE, PAGE), (limit + 1, PAGE), (base, 0)];
    assert!(identity(base, limit, touching).is_ok());
    // The box's own map, with its RAM moved over the RMRR's middle.
    let mut moved = BOX_RAM;
    moved[7] = (0x7E00_0000, PAGE);
    assert_eq!(
        identity(base, limit, moved),
        Err(Refusal::OverRam {
            base: 0x7E00_0000,
            len: PAGE
        })
    );
}

/// Verifies: `L.iommu.74`
#[test]
fn two_rmrrs_for_one_function_may_not_share_a_page() {
    let a = Identity {
        base: 0x7C00_0000,
        pages: 16,
    };
    let b = Identity {
        base: 0x7C00_F000,
        pages: 4,
    };
    let c = Identity {
        base: 0x7C01_0000,
        pages: 4,
    };
    assert_eq!(
        disjoint(&[a, b]),
        Err(Refusal::Overlapping { base: a.base }),
        "b's first page is a's last"
    );
    assert_eq!(disjoint(&[a, c]), Ok(()), "c starts after a");
    assert_eq!(
        disjoint(&[c, a, b]),
        Err(Refusal::Overlapping { base: c.base }),
        "b overlaps c, the first earlier one, as well as a"
    );
    assert!(held(&[a, c], 0x7C01_0000) && !held(&[a, c], 0x7C01_4000));
}

/// Every refusal says why on the unit's line.
#[test]
fn every_refusal_says_why() {
    use std::string::ToString;
    let said = [
        Refusal::Inverted,
        Refusal::Unaligned,
        Refusal::OutOfReach,
        Refusal::TooLarge,
        Refusal::OverRam {
            base: 0x1000,
            len: 0x2000,
        },
        Refusal::Overlapping { base: 0x5000 },
        Refusal::AllocatorFrame { page: 0x7C00_0000 },
    ]
    .map(|refusal| refusal.to_string());
    assert!(said[4].contains("0x1000-0x2fff"), "{}", said[4]);
    assert!(said[6].contains("0x7c000000"), "{}", said[6]);
    assert!(said.iter().all(|line| !line.is_empty()));
}
