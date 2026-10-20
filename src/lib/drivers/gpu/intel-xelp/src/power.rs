// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_rps.c
// (intel_rps_enable, gen6_rps_enable, rps_set, gen6_rps_set_thresholds,
// intel_rps_read_actual_frequency) and gt/intel_rc6.c (intel_rc6_enable,
// intel_rc6_unpark, intel_rc6_residency_ns):
// Copyright © 2019 Intel Corporation
// Under the MIT notice in this crate's i915/NOTICE.

//! The GT's frequency and idle power, by the host, as i915 runs them without
//! `GuC`'s `SLPC`: RPS enabled with i915's evaluation intervals and a request
//! at RP1 (or a fixed one, for measurement), RC6 programmed and enabled; and
//! a sample of what the GT does: the frequency asked for and the one it
//! runs at (`CAGF`), RC6 residency, and the package's energy from the MCHBAR
//! mirror of RAPL.
//!
//! What is left for later (`docs/INTEL-GPU.md` §7, M4): the up/down policy
//! from the PM interrupts (`gen6_rps_irq_handler`) or the EI counters; for
//! now the request stays where it was set. RC6 is only entered while no
//! forcewake is held, and the driver process holds render and GT for its
//! life (i915 releases them per access), so residency stays near zero until
//! forcewake is taken per access.

use crate::io::Mmio;
use crate::rps::{self, FreqCaps, Power};

/// The RPS state the host keeps (`struct intel_rps`'s part used here).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rps {
    /// The hardware's limits.
    pub caps: FreqCaps,
    /// `cur_freq`: the last request, in hardware units.
    pub cur: u32,
    /// `power.mode`.
    pub mode: Power,
    /// The command streamer's clock, for the PM intervals.
    pub clock_hz: u32,
}

/// One sample of the GT's frequency and power.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Sample {
    /// The request, in MHz.
    pub requested_mhz: u32,
    /// `GEN12_RPSTAT1` as read.
    pub rpstat: u32,
    /// The actual frequency (`CAGF`), in MHz.
    pub actual_mhz: u32,
    /// `GEN6_GT_GFX_RC6`, the raw residency count.
    pub rc6_count: u32,
    /// `PCU_PACKAGE_ENERGY_STATUS`, the raw counter.
    pub energy_raw: u32,
    /// `PKG_ENERGY_UNIT`: counts are 1/2^unit joules.
    pub energy_unit: u32,
}

impl Rps {
    /// `intel_rps_init`'s caps and `gen6_rps_enable`: the hardware's limits
    /// read, the down timeout and idle hysteresis written, and the first
    /// request made: `start_mhz` if given (clamped to RPn..RP0), else RP1.
    pub fn enable<M: Mmio + ?Sized>(mmio: &mut M, clock_hz: u32, start_mhz: Option<u32>) -> Self {
        let caps = rps::gen6_rps_get_freq_caps(
            mmio.read32(rps::GEN6_RP_STATE_CAP.offset()),
            mmio.read32(rps::GEN10_FREQ_INFO_REC.offset()),
        );
        for (reg, value) in rps::gen6_rps_enable() {
            mmio.write32(reg.offset(), value);
        }
        let mut state = Rps {
            caps,
            cur: caps.min_freq,
            mode: Power::Low,
            clock_hz,
        };
        // rps_reset: power mode written, then the request.
        for (reg, value) in rps::rps_set_power(Power::Low, clock_hz) {
            mmio.write32(reg.offset(), value);
        }
        let start = start_mhz.map_or(caps.rp1_freq, |mhz| rps::intel_freq_opcode(mhz, caps));
        state.set(mmio, start);
        state
    }

    /// `rps_set` with `update`: `gen6_rps_set`, then the power mode
    /// `gen6_rps_set_thresholds` picks for the new request.
    pub fn set<M: Mmio + ?Sized>(&mut self, mmio: &mut M, val: u32) {
        let val = val.clamp(self.caps.min_freq, self.caps.rp0_freq);
        let (reg, value) = rps::gen6_rps_set(val);
        mmio.write32(reg.offset(), value);
        let mode = rps::gen6_rps_power_for(self.mode, val, self.cur, self.caps);
        if mode != self.mode {
            for (reg, value) in rps::rps_set_power(mode, self.clock_hz) {
                mmio.write32(reg.offset(), value);
            }
            self.mode = mode;
        }
        self.cur = val;
    }

    /// A request in MHz: the value asked and its MHz after rounding and
    /// clamping.
    pub fn set_mhz<M: Mmio + ?Sized>(&mut self, mmio: &mut M, mhz: u32) -> u32 {
        let val = rps::intel_freq_opcode(mhz, self.caps);
        self.set(mmio, val);
        rps::intel_gpu_freq(val)
    }

    /// What the GT does now.
    pub fn sample<M: Mmio + ?Sized>(&self, mmio: &mut M) -> Sample {
        use crate::generated::intel_gt_regs as gr;
        let rpstat = mmio.read32(gr::GEN12_RPSTAT1.offset());
        let unit = crate::reg::field_get(
            rps::PKG_ENERGY_UNIT,
            mmio.read32(rps::PCU_PACKAGE_POWER_SKU_UNIT.offset()),
        );
        Sample {
            requested_mhz: rps::intel_gpu_freq(self.cur),
            rpstat,
            actual_mhz: rps::intel_gpu_freq(rps::gen12_cagf(rpstat)),
            rc6_count: mmio.read32(gr::GEN6_GT_GFX_RC6.offset()),
            energy_raw: mmio.read32(rps::PCU_PACKAGE_ENERGY_STATUS.offset()),
            energy_unit: unit,
        }
    }
}

/// `intel_rc6_enable` with `gen11_rc6_enable` for `engines`, then
/// `intel_rc6_unpark`: the thresholds and gating written, and
/// `GEN6_RC_CONTROL` set to the enable. Returns that value.
pub fn rc6_enable<M: Mmio + ?Sized>(mmio: &mut M, engines: &[crate::engine::EngineInfo]) -> u32 {
    let (writes, ctl_enable) = crate::rc6::gen11_rc6_enable(engines);
    for (reg, value) in writes.iter().flatten() {
        mmio.write32(reg.offset(), *value);
    }
    mmio.write32(
        crate::generated::intel_gt_regs::GEN6_RC_CONTROL.offset(),
        ctl_enable,
    );
    ctl_enable
}

/// What a second's two samples say: the average power in milliwatts and the
/// share of the time in RC6, in tenths of a percent.
#[must_use]
pub const fn between(before: &Sample, after: &Sample, elapsed_ns: u64) -> (u64, u64) {
    if elapsed_ns == 0 {
        return (0, 0);
    }
    let uj = rps::energy_microjoules(before.energy_raw, after.energy_raw, after.energy_unit);
    let rc6 = rps::rc6_residency_ns(after.rc6_count.wrapping_sub(before.rc6_count));
    (uj * 1_000_000 / elapsed_ns, rc6 * 1000 / elapsed_ns)
}
