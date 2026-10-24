//! AMD-Vi's encodings and decisions, against the specification's layouts,
//! QEMU 10.2's checks and nazuna's EFR (`docs/AMD-VI.md` §5.2).

extern crate std;

use std::collections::BTreeMap;
use std::vec::Vec;

use super::command::{self, Completion};
use super::event::{self, Event};
use super::table;
use super::unit;
use super::{AmdV1, amd_level};
use crate::{Encoding, Level, MapFlags, Mapper, PhysAddr, PhysMem, VirtAddr};

/// Frames as a map, for the mapper.
#[derive(Default)]
struct Frames {
    words: BTreeMap<u64, u64>,
    next: u64,
}

// SAFETY: not physical memory at all: a map, whose every table address
// comes from `allocate_table` below, unique, and reading as zero until
// written.
unsafe impl PhysMem for Frames {
    fn read(&self, at: PhysAddr) -> u64 {
        self.words.get(&at.0).copied().unwrap_or(0)
    }
    fn write(&mut self, at: PhysAddr, value: u64) {
        let _ = self.words.insert(at.0, value);
    }
    fn allocate_table(&mut self) -> Option<PhysAddr> {
        self.next += 0x1000;
        Some(PhysAddr(0x10_0000 + self.next))
    }
}

/// Every table entry the mapper wrote under `root`, by the level of the
/// table it is in.
fn tables_walked(frames: &Frames, root: u64, iova: u64) -> Vec<(Level, u64)> {
    let mut entries = Vec::new();
    let mut table = root;
    let mut level = AmdV1::ROOT_LEVEL;
    loop {
        let entry = frames.read(PhysAddr(table + level.index(VirtAddr(iova)) as u64 * 8));
        entries.push((level, entry));
        if level == Level::PAGE {
            return entries;
        }
        table = AmdV1::address(entry).0;
        level = level.next().unwrap();
    }
}

/// Verifies: L.iommu.85
#[test]
fn a_domain_walks_three_levels_and_each_table_entry_names_the_next_level() {
    let mut frames = Frames::default();
    let root = frames.allocate_table().unwrap();
    let mapper = Mapper::<AmdV1>::new(root);
    for iova in [0, (1 << 39) - 0x1000, 0x1234_5000] {
        mapper
            .map_range(
                &mut frames,
                VirtAddr(iova),
                PhysAddr(iova),
                0x1000,
                MapFlags::DMA,
            )
            .unwrap();
        assert_eq!(
            mapper.translate(&frames, VirtAddr(iova)),
            Some(PhysAddr(iova))
        );
        let walked = tables_walked(&frames, root.0, iova);
        assert_eq!(walked.len(), 3, "three levels, from GIGABYTE");
        for (level, entry) in &walked[..2] {
            assert_eq!(entry & 1, 1, "present");
            assert_eq!(
                (entry >> 9) & 7,
                amd_level(level.next().unwrap()),
                "NextLevel is the level of the table pointed to"
            );
            assert_eq!(entry >> 61 & 3, 3, "IR and IW on every table entry");
        }
        let (_, leaf) = walked[2];
        assert_eq!((leaf >> 9) & 7, 0, "a 4 KiB leaf");
        let flags = AmdV1::leaf_flags(leaf);
        assert!(flags.read && flags.write, "a DMA pin reads and writes");
    }
    assert_eq!(amd_level(Level::GIGABYTE), 3, "Mode 3's root");
    assert_eq!(amd_level(Level::PAGE), 1);
    assert!(!AmdV1::is_canonical(VirtAddr(1 << 39)));
    assert!(!AmdV1::supports_block(Level::MEGABYTE));
    assert!(!AmdV1::supports_block(Level::GIGABYTE));
}

/// A read-only pin is a leaf without `IW`.
///
/// Verifies: L.iommu.85
#[test]
fn leaf_flags_round_trip() {
    for (read, write) in [(true, false), (true, true), (false, true)] {
        let flags = MapFlags {
            read,
            write,
            ..MapFlags::DMA
        };
        let leaf = AmdV1::leaf_descriptor(PhysAddr(0x5000), Level::PAGE, flags);
        let back = AmdV1::leaf_flags(leaf);
        assert_eq!((back.read, back.write), (read, write));
        assert_eq!(AmdV1::address(leaf), PhysAddr(0x5000));
    }
}

/// The blocked entry QEMU enforces, the attached one, and the halves kept
/// apart.
///
/// Verifies: L.iommu.84, L.iommu.85
#[test]
fn device_table_entries() {
    let blocked = table::blocked(table::blocked_translating(0x7_7000));
    assert_eq!(blocked[0], 0x7_7000 | 3 << 9 | 0b11, "V, TV, Mode 3");
    assert_eq!(blocked[0] >> 61 & 3, 0, "IR and IW clear");
    assert_eq!(blocked[1], 0, "domain 0");
    assert_eq!(blocked[2], 1, "IV set, IntCtl abort, every pass bit clear");
    assert_eq!(blocked[3], 0);
    assert!(!table::dma_allowed(table::dma_half(&blocked)));
    assert!(!table::interrupts_allowed(table::interrupt_half(&blocked)));
    // QEMU's reserved bits: quadword 0's 6:2 and 63, quadword 2's 53:52.
    for entry in [blocked, table::blocked(table::untranslated())] {
        assert_eq!(entry[0] & (0x7C | 1 << 63), 0);
        assert_eq!(entry[2] & (3 << 52), 0);
    }

    let attached = table::attached(0x12_3000, 7);
    assert_eq!(attached[0], 0x12_3000 | 3 << 9 | 0b11 | 3 << 61);
    assert_eq!(attached[1], 7);
    assert!(table::dma_allowed(attached));
    assert!(table::dma_allowed(table::untranslated()), "remapping only");

    let remapped = table::remapped(0xABC_D040);
    assert_eq!(remapped & 1, 1, "IV");
    assert_eq!(remapped >> 1 & 0xF, 9, "512 entries");
    assert_eq!(remapped & 0x000F_FFFF_FFFF_FFC0, 0xABC_D040, "the table");
    assert_eq!(remapped >> 60 & 3, 2, "IntCtl remapped");
    assert_eq!(remapped & (3 << 52), 0);
    assert!(table::interrupts_allowed(remapped));
}

/// The two halves are separate words: writing one never changes the
/// other (ledger 3867, condition 4).
///
/// Verifies: L.iommu.84, L.iommu.91
#[test]
fn the_dma_half_and_the_interrupt_half_never_share_a_word() {
    let mut entry = table::blocked(table::blocked_translating(0x1000));
    let interrupts = table::remapped(0x2000);
    entry[2] = interrupts;
    let [q0, q1] = table::attached(0x3000, 5);
    entry[0] = q0;
    entry[1] = q1;
    assert_eq!(table::interrupt_half(&entry), interrupts);
    assert_eq!(table::dma_half(&entry), table::attached(0x3000, 5));
    let [q0, q1] = table::blocked_translating(0x1000);
    entry[0] = q0;
    entry[1] = q1;
    assert_eq!(
        table::interrupt_half(&entry),
        interrupts,
        "a detach keeps the vectors"
    );
}

/// Verifies: L.iommu.91, L.iommu.93
#[test]
fn interrupt_entries_messages_and_redirection_entries() {
    let entry = table::interrupt_entry(0x45, 3);
    assert_eq!(entry, 1 | 3 << 8 | 0x45 << 16, "RemapEn, fixed, physical");
    assert!(table::interrupt_entry_used(entry));
    assert!(!table::interrupt_entry_used(0));
    assert_eq!(table::destination(255), Some(255));
    assert_eq!(table::destination(256), None, "no x2APIC destination");
    assert_eq!(table::message_data(5), 5);
    assert_eq!(table::MESSAGE_ADDRESS, 0xFEE0_0000);
    assert_eq!(table::TABLE_BYTES, 2048);
    assert_eq!(table::redirection_entry(1, false, false, false), Some(1));
    assert_eq!(
        table::redirection_entry(0x3F, true, true, true),
        Some(0x3F | 1 << 13 | 1 << 15 | 1 << 16)
    );
    assert_eq!(table::redirection_entry(0x100, false, false, false), None);
}

/// Each command against the specification's layout and QEMU's checks of
/// its reserved fields (`amdvi_completion_wait`, `amdvi_inval_devtab_entry`,
/// `amdvi_inval_pages`, `amdvi_inval_inttable`, `amdvi_inval_all`).
///
/// Verifies: L.iommu.86, L.iommu.88, L.iommu.89
#[test]
fn commands() {
    let wait = command::completion_wait(Completion::Store {
        address: 0x1234_5678,
        data: 42,
    });
    assert_eq!(wait[0] >> 60, 1);
    assert_eq!(wait[0] & 1, 1, "store");
    assert_eq!(wait[0] & 0x000F_FFFF_FFFF_FFF8, 0x1234_5678);
    assert_eq!(wait[0] >> 52 & 0xFF, 0, "QEMU's reserved bits 59:52");
    assert_eq!(wait[1], 42);
    let unwritten = command::completion_wait(Completion::Unwritten);
    assert_eq!(unwritten, [1 << 60, 0], "stores nothing");

    assert_eq!(
        command::invalidate_device_entry(0x0310),
        [2 << 60 | 0x0310, 0]
    );
    let pages = command::invalidate_domain(9);
    assert_eq!(pages[0], 3 << 60 | 9 << 32);
    assert_eq!(pages[1] & 3, 3, "S and PDE");
    assert_eq!(pages[1] & 0x1F8, 0, "QEMU's reserved bits 11:3");
    assert_eq!(pages[1] & !0xFFF, 0x7FFF_FFFF_FFFF_F000, "every page");
    assert_eq!(
        command::invalidate_interrupt_table(0x18),
        [5 << 60 | 0x18, 0]
    );
    assert_eq!(command::invalidate_all(), [8 << 60, 0]);
    assert_eq!(command::illegal()[0] >> 60, 0);

    assert_eq!(command::ring_base(0x9000), 0x9000 | 8 << 56);
    assert_eq!(command::device_table_base(0x20_0000), 0x20_0000 | 511);
    assert_eq!(command::slot_register(255), 0xFF0);
    assert_eq!(command::slot_of(0xFF0), 255);
    assert_eq!(command::free_slots(0, 0), 255);
    assert_eq!(command::free_slots(5, 4), 0);
}

/// Hardware's encoding and QEMU's, told apart by the address.
///
/// Verifies: L.iommu.90
#[test]
fn events_decode_as_hardware_and_qemu_write_them() {
    let record = |devid: u64, code: u64, flags: u64, address: u64| {
        [devid | flags << 48 | code << 60, address]
    };
    // Hardware: a write refused, RW set, I clear.
    assert_eq!(
        event::decode(record(0x0310, 2, 1 << 5, 0x1234)),
        Event::Access {
            devid: 0x0310,
            page: 0x1000,
            write: true
        }
    );
    assert_eq!(
        event::decode(record(0x0310, 2, 0, 0x1234)),
        Event::Access {
            devid: 0x0310,
            page: 0x1000,
            write: false
        }
    );
    // QEMU: I always set, the write in bit 1, a DMA address.
    assert_eq!(
        event::decode(record(0x20, 2, 1 << 3 | 1 << 1, 0x1000)),
        Event::Access {
            devid: 0x20,
            page: 0x1000,
            write: true
        }
    );
    assert_eq!(
        event::decode(record(0x20, 2, 1 << 3 | 1, 0x1000)),
        Event::Access {
            devid: 0x20,
            page: 0x1000,
            write: false
        }
    );
    // An interrupt request: I set, the interrupt window.
    assert_eq!(
        event::decode(record(0x20, 2, 1 << 3, 0xFEE0_0000)),
        Event::Interrupt { devid: 0x20 }
    );
    // Bit 1 outside I's case is not a write: hardware's NX.
    assert_eq!(
        event::decode(record(0x20, 2, 1 << 1, 0x1000)),
        Event::Access {
            devid: 0x20,
            page: 0x1000,
            write: false
        }
    );
    assert_eq!(
        event::decode(record(0, 5, 0, 0x9000)),
        Event::Other { devid: 0, code: 5 }
    );
    assert!(!event::written([0, 0]));
    assert!(event::written(record(0, 2, 0, 0)));
}

/// QEMU's `dma-translation=off` EFR has HATS 11b and `HATDis`: it remaps
/// only, and its HATS is never read (ledger 3867, condition 2).
///
/// Verifies: L.iommu.82, L.iommu.83
#[test]
fn units_are_accepted_by_hat_disabled_first_then_hats() {
    // QEMU 10.2's AMDVI_DEFAULT_EXT_FEATURES: PREFETCH, PPR, IA, GT, HE,
    // GATS 1 << 12 and HATS 1 << 10 (four levels), GA.
    let qemu =
        (1 << 0) | (1 << 1) | (1 << 6) | (1 << 4) | (1 << 8) | (1 << 12) | (1 << 10) | (1 << 7);
    assert_eq!(
        unit::accept(qemu, false),
        Ok(unit::Accepted {
            translates: true,
            invalidate_all: true
        })
    );
    let hat_disabled = (qemu & !(3 << 10)) | 3 << 10;
    assert_eq!(
        unit::accept(hat_disabled, true),
        Ok(unit::Accepted {
            translates: false,
            invalidate_all: true
        }),
        "HATDis decides before HATS"
    );
    assert!(
        unit::accept(hat_disabled, false).is_err(),
        "HATS 11b translating"
    );
    let nazuna = 0x2465_77ef_a225_4afa;
    assert_eq!(
        unit::accept(nazuna, false),
        Ok(unit::Accepted {
            translates: true,
            invalidate_all: true
        })
    );
    assert!(unit::left_on(unit::CONTROL_ENABLE));
    assert!(!unit::left_on(unit::CONTROL_COHERENT));
}

/// Verifies: L.iommu.98
#[test]
fn ferrix_amdvi_off_is_the_bare_word() {
    assert!(unit::switched_off(Some("off")));
    for not in [None, Some("on"), Some("OFF"), Some("off "), Some("")] {
        assert!(!unit::switched_off(not), "{not:?}");
    }
}

/// Every other encoding's table entries name no level, so the mapper's new
/// call writes exactly what it wrote before (`docs/AMD-VI.md` §1.4).
///
/// Verifies: L.iommu.85
#[test]
fn table_descriptor_at_is_table_descriptor_for_every_other_encoding() {
    fn same<E: Encoding>() {
        for depth in 0..=3 {
            let level = Level::new(depth).unwrap();
            for flags in [MapFlags::DMA, MapFlags::KERNEL_DATA, MapFlags::USER_CODE] {
                let table = PhysAddr(0x0000_0012_3456_7000);
                assert_eq!(
                    E::table_descriptor_at(table, level, flags),
                    E::table_descriptor(table, flags),
                    "{} at level {depth}",
                    E::NAME
                );
            }
        }
    }
    same::<crate::x86_64::X86_64>();
    same::<crate::aarch64::AArch64>();
    same::<crate::armv7a::Armv7a>();
    same::<crate::stage2::ArmStage2>();
    same::<crate::vtd::VtdSecondLevel>();
    same::<crate::vtd::VtdFourLevel>();
}
