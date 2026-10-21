// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_rps.c
// (intel_rps_enable, gen6_rps_enable, rps_set, gen6_rps_set_thresholds,
// rps_timer, rps_work, intel_rps_park, intel_rps_unpark,
// intel_rps_read_actual_frequency) and gt/intel_rc6.c (intel_rc6_enable,
// intel_rc6_unpark, intel_rc6_residency_ns):
// Copyright © 2019 Intel Corporation
// Under the MIT notice in this crate's i915/NOTICE.

//! The GT's frequency and idle power, by the host, as i915 runs them without
//! `GuC`'s `SLPC`: RPS enabled with i915's evaluation intervals and a request
//! at RP1 (or a fixed one, for measurement), RC6 programmed and enabled; and
//! a sample of what the GT does: the frequency asked for and the one it
//! runs at (`CAGF`), why the PCU holds it lower (`GT0_PERF_LIMIT_REASONS`),
//! RC6 residency, and the package's energy from the MCHBAR mirror of RAPL.
//!
//! The policy is i915's for Gen12 with engine busy statistics
//! (`intel_rps_uses_timer`): [`Rps::evaluate`] is `rps_timer` and `rps_work`
//! (busy above 95 % of the interval steps the request up, below 85 % steps
//! it down, each step doubling while it continues; the interval 1 ms after a
//! step, doubling to 20 ms while nothing changes), and [`Rps::park`] and
//! [`Rps::unpark`] are `intel_rps_park` and `intel_rps_unpark`: idle, the GT
//! asks for `RPn`, and the next work restores the last request less a step.
//! Not here: the client boost to RP0 when a program waits on a request
//! (`intel_rps_boost`), which the front end would have to ask for.

use crate::io::Mmio;
use crate::rps::{self, Event, FreqCaps, Power};

/// Engines the policy counts busy time for.
pub const POLICY_ENGINES: usize = 4;

/// The RPS state the host keeps (`struct intel_rps`'s part used here).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rps {
    /// The hardware's limits.
    pub caps: FreqCaps,
    /// `cur_freq`: the request the policy works from, in hardware units.
    pub cur: u32,
    /// `last_freq`: the value last written to `GEN6_RPNSWREQ`; 0 before
    /// the first.
    pub last: u32,
    /// `power.mode`.
    pub mode: Power,
    /// The command streamer's clock, for the PM intervals.
    pub clock_hz: u32,
    /// A fixed request (`ferrix.intelgpu.freq`, or the checkpoint's holds):
    /// no policy, no park.
    pub pinned: bool,
    /// `intel_rps_is_active`: unparked.
    pub active: bool,
    /// `last_adj`: the last step, in hardware units.
    pub last_adj: i32,
    /// `pm_interval`, in ms.
    interval_ms: u32,
    /// When the timer fires next, and when the interval it ends began.
    due_ns: u64,
    from_ns: u64,
    /// Each engine's busy total when the interval began.
    busy_from: [u64; POLICY_ENGINES],
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
    /// `GT0_PERF_LIMIT_REASONS`: why the PCU holds the GT below the
    /// request (bits 15:0 now, 31:16 since last cleared).
    pub limit_reasons: u32,
    /// `GEN6_RP_CUR_UP` and `GEN6_RP_CUR_UP_EI`: the hardware's own busy
    /// count in the current up interval, and the interval's length so far.
    pub busy_up: u32,
    /// See [`Sample::busy_up`].
    pub busy_up_ei: u32,
    /// `GEN6_GT_GFX_RC6`, the raw residency count.
    pub rc6_count: u32,
    /// `PCU_PACKAGE_ENERGY_STATUS`, the raw counter.
    pub energy_raw: u32,
    /// `PKG_ENERGY_UNIT`: counts are 1/2^unit joules.
    pub energy_unit: u32,
}

/// What may hold the GT below its request, read once: the firmware's
/// limits, the package's power limit and its temperature, and how RPS is
/// programmed now.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Limits {
    /// `GEN6_RP_STATE_LIMITS`, raw.
    pub state_limits: u32,
    /// `GEN6_RP_STATE_CAP`, raw.
    pub state_cap: u32,
    /// The package's `PL1` in mW, and whether it is enabled
    /// (`PCU_PACKAGE_RAPL_LIMIT`).
    pub pl1_mw: u64,
    /// See [`Limits::pl1_mw`].
    pub pl1_enabled: bool,
    /// `PCU_PACKAGE_RAPL_LIMIT`, raw.
    pub rapl_limit: u32,
    /// The package's TDP in mW (`PCU_PACKAGE_POWER_SKU`).
    pub tdp_mw: u64,
    /// The package's temperature in degrees C (`PCU_PACKAGE_TEMPERATURE`).
    pub temperature: u32,
    /// `GEN6_RPNSWREQ`, `GEN6_RP_INTERRUPT_LIMITS` and `GEN6_RP_CONTROL`, read
    /// back.
    pub swreq: u32,
    /// See [`Limits::swreq`].
    pub interrupt_limits: u32,
    /// See [`Limits::swreq`].
    pub rp_control: u32,
}

impl Limits {
    /// Read them.
    pub fn read<M: Mmio + ?Sized>(mmio: &mut M) -> Self {
        use crate::generated::intel_gt_regs as gr;
        use crate::reg::field_get;
        let units = mmio.read32(rps::PCU_PACKAGE_POWER_SKU_UNIT.offset());
        let power_unit = field_get(rps::PKG_PWR_UNIT, units);
        let rapl = mmio.read32(rps::PCU_PACKAGE_RAPL_LIMIT.offset());
        let sku = mmio.read32(rps::PCU_PACKAGE_POWER_SKU.offset());
        Limits {
            state_limits: mmio.read32(rps::GEN6_RP_STATE_LIMITS.offset()),
            state_cap: mmio.read32(rps::GEN6_RP_STATE_CAP.offset()),
            pl1_mw: rps::power_milliwatts(field_get(rps::PKG_PWR_LIM_1, rapl), power_unit),
            pl1_enabled: rapl & rps::PKG_PWR_LIM_1_EN != 0,
            rapl_limit: rapl,
            tdp_mw: rps::power_milliwatts(field_get(rps::PKG_PKG_TDP, sku), power_unit),
            temperature: field_get(
                rps::TEMP_MASK,
                mmio.read32(rps::PCU_PACKAGE_TEMPERATURE.offset()),
            ),
            swreq: mmio.read32(gr::GEN6_RPNSWREQ.offset()),
            interrupt_limits: mmio.read32(gr::GEN6_RP_INTERRUPT_LIMITS.offset()),
            rp_control: mmio.read32(gr::GEN6_RP_CONTROL.offset()),
        }
    }
}

impl Rps {
    /// `intel_rps_init`'s caps and `gen6_rps_enable`: the hardware's limits
    /// read, the down timeout and idle hysteresis written, and the first
    /// request made: `start_mhz` if given (clamped to RPn..RP0, and then
    /// pinned there), else RP1. Unparked at `now_ns`.
    pub fn enable<M: Mmio + ?Sized>(
        mmio: &mut M,
        clock_hz: u32,
        start_mhz: Option<u32>,
        now_ns: u64,
    ) -> Self {
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
            last: 0,
            mode: Power::Low,
            clock_hz,
            pinned: start_mhz.is_some(),
            active: true,
            last_adj: 0,
            interval_ms: 1,
            due_ns: now_ns.saturating_add(1_000_000),
            from_ns: now_ns,
            busy_from: [0; POLICY_ENGINES],
        };
        // rps_reset: power mode written, then the request.
        for (reg, value) in rps::rps_set_power(Power::Low, clock_hz) {
            mmio.write32(reg.offset(), value);
        }
        let start = start_mhz.map_or(caps.rp1_freq, |mhz| rps::intel_freq_opcode(mhz, caps));
        state.set(mmio, start);
        state
    }

    /// `intel_rps_set`: `rps_set` with `update` (`gen6_rps_set`, skipped
    /// when `val` is already the last request, then the power mode
    /// `gen6_rps_set_thresholds` picks for it), and the interrupt limits
    /// (`rps_limits`).
    pub fn set<M: Mmio + ?Sized>(&mut self, mmio: &mut M, val: u32) {
        let val = val.clamp(self.caps.min_freq, self.caps.rp0_freq);
        if val != self.last {
            let (reg, value) = rps::gen6_rps_set(val);
            mmio.write32(reg.offset(), value);
            let mode = rps::gen6_rps_power_for(self.mode, val, self.cur, self.caps);
            if mode != self.mode {
                for (reg, value) in rps::rps_set_power(mode, self.clock_hz) {
                    mmio.write32(reg.offset(), value);
                }
                self.mode = mode;
            }
            self.last = val;
        }
        let (reg, value) = rps::rps_limits(self.caps, val);
        mmio.write32(reg.offset(), value);
        self.cur = val;
    }

    /// A request in MHz, pinned there or released to the policy: the value
    /// asked and its MHz after rounding and clamping.
    pub fn set_mhz<M: Mmio + ?Sized>(&mut self, mmio: &mut M, mhz: u32, pin: bool) -> u32 {
        let val = rps::intel_freq_opcode(mhz, self.caps);
        self.pinned = pin;
        self.set(mmio, val);
        rps::intel_gpu_freq(val)
    }

    /// `intel_rps_unpark` at `now_ns`, the engines' busy totals `busy`: the
    /// last request again, clamped, and the timer started at 1 ms.
    pub fn unpark<M: Mmio + ?Sized>(&mut self, mmio: &mut M, now_ns: u64, busy: &[u64]) {
        if self.active {
            return;
        }
        self.active = true;
        self.set(mmio, self.cur);
        self.start_timer(now_ns, busy);
    }

    /// `intel_rps_park`: the timer stopped and the idle frequency (`RPn`)
    /// asked for; the request the unpark returns to stepped down. A pinned
    /// request stays.
    pub fn park<M: Mmio + ?Sized>(&mut self, mmio: &mut M) {
        if !self.active || self.pinned {
            return;
        }
        self.active = false;
        let idle = self.caps.min_freq;
        if self.last <= idle {
            return;
        }
        // rps_set(rps, idle_freq, false): the request alone, no thresholds.
        let (reg, value) = rps::gen6_rps_set(idle);
        mmio.write32(reg.offset(), value);
        self.last = idle;
        let (cur, adj) = rps::park_step(self.cur, self.last_adj, self.caps.min_freq);
        self.cur = cur;
        self.last_adj = adj;
    }

    fn start_timer(&mut self, now_ns: u64, busy: &[u64]) {
        self.interval_ms = 1;
        self.from_ns = now_ns;
        self.due_ns = now_ns.saturating_add(1_000_000);
        self.take_busy(busy);
    }

    fn take_busy(&mut self, busy: &[u64]) {
        self.busy_from = [0; POLICY_ENGINES];
        for (from, &now) in self.busy_from.iter_mut().zip(busy) {
            *from = now;
        }
    }

    /// When [`Rps::evaluate`] wants to run next: only while unparked and
    /// not pinned.
    #[must_use]
    pub const fn due(&self) -> Option<u64> {
        if self.active && !self.pinned {
            Some(self.due_ns)
        } else {
            None
        }
    }

    /// `rps_timer` then `rps_work`, if due at `now_ns`: the engines' busy
    /// time since the interval began (`busy`, each engine's running total in
    /// ns), combined as i915 combines it, against the thresholds; a step up
    /// or down if asked; the next interval. Returns the event.
    pub fn evaluate<M: Mmio + ?Sized>(
        &mut self,
        mmio: &mut M,
        now_ns: u64,
        busy: &[u64],
    ) -> Option<Event> {
        if self.due()? > now_ns {
            return None;
        }
        let mut deltas = [0_u64; POLICY_ENGINES];
        for ((delta, &now), &from) in deltas.iter_mut().zip(busy).zip(&self.busy_from) {
            *delta = now.saturating_sub(from);
        }
        let dt = now_ns.saturating_sub(self.from_ns);
        let combined = rps::combined_busy(&deltas);
        let (min, max) = (self.caps.min_freq, self.caps.rp0_freq);
        let event = rps::rps_timer_event(combined, dt, self.cur, min, max);
        if event == Event::Steady {
            self.last_adj = 0;
        } else {
            let (new, adj) = rps::rps_work(event, self.cur, self.last_adj, min, max);
            self.set(mmio, new);
            self.last_adj = adj;
            self.interval_ms = 1;
        }
        self.from_ns = now_ns;
        self.take_busy(busy);
        self.due_ns = now_ns.saturating_add(u64::from(self.interval_ms) * 1_000_000);
        self.interval_ms = (self.interval_ms * 2).min(rps::BUSY_MAX_EI_MS);
        Some(event)
    }

    /// What the GT does now.
    pub fn sample<M: Mmio + ?Sized>(&self, mmio: &mut M) -> Sample {
        use crate::generated::i915_reg as r;
        use crate::generated::intel_gt_regs as gr;
        let rpstat = mmio.read32(gr::GEN12_RPSTAT1.offset());
        let unit = crate::reg::field_get(
            rps::PKG_ENERGY_UNIT,
            mmio.read32(rps::PCU_PACKAGE_POWER_SKU_UNIT.offset()),
        );
        Sample {
            requested_mhz: rps::intel_gpu_freq(self.last),
            rpstat,
            actual_mhz: rps::intel_gpu_freq(rps::gen12_cagf(rpstat)),
            limit_reasons: mmio.read32(r::GT0_PERF_LIMIT_REASONS.offset()),
            busy_up: mmio.read32(gr::GEN6_RP_CUR_UP.offset()) & 0xff_ffff,
            busy_up_ei: mmio.read32(gr::GEN6_RP_CUR_UP_EI.offset()) & 0xff_ffff,
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

/// What two samples `elapsed_ns` apart say: the average power in milliwatts
/// and the share of the time in RC6, in tenths of a percent.
#[must_use]
pub const fn between(before: &Sample, after: &Sample, elapsed_ns: u64) -> (u64, u64) {
    if elapsed_ns == 0 {
        return (0, 0);
    }
    let uj = rps::energy_microjoules(before.energy_raw, after.energy_raw, after.energy_unit);
    let rc6 = rps::rc6_residency_ns(after.rc6_count.wrapping_sub(before.rc6_count));
    (uj * 1_000_000 / elapsed_ns, rc6 * 1000 / elapsed_ns)
}
