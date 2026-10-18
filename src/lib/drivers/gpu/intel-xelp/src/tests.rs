// The table checks are translated from Linux v7.0,
// drivers/gpu/drm/i915/selftests/intel_uncore.c:
// Copyright © 2016 Intel Corporation
// Under the MIT notice in this crate's i915/NOTICE.

//! Host tests: i915's own table selftests, and each module against a model
//! of the registers it drives.

extern crate std;

use std::collections::BTreeMap;
use std::vec::Vec;

use crate::generated::intel_lrc::{GEN12_RCS_OFFSETS, GEN12_XCS_OFFSETS};
use crate::generated::intel_mocs::GEN12_MOCS_TABLE;
use crate::generated::intel_uncore::{GEN12_FW_RANGES, GEN12_SHADOWED_REGS};
use crate::io::{Clock, Mmio};
use crate::reg::{Reg, bit, field_get, field_prep, genmask, masked_bit_disable, masked_bit_enable};
use crate::{device_info, lrc, mocs, uncore};

/// `intel_fw_table_check` with `is_watertight`: every range starts one past
/// the previous end, never goes backwards, and has a length.
#[test]
fn gen12_fw_ranges_are_watertight() {
    let mut prev: i64 = -1;
    for (i, range) in GEN12_FW_RANGES.iter().enumerate() {
        assert_eq!(prev + 1, i64::from(range.start), "entry {i} not watertight");
        assert!(range.start < range.end, "entry {i} has no length");
        prev = i64::from(range.end);
    }
    // i915's last Gen12 range ends at the media VDBOX2 block.
    assert_eq!(GEN12_FW_RANGES.last().map(|r| r.end), Some(0x1d_3fff));
}

/// `intel_shadow_table_check`: ordered, no overlap, dword-aligned starts.
#[test]
fn gen12_shadowed_regs_are_ordered() {
    let mut prev: i64 = -1;
    for (i, range) in GEN12_SHADOWED_REGS.iter().enumerate() {
        assert!(range.end >= range.start, "range {i} ends before it starts");
        assert!(
            prev < i64::from(range.start),
            "range {i} overlaps the previous"
        );
        assert_eq!(range.start % 4, 0, "range {i} not dword-aligned");
        prev = i64::from(range.end);
    }
}

#[test]
fn forcewake_domains_by_offset() {
    // RING_TAIL(RENDER_RING_BASE) is in 0x2000-0x26ff: render.
    assert_eq!(
        uncore::read_fw_domains(Reg(0x2030)),
        uncore::FORCEWAKE_RENDER
    );
    // ... and shadowed, so a write needs nothing.
    assert_eq!(uncore::write_fw_domains(Reg(0x2030)), 0);
    // The execlist submit queue is shadowed too (0x2510-0x2550).
    assert!(uncore::is_shadowed(0x2510) && uncore::is_shadowed(0x2550));
    assert!(!uncore::is_shadowed(0x2554));
    // GEN11_EU_DISABLE, 0x9134: GT (0x8500-0x94cf).
    assert_eq!(uncore::read_fw_domains(Reg(0x9134)), uncore::FORCEWAKE_GT);
    // The forcewake request registers themselves need none to be written.
    assert_eq!(uncore::write_fw_domains(uncore::FORCEWAKE_RENDER_GEN9), 0);
    assert_eq!(uncore::write_fw_domains(uncore::FORCEWAKE_GT_GEN9), 0);
    // Always-on and outside NEEDS_FORCE_WAKE: GEN11_GFX_MSTR_IRQ, the pcode mailbox.
    assert_eq!(uncore::read_fw_domains(Reg(0x0019_0010)), 0);
    assert_eq!(uncore::read_fw_domains(Reg(0x0013_8124)), 0);
    // BCS0's ring (0x22000) is GT; VCS0's (0x1c0000) VDBOX0.
    assert_eq!(uncore::read_fw_domains(Reg(0x2_2034)), uncore::FORCEWAKE_GT);
    assert_eq!(
        uncore::read_fw_domains(Reg(0x1c_0034)),
        uncore::FORCEWAKE_MEDIA_VDBOX0
    );
}

#[test]
fn adl_n_domains() {
    let info = device_info::lookup(0x8086, 0x46d4).expect("ADL-N");
    assert!(device_info::lookup(0x8086, 0x46a6).is_none());
    assert!(device_info::lookup(0x10de, 0x46d4).is_none());
    let domains = uncore::fw_domains_init(info.platform_engine_mask);
    // render, GT, VDBOX0, VDBOX2, VEBOX0
    assert_eq!(
        domains.mask(),
        uncore::FORCEWAKE_RENDER
            | uncore::FORCEWAKE_GT
            | uncore::FORCEWAKE_MEDIA_VDBOX0
            | uncore::FORCEWAKE_MEDIA_VDBOX2
            | uncore::FORCEWAKE_MEDIA_VEBOX0
    );
    let vdbox2 = domains
        .masked(uncore::FORCEWAKE_MEDIA_VDBOX2)
        .next()
        .expect("VDBOX2");
    assert_eq!((vdbox2.reg_set, vdbox2.reg_ack), (Reg(0xa548), Reg(0xd58)));
}

/// Forcewake registers that ack each request, optionally only after some
/// fallback toggles, and remember every write.
struct FwModel {
    regs: BTreeMap<u32, u32>,
    acks: BTreeMap<u32, u32>,
    writes: Vec<(u32, u32)>,
    /// Requests of the kernel bit that are ignored before one is acked.
    lose: u32,
    now: u64,
}

impl FwModel {
    fn new() -> Self {
        let acks = [(0xa278, 0xd84), (0xa188, 0x0013_0044)]
            .into_iter()
            .collect();
        Self {
            regs: BTreeMap::new(),
            acks,
            writes: Vec::new(),
            lose: 0,
            now: 0,
        }
    }
}

impl Mmio for FwModel {
    fn read32(&mut self, offset: u32) -> u32 {
        self.regs.get(&offset).copied().unwrap_or(0)
    }

    fn write32(&mut self, offset: u32, value: u32) {
        self.writes.push((offset, value));
        let old = self.regs.get(&offset).copied().unwrap_or(0);
        let mask = value >> 16;
        let new = (old & !mask) | (value & mask & 0xffff);
        let _ = self.regs.insert(offset, new);
        if let Some(ack) = self.acks.get(&offset).copied() {
            let request = mask & value & uncore::FORCEWAKE_KERNEL != 0;
            if request && self.lose > 0 {
                // The request is taken, but its ack is lost until the
                // next write to the register (a fallback kick) delivers it.
                self.lose -= 1;
                return;
            }
            let _ = self.regs.insert(ack, new);
        }
    }
}

impl Clock for FwModel {
    fn now_us(&mut self) -> u64 {
        self.now
    }

    fn delay_us(&mut self, us: u32) {
        self.now += u64::from(us);
    }
}

#[test]
fn forcewake_get_and_put() {
    let mut model = FwModel::new();
    let domains = uncore::fw_domains_init(device_info::ADL_N_INFO.platform_engine_mask);
    let both = uncore::FORCEWAKE_RENDER | uncore::FORCEWAKE_GT;
    let mut clock = FwModel::new();
    uncore::fw_domains_get(&mut model, &mut clock, &domains, both).expect("woken");
    // Render's request first (id order), then GT's.
    assert_eq!(model.writes, [(0xa278, 0x0001_0001), (0xa188, 0x0001_0001)]);
    assert_eq!(model.read32(0xd84) & 1, 1);
    model.writes.clear();
    uncore::fw_domains_put(&mut model, &domains, both);
    assert_eq!(model.writes, [(0xa278, 0x0001_0000), (0xa188, 0x0001_0000)]);
    assert_eq!(model.read32(0x0013_0044) & 1, 0);
}

#[test]
fn forcewake_never_acked_times_out() {
    let mut model = FwModel::new();
    let _ = model.acks.remove(&0xa278);
    let domains = uncore::fw_domains_init(device_info::ADL_N_INFO.platform_engine_mask);
    let mut clock = FwModel::new();
    let error = uncore::fw_domains_get(&mut model, &mut clock, &domains, uncore::FORCEWAKE_RENDER);
    assert_eq!(
        error,
        Err(uncore::ForcewakeError::AckNotSet {
            domain: uncore::FORCEWAKE_RENDER,
            ack: 0
        })
    );
    // Ten fallback passes were tried: ten enables of bit 15.
    let kicks = model
        .writes
        .iter()
        .filter(|w| **w == (0xa278, masked_bit_enable(1 << 15)))
        .count();
    assert_eq!(kicks, 10);
}

#[test]
fn forcewake_lost_ack_found_by_fallback() {
    let mut model = FwModel::new();
    model.lose = 1;
    let domains = uncore::fw_domains_init(device_info::ADL_N_INFO.platform_engine_mask);
    let mut clock = FwModel::new();
    // The first request's ack is lost; the first fallback toggle delivers it.
    let result = uncore::fw_domains_get(&mut model, &mut clock, &domains, uncore::FORCEWAKE_RENDER);
    assert!(result.is_ok(), "{result:?}");
    let kicks = model
        .writes
        .iter()
        .filter(|w| **w == (0xa278, masked_bit_enable(1 << 15)))
        .count();
    assert_eq!(kicks, 1);
}

#[test]
fn register_helpers() {
    assert_eq!(bit(15), 0x8000);
    assert_eq!(genmask(7, 4), 0xf0);
    assert_eq!(genmask(31, 0), u32::MAX);
    assert_eq!(field_prep(genmask(7, 4), 0x3), 0x30);
    assert_eq!(field_get(genmask(7, 4), 0x35), 0x3);
    assert_eq!(masked_bit_enable(0x5), 0x0005_0005);
    assert_eq!(masked_bit_disable(0x5), 0x0005_0000);
}

#[test]
fn gen12_mocs_table() {
    // Entries 2..=23, 48..=51, 60..=63: 30 used.
    assert_eq!(GEN12_MOCS_TABLE.len(), 30);
    // Index 3 is the uncached entry: LE_1_UC | LE_TC_1_LLC, L3_1_UC.
    assert_eq!(mocs::get_entry_control(mocs::UC_INDEX), 0x5);
    assert_eq!(mocs::get_entry_l3cc(mocs::UC_INDEX), 0x10);
    // Unused 0, 1 and 24 take entry 2's (L3 + LLC: LE_3_WB | LE_TC_1_LLC | LE_LRUM(3), L3_3_WB).
    for unused in [0, 1, 24, 47, 52, 59] {
        assert_eq!(mocs::get_entry_control(unused), 0x37);
        assert_eq!(mocs::get_entry_l3cc(unused), 0x30);
    }
    let writes: Vec<_> = mocs::intel_mocs_init_writes().collect();
    assert_eq!(writes.len(), 64 + 32);
    assert_eq!(writes.first(), Some(&(Reg(0x4000), 0x37)));
    assert_eq!(writes.get(63), Some(&(Reg(0x40fc), 0x37)));
    // LNCFCMOCS1 holds entries 2 (low) and 3 (high).
    assert_eq!(writes.get(65), Some(&(Reg(0xb024), 0x0010_0030)));
}

#[test]
fn lrc_offsets_decode() {
    let mut regs = [0_u32; 256];
    let base = 0x2_2000; // BCS0
    let used = lrc::set_offsets(&mut regs, &GEN12_XCS_OFFSETS, base, true).expect("fits");
    // NOP(1), LRI(13) + 26, NOP(5), LRI(9) + 18, BB_END.
    assert_eq!(used, 1 + 1 + 26 + 5 + 1 + 18 + 1);
    let lri = lrc::mi_load_register_imm(13) | lrc::MI_LRI_FORCE_POSTED | lrc::MI_LRI_LRM_CS_MMIO;
    assert_eq!(regs[0], 0);
    assert_eq!(regs[1], lri);
    // Each CTX_* index names the value after its register's offset.
    let offset_at = |index: usize| regs[index - 1];
    assert_eq!(offset_at(lrc::ctx::CONTEXT_CONTROL), base + 0x244);
    assert_eq!(offset_at(lrc::ctx::RING_HEAD), base + 0x034);
    assert_eq!(offset_at(lrc::ctx::RING_TAIL), base + 0x030);
    assert_eq!(offset_at(lrc::ctx::RING_START), base + 0x038);
    assert_eq!(offset_at(lrc::ctx::RING_CTL), base + 0x03c);
    assert_eq!(offset_at(lrc::ctx::BB_STATE), base + 0x110);
    assert_eq!(
        regs[33],
        lrc::mi_load_register_imm(9) | lrc::MI_LRI_FORCE_POSTED | lrc::MI_LRI_LRM_CS_MMIO
    );
    assert_eq!(offset_at(lrc::ctx::TIMESTAMP), base + 0x3a8);
    assert_eq!(offset_at(lrc::ctx::PDP3_UDW), base + 0x28c);
    assert_eq!(offset_at(lrc::ctx::PDP0_LDW), base + 0x270);
    assert_eq!(regs[used - 1], lrc::MI_BATCH_BUFFER_END | 1);

    let mut regs = [0_u32; 512];
    let used = lrc::set_offsets(&mut regs, &GEN12_RCS_OFFSETS, 0x2000, false).expect("fits");
    // The xcs part (52), LRI(3) + 6, NOP(6), LRI(1) + 2, NOP(3 + 9 + 1),
    // LRI(51) + 102, NOP(1).
    assert_eq!(used, 52 + 7 + 6 + 3 + 13 + 103 + 1);
    // lrc_ring_mi_mode: RING_MI_MODE (0x09c) is the eighth register of the
    // 51-entry LRI, whose header is at 81.
    assert_eq!(regs[lrc::lrc_ring_mi_mode()], 0x2000 + 0x09c);
    assert!(lrc::set_offsets(&mut [0; 16], &GEN12_RCS_OFFSETS, 0x2000, false).is_err());
    assert_eq!(
        lrc::set_offsets(&mut regs, &[0x40], 0, false),
        Err(lrc::OffsetsError::EmptyLri)
    );
    assert_eq!(
        lrc::set_offsets(&mut regs, &[0x41], 0, false),
        Err(lrc::OffsetsError::Truncated)
    );
}
