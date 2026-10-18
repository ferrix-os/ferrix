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

/// `RING_TAIL(base)`.
#[must_use]
pub const fn ring_tail(base: u32) -> Reg {
    Reg(base + 0x30)
}
/// `RING_HEAD(base)`.
#[must_use]
pub const fn ring_head(base: u32) -> Reg {
    Reg(base + 0x34)
}
/// `RING_START(base)`.
#[must_use]
pub const fn ring_start(base: u32) -> Reg {
    Reg(base + 0x38)
}
/// `RING_CTL(base)`.
#[must_use]
pub const fn ring_ctl(base: u32) -> Reg {
    Reg(base + 0x3c)
}
/// `RING_IPEHR(base)`: the instruction the engine stopped on.
#[must_use]
pub const fn ring_ipehr(base: u32) -> Reg {
    Reg(base + 0x68)
}
/// `RING_INSTDONE(base)`.
#[must_use]
pub const fn ring_instdone(base: u32) -> Reg {
    Reg(base + 0x6c)
}
/// `RING_ACTHD(base)`: where the engine is reading.
#[must_use]
pub const fn ring_acthd(base: u32) -> Reg {
    Reg(base + 0x74)
}
/// `RING_HWS_PGA(base)`: the hardware status page's GGTT address.
#[must_use]
pub const fn ring_hws_pga(base: u32) -> Reg {
    Reg(base + 0x80)
}
/// `RING_HWSTAM(base)`: which status-page writes the engine makes.
#[must_use]
pub const fn ring_hwstam(base: u32) -> Reg {
    Reg(base + 0x98)
}
/// `RING_MI_MODE(base)`.
#[must_use]
pub const fn ring_mi_mode(base: u32) -> Reg {
    Reg(base + 0x9c)
}
/// `STOP_RING`, in `RING_MI_MODE`.
pub const STOP_RING: u32 = 1 << 8;
/// `RING_IMR(base)`: the engine's interrupt mask.
#[must_use]
pub const fn ring_imr(base: u32) -> Reg {
    Reg(base + 0xa8)
}
/// `RING_EIR(base)`: the engine's error identity.
#[must_use]
pub const fn ring_eir(base: u32) -> Reg {
    Reg(base + 0xb0)
}
/// `RING_ESR(base)`.
#[must_use]
pub const fn ring_esr(base: u32) -> Reg {
    Reg(base + 0xb8)
}
/// `RING_BBADDR(base)`: the batch the engine is in.
#[must_use]
pub const fn ring_bbaddr(base: u32) -> Reg {
    Reg(base + 0x140)
}
/// `RING_EXECLIST_STATUS_LO(base)`.
#[must_use]
pub const fn ring_execlist_status_lo(base: u32) -> Reg {
    Reg(base + 0x234)
}
/// `RING_EXECLIST_STATUS_HI(base)`.
#[must_use]
pub const fn ring_execlist_status_hi(base: u32) -> Reg {
    Reg(base + 0x234 + 4)
}
/// `RING_CONTEXT_CONTROL(base)`.
#[must_use]
pub const fn ring_context_control(base: u32) -> Reg {
    Reg(base + 0x244)
}
/// `RING_MODE_GEN7(base)`.
#[must_use]
pub const fn ring_mode_gen7(base: u32) -> Reg {
    Reg(base + 0x29c)
}
/// `GFX_RUN_LIST_ENABLE`, in `RING_MODE_GEN7`.
pub const GFX_RUN_LIST_ENABLE: u32 = 1 << 15;
/// `GEN11_GFX_DISABLE_LEGACY_MODE`, in `RING_MODE_GEN7`.
pub const GEN11_GFX_DISABLE_LEGACY_MODE: u32 = 1 << 3;
/// `RING_TIMESTAMP(base)`: the one register `I915_REG_READ` serves.
#[must_use]
pub const fn ring_timestamp(base: u32) -> Reg {
    Reg(base + 0x358)
}
/// `RING_CONTEXT_STATUS_PTR(base)`.
#[must_use]
pub const fn ring_context_status_ptr(base: u32) -> Reg {
    Reg(base + 0x3a0)
}
/// `RING_EXECLIST_SQ_CONTENTS(base)`: the submit queue, two dwords a port.
#[must_use]
pub const fn ring_execlist_sq_contents(base: u32) -> Reg {
    Reg(base + 0x510)
}
/// `RING_EXECLIST_CONTROL(base)`.
#[must_use]
pub const fn ring_execlist_control(base: u32) -> Reg {
    Reg(base + 0x550)
}
/// `EL_CTRL_LOAD`, in `RING_EXECLIST_CONTROL`.
pub const EL_CTRL_LOAD: u32 = 1 << 0;
