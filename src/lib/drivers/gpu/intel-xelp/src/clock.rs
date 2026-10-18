// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_gt_clock_utils.c
// (registers from gt/intel_gt_regs.h and i915_reg.h, generated):
// Copyright © 2020 Intel Corporation
// Under the MIT notice in this crate's i915/NOTICE.

//! The command streamer's timestamp frequency
//! (`I915_PARAM_CS_TIMESTAMP_FREQUENCY`), as `gen11_read_clock_frequency`
//! finds it: from `TIMESTAMP_OVERRIDE` when `CTC_MODE` says the divide logic
//! drives it, else from the crystal `RPM_CONFIG0` names, shifted by the
//! counter's step.

use crate::generated::i915_reg as ir;
use crate::generated::intel_gt_regs as gr;
use crate::io::Mmio;
use crate::reg::field_get;

/// `read_reference_ts_freq`.
#[must_use]
pub const fn read_reference_ts_freq(ts_override: u32) -> u32 {
    let base_freq = (((ts_override & ir::GEN9_TIMESTAMP_OVERRIDE_US_COUNTER_DIVIDER_MASK)
        >> ir::GEN9_TIMESTAMP_OVERRIDE_US_COUNTER_DIVIDER_SHIFT)
        + 1)
        * 1_000_000;
    let frac = (ts_override & ir::GEN9_TIMESTAMP_OVERRIDE_US_COUNTER_DENOMINATOR_MASK)
        >> ir::GEN9_TIMESTAMP_OVERRIDE_US_COUNTER_DENOMINATOR_SHIFT;
    base_freq + 1_000_000 / (frac + 1)
}

/// `gen11_get_crystal_clock_freq`: 0 for an encoding i915 does not know
/// (`MISSING_CASE`).
#[must_use]
pub const fn gen11_get_crystal_clock_freq(rpm_config0: u32) -> u32 {
    match rpm_config0 & gr::GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_MASK {
        gr::GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_24_MHZ => 24_000_000,
        gr::GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_19_2_MHZ => 19_200_000,
        gr::GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_38_4_MHZ => 38_400_000,
        gr::GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_25_MHZ => 25_000_000,
        _ => 0,
    }
}

/// `gen11_read_clock_frequency` over the three register values.
#[must_use]
pub const fn gen11_clock_frequency(ctc_mode: u32, ts_override: u32, rpm_config0: u32) -> u32 {
    if ctc_mode & gr::CTC_SOURCE_PARAMETER_MASK == gr::CTC_SOURCE_DIVIDE_LOGIC {
        read_reference_ts_freq(ts_override)
    } else {
        gen11_get_crystal_clock_freq(rpm_config0)
            >> (3 - field_get(gr::GEN10_RPM_CONFIG0_CTC_SHIFT_PARAMETER_MASK, rpm_config0))
    }
}

/// `gen11_read_clock_frequency`, reading the registers (`CTC_MODE` is in
/// the render domain, the other two always on).
pub fn gen11_read_clock_frequency<M: Mmio + ?Sized>(mmio: &mut M) -> u32 {
    let ctc = mmio.read32(gr::CTC_MODE.offset());
    let ts = mmio.read32(ir::GEN9_TIMESTAMP_OVERRIDE.offset());
    let rpm = mmio.read32(gr::RPM_CONFIG0.offset());
    gen11_clock_frequency(ctc, ts, rpm)
}
