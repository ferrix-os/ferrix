// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_rps.c,
// gt/intel_rps.h, gt/intel_gt_regs.h and intel_mchbar_regs.h:
// Copyright © 2019 Intel Corporation (intel_rps.c, intel_rps.h)
// Copyright © 2022 Intel Corporation (intel_gt_regs.h, intel_mchbar_regs.h)
// Under the MIT notice in this crate's i915/NOTICE.

//! Frequency control by the host, as i915 does it when `GuC`'s `SLPC` is not
//! in use: the hardware's limits from `GEN6_RP_STATE_CAP`, and a request
//! through `GEN6_RPNSWREQ` (`gen6_rps_set`), with the up and down evaluation
//! intervals and thresholds `rps_set_power` programs; the actual frequency
//! read back from `GEN12_RPSTAT1` (`intel_rps_get_cagf`), and the package's
//! energy from the MCHBAR mirror of its RAPL counter. Without a request the
//! GT idles near its lowest frequency.

use crate::generated::intel_gt_regs::GEN6_RPNSWREQ;
use crate::reg::{Reg, field_get, genmask};

/// `MCHBAR_MIRROR_BASE_SNB`.
pub const MCHBAR_MIRROR_BASE_SNB: u32 = 0x14_0000;
/// `GEN6_RP_STATE_CAP`.
pub const GEN6_RP_STATE_CAP: Reg = Reg(MCHBAR_MIRROR_BASE_SNB + 0x5998);
/// `GEN10_FREQ_INFO_REC`.
pub const GEN10_FREQ_INFO_REC: Reg = Reg(MCHBAR_MIRROR_BASE_SNB + 0x5ef0);
/// `SA_PERF_STATUS_0_0_0_MCHBAR_PC` (`intel_mchbar_regs.h`): the system
/// agent's clocks now, the memory controller's (QCLK) among them.
pub const SA_PERF_STATUS_0_0_0_MCHBAR_PC: Reg = Reg(MCHBAR_MIRROR_BASE_SNB + 0x5918);
/// `DG1_QCLK_RATIO_MASK`.
pub const QCLK_RATIO_MASK: u32 = crate::reg::genmask(9, 2);
/// `DG1_QCLK_REFERENCE`: set, the ratio counts 100 MHz; clear, 133.33 MHz.
pub const QCLK_REFERENCE: u32 = 1 << 10;

/// The memory controller's clock in MHz from `SA_PERF_STATUS`, as
/// `dg1_mchbar_read_qgv_point_info` reckons it (ratio times reference).
#[must_use]
pub const fn qclk_mhz(sa_perf_status: u32) -> u32 {
    let ratio = crate::reg::field_get(QCLK_RATIO_MASK, sa_perf_status);
    if sa_perf_status & QCLK_REFERENCE != 0 {
        ratio * 100
    } else {
        ratio * 400 / 3
    }
}
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

/// `intel_gt_ns_to_pm_interval` for Gen7+: nanoseconds to the PM unit's
/// intervals, which count 16 command-streamer clocks (`clock_hz`).
#[must_use]
pub const fn intel_gt_ns_to_pm_interval(clock_hz: u32, ns: u64) -> u32 {
    let clocks = ns * clock_hz as u64 / 1_000_000_000;
    clocks.div_ceil(16) as u32
}

/// `rps->power.up_threshold`: busy above this percentage of an evaluation
/// interval asks for more.
pub const UP_THRESHOLD: u32 = 95;
/// `rps->power.down_threshold`: busy below this asks for less.
pub const DOWN_THRESHOLD: u32 = 85;

/// `rps_set_power`'s modes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Power {
    /// `LOW_POWER`: up after 16 ms, down after 32 ms.
    Low,
    /// `BETWEEN`: up after 13 ms.
    Between,
    /// `HIGH_POWER`: up after 10 ms.
    High,
}

/// `rps_set_power`'s writes for `mode`: the up and down evaluation intervals
/// and thresholds, then `GEN6_RP_CONTROL` (Gen10+: no media turbo; media
/// in hardware normal mode, counted as graphics; RPS on, averaging busy up
/// and idle down).
#[must_use]
pub const fn rps_set_power(mode: Power, clock_hz: u32) -> [(Reg, u32); 5] {
    use crate::generated::intel_gt_regs as gr;
    let (ei_up, ei_down): (u64, u64) = match mode {
        Power::Low => (16_000, 32_000),
        Power::Between => (13_000, 32_000),
        Power::High => (10_000, 32_000),
    };
    [
        (
            gr::GEN6_RP_UP_EI,
            intel_gt_ns_to_pm_interval(clock_hz, ei_up * 1000),
        ),
        (
            gr::GEN6_RP_UP_THRESHOLD,
            intel_gt_ns_to_pm_interval(clock_hz, ei_up * UP_THRESHOLD as u64 * 10),
        ),
        (
            gr::GEN6_RP_DOWN_EI,
            intel_gt_ns_to_pm_interval(clock_hz, ei_down * 1000),
        ),
        (
            gr::GEN6_RP_DOWN_THRESHOLD,
            intel_gt_ns_to_pm_interval(clock_hz, ei_down * DOWN_THRESHOLD as u64 * 10),
        ),
        (
            gr::GEN6_RP_CONTROL,
            gr::GEN6_RP_MEDIA_HW_NORMAL_MODE
                | gr::GEN6_RP_MEDIA_IS_GFX
                | gr::GEN6_RP_ENABLE
                | gr::GEN6_RP_UP_BUSY_AVG
                | gr::GEN6_RP_DOWN_IDLE_AVG,
        ),
    ]
}

/// `gen6_rps_enable`'s writes: "Power down if completely idle for over
/// 50ms", and the idle hysteresis.
#[must_use]
pub const fn gen6_rps_enable() -> [(Reg, u32); 2] {
    use crate::generated::intel_gt_regs as gr;
    [
        (gr::GEN6_RP_DOWN_TIMEOUT, 50_000),
        (gr::GEN6_RP_IDLE_HYSTERSIS, 10),
    ]
}

/// `gen6_rps_set_thresholds`' choice of power mode for a request of `val`
/// from `cur`, by i915's rule, the soft limits being the hardware's.
#[must_use]
pub const fn gen6_rps_power_for(mode: Power, val: u32, cur: u32, caps: FreqCaps) -> Power {
    let mut new = match mode {
        Power::Low if val > caps.rp1_freq + 1 && val > cur => Power::Between,
        Power::Between if val <= caps.rp1_freq && val < cur => Power::Low,
        Power::Between if val >= caps.rp0_freq && val > cur => Power::High,
        Power::High if val < (caps.rp1_freq + caps.rp0_freq) >> 1 && val < cur => Power::Between,
        other => other,
    };
    if val <= caps.min_freq {
        new = Power::Low;
    }
    if val >= caps.rp0_freq {
        new = Power::High;
    }
    new
}

/// MHz to hardware units (the inverse of [`intel_gpu_freq`], as
/// `intel_freq_opcode` does on Gen9+: `DIV_ROUND_CLOSEST(val * 3, 50)`),
/// clamped to `caps`.
#[must_use]
pub const fn intel_freq_opcode(mhz: u32, caps: FreqCaps) -> u32 {
    let val = (mhz * GEN9_FREQ_SCALER + GT_FREQUENCY_MULTIPLIER / 2) / GT_FREQUENCY_MULTIPLIER;
    if val < caps.min_freq {
        caps.min_freq
    } else if val > caps.rp0_freq {
        caps.rp0_freq
    } else {
        val
    }
}

/// `intel_rps_get_cagf` for Gen12 below 12.70: the actual frequency, in
/// hardware units, from `GEN12_RPSTAT1`.
#[must_use]
pub const fn gen12_cagf(rpstat: u32) -> u32 {
    field_get(crate::generated::intel_gt_regs::GEN12_CAGF_MASK, rpstat)
}

/// `PCU_PACKAGE_POWER_SKU_UNIT`, the MCHBAR mirror of the package's RAPL
/// units (`intel_mchbar_regs.h`).
pub const PCU_PACKAGE_POWER_SKU_UNIT: Reg = Reg(MCHBAR_MIRROR_BASE_SNB + 0x5938);
/// `PKG_ENERGY_UNIT`: energy counts are 1/2^n joules.
pub const PKG_ENERGY_UNIT: u32 = genmask(12, 8);
/// `PCU_PACKAGE_ENERGY_STATUS`: the package's energy, a 32-bit counter
/// that wraps (`i915_hwmon.c` reads it on discrete parts; on this
/// integrated one the mirror is read as the same, unverified until the box
/// shows it moving).
pub const PCU_PACKAGE_ENERGY_STATUS: Reg = Reg(MCHBAR_MIRROR_BASE_SNB + 0x593c);

/// Microjoules between two energy readings `before` and `after` (the
/// counter wraps at 32 bits) in units of 1/2^`unit` J.
#[must_use]
pub const fn energy_microjoules(before: u32, after: u32, unit: u32) -> u64 {
    let counts = after.wrapping_sub(before) as u64;
    (counts * 1_000_000) >> (unit & 31)
}

/// `GEN6_GT_GFX_RC6`'s count in nanoseconds: 1.28 us units on Gen6+ outside
/// VLV/CHV and Gen9 LP (`intel_rc6_residency_ns`).
#[must_use]
pub const fn rc6_residency_ns(count: u32) -> u64 {
    count as u64 * 1280
}

/// `rps_limits` for Gen9+, written by `intel_rps_set` to
/// `GEN6_RP_INTERRUPT_LIMITS`: the soft maximum (`RP0` here, nothing lowers
/// it) in bits 31:23, and the soft minimum (`RPn`) in bits 22:14 once the
/// request is at or below it.
#[must_use]
pub const fn rps_limits(caps: FreqCaps, val: u32) -> (Reg, u32) {
    let mut limits = caps.rp0_freq << 23;
    if val <= caps.min_freq {
        limits |= caps.min_freq << 14;
    }
    (
        crate::generated::intel_gt_regs::GEN6_RP_INTERRUPT_LIMITS,
        limits,
    )
}

/// `GEN6_RP_STATE_LIMITS`, beside the caps in the MCHBAR mirror: the limits
/// the firmware set (i915 reads it on no current part; said by the
/// frequency checkpoint, raw).
pub const GEN6_RP_STATE_LIMITS: Reg = Reg(MCHBAR_MIRROR_BASE_SNB + 0x5994);
/// `PCU_PACKAGE_POWER_SKU`'s low word: `PKG_PKG_TDP` in its bits 14:0.
pub const PCU_PACKAGE_POWER_SKU: Reg = Reg(MCHBAR_MIRROR_BASE_SNB + 0x5930);
/// `PKG_PKG_TDP`.
pub const PKG_PKG_TDP: u32 = genmask(14, 0);
/// `PKG_PWR_UNIT`, in `PCU_PACKAGE_POWER_SKU_UNIT`: power counts are 1/2^n W.
pub const PKG_PWR_UNIT: u32 = genmask(3, 0);
/// `PCU_PACKAGE_TEMPERATURE`.
pub const PCU_PACKAGE_TEMPERATURE: Reg = Reg(MCHBAR_MIRROR_BASE_SNB + 0x5978);
/// `TEMP_MASK`.
pub const TEMP_MASK: u32 = genmask(7, 0);
/// `PCU_PACKAGE_RAPL_LIMIT`: the package's power limit 1 (`PL1`).
pub const PCU_PACKAGE_RAPL_LIMIT: Reg = Reg(MCHBAR_MIRROR_BASE_SNB + 0x59a0);
/// `PKG_PWR_LIM_1`.
pub const PKG_PWR_LIM_1: u32 = genmask(14, 0);
/// `PKG_PWR_LIM_1_EN`.
pub const PKG_PWR_LIM_1_EN: u32 = 1 << 15;

/// Milliwatts from a count of `PKG_PWR_UNIT` units (1/2^`unit` W).
#[must_use]
pub const fn power_milliwatts(count: u32, unit: u32) -> u64 {
    (count as u64 * 1000) >> (unit & 31)
}

/// The names `GT0_PERF_LIMIT_REASONS`' status bits have in i915's
/// `throttle_reason_*` attributes, in bit order: why the PCU holds the GT
/// below the request.
pub const LIMIT_REASONS: [(u32, &str); 8] = {
    use crate::generated::i915_reg as r;
    [
        (r::PROCHOT_MASK, "prochot"),
        (r::THERMAL_LIMIT_MASK, "thermal"),
        (r::RATL_MASK, "ratl"),
        (r::VR_THERMALERT_MASK, "vr_thermalert"),
        (r::VR_TDC_MASK, "vr_tdc"),
        (r::POWER_LIMIT_4_MASK, "pl4"),
        (r::POWER_LIMIT_1_MASK, "pl1"),
        (r::POWER_LIMIT_2_MASK, "pl2"),
    ]
};

// The host's policy, as i915 runs it on Gen12 without SLPC: `rps_timer`
// samples each engine's busy time and asks `rps_work` for a step up or down,
// and the GT's park and unpark move the request to the idle frequency and
// back.

/// `BUSY_MAX_EI`: the longest the evaluation timer waits, in ms.
pub const BUSY_MAX_EI_MS: u32 = 20;

/// What `rps_timer` found over one interval.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// `GEN6_PM_RP_UP_THRESHOLD`: busy above `UP_THRESHOLD`.
    Up,
    /// `GEN6_PM_RP_DOWN_THRESHOLD`: busy below `DOWN_THRESHOLD`.
    Down,
    /// Neither: `last_adj` is reset.
    Steady,
}

/// `rps_timer`'s combination of the engines' busy times: the busiest in
/// full, the next two by half and a quarter ("we hedge our bets by only
/// contributing a factor of the distributed load").
#[must_use]
pub fn combined_busy(busy_ns: &[u64]) -> u64 {
    let mut max = [0_u64; 3];
    for &busy in busy_ns {
        let mut busy = busy;
        for slot in &mut max {
            if busy > *slot {
                core::mem::swap(&mut busy, slot);
            }
        }
    }
    let mut total = max[0];
    for (i, &busy) in max.iter().enumerate().skip(1) {
        if busy == 0 {
            break;
        }
        total += busy >> i;
    }
    total
}

/// `rps_timer`'s decision for `busy` ns in `dt` ns at request `cur`, within
/// the soft limits `min..=max`.
#[must_use]
pub const fn rps_timer_event(busy: u64, dt: u64, cur: u32, min: u32, max: u32) -> Event {
    if 100 * busy > UP_THRESHOLD as u64 * dt && cur < max {
        Event::Up
    } else if 100 * busy < DOWN_THRESHOLD as u64 * dt && cur > min {
        Event::Down
    } else {
        Event::Steady
    }
}

/// `rps_work` for one event without a waiting client: the new request and
/// the new `last_adj`. Up steps double while they continue (1, 2, 4, ...
/// units of 16.67 MHz), down steps likewise; the result is clamped to
/// `min..=max`.
#[must_use]
pub const fn rps_work(event: Event, cur: u32, last_adj: i32, min: u32, max: u32) -> (u32, i32) {
    let adj = match event {
        Event::Up if cur >= max => 0,
        Event::Up if last_adj > 0 => last_adj.saturating_mul(2),
        Event::Up => 1,
        Event::Down if cur <= min => 0,
        Event::Down if last_adj < 0 => last_adj.saturating_mul(2),
        Event::Down => -1,
        Event::Steady => 0,
    };
    let new = cur as i64 + adj as i64;
    let new = if new < min as i64 {
        min
    } else if new > max as i64 {
        max
    } else {
        new as u32
    };
    (new, adj)
}

/// `intel_rps_park`'s step for the next unpark: "treat this idle point as a
/// downclock interrupt", by an even bin: the new `last_adj` and the request
/// the unpark will start from, at least `min`.
#[must_use]
pub const fn park_step(cur: u32, last_adj: i32, min: u32) -> (u32, i32) {
    let adj = if last_adj < 0 {
        last_adj.saturating_mul(2)
    } else {
        -2
    };
    let new = cur as i64 + adj as i64;
    (if new < min as i64 { min } else { new as u32 }, adj)
}

// Ferrix's additions to the policy, each a pure rule so the host tests can
// pin it. i915 parks RPS when the GT's last wakeref goes and leans on the
// client boost (`intel_rps_boost`) to bring a waiting program's work back
// up; the rules below keep a frame's work from running at the idle
// frequency between its submissions.

/// How long nothing may be in flight before RPS parks: one longest
/// evaluation interval ([`BUSY_MAX_EI_MS`]) past the last retire. A gap
/// shorter than that is a gap inside a frame's work, which i915 would not
/// see as idle either (its GT wakeref outlives a retire by the engines'
/// heartbeat and the GT's own idle delay).
pub const IDLE_HOLD_NS: u64 = BUSY_MAX_EI_MS as u64 * 1_000_000;

/// What the park rule asks for at one service.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Park {
    /// Work is in flight: unparked, whatever the timer said.
    Unpark,
    /// Leave RPS as it is.
    Hold,
    /// Idle for [`IDLE_HOLD_NS`]: `intel_rps_park`.
    Park,
}

/// The park rule. `in_flight`: a request submitted and not retired, in the
/// back end's ports or queues, or held by the front end until its fences
/// signal; `active`: RPS is unparked; `idle_ns`: how long nothing has been
/// in flight. Never parks while anything is in flight.
#[must_use]
pub const fn park_decision(in_flight: bool, active: bool, idle_ns: u64) -> Park {
    if in_flight {
        Park::Unpark
    } else if active && idle_ns >= IDLE_HOLD_NS {
        Park::Park
    } else {
        Park::Hold
    }
}

/// The lowest request the policy steps down to while unparked, and the
/// lowest the unpark returns to: RP1, the efficient frequency (where
/// i915's down timeout stops first, `rps->efficient_freq`), never below
/// `RPn`. Below it the GT goes only by parking.
#[must_use]
pub const fn active_floor(caps: FreqCaps) -> u32 {
    if caps.rp1_freq > caps.min_freq {
        caps.rp1_freq
    } else {
        caps.min_freq
    }
}

/// `rps_work` with `client_boost` (a program waits on a request not yet
/// retired: `rps->num_waiters`) and the boost frequency `boost`
/// (`rps->boost_freq`, RP0 unless set): below it, straight to it; an up
/// event steps as ever; any other event holds the request (`adj = 0`).
/// Without a waiter, [`rps_work`] within `floor..=max`.
#[must_use]
pub const fn rps_work_boost(
    event: Event,
    cur: u32,
    last_adj: i32,
    floor: u32,
    max: u32,
    client_boost: bool,
    boost: u32,
) -> (u32, i32) {
    if client_boost && cur < boost {
        let boost = if boost > max { max } else { boost };
        (boost, 0)
    } else if matches!(event, Event::Up) {
        rps_work(event, cur, last_adj, floor, max)
    } else if client_boost {
        (cur, 0)
    } else {
        rps_work(event, cur, last_adj, floor, max)
    }
}
