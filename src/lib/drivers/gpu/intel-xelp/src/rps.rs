// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_rps.c,
// gt/intel_rps.h, gt/intel_gt_regs.h and intel_mchbar_regs.h:
// Copyright © 2019 Intel Corporation (intel_rps.c, intel_rps.h)
// Copyright © 2022 Intel Corporation (intel_gt_regs.h, intel_mchbar_regs.h)
// Under the MIT notice in this crate's i915/NOTICE.

//! Frequency control by the host, as i915 does it when `GuC`'s `SLPC` is not
//! in use: the hardware's limits from `GEN6_RP_STATE_CAP`, and a request
//! through `GEN6_RPNSWREQ`. Without a request the GT idles near its lowest
//! frequency, which is what M4 fixes.

use crate::generated::intel_gt_regs::GEN6_RPNSWREQ;
use crate::reg::{Reg, field_get, genmask};

/// `MCHBAR_MIRROR_BASE_SNB`.
pub const MCHBAR_MIRROR_BASE_SNB: u32 = 0x14_0000;
/// `GEN6_RP_STATE_CAP`.
pub const GEN6_RP_STATE_CAP: Reg = Reg(MCHBAR_MIRROR_BASE_SNB + 0x5998);
/// `GEN10_FREQ_INFO_REC`.
pub const GEN10_FREQ_INFO_REC: Reg = Reg(MCHBAR_MIRROR_BASE_SNB + 0x5ef0);
/// `RPE_MASK`, in `GEN10_FREQ_INFO_REC`.
pub const RPE_MASK: u32 = genmask(15, 8);
/// `GEN9_FREQ_SCALER`: hardware units are 50/3 MHz.
pub const GEN9_FREQ_SCALER: u32 = 3;
/// `GT_FREQUENCY_MULTIPLIER`.
pub const GT_FREQUENCY_MULTIPLIER: u32 = 50;

/// `struct intel_rps_freq_caps`, in hardware units.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FreqCaps {
    /// `rp0_freq`: the highest.
    pub rp0_freq: u32,
    /// `rp1_freq`: the efficient one.
    pub rp1_freq: u32,
    /// `min_freq` (`RPn`).
    pub min_freq: u32,
}

/// `__gen6_rps_get_freq_caps` for Gen11+ (not Gen9 LP): `RP0` and `RPn` from
/// `rp_state_cap`, RP1 from `freq_info_rec`, each "in units of 50 MHz",
/// converted to 16.67 MHz units.
#[must_use]
pub const fn gen6_rps_get_freq_caps(rp_state_cap: u32, freq_info_rec: u32) -> FreqCaps {
    FreqCaps {
        rp0_freq: (rp_state_cap & 0xff) * GEN9_FREQ_SCALER,
        rp1_freq: field_get(RPE_MASK, freq_info_rec) * GEN9_FREQ_SCALER,
        min_freq: ((rp_state_cap >> 16) & 0xff) * GEN9_FREQ_SCALER,
    }
}

/// `intel_gpu_freq` for Gen9+: hardware units to MHz, rounded.
#[must_use]
pub const fn intel_gpu_freq(val: u32) -> u32 {
    (val * GT_FREQUENCY_MULTIPLIER + GEN9_FREQ_SCALER / 2) / GEN9_FREQ_SCALER
}

/// `GEN9_FREQUENCY(x)`.
#[must_use]
pub const fn gen9_frequency(val: u32) -> u32 {
    val << 23
}

/// `gen6_rps_set` for Gen9+: the write that requests `val`.
#[must_use]
pub const fn gen6_rps_set(val: u32) -> (Reg, u32) {
    (GEN6_RPNSWREQ, gen9_frequency(val))
}
