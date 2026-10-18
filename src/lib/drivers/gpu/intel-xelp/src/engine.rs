// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_engine_regs.h,
// gt/intel_engine_types.h, gt/intel_engine.h and i915_reg.h:
// Copyright © 2022 Intel Corporation (intel_engine_regs.h)
// Copyright © 2019 Intel Corporation (intel_engine_types.h, intel_engine.h)
// Copyright 2003 Tungsten Graphics, Inc., Cedar Park, Texas. (i915_reg.h)
// Under the MIT notice in this crate's i915/NOTICE.

//! The engines ADL-P/N has, where their registers are, and the per-engine
//! registers this crate touches, by i915's names (`RING_*(base)`).

use crate::device_info::engine as id;
use crate::reg::Reg;

/// `RENDER_CLASS`.
pub const RENDER_CLASS: u8 = 0;
/// `VIDEO_DECODE_CLASS`.
pub const VIDEO_DECODE_CLASS: u8 = 1;
/// `VIDEO_ENHANCEMENT_CLASS`.
pub const VIDEO_ENHANCEMENT_CLASS: u8 = 2;
/// `COPY_ENGINE_CLASS`.
pub const COPY_ENGINE_CLASS: u8 = 3;

/// `RENDER_RING_BASE`.
pub const RENDER_RING_BASE: u32 = 0x0_2000;
/// `BLT_RING_BASE`.
pub const BLT_RING_BASE: u32 = 0x2_2000;
/// `GEN11_BSD_RING_BASE` (VCS0).
pub const GEN11_BSD_RING_BASE: u32 = 0x1c_0000;
/// `GEN11_BSD3_RING_BASE` (VCS2).
pub const GEN11_BSD3_RING_BASE: u32 = 0x1d_0000;
/// `GEN11_VEBOX_RING_BASE` (VECS0).
pub const GEN11_VEBOX_RING_BASE: u32 = 0x1c_8000;

/// `struct engine_info` of `intel_engine_cs.c`'s `intel_engines[]`, for the
/// engines of `adl_p_info`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EngineInfo {
    /// `enum intel_engine_id`.
    pub id: u32,
    /// `.class`.
    pub class: u8,
    /// `.instance`.
    pub instance: u8,
    /// `.mmio_bases`' Gen11+ base.
    pub mmio_base: u32,
    /// i915's name for it.
    pub name: &'static str,
}

/// `intel_engines[]`, the five ADL-P/N engines, in id order.
pub const ENGINES: [EngineInfo; 5] = [
    EngineInfo {
        id: id::RCS0,
        class: RENDER_CLASS,
        instance: 0,
        mmio_base: RENDER_RING_BASE,
        name: "rcs0",
    },
    EngineInfo {
        id: id::BCS0,
        class: COPY_ENGINE_CLASS,
        instance: 0,
        mmio_base: BLT_RING_BASE,
        name: "bcs0",
    },
    EngineInfo {
        id: id::VCS0,
        class: VIDEO_DECODE_CLASS,
        instance: 0,
        mmio_base: GEN11_BSD_RING_BASE,
        name: "vcs0",
    },
    EngineInfo {
        id: id::VCS2,
        class: VIDEO_DECODE_CLASS,
        instance: 2,
        mmio_base: GEN11_BSD3_RING_BASE,
        name: "vcs2",
    },
    EngineInfo {
        id: id::VECS0,
        class: VIDEO_ENHANCEMENT_CLASS,
        instance: 0,
        mmio_base: GEN11_VEBOX_RING_BASE,
        name: "vecs0",
    },
];

/// The per-engine registers and fields: i915's own `intel_engine_regs.h`,
/// generated ([`crate::generated::intel_engine_regs`]).
pub use crate::generated::intel_engine_regs::{
    EL_CTRL_LOAD, GEN11_GFX_DISABLE_LEGACY_MODE, GFX_RUN_LIST_ENABLE, STOP_RING, ring_acthd,
    ring_bbaddr, ring_context_control, ring_context_status_ptr, ring_ctl, ring_eir, ring_esr,
    ring_execlist_control, ring_execlist_sq_contents, ring_execlist_status_lo, ring_head,
    ring_hws_pga, ring_hwstam, ring_imr, ring_instdone, ring_ipehr, ring_mi_mode, ring_mode_gen7,
    ring_start, ring_tail, ring_timestamp,
};

/// `RING_EXECLIST_STATUS_HI(base)`: `_MMIO((base) + 0x234 + 4)`, a shape the
/// generator leaves to hand.
#[must_use]
pub const fn ring_execlist_status_hi(base: u32) -> Reg {
    Reg(base + 0x234 + 4)
}
