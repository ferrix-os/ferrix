// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_execlists_submission.c,
// gt/intel_lrc.c, gt/intel_lrc.h, gt/intel_lrc_reg.h and gt/intel_engine.h:
// Copyright © 2014 Intel Corporation (intel_execlists_submission.c, intel_lrc.c)
// Copyright © 2014-2018 Intel Corporation (intel_lrc_reg.h)
// Copyright © 2019 Intel Corporation (intel_engine.h)
// Under the MIT notice in this crate's i915/NOTICE.

//! Execlist submission on Gen12.0, as i915 does it without `GuC`: a context is
//! named by a 64-bit *descriptor* (its image's GGTT address, its addressing
//! mode and a context id), written into the engine's submit queue and loaded
//! with `EL_CTRL_LOAD`. The engine reports each switch in the *context
//! status buffer*, twelve 64-bit entries in the hardware status page, which
//! `process_csb` walks from its read pointer to the engine's write pointer.

use crate::engine::{EL_CTRL_LOAD, ring_execlist_control, ring_execlist_sq_contents};
use crate::reg::{Reg, field_get, genmask};

/// `INTEL_LEGACY_32B_CONTEXT`.
pub const INTEL_LEGACY_32B_CONTEXT: u32 = 1;
/// `INTEL_LEGACY_64B_CONTEXT`: a 4-level PPGTT.
pub const INTEL_LEGACY_64B_CONTEXT: u32 = 3;
/// `GEN8_CTX_VALID`.
pub const GEN8_CTX_VALID: u32 = 1 << 0;
/// `GEN8_CTX_PRIVILEGE`.
pub const GEN8_CTX_PRIVILEGE: u32 = 1 << 8;
/// `GEN8_CTX_ADDRESSING_MODE_SHIFT`.
pub const GEN8_CTX_ADDRESSING_MODE_SHIFT: u32 = 3;
/// `CTX_DESC_FORCE_RESTORE`.
pub const CTX_DESC_FORCE_RESTORE: u64 = 1 << 2;
/// `CTX_GTT_ADDRESS_MASK`.
pub const CTX_GTT_ADDRESS_MASK: u32 = genmask(31, 12);
/// `GEN11_SW_CTX_ID_SHIFT`.
pub const GEN11_SW_CTX_ID_SHIFT: u32 = 37;
/// `GEN11_ENGINE_INSTANCE_SHIFT`.
pub const GEN11_ENGINE_INSTANCE_SHIFT: u32 = 48;
/// `GEN11_ENGINE_CLASS_SHIFT`.
pub const GEN11_ENGINE_CLASS_SHIFT: u32 = 61;
/// `GEN11_MAX_CONTEXT_HW_ID`, exclusive.
pub const GEN11_MAX_CONTEXT_HW_ID: u32 = 1 << 11;
/// `GEN12_MAX_CONTEXT_HW_ID`.
pub const GEN12_MAX_CONTEXT_HW_ID: u32 = GEN11_MAX_CONTEXT_HW_ID - 1;

/// `lrc_descriptor`: the low dword, from the context image's GGTT offset
/// (page-aligned) and whether its VM is 4-level. Gen12 sets no
/// `GEN8_CTX_L3LLC_COHERENT` (Gen8 only).
#[must_use]
pub const fn lrc_descriptor(ggtt_offset: u32, vm_is_4lvl: bool) -> u32 {
    let mode = if vm_is_4lvl {
        INTEL_LEGACY_64B_CONTEXT
    } else {
        INTEL_LEGACY_32B_CONTEXT
    };
    (ggtt_offset & CTX_GTT_ADDRESS_MASK)
        | (mode << GEN8_CTX_ADDRESSING_MODE_SHIFT)
        | GEN8_CTX_VALID
        | GEN8_CTX_PRIVILEGE
}

/// The engine's part of the context id, `execlists->ccid`, as
/// `intel_execlists_submission_setup` makes it for Gen11 to 12.50.
#[must_use]
pub const fn engine_ccid(class: u8, instance: u8) -> u32 {
    ((instance as u32) << (GEN11_ENGINE_INSTANCE_SHIFT - 32))
        | ((class as u32) << (GEN11_ENGINE_CLASS_SHIFT - 32))
}

/// `__execlists_schedule_in`'s `ce->lrc.ccid` below 12.55: the free tag
/// (0 to 62, from `engine->context_tag`) plus one, shifted to the software
/// context id, with the engine's part.
#[must_use]
pub const fn context_ccid(tag: u32, engine_ccid: u32) -> u32 {
    ((1 + tag) << (GEN11_SW_CTX_ID_SHIFT - 32)) | engine_ccid
}

/// `ce->lrc.desc`: `lrca` in the low dword, `ccid` in the high.
#[must_use]
pub const fn descriptor(lrca: u32, ccid: u32) -> u64 {
    lrca as u64 | (ccid as u64) << 32
}

/// `execlists_num_ports`: `port_mask` is 1 (`intel_engine_cs.c`).
pub const NUM_PORTS: u32 = 2;

/// `execlists_submit_ports` with `write_desc` for an engine with the submit
/// queue (`HAS_LOGICAL_RING_ELSQ`): every port is written, the empty ones
/// with 0, from the last to the first ("the submit queue is not cleared
/// after being submitted ... so we always write the same number of elsq
/// entries"), then `EL_CTRL_LOAD`. The writes, in order.
#[must_use]
pub fn submit_ports(
    base: u32,
    ports: [u64; NUM_PORTS as usize],
) -> [(Reg, u32); 2 * NUM_PORTS as usize + 1] {
    let queue = ring_execlist_sq_contents(base).offset();
    let mut writes = [(Reg(0), 0); 2 * NUM_PORTS as usize + 1];
    let mut at = 0;
    for port in (0..NUM_PORTS).rev() {
        let desc = ports.get(port as usize).copied().unwrap_or(0);
        if let Some(slot) = writes.get_mut(at) {
            *slot = (Reg(queue + port * 8), desc as u32);
        }
        if let Some(slot) = writes.get_mut(at + 1) {
            *slot = (Reg(queue + port * 8 + 4), (desc >> 32) as u32);
        }
        at += 2;
    }
    if let Some(slot) = writes.get_mut(at) {
        *slot = (ring_execlist_control(base), EL_CTRL_LOAD);
    }
    writes
}

/// `I915_HWS_CSB_BUF0_INDEX`: the status buffer's first dword in the HWSP.
pub const I915_HWS_CSB_BUF0_INDEX: usize = 0x10;
/// `ICL_HWS_CSB_WRITE_INDEX`: the dword the engine writes its CSB write
/// pointer to, Gen11+.
pub const ICL_HWS_CSB_WRITE_INDEX: usize = 0x2f;
/// `GEN11_CSB_ENTRIES`.
pub const GEN11_CSB_ENTRIES: u32 = 12;
/// `GEN11_CSB_PTR_MASK`.
pub const GEN11_CSB_PTR_MASK: u32 = 0xf;
/// `GEN11_CSB_READ_PTR_MASK`, in `RING_CONTEXT_STATUS_PTR`.
pub const GEN11_CSB_READ_PTR_MASK: u32 = GEN11_CSB_PTR_MASK << 8;
/// `GEN11_CSB_WRITE_PTR_MASK`.
pub const GEN11_CSB_WRITE_PTR_MASK: u32 = GEN11_CSB_PTR_MASK;

/// `GEN12_CTX_STATUS_SWITCHED_TO_NEW_QUEUE`, in the lower dword.
pub const GEN12_CTX_STATUS_SWITCHED_TO_NEW_QUEUE: u32 = 0x1;
/// `GEN12_CSB_SW_CTX_ID_MASK`.
pub const GEN12_CSB_SW_CTX_ID_MASK: u32 = genmask(25, 15);
/// `GEN12_IDLE_CTX_ID`.
pub const GEN12_IDLE_CTX_ID: u32 = 0x7ff;

/// `GEN12_CSB_CTX_VALID(dw)`.
#[must_use]
pub const fn gen12_csb_ctx_valid(dw: u32) -> bool {
    field_get(GEN12_CSB_SW_CTX_ID_MASK, dw) != GEN12_IDLE_CTX_ID
}

/// `GEN12_CTX_SWITCH_DETAIL(dw)`, in the upper dword.
#[must_use]
pub const fn gen12_ctx_switch_detail(dw: u32) -> u32 {
    dw & 0xf
}

/// What a CSB entry means for the ports, by `gen12_csb_parse`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CsbEvent {
    /// `true` from `gen12_csb_parse`: the pending ports were promoted to
    /// active (a new queue took effect).
    Promote,
    /// `false`: the active port completed (or was preempted to idle).
    Complete,
    /// Neither, which i915 calls a bug (`GEM_BUG_ON`): a context going away
    /// with a switch detail, or a promotion with no valid context to.
    Unexpected {
        /// The entry.
        csb: u64,
    },
}

/// `gen12_csb_parse` with `__gen12_csb_parse`; the cases i915 asserts
/// against are reported, not trusted.
#[must_use]
pub const fn gen12_csb_parse(csb: u64) -> CsbEvent {
    let lower = csb as u32;
    let upper = (csb >> 32) as u32;
    let ctx_to_valid = gen12_csb_ctx_valid(lower);
    let ctx_away_valid = gen12_csb_ctx_valid(upper);
    let new_queue = lower & GEN12_CTX_STATUS_SWITCHED_TO_NEW_QUEUE != 0;
    if !ctx_away_valid || new_queue {
        if ctx_to_valid {
            CsbEvent::Promote
        } else {
            CsbEvent::Unexpected { csb }
        }
    } else if gen12_ctx_switch_detail(upper) != 0 {
        CsbEvent::Unexpected { csb }
    } else {
        CsbEvent::Complete
    }
}

/// The CSB entries `process_csb` reads, from the one after `head` (the last
/// read) to `tail` (the engine's write pointer) inclusive, wrapping at
/// [`GEN11_CSB_ENTRIES`]. `None` for a pointer out of range, which i915
/// treats as a hardware fault and resets.
pub fn csb_indices(head: u32, tail: u32) -> Option<impl Iterator<Item = u32>> {
    if head >= GEN11_CSB_ENTRIES || tail >= GEN11_CSB_ENTRIES {
        return None;
    }
    let count = (tail + GEN11_CSB_ENTRIES - head) % GEN11_CSB_ENTRIES;
    Some((1..=count).map(move |step| (head + step) % GEN11_CSB_ENTRIES))
}
