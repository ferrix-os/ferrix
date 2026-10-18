//! Host tests for M2, "a GPU command executes" (`docs/INTEL-GPU.md` §7):
//! the PPGTT, the GGTT window's binds, the PAT and MOCS read back, and
//! (later slices) the execlist engine model.

extern crate std;

use std::collections::BTreeMap;
use std::vec::Vec;

use crate::gtt::{self, CacheLevel};
use crate::io::{GgttPtes, Mmio};
use crate::ppgtt::{Ppgtt, PpgttError, PtMemory};

/// Page-table memory: pages by device address, from 0x1000 up, at most
/// `limit` live at once.
#[derive(Default)]
struct PtModel {
    pages: BTreeMap<u64, Vec<u64>>,
    next: u64,
    limit: Option<usize>,
    freed: Vec<u64>,
}

impl PtMemory for PtModel {
    fn alloc_pt(&mut self) -> Option<u64> {
        if self.limit.is_some_and(|limit| self.pages.len() >= limit) {
            return None;
        }
        self.next += 0x1000;
        // Not zeroed: a table must be filled by whoever takes it.
        let _ = self.pages.insert(self.next, std::vec![0xdead_beef; 512]);
        Some(self.next)
    }

    fn free_pt(&mut self, table: u64) {
        assert!(
            self.pages.remove(&table).is_some(),
            "freed twice: {table:#x}"
        );
        self.freed.push(table);
    }

    fn read_entry(&mut self, table: u64, index: usize) -> u64 {
        self.pages[&table][index]
    }

    fn write_entry(&mut self, table: u64, index: usize, value: u64) {
        self.pages.get_mut(&table).expect("a live table")[index] = value;
    }
}

#[test]
fn ppgtt_scratch_chain() {
    let mut mem = PtModel::default();
    let vm = Ppgtt::create(&mut mem).expect("pages");
    // The scratch page, three scratch tables and the PML4.
    assert_eq!(mem.pages.len(), 5);
    assert!(mem.pages[&vm.scratch[0]].iter().all(|&e| e == 0));
    // The scratch PTE is uncached: PAT index 3, bits 3 and 4.
    assert_eq!(
        vm.scratch_encode[0],
        vm.scratch[0] | gtt::GEN8_PAGE_PRESENT | gtt::GEN8_PAGE_RW | (1 << 3) | (1 << 4)
    );
    for lvl in 1..4 {
        assert!(
            mem.pages[&vm.scratch[lvl]]
                .iter()
                .all(|&e| e == vm.scratch_encode[lvl - 1]),
            "level {lvl}"
        );
        assert_eq!(
            vm.scratch_encode[lvl],
            gtt::gen8_pde_encode(vm.scratch[lvl], CacheLevel::None)
        );
    }
    assert!(
        mem.pages[&vm.pml4]
            .iter()
            .all(|&e| e == vm.scratch_encode[3])
    );
    // Nothing is mapped: every address reads the scratch page.
    assert_eq!(vm.translate(&mut mem, 0), None);
    assert_eq!(vm.translate(&mut mem, 0xffff_ffff_f000), None);
}

#[test]
fn ppgtt_range_helpers_match_i915() {
    use crate::ppgtt::{gen8_pd_range, gen8_pt_count};
    // Pages 510..514 at level 1 (the leaf tables' directory): two entries,
    // from index 0.
    assert_eq!(gen8_pd_range(510, 514, 1), (0, 2));
    // The same range seen from the leaf level: entries 510.. to the end.
    assert_eq!(gen8_pd_range(510, 514, 0), (510, 2));
    assert_eq!(gen8_pt_count(510, 514), 2);
    assert_eq!(gen8_pt_count(512, 514), 2);
    // The whole 48-bit space at the top: all 512 PML4 entries.
    assert_eq!(gen8_pd_range(0, 1 << 36, 3), (0, 512));
}

#[test]
fn ppgtt_maps_across_every_level_boundary() {
    let mut mem = PtModel::default();
    let vm = Ppgtt::create(&mut mem).expect("pages");
    // Four pages straddling a 512 GiB boundary, so two PML4 entries, two
    // PDPs, two PDs and two leaf tables are needed.
    let start = (512_u64 << 30) - 2 * 4096;
    vm.alloc_va_range(&mut mem, start, 4 * 4096)
        .expect("tables");
    assert_eq!(mem.pages.len(), 5 + 6);
    let pages = [0x10_0000_u64, 0x20_0000, 0x30_0000, 0x40_0000];
    assert_eq!(vm.insert_entries(&mut mem, pages, start, 0), Ok(4));
    for (n, &page) in pages.iter().enumerate() {
        let at = start + n as u64 * 4096;
        assert_eq!(vm.translate(&mut mem, at + 0x24), Some(page + 0x24));
    }
    // A neighbour outside the range is still scratch.
    assert_eq!(vm.translate(&mut mem, start - 4096), None);
    assert_eq!(vm.translate(&mut mem, start + 4 * 4096), None);
    // Allocating again takes nothing new.
    vm.alloc_va_range(&mut mem, start, 4 * 4096)
        .expect("tables");
    assert_eq!(mem.pages.len(), 11);
    // The PAT index lands in bits 3, 4 and 7, as gen12_pte_encode puts it.
    vm.insert_entry(&mut mem, 0x50_0000, start, 2)
        .expect("allocated");
    let leaf_entry = {
        let pml4e = mem.read_entry(vm.pml4, crate::ppgtt::gen8_pd_index(start >> 12, 3));
        let pdpe = mem.read_entry(
            pml4e & crate::ppgtt::PTE_ADDR_MASK,
            crate::ppgtt::gen8_pd_index(start >> 12, 2),
        );
        let pde = mem.read_entry(
            pdpe & crate::ppgtt::PTE_ADDR_MASK,
            crate::ppgtt::gen8_pd_index(start >> 12, 1),
        );
        mem.read_entry(
            pde & crate::ppgtt::PTE_ADDR_MASK,
            crate::ppgtt::gen8_pd_index(start >> 12, 0),
        )
    };
    assert_eq!(leaf_entry, 0x50_0000 | 0b11 | gtt::GEN12_PPGTT_PTE_PAT1);

    // Clearing all of it gives every table back.
    vm.clear_range(&mut mem, start, 4 * 4096).expect("range");
    assert_eq!(mem.pages.len(), 5);
    assert!(
        mem.pages[&vm.pml4]
            .iter()
            .all(|&e| e == vm.scratch_encode[3])
    );
    assert_eq!(vm.translate(&mut mem, start), None);
    vm.destroy(&mut mem);
    assert!(mem.pages.is_empty());
}

#[test]
fn ppgtt_partial_clear_keeps_the_rest() {
    let mut mem = PtModel::default();
    let vm = Ppgtt::create(&mut mem).expect("pages");
    let base = 0x1_0000_0000_u64;
    vm.alloc_va_range(&mut mem, base, 8 * 4096).expect("tables");
    let pages: Vec<u64> = (0..8).map(|n| 0x100_0000 + n * 4096).collect();
    let _ = vm.insert_entries(&mut mem, pages.iter().copied(), base, 0);
    vm.clear_range(&mut mem, base, 4 * 4096).expect("range");
    // Same leaf table: nothing is freed.
    assert_eq!(mem.pages.len(), 5 + 3);
    assert_eq!(vm.translate(&mut mem, base), None);
    assert_eq!(vm.translate(&mut mem, base + 4 * 4096), Some(pages[4]));
    // Clearing a range that maps nothing is harmless.
    vm.clear_range(&mut mem, 0x7000_0000_0000, 4096)
        .expect("range");
    assert_eq!(vm.translate(&mut mem, base + 7 * 4096), Some(pages[7]));
}

#[test]
fn ppgtt_refusals() {
    let mut mem = PtModel::default();
    let vm = Ppgtt::create(&mut mem).expect("pages");
    assert_eq!(
        vm.insert_entry(&mut mem, 0x1000, 0x2000, 0),
        Err(PpgttError::NotAllocated)
    );
    assert_eq!(
        vm.alloc_va_range(&mut mem, 0x1001, 4096),
        Err(PpgttError::Range)
    );
    assert_eq!(vm.alloc_va_range(&mut mem, 0, 0), Err(PpgttError::Range));
    assert_eq!(
        vm.alloc_va_range(&mut mem, (1 << 48) - 4096, 8192),
        Err(PpgttError::Range)
    );
    vm.alloc_va_range(&mut mem, 0, 4096).expect("tables");
    // A PAT index past the table, and an address with bits below the page.
    assert_eq!(
        vm.insert_entry(&mut mem, 0x1000, 0, gtt::MAX_PAT_INDEX + 1),
        Err(PpgttError::Range)
    );
    assert_eq!(
        vm.insert_entry(&mut mem, 0x1004, 0, 0),
        Err(PpgttError::Range)
    );
    // Out of pages while creating: what was taken is given back.
    let mut short = PtModel {
        limit: Some(3),
        ..PtModel::default()
    };
    assert_eq!(Ppgtt::create(&mut short), Err(PpgttError::NoMemory));
    assert!(short.pages.is_empty());
    assert_eq!(short.freed.len(), 3);
    // Out of pages while allocating tables.
    let mut tight = PtModel {
        limit: Some(6),
        ..PtModel::default()
    };
    let vm = Ppgtt::create(&mut tight).expect("pages");
    assert_eq!(
        vm.alloc_va_range(&mut tight, 0, 4096),
        Err(PpgttError::NoMemory)
    );
}

/// The GGTT's entries, and one that drops the high half of every write as
/// a mapping of the wrong kind would.
#[derive(Default)]
struct Ptes {
    entries: BTreeMap<u64, u64>,
    drop_high: bool,
}

impl GgttPtes for Ptes {
    fn read_pte(&mut self, index: u64) -> u64 {
        self.entries.get(&index).copied().unwrap_or(0)
    }

    fn write_pte(&mut self, index: u64, pte: u64) {
        let pte = if self.drop_high {
            pte & 0xffff_ffff
        } else {
            pte
        };
        let _ = self.entries.insert(index, pte);
    }
}

#[test]
fn ggtt_window_binds_only_its_own_ranges() {
    use crate::ggtt_space::{BindError, GgttSpace, Range};
    let mut space = GgttSpace::above_firmware(Some(0x1d4), 4 << 30);
    let ring = space.alloc(4 * 4096, 4096).expect("ring");
    let mut ptes = Ptes::default();
    let pages = [0x1_2345_6000_u64, 0x1_2345_7000, 0x8000, 0x9000];
    assert_eq!(space.bind(&mut ptes, ring, &pages), Ok(4));
    assert_eq!(ptes.entries[&0x1d5], 0x1_2345_6001);
    assert_eq!(ptes.entries[&0x1d8], 0x9001);
    // Nothing at or below the firmware's last entry was written.
    assert!(ptes.entries.keys().all(|&index| index > 0x1d4));
    // A range it did not hand out, or too many pages, writes nothing.
    let firmware = Range {
        start: 0,
        len: 4096,
    };
    assert_eq!(
        space.bind(&mut ptes, firmware, &pages[..1]),
        Err(BindError::NotOurs)
    );
    let small = space.alloc(4096, 4096).expect("page");
    assert_eq!(
        space.bind(&mut ptes, small, &pages),
        Err(BindError::NotOurs)
    );
    assert_eq!(ptes.entries.len(), 4);
    // The readback catches an entry that did not stick whole.
    let mut lossy = Ptes {
        drop_high: true,
        ..Ptes::default()
    };
    assert_eq!(
        space.bind(&mut lossy, ring, &pages),
        Err(BindError::Readback {
            index: 0x1d5,
            read: 0x2345_6001
        })
    );
    space.clear(&mut ptes, ring, 0x7000).expect("ours");
    assert!((0x1d5..0x1d9).all(|i| ptes.entries[&i] == 0x7000));
}

/// Registers that keep what is written, except at `stuck`, which keeps 0.
#[derive(Default)]
struct Sticky {
    values: BTreeMap<u32, u32>,
    stuck: Option<u32>,
}

impl Mmio for Sticky {
    fn read32(&mut self, offset: u32) -> u32 {
        self.values.get(&offset).copied().unwrap_or(0)
    }

    fn write32(&mut self, offset: u32, value: u32) {
        if self.stuck != Some(offset) {
            let _ = self.values.insert(offset, value);
        }
    }
}

#[test]
fn pat_and_mocs_read_back() {
    use crate::{gt, mocs};
    let mut regs = Sticky::default();
    gt::setup_private_pat(&mut regs);
    assert_eq!(gt::verify_private_pat(&mut regs).count(), 0);
    for (reg, value) in mocs::intel_mocs_init_writes() {
        regs.write32(reg.offset(), value);
    }
    assert_eq!(mocs::intel_mocs_verify(&mut regs).count(), 0);
    // One register that does not take its value is named, with both values.
    let mut regs = Sticky {
        stuck: Some(0x4804),
        ..Sticky::default()
    };
    gt::setup_private_pat(&mut regs);
    let lost: Vec<_> = gt::verify_private_pat(&mut regs).collect();
    assert_eq!(lost, [(gtt::gen12_pat_index(1), gtt::GEN8_PPAT_WC, 0)]);
    let mut regs = Sticky {
        stuck: Some(mocs::gen9_lncfcmocs(0).offset()),
        ..Sticky::default()
    };
    for (reg, value) in mocs::intel_mocs_init_writes() {
        regs.write32(reg.offset(), value);
    }
    let lost: Vec<_> = mocs::intel_mocs_verify(&mut regs).collect();
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].0, mocs::gen9_lncfcmocs(0));
}
