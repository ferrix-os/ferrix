// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_workarounds.c,
// gt/intel_workarounds_types.h, gt/intel_gt_regs.h and gt/intel_engine_regs.h:
// Copyright © 2014-2018 Intel Corporation (intel_workarounds.c)
// Copyright © 2019 Intel Corporation (intel_workarounds_types.h)
// Copyright © 2022 Intel Corporation (intel_gt_regs.h, intel_engine_regs.h)
// Under the MIT notice in this crate's i915/NOTICE.

//! i915's workaround lists for ADL-P and ADL-N (Gen12.0), and the framework
//! that builds, applies and verifies them.
//!
//! i915 keeps four lists. The GT list is written once over MMIO at
//! initialisation (`intel_gt_apply_workarounds`). Each engine has its own
//! list, written when the engine starts (`intel_engine_apply_workarounds`).
//! The context list is emitted as one `MI_LOAD_REGISTER_IMM` into the first
//! request on each engine, so the recorded default context image carries it
//! (`intel_engine_emit_ctx_wa`). The whitelist names the registers user
//! batches may touch, written into `RING_FORCE_TO_NONPRIV`
//! (`intel_engine_apply_whitelist`).
//!
//! Each builder below is the i915 function of the same name with every
//! branch for another platform dropped. Each entry keeps i915's
//! workaround numbers, and the condition that admits ADL-P/N is named where
//! i915 tests one. The lists are built through `_wa_add` exactly as i915
//! builds them: sorted by offset, entries for the same register merged.
//!
//! MCR (multicast/replicated) registers: below 12.55, a write is multicast
//! by default and a read goes to the instance `GEN8_MCR_SELECTOR` steers to,
//! which the GT list sets first (`icl_wa_init_mcr`). So an MCR entry is
//! written and read as any other here, and only marked.

use crate::engine::{
    COPY_ENGINE_CLASS, EngineInfo, RENDER_CLASS, RENDER_RING_BASE, VIDEO_DECODE_CLASS,
};
use crate::io::Mmio;
use crate::lrc::mi_load_register_imm;
use crate::reg::{Reg, field_prep, genmask, masked_bit_disable, masked_bit_enable, masked_field};

/// `struct i915_wa`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Wa {
    /// `reg` (or `mcr_reg`).
    pub reg: Reg,
    /// `clr`: the bits a read-modify-write clears; 0 for a plain write.
    pub clr: u32,
    /// `set`.
    pub set: u32,
    /// `read`: the bits `wa_verify` checks.
    pub read: u32,
    /// `masked_reg`.
    pub masked_reg: bool,
    /// `is_mcr`.
    pub is_mcr: bool,
}

/// The most entries a list here holds: more than any ADL-P list has, so a
/// list that fills it is a translation error, which [`WaList::overflowed`]
/// reports.
pub const WA_LIST_MAX: usize = 32;

/// `struct i915_wa_list`, in a fixed array.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WaList {
    list: [Option<Wa>; WA_LIST_MAX],
    count: usize,
    /// `wa_count`: entries added, merged ones included.
    pub wa_count: u32,
    /// Entries `_wa_add` discarded as overwritten ("Discarding overwritten
    /// w/a for reg"), which i915 logs as an error.
    pub discarded: u32,
    overflowed: bool,
}

impl Default for WaList {
    fn default() -> Self {
        Self::new()
    }
}

impl WaList {
    /// An empty list (`wa_init_start`).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            list: [None; WA_LIST_MAX],
            count: 0,
            wa_count: 0,
            discarded: 0,
            overflowed: false,
        }
    }

    /// The entries, in offset order.
    pub fn iter(&self) -> impl Iterator<Item = &Wa> {
        self.list.iter().take(self.count).flatten()
    }

    /// `wal->count`.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.count
    }

    /// Whether the list has no entries.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Whether an add found the list full.
    #[must_use]
    pub const fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// `_wa_add`: insert in offset order, or merge into the entry for the
    /// same register.
    fn add(&mut self, wa: Wa) {
        let offset = wa.reg.offset();
        let slots = self.list.get_mut(..self.count).unwrap_or(&mut []);
        if let Some(existing) = slots
            .iter_mut()
            .flatten()
            .find(|e| e.reg.offset() == offset)
        {
            if (wa.clr | existing.clr) != 0 && (wa.clr & !existing.clr) == 0 {
                self.discarded += 1;
                existing.set &= !wa.clr;
            }
            self.wa_count += 1;
            existing.set |= wa.set;
            existing.clr |= wa.clr;
            existing.read |= wa.read;
            return;
        }
        if self.count >= WA_LIST_MAX {
            self.overflowed = true;
            return;
        }
        let at = slots
            .iter()
            .flatten()
            .take_while(|e| e.reg.offset() < offset)
            .count();
        let tail = self.list.get_mut(at..=self.count).unwrap_or(&mut []);
        tail.rotate_right(1);
        if let Some(slot) = tail.first_mut() {
            *slot = Some(wa);
        }
        self.count += 1;
        self.wa_count += 1;
    }

    /// `wa_add`.
    fn wa_add(&mut self, reg: Reg, clear: u32, set: u32, read_mask: u32, masked_reg: bool) {
        self.add(Wa {
            reg,
            clr: clear,
            set,
            read: read_mask,
            masked_reg,
            is_mcr: false,
        });
    }

    /// `wa_mcr_add`.
    fn wa_mcr_add(&mut self, reg: Reg, clear: u32, set: u32, read_mask: u32, masked_reg: bool) {
        self.add(Wa {
            reg,
            clr: clear,
            set,
            read: read_mask,
            masked_reg,
            is_mcr: true,
        });
    }

    /// `wa_write_clr_set`.
    fn write_clr_set(&mut self, reg: Reg, clear: u32, set: u32) {
        self.wa_add(reg, clear, set, clear | set, false);
    }

    /// `wa_mcr_write_clr_set`.
    fn mcr_write_clr_set(&mut self, reg: Reg, clear: u32, set: u32) {
        self.wa_mcr_add(reg, clear, set, clear | set, false);
    }

    /// `wa_write_or`.
    fn write_or(&mut self, reg: Reg, set: u32) {
        self.write_clr_set(reg, set, set);
    }

    /// `wa_mcr_write_or`.
    fn mcr_write_or(&mut self, reg: Reg, set: u32) {
        self.mcr_write_clr_set(reg, set, set);
    }

    /// `wa_write_clr`.
    fn write_clr(&mut self, reg: Reg, clr: u32) {
        self.write_clr_set(reg, clr, 0);
    }

    /// `wa_masked_en`.
    fn masked_en(&mut self, reg: Reg, val: u32) {
        self.wa_add(reg, 0, masked_bit_enable(val), val, true);
    }

    /// `wa_mcr_masked_en`.
    fn mcr_masked_en(&mut self, reg: Reg, val: u32) {
        self.wa_mcr_add(reg, 0, masked_bit_enable(val), val, true);
    }

    /// `wa_masked_dis`.
    #[expect(
        dead_code,
        reason = "i915's helper, kept for the lists that will use it"
    )]
    fn masked_dis(&mut self, reg: Reg, val: u32) {
        self.wa_add(reg, 0, masked_bit_disable(val), val, true);
    }

    /// `wa_masked_field_set`.
    fn masked_field_set(&mut self, reg: Reg, mask: u32, val: u32) {
        self.wa_add(reg, 0, masked_field(mask, val), mask, true);
    }

    /// `whitelist_reg_ext`: a register (with its access and range flags in
    /// the low and high bits of the offset) user batches may reach.
    fn whitelist_reg_ext(&mut self, reg: Reg, flags: u32) {
        if self.count >= RING_MAX_NONPRIV_SLOTS {
            self.overflowed = true;
            return;
        }
        self.add(Wa {
            reg: Reg(reg.offset() | flags),
            clr: 0,
            set: 0,
            read: 0,
            masked_reg: false,
            is_mcr: false,
        });
    }

    /// `whitelist_reg`: read and write, one register.
    fn whitelist_reg(&mut self, reg: Reg) {
        self.whitelist_reg_ext(reg, RING_FORCE_TO_NONPRIV_ACCESS_RW);
    }
}

/// The registers and fields the lists below name: i915's own definitions,
/// generated from `intel_gt_regs.h`, `intel_engine_regs.h` and `i915_reg.h`,
/// and the few function-like macros the generator does not translate.
pub mod regs {
    pub use crate::generated::i915_reg::{
        GEN7_FF_THREAD_MODE, GEN12_FF_TESSELATION_DOP_GATE_DISABLE,
    };
    pub use crate::generated::intel_engine_regs::{
        BLIT_CCTL_DST_MOCS_MASK, BLIT_CCTL_MASK, BLIT_CCTL_SRC_MOCS_MASK, CMD_CCTL_MOCS_MASK,
        CMD_CCTL_READ_OVERRIDE_MASK, CMD_CCTL_WRITE_OVERRIDE_MASK, GEN8_RC_SEMA_IDLE_MSG_DISABLE,
        GEN12_WAIT_FOR_EVENT_POWER_DOWN_DISABLE, IECPUNIT_CLKGATE_DIS, blit_cctl, ring_cmd_cctl,
        ring_ctx_timestamp, ring_force_to_nonpriv, ring_nopid, ring_psmi_ctl, vdbox_cgctl3f10,
    };
    pub use crate::generated::intel_gt_regs::{
        COMMON_SLICE_CHICKEN4, DFR_DISABLE, DISABLE_TDC_LOAD_BALANCING_CALC, ENABLE_SMALLPL,
        FF_DOP_CLOCK_GATE_DISABLE, FF_MODE2_GS_TIMER_224, FF_MODE2_GS_TIMER_MASK,
        FF_MODE2_TDS_TIMER_128, FF_MODE2_TDS_TIMER_MASK, GEN7_COMMON_SLICE_CHICKEN1,
        GEN7_FF_SLICE_CS_CHICKEN1, GEN7_MISCCPCTL, GEN8_CS_CHICKEN1, GEN8_GARBCNTL,
        GEN8_MCR_SELECTOR, GEN8_ROW_CHICKEN2, GEN8_WM_CHICKEN2, GEN9_CS_DEBUG_MODE1,
        GEN9_FFSC_PERCTX_PREEMPT_CTRL, GEN9_PREEMPT_GPGPU_LEVEL_MASK,
        GEN9_PREEMPT_GPGPU_THREAD_GROUP_LEVEL, GEN9_ROW_CHICKEN4, GEN10_DFR_RATIO_EN_AND_CHICKEN,
        GEN10_SAMPLER_MODE, GEN11_COMMON_SLICE_CHICKEN3, GEN11_INDIRECT_STATE_BASE_ADDR_OVERRIDE,
        GEN11_MCR_SLICE_MASK, GEN11_MCR_SUBSLICE_MASK, GEN12_BUS_HASH_CTL_BIT_EXC,
        GEN12_DISABLE_CPS_AWARE_COLOR_PIPE, GEN12_DISABLE_EARLY_READ, GEN12_DISABLE_TDL_PUSH,
        GEN12_DOP_CLOCK_GATE_RENDER_ENABLE, GEN12_FF_MODE2, GEN12_PUSH_CONST_DEREF_HOLD_DIS,
        HIZ_CHICKEN, HZ_DEPTH_TEST_LE_GE_OPT_DISABLE, PS_INVOCATION_COUNT,
        WAIT_ON_DEPTH_STALL_DONE_DISABLE,
    };

    /// `GEN11_MCR_SLICE(slice)`.
    #[must_use]
    pub const fn gen11_mcr_slice(slice: u32) -> u32 {
        (slice & 0xf) << 27
    }
    /// `GEN11_MCR_SUBSLICE(subslice)`.
    #[must_use]
    pub const fn gen11_mcr_subslice(subslice: u32) -> u32 {
        (subslice & 0x7) << 24
    }
    /// `GEN9_PREEMPT_GPGPU_LEVEL(hi, lo)`.
    #[must_use]
    pub const fn gen9_preempt_gpgpu_level(hi: u32, lo: u32) -> u32 {
        (hi << 2) | (lo << 1)
    }
}

use regs::*;

/// `RING_MAX_NONPRIV_SLOTS`.
pub const RING_MAX_NONPRIV_SLOTS: usize = 12;
/// `RING_FORCE_TO_NONPRIV_ACCESS_RW`.
pub const RING_FORCE_TO_NONPRIV_ACCESS_RW: u32 = 0;
/// `RING_FORCE_TO_NONPRIV_ACCESS_RD`.
pub const RING_FORCE_TO_NONPRIV_ACCESS_RD: u32 = 1 << 28;
/// `RING_FORCE_TO_NONPRIV_RANGE_4`.
pub const RING_FORCE_TO_NONPRIV_RANGE_4: u32 = 1 << 0;
/// `RING_FORCE_TO_NONPRIV_ADDRESS_MASK`.
pub const RING_FORCE_TO_NONPRIV_ADDRESS_MASK: u32 = genmask(25, 2);

/// `BLIT_CCTL_MOCS(dst, src)`.
#[must_use]
pub const fn blit_cctl_mocs(dst: u32, src: u32) -> u32 {
    field_prep(BLIT_CCTL_DST_MOCS_MASK, dst << 1) | field_prep(BLIT_CCTL_SRC_MOCS_MASK, src << 1)
}

/// `CMD_CCTL_MOCS_OVERRIDE(write, read)`.
#[must_use]
pub const fn cmd_cctl_mocs_override(write: u32, read: u32) -> u32 {
    field_prep(CMD_CCTL_WRITE_OVERRIDE_MASK, write << 1)
        | field_prep(CMD_CCTL_READ_OVERRIDE_MASK, read << 1)
}

/// `gt_init_workarounds` for ADL-P/N: `gt_tuning_settings` (nothing below
/// 12.70 or DG2), then `gen12_gt_workarounds_init` (`GRAPHICS_VER == 12`).
/// `subslice_mask` is the fused-on dual-subslices (`intel_sseu_get_hsw_subslices`);
/// `engines` the engines present.
#[must_use]
pub fn gt_init_workarounds(subslice_mask: u8, engines: &[EngineInfo]) -> WaList {
    let mut wal = WaList::new();
    // icl_wa_init_mcr: steer reads to the lowest subslice that is not fused
    // off, "if we steer register reads to one of the higher subslices, we
    // run the risk of reading back 0's or random garbage".
    let subslice = subslice_mask.trailing_zeros().min(7);
    // __add_mcr_wa -> __set_mcr_steering(wal, GEN8_MCR_SELECTOR, 0, subslice)
    wal.write_clr_set(
        GEN8_MCR_SELECTOR,
        GEN11_MCR_SLICE_MASK | GEN11_MCR_SUBSLICE_MASK,
        gen11_mcr_slice(0) | gen11_mcr_subslice(subslice),
    );
    // Wa_14011060649:tgl,rkl,dg1,adl-s,adl-p
    for engine in engines {
        if engine.class != VIDEO_DECODE_CLASS || engine.instance % 2 != 0 {
            continue;
        }
        wal.write_or(vdbox_cgctl3f10(engine.mmio_base), IECPUNIT_CLKGATE_DIS);
    }
    // Wa_14011059788:tgl,rkl,adl-s,dg1,adl-p
    wal.mcr_write_or(GEN10_DFR_RATIO_EN_AND_CHICKEN, DFR_DISABLE);
    // Wa_14015795083: firmware may lock MISCCPCTL, so no read-back.
    wal.wa_add(
        GEN7_MISCCPCTL,
        GEN12_DOP_CLOCK_GATE_RENDER_ENABLE,
        0,
        0,
        false,
    );
    wal
}

/// `__intel_engine_init_ctx_wa` for ADL-P/N: `gen12_ctx_gt_fake_wa_init`
/// for every engine (only `gen12_ctx_gt_mocs_init` below 12.55), then, on
/// the render engine, `gen12_ctx_workarounds_init`.
#[must_use]
pub fn ctx_workarounds(engine: &EngineInfo, mocs_uc_index: u32) -> WaList {
    let mut wal = WaList::new();
    // gen12_ctx_gt_mocs_init: BLIT_CCTL to uncached.
    if engine.class == COPY_ENGINE_CLASS {
        wal.write_clr_set(
            blit_cctl(engine.mmio_base),
            BLIT_CCTL_MASK,
            blit_cctl_mocs(mocs_uc_index, mocs_uc_index),
        );
    }
    if engine.class != RENDER_CLASS {
        return wal;
    }
    gen12_ctx_workarounds_init(&mut wal);
    wal
}

/// `gen12_ctx_workarounds_init`, for a part that is not DG1.
fn gen12_ctx_workarounds_init(wal: &mut WaList) {
    // Wa_1409142259:tgl,dg1,adl-p,adl-n, Wa_1409347922:tgl,dg1,adl-p,
    // Wa_1409252684:tgl,dg1,adl-p, Wa_1409217633:tgl,dg1,adl-p,
    // Wa_1409207793:tgl,dg1,adl-p, Wa_1409178076:tgl,dg1,adl-p,adl-n,
    // Wa_1408979724:tgl,dg1,adl-p,adl-n, Wa_14010443199:tgl,rkl,dg1,adl-p,adl-n,
    // Wa_14010698770:tgl,rkl,dg1,adl-s,adl-p,adl-n,
    // Wa_1409342910:tgl,rkl,dg1,adl-s,adl-p,adl-n,
    // Wa_22010465259:tgl,rkl,dg1,adl-s,adl-p,adl-n
    wal.masked_en(
        GEN11_COMMON_SLICE_CHICKEN3,
        GEN12_DISABLE_CPS_AWARE_COLOR_PIPE,
    );
    // WaDisableGPGPUMidThreadPreemption:gen12
    wal.masked_field_set(
        GEN8_CS_CHICKEN1,
        GEN9_PREEMPT_GPGPU_LEVEL_MASK,
        GEN9_PREEMPT_GPGPU_THREAD_GROUP_LEVEL,
    );
    // Wa_16011163337 GS_TIMER, TDS_TIMER (Wa_1604555607), per Wa_1608008084:
    // the read-back cannot be trusted, so write the whole register.
    wal.wa_add(
        GEN12_FF_MODE2,
        !0,
        FF_MODE2_TDS_TIMER_128 | FF_MODE2_GS_TIMER_224,
        0,
        false,
    );
    // !IS_DG1: Wa_1806527549, Wa_1606376872
    wal.masked_en(HIZ_CHICKEN, HZ_DEPTH_TEST_LE_GE_OPT_DISABLE);
    wal.masked_en(COMMON_SLICE_CHICKEN4, DISABLE_TDC_LOAD_BALANCING_CALC);
    // "This bit must be set to enable performance optimization for fast clears."
    wal.mcr_write_or(GEN8_WM_CHICKEN2, WAIT_ON_DEPTH_STALL_DONE_DISABLE);
}

/// `engine_init_workarounds` for ADL-P/N: `engine_fake_wa_init` for every
/// engine; `general_render_compute_wa_init` on the first render engine; and
/// `rcs_engine_wa_init` (render) or `xcs_engine_wa_init` (others, which has
/// nothing for Gen12.0).
#[must_use]
pub fn engine_workarounds(engine: &EngineInfo, mocs_uc_index: u32) -> WaList {
    let mut wal = WaList::new();
    // engine_fake_wa_init: RING_CMD_CCTL to the uncached MOCS entry.
    wal.masked_field_set(
        ring_cmd_cctl(engine.mmio_base),
        CMD_CCTL_MOCS_MASK,
        cmd_cctl_mocs_override(mocs_uc_index, mocs_uc_index),
    );
    if engine.class == RENDER_CLASS {
        general_render_compute_wa_init(&mut wal);
        rcs_engine_wa_init(&mut wal);
    }
    wal
}

/// `general_render_compute_wa_init` (with
/// `add_render_compute_tuning_settings`) for ADL-P/N.
fn general_render_compute_wa_init(wal: &mut WaList) {
    // add_render_compute_tuning_settings: GRAPHICS_VER == 12 && < 12.55
    wal.write_clr(GEN8_GARBCNTL, GEN12_BUS_HASH_CTL_BIT_EXC);
    // GRAPHICS_VER >= 11: WaSetInidrectStateOverride (not a Wa), BSpec 46052
    wal.mcr_masked_en(GEN10_SAMPLER_MODE, GEN11_INDIRECT_STATE_BASE_ADDR_OVERRIDE);
}

/// `rcs_engine_wa_init` for ADL-P/N.
fn rcs_engine_wa_init(wal: &mut WaList) {
    // IS_ALDERLAKE_P: Wa_1606700617:tgl,dg1,adl-p,
    // Wa_22010271021:tgl,rkl,dg1,adl-s,adl-p, Wa_14010826681:tgl,dg1,rkl,adl-p
    wal.masked_en(GEN9_CS_DEBUG_MODE1, FF_DOP_CLOCK_GATE_DISABLE);
    // IS_ALDERLAKE_P: Wa_1606931601:tgl,rkl,dg1,adl-s,adl-p
    wal.mcr_masked_en(GEN8_ROW_CHICKEN2, GEN12_DISABLE_EARLY_READ);
    // Wa_1407928979, Wa_18011464164, Wa_22010931296, Wa_14010919138:rkl,dg1,adl-s,adl-p
    wal.write_or(GEN7_FF_THREAD_MODE, GEN12_FF_TESSELATION_DOP_GATE_DISABLE);
    // Wa_1406941453:tgl,rkl,dg1,adl-s,adl-p
    wal.mcr_masked_en(GEN10_SAMPLER_MODE, ENABLE_SMALLPL);
    // IS_ALDERLAKE_P: Wa_1409804808
    wal.mcr_masked_en(GEN8_ROW_CHICKEN2, GEN12_PUSH_CONST_DEREF_HOLD_DIS);
    // Wa_14010229206
    wal.mcr_masked_en(GEN9_ROW_CHICKEN4, GEN12_DISABLE_TDL_PUSH);
    // IS_ALDERLAKE_P: Wa_1607297627
    wal.masked_en(
        ring_psmi_ctl(RENDER_RING_BASE),
        GEN12_WAIT_FOR_EVENT_POWER_DOWN_DISABLE | GEN8_RC_SEMA_IDLE_MSG_DISABLE,
    );
    // GRAPHICS_VER >= 9: preemption settings per context (Option 2), so
    // userspace can apply Wa_16013994831 and the like itself.
    wal.masked_en(GEN7_FF_SLICE_CS_CHICKEN1, GEN9_FFSC_PERCTX_PREEMPT_CTRL);
}

/// `intel_engine_init_whitelist` -> `tgl_whitelist_build` (`GRAPHICS_VER ==
/// 12`), with `allow_read_ctx_timestamp`.
#[must_use]
pub fn whitelist(engine: &EngineInfo) -> WaList {
    let mut w = WaList::new();
    // allow_read_ctx_timestamp
    if engine.class != RENDER_CLASS {
        w.whitelist_reg_ext(
            ring_ctx_timestamp(engine.mmio_base),
            RING_FORCE_TO_NONPRIV_ACCESS_RD,
        );
    }
    if engine.class == RENDER_CLASS {
        // WaAllowPMDepthAndInvocationCountAccessFromUMD:tgl, Wa_1408556865:tgl:
        // PS_INVOCATION_COUNT and the three registers after it.
        w.whitelist_reg_ext(
            PS_INVOCATION_COUNT,
            RING_FORCE_TO_NONPRIV_ACCESS_RD | RING_FORCE_TO_NONPRIV_RANGE_4,
        );
        // Wa_1808121037:tgl, Wa_14012131227:dg1, Wa_1508744258:tgl,rkl,dg1,adl-s,adl-p
        w.whitelist_reg(GEN7_COMMON_SLICE_CHICKEN1);
        // Wa_1806527549:tgl
        w.whitelist_reg(HIZ_CHICKEN);
        // Required by recommended tuning setting (not a workaround)
        w.whitelist_reg(GEN11_COMMON_SLICE_CHICKEN3);
    }
    w
}

/// `wa_list_apply`: read-modify-write each entry (a read only when it
/// clears bits), writing only what changes, as i915's open-coded RMW does.
/// The caller holds forcewake for every entry (`wal_get_fw_for_rmw`).
pub fn wa_list_apply<M: Mmio + ?Sized>(mmio: &mut M, wal: &WaList) {
    for wa in wal.iter() {
        let old = if wa.clr != 0 {
            mmio.read32(wa.reg.offset())
        } else {
            0
        };
        let val = (old & !wa.clr) | wa.set;
        if val != old || wa.clr == 0 {
            mmio.write32(wa.reg.offset(), val);
        }
    }
}

/// `wa_list_verify` with `wa_verify`: the entries whose bits did not stick,
/// as (register, value read). i915 logs each as "workaround lost".
pub fn wa_list_verify<'a, M: Mmio + ?Sized>(
    mmio: &'a mut M,
    wal: &'a WaList,
) -> impl Iterator<Item = (Reg, u32)> + 'a {
    wal.iter().filter_map(move |wa| {
        let cur = mmio.read32(wa.reg.offset());
        ((cur ^ wa.set) & wa.read != 0).then_some((wa.reg, cur))
    })
}

/// `intel_engine_emit_ctx_wa`'s `MI_LOAD_REGISTER_IMM`: the dwords, into
/// `out`, between the two `EMIT_BARRIER` flushes the caller emits. A masked
/// or fully written entry takes its `set`; any other reads the register
/// (under forcewake) and applies `clr` and `set`. Returns the dwords used,
/// or `None` if `out` is short.
pub fn emit_ctx_wa<M: Mmio + ?Sized>(mmio: &mut M, wal: &WaList, out: &mut [u32]) -> Option<usize> {
    if wal.is_empty() {
        return Some(0);
    }
    let needed = wal.len() * 2 + 2;
    let out = out.get_mut(..needed)?;
    let mut at = 0;
    let mut push = |dword: u32| {
        if let Some(slot) = out.get_mut(at) {
            *slot = dword;
        }
        at += 1;
    };
    push(mi_load_register_imm(wal.len() as u32));
    for wa in wal.iter() {
        let val = if wa.masked_reg || (wa.clr | wa.set) == u32::MAX {
            wa.set
        } else {
            (mmio.read32(wa.reg.offset()) & !wa.clr) | wa.set
        };
        push(wa.reg.offset());
        push(val);
    }
    push(0); // MI_NOOP
    Some(needed)
}

/// `intel_engine_apply_whitelist`: the `RING_FORCE_TO_NONPRIV` writes, the
/// unused slots pointed at `RING_NOPID` "just in case of garbage".
#[must_use]
pub fn apply_whitelist(base: u32, w: &WaList) -> [(Reg, u32); RING_MAX_NONPRIV_SLOTS] {
    let mut writes = [(Reg(0), ring_nopid(base).offset()); RING_MAX_NONPRIV_SLOTS];
    for (i, slot) in writes.iter_mut().enumerate() {
        slot.0 = ring_force_to_nonpriv(base, i as u32);
    }
    for (slot, wa) in writes.iter_mut().zip(w.iter()) {
        slot.1 = wa.reg.offset();
    }
    writes
}
