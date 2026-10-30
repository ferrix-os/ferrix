// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/gen8_engine_cs.c,
// gt/intel_engine_types.h, gt/intel_engine.h, gt/intel_lrc.h and
// gt/intel_gt_regs.h:
// Copyright © 2014 Intel Corporation (gen8_engine_cs.c)
// Copyright © 2019 Intel Corporation (intel_engine_types.h, intel_engine.h)
// Copyright © 2014 Intel Corporation (intel_lrc.h)
// Copyright © 2022 Intel Corporation (intel_gt_regs.h)
// Under the MIT notice in this crate's i915/NOTICE.

//! What i915 writes into a ring around each request on Gen12.0 with
//! execlists: the flushes and invalidations before and after the payload
//! (`gen12_emit_flush_rcs`/`xcs`, the AUX-table invalidation anv's
//! compression needs), the start of the user's batch (`gen8_emit_bb_start`)
//! and the breadcrumbs that mark a request started and finished
//! (`gen8_emit_init_breadcrumb`, `gen12_emit_fini_breadcrumb_*`).
//!
//! Left out, as for ADL-P/N they are never taken: every 12.55+ and DG2
//! branch (`mtl_dummy_pipe_control`, `CCS_FLUSH`, the hold-switchout and
//! predicate workarounds) and the compute class.

use crate::commands::{
    Cs, gen8_emit_flush_dw, gen8_emit_ggtt_write, gen8_emit_pipe_control,
    gen12_emit_ggtt_write_rcs, gen12_emit_pipe_control, gpu,
};
use crate::device_info::engine as id;
use crate::engine::{COPY_ENGINE_CLASS, EngineInfo, RENDER_CLASS, VIDEO_DECODE_CLASS};
use crate::lrc::mi_load_register_imm;
use crate::reg::Reg;

/// `EMIT_INVALIDATE`.
pub const EMIT_INVALIDATE: u32 = 1 << 0;
/// `EMIT_FLUSH`.
pub const EMIT_FLUSH: u32 = 1 << 1;
/// `EMIT_BARRIER`.
pub const EMIT_BARRIER: u32 = EMIT_INVALIDATE | EMIT_FLUSH;
/// `I915_DISPATCH_SECURE`: the batch runs privileged, from the GGTT. Only
/// the driver's own batches may; a client's never does.
pub const I915_DISPATCH_SECURE: u32 = 1 << 0;

/// `LRC_PPHWSP_SCRATCH_ADDR`: the scratch dword in each context's
/// per-process status page that post-sync writes go to.
pub const LRC_PPHWSP_SCRATCH_ADDR: u32 = 0x34 * 4;
/// `I915_GEM_HWS_PREEMPT_ADDR`: the engine status page's preemption flag.
pub const I915_GEM_HWS_PREEMPT_ADDR: u32 = 0x32 * 4;
/// `I915_GEM_HWS_SEQNO_ADDR`.
pub const I915_GEM_HWS_SEQNO_ADDR: u32 = 0x40 * 4;

/// `gen12_get_aux_inv_reg`.
#[must_use]
pub const fn gen12_get_aux_inv_reg(engine_id: u32) -> Option<Reg> {
    match engine_id {
        id::RCS0 => Some(Reg(0x4208)),  // GEN12_CCS_AUX_INV
        id::BCS0 => Some(Reg(0x4248)),  // GEN12_BCS0_AUX_INV
        id::VCS0 => Some(Reg(0x4218)),  // GEN12_VD0_AUX_INV
        id::VCS2 => Some(Reg(0x4298)),  // GEN12_VD2_AUX_INV
        id::VECS0 => Some(Reg(0x4238)), // GEN12_VE0_AUX_INV
        _ => None,
    }
}

/// `AUX_INV`.
pub const AUX_INV: u32 = 1 << 0;

/// `gen12_needs_ccs_aux_inv`: an engine with an AUX register, on a part
/// without flat CCS (`HAS_FLAT_CCS` is 12.50+ only): every ADL-P/N engine.
#[must_use]
pub const fn gen12_needs_ccs_aux_inv(engine: &EngineInfo) -> bool {
    gen12_get_aux_inv_reg(engine.id).is_some()
}

/// `gen12_emit_aux_table_inv`: invalidate the AUX table, then wait for the
/// register to read 0 again. `gsi_offset` is 0 on this part.
pub fn gen12_emit_aux_table_inv(engine: &EngineInfo, cs: &mut Cs<'_>) {
    let Some(inv_reg) = gen12_get_aux_inv_reg(engine.id) else {
        return;
    };
    cs.push(mi_load_register_imm(1) | gpu::MI_LRI_MMIO_REMAP_EN);
    cs.push(inv_reg.offset());
    cs.push(AUX_INV);

    cs.push(
        gpu::MI_SEMAPHORE_WAIT_TOKEN
            | gpu::MI_SEMAPHORE_REGISTER_POLL
            | gpu::MI_SEMAPHORE_POLL
            | gpu::MI_SEMAPHORE_SAD_EQ_SDD,
    );
    cs.push(0);
    cs.push(inv_reg.offset());
    cs.push(0);
    cs.push(0);
}

/// `preparser_disable`.
#[must_use]
pub const fn preparser_disable(state: bool) -> u32 {
    gpu::MI_ARB_CHECK | 1 << 8 | state as u32
}

/// `gen12_emit_flush_rcs`, for ADL-P/N (has the 3D pipeline, below 12.70).
pub fn gen12_emit_flush_rcs(engine: &EngineInfo, mode: u32, cs: &mut Cs<'_>) {
    // "On Aux CCS platforms the invalidation of the Aux table requires
    // quiescing memory traffic beforehand."
    if mode & EMIT_FLUSH != 0 || gen12_needs_ccs_aux_inv(engine) {
        let bit_group_0 = gpu::PIPE_CONTROL0_HDC_PIPELINE_FLUSH;
        let mut bit_group_1 = 0;
        if mode & EMIT_FLUSH != 0 {
            bit_group_1 |= gpu::PIPE_CONTROL_FLUSH_L3;
        }
        bit_group_1 |= gpu::PIPE_CONTROL_TILE_CACHE_FLUSH;
        bit_group_1 |= gpu::PIPE_CONTROL_RENDER_TARGET_CACHE_FLUSH;
        bit_group_1 |= gpu::PIPE_CONTROL_DEPTH_CACHE_FLUSH;
        // Wa_1409600907:tgl,adl-p
        bit_group_1 |= gpu::PIPE_CONTROL_DEPTH_STALL;
        bit_group_1 |= gpu::PIPE_CONTROL_DC_FLUSH_ENABLE;
        bit_group_1 |= gpu::PIPE_CONTROL_FLUSH_ENABLE;
        bit_group_1 |= gpu::PIPE_CONTROL_STORE_DATA_INDEX;
        bit_group_1 |= gpu::PIPE_CONTROL_QW_WRITE;
        bit_group_1 |= gpu::PIPE_CONTROL_CS_STALL;
        gen12_emit_pipe_control(cs, bit_group_0, bit_group_1, LRC_PPHWSP_SCRATCH_ADDR);
    }

    if mode & EMIT_INVALIDATE != 0 {
        let flags = gpu::PIPE_CONTROL_COMMAND_CACHE_INVALIDATE
            | gpu::PIPE_CONTROL_TLB_INVALIDATE
            | gpu::PIPE_CONTROL_INSTRUCTION_CACHE_INVALIDATE
            | gpu::PIPE_CONTROL_TEXTURE_CACHE_INVALIDATE
            | gpu::PIPE_CONTROL_VF_CACHE_INVALIDATE
            | gpu::PIPE_CONTROL_CONST_CACHE_INVALIDATE
            | gpu::PIPE_CONTROL_STATE_CACHE_INVALIDATE
            | gpu::PIPE_CONTROL_STORE_DATA_INDEX
            | gpu::PIPE_CONTROL_QW_WRITE
            | gpu::PIPE_CONTROL_CS_STALL;
        // "Prevent the pre-parser from skipping past the TLB invalidate and
        // loading a stale page for the batch buffer / request payload."
        cs.push(preparser_disable(true));
        gen8_emit_pipe_control(cs, flags, LRC_PPHWSP_SCRATCH_ADDR);
        gen12_emit_aux_table_inv(engine, cs);
        cs.push(preparser_disable(false));
    }
}

/// `gen12_emit_flush_rcs(EMIT_INVALIDATE)` without the AUX table's
/// invalidation and the flush that quiesces memory before it: the caches
/// and TLBs invalidated, what a request needs when nothing changed the AUX
/// table since the last (H4 of `docs/GAME-MODE.md` §5.7; measured, not
/// used by the requests).
pub fn gen12_emit_invalidate_rcs_without_aux(cs: &mut Cs<'_>) {
    let flags = gpu::PIPE_CONTROL_COMMAND_CACHE_INVALIDATE
        | gpu::PIPE_CONTROL_TLB_INVALIDATE
        | gpu::PIPE_CONTROL_INSTRUCTION_CACHE_INVALIDATE
        | gpu::PIPE_CONTROL_TEXTURE_CACHE_INVALIDATE
        | gpu::PIPE_CONTROL_VF_CACHE_INVALIDATE
        | gpu::PIPE_CONTROL_CONST_CACHE_INVALIDATE
        | gpu::PIPE_CONTROL_STATE_CACHE_INVALIDATE
        | gpu::PIPE_CONTROL_STORE_DATA_INDEX
        | gpu::PIPE_CONTROL_QW_WRITE
        | gpu::PIPE_CONTROL_CS_STALL;
    cs.push(preparser_disable(true));
    gen8_emit_pipe_control(cs, flags, LRC_PPHWSP_SCRATCH_ADDR);
    cs.push(preparser_disable(false));
}

/// `gen12_emit_flush_xcs(EMIT_INVALIDATE)` without the AUX table's
/// invalidation (H4, measured only).
pub fn gen12_emit_invalidate_xcs_without_aux(cs: &mut Cs<'_>) {
    cs.push(preparser_disable(true));
    cs.push(
        (gpu::MI_FLUSH_DW + 1)
            | gpu::MI_FLUSH_DW_STORE_INDEX
            | gpu::MI_FLUSH_DW_OP_STOREDW
            | gpu::MI_INVALIDATE_TLB,
    );
    cs.push(LRC_PPHWSP_SCRATCH_ADDR);
    cs.push(0);
    cs.push(0);
    cs.push(preparser_disable(false));
}

/// `gen12_emit_flush_xcs`.
pub fn gen12_emit_flush_xcs(engine: &EngineInfo, mode: u32, cs: &mut Cs<'_>) {
    if mode & EMIT_INVALIDATE != 0 {
        cs.push(preparser_disable(true));
    }
    // "We always require a command barrier so that subsequent commands,
    // such as breadcrumb interrupts, are strictly ordered wrt the contents
    // of the write cache being flushed to memory."
    let mut cmd =
        (gpu::MI_FLUSH_DW + 1) | gpu::MI_FLUSH_DW_STORE_INDEX | gpu::MI_FLUSH_DW_OP_STOREDW;
    if mode & EMIT_INVALIDATE != 0 {
        cmd |= gpu::MI_INVALIDATE_TLB;
        if engine.class == VIDEO_DECODE_CLASS {
            cmd |= gpu::MI_INVALIDATE_BSD;
        }
        if gen12_needs_ccs_aux_inv(engine) && engine.class == COPY_ENGINE_CLASS {
            cmd |= gpu::MI_FLUSH_DW_CCS;
        }
    }
    cs.push(cmd);
    cs.push(LRC_PPHWSP_SCRATCH_ADDR);
    cs.push(0); // upper addr
    cs.push(0); // value
    gen12_emit_aux_table_inv(engine, cs);
    if mode & EMIT_INVALIDATE != 0 {
        cs.push(preparser_disable(false));
    }
}

/// `gen12_emit_flush_rcs` or `_xcs`, by the engine's class.
pub fn gen12_emit_flush(engine: &EngineInfo, mode: u32, cs: &mut Cs<'_>) {
    if engine.class == RENDER_CLASS {
        gen12_emit_flush_rcs(engine, mode, cs);
    } else {
        gen12_emit_flush_xcs(engine, mode, cs);
    }
}

/// `gen8_emit_bb_start`: arbitration on around the batch, so it may be
/// preempted; a batch not `secure` runs from the PPGTT (bit 8).
pub fn gen8_emit_bb_start(cs: &mut Cs<'_>, offset: u64, flags: u32) {
    cs.push(gpu::MI_ARB_ON_OFF | gpu::MI_ARB_ENABLE);
    cs.push(
        gpu::MI_BATCH_BUFFER_START_GEN8
            | if flags & I915_DISPATCH_SECURE != 0 {
                0
            } else {
                1 << 8
            },
    );
    cs.push(offset as u32);
    cs.push((offset >> 32) as u32);
    cs.push(gpu::MI_ARB_ON_OFF | gpu::MI_ARB_DISABLE);
    cs.push(gpu::MI_NOOP);
}

/// `gen8_emit_init_breadcrumb`: write `seqno - 1` to the request's seqno
/// slot, so "started" can be told from "not started" after a hang.
pub fn gen8_emit_init_breadcrumb(cs: &mut Cs<'_>, hwsp_offset: u32, seqno: u32) {
    cs.push(gpu::MI_STORE_DWORD_IMM_GEN4 | gpu::MI_USE_GGTT);
    cs.push(hwsp_offset);
    cs.push(0);
    cs.push(seqno.wrapping_sub(1));
    cs.push(gpu::MI_NOOP);
    cs.push(gpu::MI_ARB_CHECK);
}

/// `gen12_emit_preempt_busywait`: wait while the engine's preemption flag
/// is set.
fn gen12_emit_preempt_busywait(cs: &mut Cs<'_>, preempt_address: u32) {
    cs.push(gpu::MI_ARB_CHECK); // "trigger IDLE->ACTIVE first"
    cs.push(
        gpu::MI_SEMAPHORE_WAIT_TOKEN
            | gpu::MI_SEMAPHORE_GLOBAL_GTT
            | gpu::MI_SEMAPHORE_POLL
            | gpu::MI_SEMAPHORE_SAD_EQ_SDD,
    );
    cs.push(0);
    cs.push(preempt_address);
    cs.push(0);
    cs.push(0);
}

/// Where a request's tail ends: `rq->tail` (after the user interrupt and
/// the busy-wait) and `rq->wa_tail` (after `gen8_emit_wa_tail`'s two dwords),
/// as dword counts into the emitted span.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tails {
    /// `rq->tail`.
    pub tail: usize,
    /// `rq->wa_tail`.
    pub wa_tail: usize,
}

/// `gen12_emit_fini_breadcrumb_tail`: the user interrupt, arbitration on,
/// the preemption busy-wait (execlists have semaphores), then
/// `gen8_emit_wa_tail`'s `MI_ARB_CHECK`, `MI_NOOP` (`WaIdleLiteRestore`).
fn gen12_emit_fini_breadcrumb_tail(cs: &mut Cs<'_>, preempt_address: u32) -> Tails {
    cs.push(gpu::MI_USER_INTERRUPT);
    cs.push(gpu::MI_ARB_ON_OFF | gpu::MI_ARB_ENABLE);
    gen12_emit_preempt_busywait(cs, preempt_address);
    let tail = cs.len();
    cs.push(gpu::MI_ARB_CHECK);
    cs.push(gpu::MI_NOOP);
    Tails {
        tail,
        wa_tail: cs.len(),
    }
}

/// `gen12_emit_fini_breadcrumb_xcs`: "Stalling flush before seqno write;
/// post-sync not".
pub fn gen12_emit_fini_breadcrumb_xcs(
    cs: &mut Cs<'_>,
    seqno: u32,
    hwsp_offset: u32,
    preempt_address: u32,
) -> Tails {
    gen8_emit_flush_dw(cs, 0, 0, 0);
    gen8_emit_ggtt_write(cs, seqno, hwsp_offset, 0);
    gen12_emit_fini_breadcrumb_tail(cs, preempt_address)
}

/// `gen12_emit_fini_breadcrumb_rcs` for ADL-P/N.
pub fn gen12_emit_fini_breadcrumb_rcs(
    cs: &mut Cs<'_>,
    seqno: u32,
    hwsp_offset: u32,
    preempt_address: u32,
) -> Tails {
    let flags = gpu::PIPE_CONTROL_CS_STALL
        | gpu::PIPE_CONTROL_TLB_INVALIDATE
        | gpu::PIPE_CONTROL_TILE_CACHE_FLUSH
        | gpu::PIPE_CONTROL_RENDER_TARGET_CACHE_FLUSH
        | gpu::PIPE_CONTROL_DEPTH_CACHE_FLUSH
        | gpu::PIPE_CONTROL_DC_FLUSH_ENABLE
        | gpu::PIPE_CONTROL_FLUSH_ENABLE
        // GRAPHICS_VER_FULL < 12.70
        | gpu::PIPE_CONTROL_FLUSH_L3
        // Wa_1409600907
        | gpu::PIPE_CONTROL_DEPTH_STALL;
    gen12_emit_pipe_control(cs, gpu::PIPE_CONTROL0_HDC_PIPELINE_FLUSH, flags, 0);
    gen12_emit_ggtt_write_rcs(
        cs,
        seqno,
        hwsp_offset,
        0,
        gpu::PIPE_CONTROL_FLUSH_ENABLE | gpu::PIPE_CONTROL_CS_STALL,
    );
    gen12_emit_fini_breadcrumb_tail(cs, preempt_address)
}
