// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_rc6.c,
// gt/intel_gt_regs.h, gt/intel_engine_regs.h and gt/uc/intel_guc_reg.h:
// Copyright © 2019 Intel Corporation (intel_rc6.c)
// Copyright © 2022 Intel Corporation (intel_gt_regs.h, intel_engine_regs.h)
// Copyright © 2014-2019 Intel Corporation (intel_guc_reg.h)
// Under the MIT notice in this crate's i915/NOTICE.

//! RC6 (the GT's idle power state) and power gating, as `gen11_rc6_enable`
//! programs them when `GuC` does not run RC6 (`intel_uc_uses_guc_rc` false):
//! thresholds, then the gating policies, then the enable, which i915 writes
//! to `GEN6_RC_CONTROL` when the GT is unparked (`intel_rc6_unpark`).

use crate::engine::{EngineInfo, VIDEO_DECODE_CLASS};
use crate::generated::intel_engine_regs::ring_max_idle;
use crate::generated::intel_gt_regs as gr;
use crate::reg::Reg;

/// `GUC_MAX_IDLE_COUNT` (`intel_guc_reg.h`, not vendored).
pub const GUC_MAX_IDLE_COUNT: Reg = Reg(0xc3e4);

/// `GEN6_RC_CTL_EI_MODE(x)`.
#[must_use]
pub const fn gen6_rc_ctl_ei_mode(x: u32) -> u32 {
    x << 27
}

/// `VDN_HCP_POWERGATE_ENABLE(n)`.
#[must_use]
pub const fn vdn_hcp_powergate_enable(n: u32) -> u32 {
    1 << (3 + 2 * n)
}

/// `VDN_MFX_POWERGATE_ENABLE(n)`.
#[must_use]
pub const fn vdn_mfx_powergate_enable(n: u32) -> u32 {
    1 << (4 + 2 * n)
}

/// `gen11_rc6_enable`'s writes, in order, for `engines`; then the value
/// `rc6->ctl_enable` takes (`intel_guc_rc_enable` fails without `GuC`, so the
/// hardware enable, RC6 and EI mode 1).
#[must_use]
pub fn gen11_rc6_enable(engines: &[EngineInfo]) -> ([Option<(Reg, u32)>; 16], u32) {
    let mut writes = [None; 16];
    let mut at = 0;
    let mut put = |reg: Reg, value: u32| {
        if let Some(slot) = writes.get_mut(at) {
            *slot = Some((reg, value));
        }
        at += 1;
    };
    // 2b: Program RC6 thresholds.
    put(gr::GEN6_RC6_WAKE_RATE_LIMIT, (54 << 16) | 85);
    put(gr::GEN10_MEDIA_WAKE_RATE_LIMIT, 150);
    put(gr::GEN6_RC_EVALUATION_INTERVAL, 125_000); // 12500 * 1280ns
    put(gr::GEN6_RC_IDLE_HYSTERSIS, 25); // 25 * 1280ns
    for engine in engines {
        put(ring_max_idle(engine.mmio_base), 10);
    }
    put(GUC_MAX_IDLE_COUNT, 0xa);
    put(gr::GEN6_RC_SLEEP, 0);
    put(gr::GEN6_RC6_THRESHOLD, 50_000); // 50/125ms per EI
    // 2c: Program Coarse Power Gating Policies.
    put(gr::GEN9_MEDIA_PG_IDLE_HYSTERESIS, 60);
    put(gr::GEN9_RENDER_PG_IDLE_HYSTERESIS, 60);
    // 3a: power gating; GRAPHICS_VER >= 12 && !IS_DG1: each VCS too.
    let mut pg_enable =
        gr::GEN9_RENDER_PG_ENABLE | gr::GEN9_MEDIA_PG_ENABLE | gr::GEN11_MEDIA_SAMPLER_PG_ENABLE;
    for engine in engines.iter().filter(|e| e.class == VIDEO_DECODE_CLASS) {
        let i = u32::from(engine.instance);
        pg_enable |= vdn_hcp_powergate_enable(i) | vdn_mfx_powergate_enable(i);
    }
    put(gr::GEN9_PG_ENABLE, pg_enable);
    let ctl_enable =
        gr::GEN6_RC_CTL_HW_ENABLE | gr::GEN6_RC_CTL_RC6_ENABLE | gen6_rc_ctl_ei_mode(1);
    (writes, ctl_enable)
}
