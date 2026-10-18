// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_reset.c,
// gt/intel_engine_cs.c (intel_engine_stop_cs, intel_engine_cancel_stop_cs,
// intel_engine_wait_for_pending_mi_fw, get_reset_domain) and
// gt/intel_execlists_submission.c (execlists_reset_prepare,
// execlists_reset_active):
// Copyright © 2008-2018 Intel Corporation (intel_reset.c)
// Copyright © 2016 Intel Corporation (intel_engine_cs.c)
// Copyright © 2014 Intel Corporation (intel_execlists_submission.c)
// Under the MIT notice in this crate's i915/NOTICE.

//! Resetting one engine on Gen12.0, as i915's `__intel_engine_reset_bh`
//! does under execlists: stop the command streamer, wait out the
//! forcewakes it asked for (`Wa_22011802037`), ask the engine to get ready
//! (`RING_RESET_CTL`), reset its domain alone (`GEN6_GDRST`), and cancel
//! the request. Never the whole GT and never the function: the firmware's
//! plane is the screen (`docs/INTEL-GPU.md` §4.3).
//!
//! Then the context that hung is rewound: its request's commands skipped,
//! its image's `RING_MI_MODE` no longer stopped and its ring restarted at
//! the request's head, so the replay runs the breadcrumbs alone and its
//! seqno is written ([`rewind`]). The engine is resumed by
//! [`crate::gt::intel_engine_resume`] and the ports by
//! [`crate::execlists::Execlists::reset`].

use crate::device_info::engine as id;
use crate::engine::{EngineInfo, ring_mi_mode, ring_mode_gen7};
use crate::generated::intel_engine_regs::{
    GEN12_GFX_PREFETCH_DISABLE, MODE_IDLE, RESET_CTL_CAT_ERROR, RESET_CTL_READY_TO_RESET,
    RESET_CTL_REQUEST_RESET, STOP_RING, ring_reset_ctl,
};
use crate::generated::intel_gt_regs::{
    GEN6_GDRST, GEN9_PWRGT_DOMAIN_STATUS, GEN11_GRDOM_BLT, GEN11_GRDOM_RENDER, MSG_IDLE_BCS,
    MSG_IDLE_CS, MSG_IDLE_FW_MASK, MSG_IDLE_FW_SHIFT,
};
use crate::io::{Clock, Mmio, wait_for_register};
use crate::lrc::{self, RingState};
use crate::reg::{Reg, masked_bit_disable, masked_bit_enable};
use crate::request::Built;
use crate::submission::HwContext;

/// `get_reset_domain` for Gen11+, the engines this driver runs.
#[must_use]
pub const fn reset_domain(engine: &EngineInfo) -> Option<u32> {
    match engine.id {
        id::RCS0 => Some(GEN11_GRDOM_RENDER),
        id::BCS0 => Some(GEN11_GRDOM_BLT),
        _ => None,
    }
}

/// `__intel_engine_stop_cs`'s fast timeout, which `intel_engine_stop_cs`
/// gives it (1000 us); the slow one (`stop_timeout`, 100 ms by default)
/// is [`STOP_SLOW_TIMEOUT_US`].
pub const STOP_FAST_TIMEOUT_US: u64 = 1_000;
/// `CONFIG_DRM_I915_STOP_TIMEOUT`.
pub const STOP_SLOW_TIMEOUT_US: u64 = 100_000;
/// `gen8_engine_reset_prepare`'s wait for the engine's answer.
pub const RESET_READY_TIMEOUT_US: u64 = 700;
/// `gen6_hw_domain_reset`'s wait for the reset bits to clear.
pub const DOMAIN_RESET_TIMEOUT_US: u64 = 2_000;
/// `__gpm_wait_for_fw_complete`'s wait.
pub const GPM_FW_TIMEOUT_US: u64 = 5_000;

/// How each step of [`intel_engine_reset`] went: `Err` holds the register
/// value the wait last read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ResetReport {
    /// `intel_engine_stop_cs`: `RING_MI_MODE` reached `MODE_IDLE`. i915 goes
    /// on when it does not ("does not adversely affect functionality").
    pub stopped: Result<u32, u32>,
    /// `intel_engine_wait_for_pending_mi_fw`: the forcewakes the CS had
    /// asked for, and whether GPM completed them.
    pub pending_fw: u32,
    /// The wait for them, when there were any.
    pub pending_fw_done: Result<u32, u32>,
    /// `gen8_engine_reset_prepare`: the engine said it is ready.
    pub ready: Result<u32, u32>,
    /// `gen6_hw_domain_reset`: the domain's reset bits cleared (both of
    /// i915's two writes below 12.70).
    pub reset: Result<u32, u32>,
}

impl ResetReport {
    /// Whether the engine was reset: i915 resets even when the prepare
    /// timed out only on a retry; here a refused prepare stops the reset.
    #[must_use]
    pub const fn is_reset(&self) -> bool {
        self.ready.is_ok() && self.reset.is_ok()
    }
}

/// `intel_engine_stop_cs` with `Wa_22011802037` (every Gen12 below 12.70):
/// `STOP_RING` and the prefetch disabled, then a wait for `MODE_IDLE`.
pub fn intel_engine_stop_cs<M, C>(mmio: &mut M, clock: &mut C, base: u32) -> Result<u32, u32>
where
    M: Mmio + ?Sized,
    C: Clock + ?Sized,
{
    let mode = ring_mi_mode(base).offset();
    mmio.write32(mode, masked_bit_enable(STOP_RING));
    mmio.write32(
        ring_mode_gen7(base).offset(),
        masked_bit_enable(GEN12_GFX_PREFETCH_DISABLE),
    );
    wait_for_register(
        mmio,
        clock,
        mode,
        MODE_IDLE,
        MODE_IDLE,
        STOP_FAST_TIMEOUT_US + STOP_SLOW_TIMEOUT_US,
    )
}

/// `intel_engine_cancel_stop_cs`.
pub fn intel_engine_cancel_stop_cs<M: Mmio + ?Sized>(mmio: &mut M, base: u32) {
    mmio.write32(ring_mi_mode(base).offset(), masked_bit_disable(STOP_RING));
}

/// `__cs_pending_mi_force_wakes`: the forcewake domains the engine's CS
/// asked for and GPM has not answered ("bits[29:25] & bits[13:9]").
pub fn cs_pending_mi_force_wakes<M: Mmio + ?Sized>(mmio: &mut M, engine: &EngineInfo) -> u32 {
    let reg: Reg = match engine.id {
        id::RCS0 => MSG_IDLE_CS,
        id::BCS0 => MSG_IDLE_BCS,
        _ => return 0,
    };
    let val = mmio.read32(reg.offset());
    (val & (val >> 16) & MSG_IDLE_FW_MASK) >> MSG_IDLE_FW_SHIFT
}

/// `gen8_engine_reset_prepare`: a catastrophic error is cleared first;
/// otherwise the engine is asked to get ready, unless it already is.
pub fn gen8_engine_reset_prepare<M, C>(mmio: &mut M, clock: &mut C, base: u32) -> Result<u32, u32>
where
    M: Mmio + ?Sized,
    C: Clock + ?Sized,
{
    let reg = ring_reset_ctl(base).offset();
    let ack = mmio.read32(reg);
    let (request, mask, ack) = if ack & RESET_CTL_CAT_ERROR != 0 {
        (RESET_CTL_CAT_ERROR, RESET_CTL_CAT_ERROR, 0)
    } else if ack & RESET_CTL_READY_TO_RESET == 0 {
        (
            RESET_CTL_REQUEST_RESET,
            RESET_CTL_READY_TO_RESET,
            RESET_CTL_READY_TO_RESET,
        )
    } else {
        return Ok(ack);
    };
    mmio.write32(reg, masked_bit_enable(request));
    wait_for_register(mmio, clock, reg, mask, ack, RESET_READY_TIMEOUT_US)
}

/// `gen8_engine_reset_cancel`.
pub fn gen8_engine_reset_cancel<M: Mmio + ?Sized>(mmio: &mut M, base: u32) {
    mmio.write32(
        ring_reset_ctl(base).offset(),
        masked_bit_disable(RESET_CTL_REQUEST_RESET),
    );
}

/// `gen6_hw_domain_reset`: write the domains to `GEN6_GDRST` and wait for
/// the hardware to clear them, twice below 12.70, then 50 us.
pub fn gen6_hw_domain_reset<M, C>(mmio: &mut M, clock: &mut C, mask: u32) -> Result<u32, u32>
where
    M: Mmio + ?Sized,
    C: Clock + ?Sized,
{
    let mut result = Ok(0);
    for _ in 0..2 {
        mmio.write32(GEN6_GDRST.offset(), mask);
        result = wait_for_register(
            mmio,
            clock,
            GEN6_GDRST.offset(),
            mask,
            0,
            DOMAIN_RESET_TIMEOUT_US,
        );
        if result.is_err() {
            break;
        }
    }
    clock.delay_us(50);
    result
}

/// `__intel_engine_reset_bh`'s hardware part for one engine under
/// execlists: `execlists_reset_prepare` (`intel_engine_stop_cs`,
/// `intel_engine_wait_for_pending_mi_fw`), `gen8_reset_engines` for the
/// engine alone (prepare, `__gen11_reset_engines` by its domain, cancel),
/// and `intel_engine_cancel_stop_cs`. `None` for an engine with no reset
/// domain here.
pub fn intel_engine_reset<M, C>(
    mmio: &mut M,
    clock: &mut C,
    engine: &EngineInfo,
) -> Option<ResetReport>
where
    M: Mmio + ?Sized,
    C: Clock + ?Sized,
{
    let domain = reset_domain(engine)?;
    let base = engine.mmio_base;
    let stopped = intel_engine_stop_cs(mmio, clock, base);
    let pending_fw = cs_pending_mi_force_wakes(mmio, engine);
    let pending_fw_done = if pending_fw == 0 {
        Ok(0)
    } else {
        clock.delay_us(1);
        let done = wait_for_register(
            mmio,
            clock,
            GEN9_PWRGT_DOMAIN_STATUS.offset(),
            pending_fw,
            pending_fw,
            GPM_FW_TIMEOUT_US,
        );
        clock.delay_us(1);
        done
    };
    let ready = gen8_engine_reset_prepare(mmio, clock, base);
    // "if (ret && !reset_non_ready) goto skip_reset": the first try does
    // not reset an engine that did not say it was ready.
    let reset = if ready.is_ok() {
        gen6_hw_domain_reset(mmio, clock, domain)
    } else {
        Err(0)
    };
    gen8_engine_reset_cancel(mmio, base);
    intel_engine_cancel_stop_cs(mmio, base);
    Some(ResetReport {
        stopped,
        pending_fw,
        pending_fw_done,
        ready,
        reset,
    })
}

/// `execlists_reset_active` for the context that hung, with
/// `__i915_request_reset(rq, guilty)`: the context banned, its image's
/// `RING_MI_MODE` no longer stopped (`lrc_reset_regs`) and its ring
/// restarting at the hung request's head (`lrc_update_regs(ce, engine,
/// head)`). `regs` is the image's register state. The caller first skips
/// the hung request and every later one already in the ring
/// ([`crate::request::skip`], `__i915_request_skip`). Returns the
/// descriptor for the replay, with `CTX_DESC_FORCE_RESTORE` as
/// `lrc_update_regs` returns it; the image already holds the tail.
pub fn rewind(context: &mut HwContext, regs: &mut [u32], hung: &Built, later: &[Built]) -> u64 {
    context.banned = true;
    lrc::reset_stop_ring(regs);
    let tail = later.last().unwrap_or(hung).tail;
    // lrc_update_regs puts ring->tail in the image; the replay's
    // submission writes it again with the descriptor.
    lrc::lrc_update_regs_ring(
        regs,
        RingState {
            start: context.place.ring_ggtt,
            head: hung.head,
            tail,
            size: context.place.ring_size,
        },
    );
    context.submitted_tail = tail;
    context.desc | crate::execlists::CTX_DESC_FORCE_RESTORE
}
