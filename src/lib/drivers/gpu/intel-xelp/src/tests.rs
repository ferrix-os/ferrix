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

#[test]
fn ppgtt_and_ggtt_entries() {
    use crate::gtt;
    // WB (PAT index 0) and UC (3), as TGL_CACHELEVEL maps them.
    assert_eq!(gtt::cachelevel_to_pat(gtt::CacheLevel::Llc), 0);
    assert_eq!(gtt::cachelevel_to_pat(gtt::CacheLevel::None), 3);
    assert_eq!(gtt::gen12_pte_encode(0x1234_5000, 0, 0), 0x1234_5003);
    assert_eq!(
        gtt::gen12_pte_encode(0x1234_5000, 3, 0),
        0x1234_5003 | (1 << 3) | (1 << 4)
    );
    assert_eq!(
        gtt::gen12_pte_encode(0x1234_5000, 4, gtt::PTE_READ_ONLY),
        0x1234_5001 | (1 << 7)
    );
    assert_eq!(gtt::gen8_pde_encode(0x8000, gtt::CacheLevel::Llc), 0x8003);
    assert_eq!(
        gtt::gen8_pde_encode(0x8000, gtt::CacheLevel::None),
        0x8003 | 0x18
    );
    assert_eq!(gtt::gen8_ggtt_pte_encode(0x7c00_0000), 0x7c00_0001);
    assert_eq!(gtt::gen8_ggtt_pte_decode(0x7c00_0001), Some(0x7c00_0000));
    assert_eq!(gtt::gen8_ggtt_pte_decode(0x7c00_0000), None);
    // GGMS 3 is 8 MiB of entries: 1 Mi entries, 4 GiB of GGTT.
    let gmch = 3 << gtt::BDW_GMCH_GGMS_SHIFT;
    assert_eq!(gtt::gen8_get_total_gtt_size(gmch), 8 << 20);
    assert_eq!(gtt::ggtt_total(gmch), 4 << 30);
    assert_eq!(gtt::ggtt_total(0), 0);
    // A 48-bit address splits 9 bits a level above the 12-bit page.
    let addr: u64 = (5 << 39) | (6 << 30) | (7 << 21) | (8 << 12) | 0x123;
    assert_eq!(
        [0, 1, 2, 3].map(|l| gtt::gen8_pte_index(addr, l)),
        [8, 7, 6, 5]
    );
    let pat = gtt::tgl_setup_private_ppat();
    assert_eq!(pat[1], (Reg(0x4804), gtt::GEN8_PPAT_WC));
    assert_eq!(pat[3], (Reg(0x480c), gtt::GEN8_PPAT_UC));
}

#[test]
fn execlist_descriptors_and_ports() {
    use crate::{engine, execlists};
    let lrca = execlists::lrc_descriptor(0x0010_2000, true);
    // 4-level (3 << 3), valid, privileged.
    assert_eq!(lrca, 0x0010_2000 | 0x18 | 0x1 | 0x100);
    let rcs = engine::ENGINES[0];
    let ccid = execlists::context_ccid(0, execlists::engine_ccid(rcs.class, rcs.instance));
    // Tag 0 is software context id 1, at bit 37 of the descriptor.
    assert_eq!(execlists::descriptor(lrca, ccid) >> 37 & 0x7ff, 1);
    let bcs = engine::ENGINES[1];
    let bccid = execlists::engine_ccid(bcs.class, bcs.instance);
    assert_eq!(u64::from(bccid) << 32 >> 61, 3);
    let desc = execlists::descriptor(lrca, ccid);
    let writes = execlists::submit_ports(rcs.mmio_base, [desc, 0]);
    assert_eq!(
        writes,
        [
            (Reg(0x2518), 0),
            (Reg(0x251c), 0),
            (Reg(0x2510), desc as u32),
            (Reg(0x2514), (desc >> 32) as u32),
            (Reg(0x2550), 1),
        ]
    );
    // All of the submit queue is shadowed, so these writes need no forcewake;
    // the control register is too.
    assert!(
        writes
            .iter()
            .all(|(reg, _)| uncore::write_fw_domains(*reg) == 0)
    );
}

#[test]
fn gen12_csb() {
    use crate::execlists::{CsbEvent, csb_indices, gen12_csb_parse};
    let idle = 0x7ff << 15;
    let ctx = |id: u64| id << 15;
    // From idle to a context: promote.
    assert_eq!(gen12_csb_parse(ctx(1) | (idle << 32)), CsbEvent::Promote);
    // A context completes to idle: away valid, to idle.
    assert_eq!(gen12_csb_parse(idle | (ctx(1) << 32)), CsbEvent::Complete);
    // A new queue taking over a running context: promote.
    assert_eq!(
        gen12_csb_parse(ctx(2) | 1 | (ctx(1) << 32)),
        CsbEvent::Promote
    );
    // Away with a switch detail, or a promotion to nothing, are reported.
    assert!(matches!(
        gen12_csb_parse(idle | ((ctx(1) | 5) << 32)),
        CsbEvent::Unexpected { .. }
    ));
    assert!(matches!(
        gen12_csb_parse(idle | (idle << 32)),
        CsbEvent::Unexpected { .. }
    ));
    let walked: Vec<u32> = csb_indices(10, 1).expect("in range").collect();
    assert_eq!(walked, [11, 0, 1]);
    assert_eq!(csb_indices(4, 4).expect("in range").count(), 0);
    assert!(csb_indices(0, 12).is_none());
}

/// Every ioctl number against what the probe printed from Linux 7.0's
/// `i915_drm.h` (`docs/INTEL-GPU.md` Appendix A).
#[test]
fn ioctl_numbers_match_linux() {
    use crate::uapi::ioctl;
    let pinned = [
        (ioctl::GETPARAM, 0xc010_6446),
        (ioctl::QUERY, 0xc010_6479),
        (ioctl::GEM_CREATE, 0xc010_645b),
        (ioctl::GEM_CREATE_EXT, 0xc018_647c),
        (ioctl::GEM_MMAP_OFFSET, 0xc020_6464),
        (ioctl::GEM_EXECBUFFER2_WR, 0xc040_6469),
        (ioctl::GEM_EXECBUFFER2, 0x4040_6469),
        (ioctl::GEM_WAIT, 0xc010_646c),
        (ioctl::GEM_BUSY, 0xc008_6457),
        (ioctl::GEM_SET_TILING, 0xc010_6461),
        (ioctl::GEM_GET_TILING, 0xc010_6462),
        (ioctl::GEM_SET_DOMAIN, 0x400c_645f),
        (ioctl::GEM_SET_CACHING, 0x4008_646f),
        (ioctl::GEM_MADVISE, 0xc00c_6466),
        (ioctl::GEM_USERPTR, 0xc018_6473),
        (ioctl::GEM_GET_APERTURE, 0x8010_6463),
        (ioctl::GEM_CONTEXT_CREATE_EXT, 0xc010_646d),
        (ioctl::GEM_CONTEXT_DESTROY, 0x4008_646e),
        (ioctl::GEM_CONTEXT_GETPARAM, 0xc018_6474),
        (ioctl::GEM_CONTEXT_SETPARAM, 0xc018_6475),
        (ioctl::GEM_VM_CREATE, 0xc010_647a),
        (ioctl::GEM_VM_DESTROY, 0x4010_647b),
        (ioctl::GET_RESET_STATS, 0xc018_6472),
        (ioctl::REG_READ, 0xc010_6471),
    ];
    for (ours, linux) in pinned {
        assert_eq!(ours, linux, "{ours:#x} vs {linux:#x}");
    }
}

#[test]
fn uapi_layouts_round_trip() {
    use crate::uapi::{ExecObject2, Execbuffer2, QueryItem};
    // The probe's sizes: execbuffer2 64, exec_object2 56.
    assert_eq!(Execbuffer2::SIZE, 64);
    assert_eq!(ExecObject2::SIZE, 56);
    let object = ExecObject2 {
        handle: 7,
        offset: 0x1_0000_0000,
        flags: 1 << 4,
        ..Default::default()
    };
    let bytes = object.to_bytes();
    assert_eq!(&bytes[0..4], &7_u32.to_le_bytes());
    assert_eq!(&bytes[24..32], &0x1_0000_0000_u64.to_le_bytes());
    assert_eq!(ExecObject2::from_bytes(&bytes), Some(object));
    assert_eq!(ExecObject2::from_bytes(&bytes[..55]), None);
    let item = QueryItem {
        query_id: 1,
        length: -22,
        flags: 0,
        data_ptr: 0xdead_0000,
    };
    assert_eq!(QueryItem::from_bytes(&item.to_bytes()), Some(item));
    assert_eq!(&item.to_bytes()[8..12], &(-22_i32).to_le_bytes());
}

#[test]
fn gen12_topology() {
    use crate::sseu::{Fuses, TOPOLOGY_LEN, fill_topology_info, gen12_sseu_info_init};
    // Three dual-subslices of eight EUs (four pairs fused on), as a 24-EU
    // part might be; the box's own fuses are M1's to read.
    let info = gen12_sseu_info_init(Fuses {
        slice_enable: 1,
        geometry_dss_enable: 0b111,
        eu_disable: 0xf0,
    });
    assert_eq!(
        (
            info.subslice_mask,
            info.eu_mask,
            info.eu_per_subslice,
            info.eu_total
        ),
        (0b111, 0xff, 8, 24)
    );
    // Bits above the six dual-subslices are not subslices.
    let wide = gen12_sseu_info_init(Fuses {
        slice_enable: 1,
        geometry_dss_enable: 0xffff_ffc3,
        eu_disable: 0,
    });
    assert_eq!((wide.subslice_mask, wide.eu_total), (0b11, 32));
    let topo = fill_topology_info(&info);
    assert_eq!(TOPOLOGY_LEN, 16 + 1 + 1 + 12);
    // max_slices 1, max_subslices 6, max_eus 16, subslice_offset 1,
    // subslice_stride 1, eu_offset 2, eu_stride 2.
    assert_eq!(
        &topo[0..16],
        &[0, 0, 1, 0, 6, 0, 16, 0, 1, 0, 1, 0, 2, 0, 2, 0]
    );
    assert_eq!(topo[16], 1);
    assert_eq!(topo[17], 0b111);
    assert_eq!(&topo[18..24], &[0xff, 0, 0xff, 0, 0xff, 0]);
    assert_eq!(&topo[24..30], &[0; 6]);
}

/// A register file: reads give what was written (or a preset), writes are
/// kept in order.
#[derive(Default)]
struct Regs {
    values: BTreeMap<u32, u32>,
    writes: Vec<(u32, u32)>,
}

impl Mmio for Regs {
    fn read32(&mut self, offset: u32) -> u32 {
        self.values.get(&offset).copied().unwrap_or(0)
    }

    fn write32(&mut self, offset: u32, value: u32) {
        self.writes.push((offset, value));
        let _ = self.values.insert(offset, value);
    }
}

#[test]
fn adl_workaround_lists() {
    use crate::engine::ENGINES;
    use crate::workarounds::{self, regs};
    let gt = workarounds::gt_init_workarounds(0b110, &ENGINES);
    let offsets: Vec<u32> = gt.iter().map(|wa| wa.reg.offset()).collect();
    // Sorted by offset; MCR steering first; VCS0 and VCS2 (even instances).
    assert_eq!(offsets, [0xfdc, 0x9424, 0x9550, 0x1c_3f10, 0x1d_3f10]);
    let mcr = gt.iter().next().expect("steering");
    // The lowest present subslice of 0b110 is 1.
    assert_eq!(mcr.set, 1 << 24);
    assert_eq!(mcr.clr, (0xf << 27) | (0x7 << 24));
    assert!(!gt.overflowed() && gt.discarded == 0);

    let rcs = &ENGINES[0];
    let engine = workarounds::engine_workarounds(rcs, mocs::UC_INDEX);
    // GEN8_ROW_CHICKEN2 is named twice and merged into one entry.
    let row = engine
        .iter()
        .find(|wa| wa.reg == regs::GEN8_ROW_CHICKEN2)
        .expect("row chicken");
    assert_eq!(row.set, masked_bit_enable((1 << 14) | (1 << 8)));
    assert_eq!(row.read, (1 << 14) | (1 << 8));
    // GEN10_SAMPLER_MODE too: the indirect state override and ENABLE_SMALLPL.
    let sampler = engine
        .iter()
        .find(|wa| wa.reg == regs::GEN10_SAMPLER_MODE)
        .expect("sampler");
    assert_eq!(sampler.set, masked_bit_enable(1 | (1 << 15)));
    // RING_CMD_CCTL: uncached (index 3) both ways, << 1 as i915 encodes it.
    let cctl = engine
        .iter()
        .find(|wa| wa.reg == Reg(0x20c4))
        .expect("cmd cctl");
    assert_eq!(cctl.set & 0xffff, (6 << 7) | 6);
    // CMD_CCTL, GARBCNTL, SAMPLER_MODE (2), CS_DEBUG_MODE1, ROW_CHICKEN2 (2),
    // FF_THREAD_MODE, ROW_CHICKEN4, PSMI_CTL, FF_SLICE_CS_CHICKEN1.
    assert_eq!(engine.len(), 9);
    assert_eq!(engine.wa_count, 11);
    let offsets: Vec<u32> = engine.iter().map(|wa| wa.reg.offset()).collect();
    assert!(offsets.windows(2).all(|w| w[0] < w[1]));

    let ctx = workarounds::ctx_workarounds(rcs, mocs::UC_INDEX);
    assert_eq!(ctx.len(), 6);
    let bcs_ctx = workarounds::ctx_workarounds(&ENGINES[1], mocs::UC_INDEX);
    assert_eq!(
        bcs_ctx
            .iter()
            .map(|wa| (wa.reg, wa.set))
            .collect::<Vec<_>>(),
        [(Reg(0x2_2204), (6 << 8) | 6)]
    );
}

#[test]
fn workarounds_apply_verify_emit() {
    use crate::engine::ENGINES;
    use crate::workarounds::{self, regs};
    let rcs = &ENGINES[0];
    let wal = workarounds::engine_workarounds(rcs, mocs::UC_INDEX);
    let mut mmio = Regs::default();
    let _ = mmio.values.insert(regs::GEN8_GARBCNTL.offset(), 0xff);
    workarounds::wa_list_apply(&mut mmio, &wal);
    // GEN8_GARBCNTL: bit 7 cleared by read-modify-write.
    assert_eq!(mmio.values[&regs::GEN8_GARBCNTL.offset()], 0x7f);
    assert_eq!(workarounds::wa_list_verify(&mut mmio, &wal).count(), 0);
    // A bit that did not stick is reported.
    let _ = mmio.values.insert(regs::GEN7_FF_THREAD_MODE.offset(), 0);
    let lost: Vec<_> = workarounds::wa_list_verify(&mut mmio, &wal).collect();
    assert_eq!(lost, [(regs::GEN7_FF_THREAD_MODE, 0)]);

    let ctx = workarounds::ctx_workarounds(rcs, mocs::UC_INDEX);
    let mut out = [0_u32; 64];
    let mut mmio = Regs::default();
    let _ = mmio.values.insert(regs::GEN8_WM_CHICKEN2.offset(), 0x100);
    let used = workarounds::emit_ctx_wa(&mut mmio, &ctx, &mut out).expect("fits");
    assert_eq!(used, 6 * 2 + 2);
    assert_eq!(out[0], lrc::mi_load_register_imm(6));
    let pairs: Vec<(u32, u32)> = out[1..13].chunks(2).map(|p| (p[0], p[1])).collect();
    // WM_CHICKEN2 is read and or-ed; FF_MODE2 written whole without a read.
    assert!(pairs.contains(&(0x5584, 0x100 | (1 << 5))));
    assert!(pairs.contains(&(0x6604, (4 << 16) | (224 << 24))));
    assert_eq!(out[13], 0);
    assert!(workarounds::emit_ctx_wa(&mut mmio, &ctx, &mut [0; 8]).is_none());
}

#[test]
fn adl_whitelists() {
    use crate::engine::ENGINES;
    use crate::workarounds::{self, RING_FORCE_TO_NONPRIV_ACCESS_RD};
    let rcs = workarounds::whitelist(&ENGINES[0]);
    let regs: Vec<u32> = rcs.iter().map(|wa| wa.reg.offset()).collect();
    // Sorted by offset with the flags: 0x7010, 0x7018, 0x7304, then the
    // read-only range of four at 0x2348.
    assert_eq!(
        regs,
        [
            0x7010,
            0x7018,
            0x7304,
            0x2348 | RING_FORCE_TO_NONPRIV_ACCESS_RD | 1
        ]
    );
    let writes = workarounds::apply_whitelist(0x2000, &rcs);
    assert_eq!(writes[0], (Reg(0x24d0), 0x7010));
    assert_eq!(writes[4], (Reg(0x24e0), 0x2094)); // RING_NOPID
    assert_eq!(writes[11].0, Reg(0x24fc));
    let bcs = workarounds::whitelist(&ENGINES[1]);
    assert_eq!(
        bcs.iter().map(|wa| wa.reg.offset()).collect::<Vec<_>>(),
        [0x2_23a8 | RING_FORCE_TO_NONPRIV_ACCESS_RD]
    );
}

#[test]
fn request_emitters() {
    use crate::commands::{Cs, emit_fast_copy, emit_store_dword, gpu};
    use crate::engine::ENGINES;
    use crate::engine_cs::{self, EMIT_BARRIER, EMIT_FLUSH, EMIT_INVALIDATE};
    let (rcs, bcs) = (&ENGINES[0], &ENGINES[1]);
    let mut ring = [0_u32; 64];
    // A render flush is one 6-dword PIPE_CONTROL; invalidation adds the
    // preparser fence, a PIPE_CONTROL and the 8-dword AUX invalidation.
    let mut cs = Cs::new(&mut ring);
    engine_cs::gen12_emit_flush(rcs, EMIT_FLUSH, &mut cs);
    assert_eq!(cs.len(), 6);
    let mut cs = Cs::new(&mut ring);
    engine_cs::gen12_emit_flush(rcs, EMIT_BARRIER, &mut cs);
    assert_eq!(cs.len(), 6 + 1 + 6 + 8 + 1);
    assert!(!cs.overflowed());
    // PIPE_CONTROL(6) header, then the AUX register (GEN12_CCS_AUX_INV).
    assert_eq!(
        ring[0],
        (3 << 29) | (3 << 27) | (2 << 24) | 4 | gpu::PIPE_CONTROL0_HDC_PIPELINE_FLUSH
    );
    assert_eq!(ring[6], engine_cs::preparser_disable(true));
    assert_eq!(
        ring[13],
        lrc::mi_load_register_imm(1) | gpu::MI_LRI_MMIO_REMAP_EN
    );
    assert_eq!((ring[14], ring[15]), (0x4208, 1));
    // The copy engine: MI_FLUSH_DW with TLB and CCS invalidation.
    let mut cs = Cs::new(&mut ring);
    engine_cs::gen12_emit_flush(bcs, EMIT_INVALIDATE, &mut cs);
    assert_eq!(cs.len(), 1 + 4 + 8 + 1);
    assert_eq!(
        ring[1] & (gpu::MI_INVALIDATE_TLB | gpu::MI_FLUSH_DW_CCS),
        gpu::MI_INVALIDATE_TLB | gpu::MI_FLUSH_DW_CCS
    );
    assert_eq!(ring[6], 0x4248); // GEN12_BCS0_AUX_INV

    let mut cs = Cs::new(&mut ring);
    engine_cs::gen8_emit_bb_start(&mut cs, 0x1_2345_6000, 0);
    assert_eq!(
        &ring[..6],
        &[
            gpu::MI_ARB_ON_OFF | 1,
            0x1880_0101,
            0x2345_6000,
            1,
            gpu::MI_ARB_ON_OFF,
            0
        ]
    );

    let mut cs = Cs::new(&mut ring);
    let tails = engine_cs::gen12_emit_fini_breadcrumb_rcs(&mut cs, 7, 0x1000, 0x80c8);
    // PIPE_CONTROL flush, PIPE_CONTROL seqno write, user interrupt, arb on,
    // the 6-dword busy-wait: tail; then the WaIdleLiteRestore pair.
    assert_eq!(
        tails,
        engine_cs::Tails {
            tail: 6 + 6 + 2 + 6,
            wa_tail: 22
        }
    );
    assert_eq!((ring[8], ring[10]), (0x1000, 7));
    assert_eq!(ring[12], gpu::MI_USER_INTERRUPT);
    assert_eq!(ring[17], 0x80c8);

    let mut cs = Cs::new(&mut ring);
    let tails = engine_cs::gen12_emit_fini_breadcrumb_xcs(&mut cs, 9, 0x1040, 0x80c8);
    assert_eq!(tails.wa_tail, 4 + 4 + 2 + 6 + 2);
    assert_eq!(
        (ring[4], ring[5], ring[7]),
        (
            (gpu::MI_FLUSH_DW + 1) | gpu::MI_FLUSH_DW_OP_STOREDW,
            0x1040 | 4,
            9
        )
    );

    let mut cs = Cs::new(&mut ring);
    emit_store_dword(&mut cs, 0x8000_1000, 0xc0ff_ee00);
    emit_fast_copy(&mut cs, 0x20_0000, 0x10_0000, 8192);
    assert_eq!(&ring[..4], &[0x1000_0002, 0x8000_1000, 0, 0xc0ff_ee00]);
    assert_eq!(ring[4], (2 << 29) | (0x42 << 22) | 8);
    assert_eq!(ring[7], (2 << 16) | 1024);
    let mut short = [0_u32; 3];
    let mut cs = Cs::new(&mut short);
    emit_store_dword(&mut cs, 0, 0);
    assert!(cs.overflowed());
}

#[test]
fn lrc_register_state() {
    use crate::engine::ENGINES;
    use lrc::{OffsetsTable, RingState, ctx};
    let mut regs = [0xdead_beef_u32; 1024];
    let pml4 = 0x1_2345_6000;
    lrc::lrc_init_regs(&mut regs, OffsetsTable::Rcs, 0x2000, true, pml4).expect("fits");
    // Inhibited first image: synchronous switches and restore inhibited.
    assert_eq!(regs[ctx::CONTEXT_CONTROL], 0x0009_0008 | 1);
    assert_eq!((regs[ctx::PDP0_UDW], regs[ctx::PDP0_LDW]), (1, 0x2345_6000));
    assert_eq!(regs[lrc::lrc_ring_mi_mode() + 1], STOP_RING_MASKED_OFF);
    // The page was cleared, except what the image wrote.
    assert_eq!(regs[600], 0);
    lrc::lrc_update_regs(
        &mut regs,
        RingState {
            start: 0x10_0000,
            head: 0,
            tail: 0x40,
            size: 16 << 10,
        },
        true,
        0x20_e000,
        64,
    );
    assert_eq!(regs[ctx::RING_CTL], (12 << 10) | 1);
    assert_eq!(regs[ctx::R_PWR_CLK_STATE], 0x8004_1000);
    assert_eq!(regs[lrc::LRC_RING_INDIRECT_PTR + 1], 0x20_e000 | 1);
    assert_eq!(regs[lrc::LRC_RING_INDIRECT_OFFSET + 1], 0xd << 6);
    assert_eq!(lrc::alloc_state_size(OffsetsTable::Rcs), (16 * 4096, 14));

    let mut page = [0_u32; 1024];
    let size = lrc::setup_indirect_ctx_bb(&mut page, &ENGINES[0], 0x20_0000).expect("fits");
    // timestamp (4 + 3 + 3), cmd buf (4 + 3), scratch (4), aux (8),
    // state cache (3) = 32 dwords, already a whole number of cache lines.
    assert_eq!(size, 32 * 4);
    assert_eq!(page[2], 0x20_0000 + 4096 + (ctx::TIMESTAMP as u32) * 4);
    assert_eq!(page[512 + 1], 0x20_0000 + 14 * 4096 + 4096 - 8);
    let mut page = [0_u32; 1024];
    let size = lrc::setup_indirect_ctx_bb(&mut page, &ENGINES[1], 0x30_0000).expect("fits");
    // timestamp 10, scratch 4, aux 8 = 22, padded to 32.
    assert_eq!(size, 32 * 4);
    assert_eq!(page[22..32], [0; 10]);
}

/// `RING_MI_MODE` in a fresh image: `STOP_RING` masked off.
const STOP_RING_MASKED_OFF: u32 = 1 << 24;

/// The registers some modules spell out themselves, against i915's own
/// headers as the generator read them: a typo in either fails here.
#[test]
fn hand_written_registers_match_i915_headers() {
    use crate::generated::{i915_reg as i915, intel_engine_regs as er, intel_gt_regs as gr};
    use crate::{engine, gtt, sseu};
    assert_eq!(uncore::FORCEWAKE_RENDER_GEN9, gr::FORCEWAKE_RENDER_GEN9);
    assert_eq!(
        uncore::FORCEWAKE_ACK_RENDER_GEN9,
        gr::FORCEWAKE_ACK_RENDER_GEN9
    );
    assert_eq!(uncore::FORCEWAKE_GT_GEN9, gr::FORCEWAKE_GT_GEN9);
    assert_eq!(uncore::FORCEWAKE_ACK_GT_GEN9, gr::FORCEWAKE_ACK_GT_GEN9);
    assert_eq!(uncore::FORCEWAKE_KERNEL, gr::FORCEWAKE_KERNEL);
    assert_eq!(
        uncore::FORCEWAKE_KERNEL_FALLBACK,
        gr::FORCEWAKE_KERNEL_FALLBACK
    );
    for n in 0..4 {
        assert_eq!(
            uncore::forcewake_media_vdbox_gen11(n),
            gr::forcewake_media_vdbox_gen11(n)
        );
        assert_eq!(
            uncore::forcewake_ack_media_vdbox_gen11(n),
            gr::forcewake_ack_media_vdbox_gen11(n)
        );
        assert_eq!(
            uncore::forcewake_media_vebox_gen11(n),
            gr::forcewake_media_vebox_gen11(n)
        );
        assert_eq!(
            uncore::forcewake_ack_media_vebox_gen11(n),
            gr::forcewake_ack_media_vebox_gen11(n)
        );
    }
    for i in 0..8 {
        assert_eq!(gtt::gen12_pat_index(i), gr::gen12_pat_index(i));
    }
    // GEN8_PPAT_* are intel_gtt.h's, which is not vendored.
    for i in 0..64 {
        assert_eq!(mocs::gen12_global_mocs(i), gr::gen12_global_mocs(i));
        assert_eq!(mocs::gen9_lncfcmocs(i), gr::gen9_lncfcmocs(i));
    }
    assert_eq!(sseu::GEN11_EU_DISABLE, gr::GEN11_EU_DISABLE);
    assert_eq!(sseu::GEN11_EU_DIS_MASK, gr::GEN11_EU_DIS_MASK);
    assert_eq!(sseu::GEN11_GT_SLICE_ENABLE, gr::GEN11_GT_SLICE_ENABLE);
    assert_eq!(sseu::GEN11_GT_S_ENA_MASK, gr::GEN11_GT_S_ENA_MASK);
    assert_eq!(
        sseu::GEN12_GT_GEOMETRY_DSS_ENABLE,
        gr::GEN12_GT_GEOMETRY_DSS_ENABLE
    );
    assert_eq!(
        sseu::GEN11_GT_VEBOX_VDBOX_DISABLE,
        gr::GEN11_GT_VEBOX_VDBOX_DISABLE
    );
    assert_eq!(engine::RENDER_RING_BASE, i915::RENDER_RING_BASE);
    assert_eq!(engine::BLT_RING_BASE, i915::BLT_RING_BASE);
    assert_eq!(engine::GEN11_BSD_RING_BASE, i915::GEN11_BSD_RING_BASE);
    assert_eq!(engine::GEN11_BSD3_RING_BASE, i915::GEN11_BSD3_RING_BASE);
    assert_eq!(engine::GEN11_VEBOX_RING_BASE, i915::GEN11_VEBOX_RING_BASE);
    assert_eq!(
        engine::ring_execlist_status_hi(0x2000),
        Reg(er::ring_execlist_status_lo(0x2000).offset() + 4)
    );
    assert_eq!(gtt::GEN12_GUC_TLB_INV_CR, Reg(0xcee8)); // in uc/intel_guc_reg.h, not vendored
    assert_eq!(
        lrc::CTX_CTRL_ENGINE_CTX_RESTORE_INHIBIT,
        er::CTX_CTRL_ENGINE_CTX_RESTORE_INHIBIT
    );
    assert_eq!(
        lrc::CTX_CTRL_INHIBIT_SYN_CTX_SWITCH,
        er::CTX_CTRL_INHIBIT_SYN_CTX_SWITCH
    );
    assert_eq!(lrc::RING_VALID, er::RING_VALID);
    assert_eq!(lrc::STOP_RING, er::STOP_RING);
    for id in [0, 1, 10, 12, 18] {
        let aux = crate::engine_cs::gen12_get_aux_inv_reg(id).expect("aux");
        let named = match id {
            0 => gr::GEN12_CCS_AUX_INV,
            1 => gr::GEN12_BCS0_AUX_INV,
            10 => gr::GEN12_VD0_AUX_INV,
            12 => gr::GEN12_VD2_AUX_INV,
            _ => gr::GEN12_VE0_AUX_INV,
        };
        assert_eq!(aux, named);
    }
    assert_eq!(crate::engine_cs::AUX_INV, gr::AUX_INV);
}

/// The context image's indices and the status buffer's fields, against
/// `intel_lrc_reg.h`.
#[test]
fn context_indices_match_i915_headers() {
    use crate::execlists;
    use crate::generated::intel_lrc_reg as lr;
    assert_eq!(execlists::GEN11_CSB_PTR_MASK, lr::GEN11_CSB_PTR_MASK);
    assert_eq!(execlists::GEN11_CSB_ENTRIES, lr::GEN11_CSB_ENTRIES);
    assert_eq!(
        execlists::GEN12_MAX_CONTEXT_HW_ID,
        lr::GEN12_MAX_CONTEXT_HW_ID
    );
    let indices = [
        (lrc::ctx::CONTEXT_CONTROL, lr::CTX_CONTEXT_CONTROL),
        (lrc::ctx::RING_HEAD, lr::CTX_RING_HEAD),
        (lrc::ctx::RING_TAIL, lr::CTX_RING_TAIL),
        (lrc::ctx::RING_START, lr::CTX_RING_START),
        (lrc::ctx::RING_CTL, lr::CTX_RING_CTL),
        (lrc::ctx::BB_STATE, lr::CTX_BB_STATE),
        (lrc::ctx::TIMESTAMP, lr::CTX_TIMESTAMP),
        (lrc::ctx::PDP3_UDW, lr::CTX_PDP3_UDW),
        (lrc::ctx::PDP0_LDW, lr::CTX_PDP0_LDW),
        (lrc::ctx::R_PWR_CLK_STATE, lr::CTX_R_PWR_CLK_STATE),
    ];
    for (ours, theirs) in indices {
        assert_eq!(ours as u32, theirs);
    }
    assert_eq!(
        lrc::GEN12_CTX_RCS_INDIRECT_CTX_OFFSET_DEFAULT,
        lr::GEN12_CTX_RCS_INDIRECT_CTX_OFFSET_DEFAULT
    );
}

/// The box's own fuses and frequencies, as i915 reported them on it under
/// Linux 7.0 (M0, 2026-10-18: `gt0/sseu_topology`, `i915_frequency_info`).
#[test]
fn the_boxs_gt() {
    use crate::{rps, sseu};
    // 2 dual-subslices (0x3) of 12 EUs (0x3f3f: pairs 3 and 7 fused off).
    let info = sseu::gen12_sseu_info_init(sseu::Fuses {
        slice_enable: 1,
        geometry_dss_enable: 0x3,
        eu_disable: 0x88,
    });
    assert_eq!(
        (
            info.subslice_mask,
            info.eu_mask,
            info.eu_per_subslice,
            info.eu_total
        ),
        (0x3, 0x3f3f, 12, 24)
    );
    // RP0 1000 MHz, RPn 200 MHz (50 MHz units in RP_STATE_CAP), RPe 300.
    let caps = rps::gen6_rps_get_freq_caps((4 << 16) | 20, 6 << 8);
    assert_eq!((caps.rp0_freq, caps.rp1_freq, caps.min_freq), (60, 18, 12));
    assert_eq!(
        [caps.rp0_freq, caps.rp1_freq, caps.min_freq].map(rps::intel_gpu_freq),
        [1000, 300, 200]
    );
    assert_eq!(rps::gen6_rps_set(60), (Reg(0xa008), 60 << 23));
}

#[test]
fn rc6_programming() {
    use crate::engine::ENGINES;
    use crate::rc6;
    let (writes, ctl) = rc6::gen11_rc6_enable(&ENGINES);
    let writes: Vec<(Reg, u32)> = writes.iter().flatten().copied().collect();
    // 4 thresholds, 5 engines' RING_MAX_IDLE, GuC idle, sleep, threshold,
    // 2 hystereses, PG enable.
    assert_eq!(writes.len(), 4 + 5 + 3 + 2 + 1);
    assert_eq!(writes[4], (Reg(0x2054), 10));
    // Render, media, sampler; VCS0 and VCS2's HCP and MFX gates.
    assert_eq!(
        writes.last(),
        Some(&(Reg(0xa210), 0x7 | (0x3 << 3) | (0x3 << 7)))
    );
    assert_eq!(ctl, (1 << 31) | (1 << 18) | (1 << 27));
}

/// An interrupt controller with one user interrupt pending on RCS0.
struct IrqModel {
    regs: Regs,
    pending: bool,
}

impl Mmio for IrqModel {
    fn read32(&mut self, offset: u32) -> u32 {
        match offset {
            0x19_0010 if self.pending => 1,             // GEN11_GT_DW_IRQ(0)
            0x19_0018 if self.pending => 1,             // bank 0, bit 0
            0x19_0060 if self.pending => (1 << 31) | 1, // valid, class 0, instance 0, USER
            _ => self.regs.read32(offset),
        }
    }

    fn write32(&mut self, offset: u32, value: u32) {
        if offset == 0x19_0018 {
            self.pending = false;
        }
        self.regs.write32(offset, value);
    }
}

#[test]
fn gen11_interrupts() {
    use crate::engine::ENGINES;
    use crate::irq;
    let mut model = IrqModel {
        regs: Regs::default(),
        pending: true,
    };
    let mut clock = FwModel::new();
    let sources = irq::gen11_irq_handler(&mut model, &mut clock).expect("handled");
    let got: Vec<_> = sources.iter().copied().collect();
    assert_eq!(
        got,
        [irq::EngineIrq {
            class: 0,
            instance: 0,
            iir: 1
        }]
    );
    assert_eq!(
        irq::engine_of(&ENGINES, &got[0]).map(|e| e.name),
        Some("rcs0")
    );
    // Master off, selector, identity acknowledged, bank cleared, master on.
    assert_eq!(
        model.regs.writes,
        [
            (0x19_0010, 0),
            (0x19_0070, 1),
            (0x19_0060, 1 << 31),
            (0x19_0018, 1),
            (0x19_0010, 1 << 31)
        ]
    );
    // Nothing pending: IRQ_NONE, master back on.
    assert!(irq::gen11_irq_handler(&mut model, &mut clock).is_none());
    let enable = irq::gen11_gt_irq_postinstall();
    assert_eq!(enable[0], (Reg(0x19_0030), 0x0909_0909));
    assert_eq!(enable[2], (Reg(0x19_0090), !0x0909_0000));
}

#[test]
fn media_fuses() {
    use crate::device_info::{ADL_N_INFO, engine, engine_mask_apply_media_fuses};
    // VDBOX 2 disabled (bit 2), VEBOX 0 present: the box's set.
    let mask = engine_mask_apply_media_fuses(ADL_N_INFO.platform_engine_mask, 1 << 2);
    assert_eq!(
        mask,
        engine::bit(engine::RCS0)
            | engine::bit(engine::BCS0)
            | engine::bit(engine::VCS0)
            | engine::bit(engine::VECS0)
    );
    assert_eq!(
        engine_mask_apply_media_fuses(ADL_N_INFO.platform_engine_mask, 0),
        ADL_N_INFO.platform_engine_mask
    );
}

#[test]
fn wopcm_lock() {
    let mut regs = Regs::default();
    assert!(!crate::uc::uc_is_wopcm_locked(&mut regs));
    let _ = regs.values.insert(0xc340, 1);
    assert!(crate::uc::uc_is_wopcm_locked(&mut regs));
    let mut regs = Regs::default();
    let _ = regs.values.insert(0xc050, 0x3f_0001);
    assert!(crate::uc::uc_is_wopcm_locked(&mut regs));
}

#[test]
fn cs_timestamp_frequency() {
    use crate::clock::gen11_clock_frequency;
    // Crystal 38.4 MHz, shift parameter 3: no division.
    assert_eq!(gen11_clock_frequency(0, 0, 0x10 | (3 << 1)), 38_400_000);
    // 19.2 MHz at shift 1: divided by 4.
    assert_eq!(gen11_clock_frequency(0, 0, 0x08 | (1 << 1)), 4_800_000);
    // Divide logic: override 0x00 base 1 MHz, denominator 0: + 1 MHz.
    assert_eq!(
        gen11_clock_frequency(1, 0x0000_0012, 0),
        19_000_000 + 1_000_000
    );
}

#[test]
fn gt_and_engine_bring_up() {
    use crate::engine::ENGINES;
    use crate::{gt, workarounds};
    let mut regs = Regs::default();
    gt::setup_private_pat(&mut regs);
    let wal = workarounds::gt_init_workarounds(0b11, &ENGINES[..4]);
    let report = gt::intel_gt_init_hw(&mut regs, &wal).expect("unlocked");
    assert_eq!(report.lost_workarounds, 0);
    // The PAT, then the GT list, then 64 + 32 MOCS writes. On registers that
    // read 0 the steering to subslice 0 and MISCCPCTL's clear change
    // nothing, so wa_list_apply's read-modify-write skips them, as i915's
    // does: DFR_DISABLE and the two VDBOX clock gates remain.
    assert_eq!(regs.writes[0], (0x4800, 3));
    assert_eq!(
        regs.writes[8..11],
        [(0x9550, 1 << 9), (0x1c_3f10, 1 << 22), (0x1d_3f10, 1 << 22)]
    );
    assert_eq!(regs.writes.len(), 8 + 3 + 96);
    // A locked WOPCM stops it before the MOCS.
    let mut locked = Regs::default();
    let _ = locked.values.insert(0xc050, 1);
    assert_eq!(
        gt::intel_gt_init_hw(&mut locked, &wal),
        Err(gt::InitError::WopcmLocked)
    );

    let rcs = &ENGINES[0];
    let mut regs = Regs::default();
    let report = gt::intel_engine_resume(
        &mut regs,
        rcs.mmio_base,
        &workarounds::engine_workarounds(rcs, mocs::UC_INDEX),
        &workarounds::whitelist(rcs),
        0x1000,
    );
    assert_eq!(report.still_in_error, 0);
    let tail: Vec<(u32, u32)> = regs.writes.iter().rev().take(7).rev().copied().collect();
    assert_eq!(
        tail,
        [
            (0x2098, !0),
            (0x229c, masked_bit_enable(1 << 3)),
            (0x209c, masked_bit_disable(1 << 8)),
            (0x2080, 0x1000),
            (0x20b4, !0),
            (0x20b0, !0),
            (0x20b4, !1),
        ]
    );
}

#[test]
fn ring_space_and_wrap() {
    use crate::ring::{NoSpace, Reserved, Ring};
    let mut ring = Ring::new(4096);
    assert_eq!(ring.space, 4096 - 64);
    assert_eq!(
        ring.begin(6, 0),
        Ok(Reserved {
            at: 0,
            wrap_from: None
        })
    );
    assert_eq!(ring.begin(3, 0), Err(NoSpace)); // odd
    // Fill to near the end, the engine reading behind.
    let mut ring = Ring::new(4096);
    ring.emit = 4096 - 16;
    ring.update_head(4096 - 16);
    // 8 dwords do not fit in the last 16 bytes: they wrap, with padding.
    assert_eq!(
        ring.begin(8, 0),
        Ok(Reserved {
            at: 0,
            wrap_from: Some(4096 - 16)
        })
    );
    assert_eq!(ring.tail(), 32);
    // The engine at 0 has read nothing new: no room for a page more.
    assert_eq!(ring.begin(1024, 0), Err(NoSpace));
}

/// A command streamer good enough to run what the driver emits: it walks a
/// ring from head to tail, follows `MI_BATCH_BUFFER_START` into a batch, and
/// performs the stores (`MI_STORE_DWORD_IMM`, a `PIPE_CONTROL` with a
/// post-sync write, `MI_FLUSH_DW` with a store), counting user interrupts.
/// Anything else is skipped by its length.
struct ModelCs {
    ggtt: BTreeMap<u32, u32>,
    ppgtt: BTreeMap<u64, u32>,
    interrupts: u32,
}

impl ModelCs {
    fn run(
        &mut self,
        dwords: &[u32],
        mut at: usize,
        end: usize,
        batches: &BTreeMap<u64, Vec<u32>>,
    ) {
        use crate::commands::gpu;
        while at < end {
            let dw0 = dwords[at];
            let client = dw0 >> 29;
            let opcode = (dw0 >> 23) & 0x3f;
            let len = match (client, opcode) {
                (0, 0x00 | 0x02 | 0x05 | 0x08 | 0x0a) => 1, // NOOP, USER_INTERRUPT, ARB_CHECK, ARB_ON_OFF, BB_END
                (0, _) => (dw0 & 0xff) as usize + 2,
                (3, _) => (dw0 & 0xff) as usize + 2, // PIPE_CONTROL
                _ => panic!("unknown command {dw0:#x} at {at}"),
            };
            let cmd = &dwords[at..at + len];
            match (client, opcode) {
                (0, 0x02) => self.interrupts += 1,
                (0, 0x0a) => return,
                (0, 0x20) if dw0 & gpu::MI_USE_GGTT != 0 => {
                    let _ = self.ggtt.insert(cmd[1], cmd[3]);
                }
                (0, 0x20) => {
                    let _ = self
                        .ppgtt
                        .insert(u64::from(cmd[1]) | u64::from(cmd[2]) << 32, cmd[3]);
                }
                (0, 0x26)
                    if dw0 & gpu::MI_FLUSH_DW_OP_STOREDW != 0
                        && cmd[1] & gpu::MI_FLUSH_DW_USE_GTT != 0 =>
                {
                    let _ = self.ggtt.insert(cmd[1] & !7, cmd[3]);
                }
                (0, 0x31) => {
                    let address = u64::from(cmd[1]) | u64::from(cmd[2]) << 32;
                    let batch = &batches[&address];
                    self.run(batch, 0, batch.len(), batches);
                }
                (3, _)
                    if cmd[1] & gpu::PIPE_CONTROL_QW_WRITE != 0
                        && cmd[1] & gpu::PIPE_CONTROL_GLOBAL_GTT_IVB != 0 =>
                {
                    let _ = self.ggtt.insert(cmd[2], cmd[4]);
                }
                _ => {}
            }
            at += len;
        }
    }
}

#[test]
fn a_request_runs_on_a_model_engine() {
    use crate::commands::{Cs, emit_store_dword};
    use crate::engine::ENGINES;
    use crate::{request, ring::Ring};
    let mut batches = BTreeMap::new();
    let mut batch = std::vec![0_u32; 6];
    let mut cs = Cs::new(&mut batch);
    emit_store_dword(&mut cs, 0x1_0000_1000, 0xc0ff_ee00);
    batch[4] = lrc::MI_BATCH_BUFFER_END;
    let _ = batches.insert(0x1_0000_0000, batch);
    for engine in &ENGINES[..2] {
        let mut ring = Ring::new(16 << 10);
        let mut memory = std::vec![0_u32; 4096];
        let built = request::build(&mut ring, &mut memory, engine, 0x1_0000_0000, 5, 0x40, 0xc8)
            .expect("room");
        let mut model = ModelCs {
            ggtt: BTreeMap::new(),
            ppgtt: BTreeMap::new(),
            interrupts: 0,
        };
        model.run(
            &memory,
            (built.head / 4) as usize,
            (built.wa_tail / 4) as usize,
            &batches,
        );
        // The batch's store, the seqno (4 then 5), one interrupt.
        assert_eq!(
            model.ppgtt.get(&0x1_0000_1000),
            Some(&0xc0ff_ee00),
            "{}",
            engine.name
        );
        assert_eq!(model.ggtt.get(&0x40), Some(&5), "{}", engine.name);
        assert_eq!(model.interrupts, 1, "{}", engine.name);
        assert_eq!(built.wa_tail - built.tail, 8);
        // A second request follows the first.
        let next = request::build(&mut ring, &mut memory, engine, 0x1_0000_0000, 6, 0x40, 0xc8)
            .expect("room");
        assert_eq!(next.head, built.wa_tail);
    }
}

#[test]
fn ggtt_space() {
    use crate::ggtt_space::{GgttSpace, Range, SpaceError};
    // Firmware entries up to 0x1d4 (a 1920000-byte framebuffer at 0).
    let mut space = GgttSpace::above_firmware(Some(0x1d4), 4 << 30);
    let ring = space.alloc(16 << 10, 4096).expect("ring");
    assert_eq!(
        ring,
        Range {
            start: 0x1d5 * 4096,
            len: 16 << 10
        }
    );
    let ctx = space.alloc(88 << 10, 64 << 10).expect("context");
    assert_eq!(ctx.start % (64 << 10), 0);
    assert!(ctx.start >= ring.start + ring.len);
    assert!(space.free(ring));
    assert!(!space.free(ring));
    // The freed gap is reused first.
    assert_eq!(space.alloc(4096, 4096).expect("page").start, 0x1d5 * 4096);
    assert_eq!(space.alloc(0, 4096), Err(SpaceError::Invalid));
    assert_eq!(space.alloc(4096, 3000), Err(SpaceError::Invalid));
    let mut small = GgttSpace::new(0, 8192);
    let _ = small.alloc(8192, 4096).expect("all");
    assert_eq!(small.alloc(4096, 4096), Err(SpaceError::Full));
    assert_eq!(small.used_bytes(), 8192);
}
