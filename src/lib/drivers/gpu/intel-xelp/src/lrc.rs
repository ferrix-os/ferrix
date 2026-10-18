// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_lrc.c,
// gt/intel_lrc_reg.h and gt/intel_gpu_commands.h:
// Copyright © 2014 Intel Corporation (intel_lrc.c)
// Copyright © 2014-2018 Intel Corporation (intel_lrc_reg.h)
// Copyright © 2003-2018 Intel Corporation (intel_gpu_commands.h)
// Under the MIT notice in this crate's i915/NOTICE.

//! The logical ring context image: what the engine saves and restores per
//! context. Its register state is a series of `MI_LOAD_REGISTER_IMM`s whose
//! register offsets i915 keeps as byte code per engine class
//! (`gen12_rcs_offsets`, `gen12_xcs_offsets`, generated into
//! [`crate::generated`]). `set_offsets` turns that code into the image's
//! headers and offsets; the values are filled in by `init_common_regs` and
//! its siblings.

use crate::generated::intel_lrc::{GEN12_RCS_OFFSETS, GEN12_XCS_OFFSETS};

/// `MI_LOAD_REGISTER_IMM(x)`: `MI_INSTR(0x22, 2*(x)-1)`.
#[must_use]
pub const fn mi_load_register_imm(count: u32) -> u32 {
    (0x22 << 23) | (2 * count - 1)
}

/// `MI_LRI_FORCE_POSTED`.
pub const MI_LRI_FORCE_POSTED: u32 = 1 << 12;
/// `MI_LRI_LRM_CS_MMIO`: offsets are relative to the engine (Gen11+).
pub const MI_LRI_LRM_CS_MMIO: u32 = 1 << 19;
/// `MI_BATCH_BUFFER_END`: `MI_INSTR(0x0a, 0)`.
pub const MI_BATCH_BUFFER_END: u32 = 0x0a << 23;

/// `intel_lrc_reg.h`'s indices into the register state: each names the
/// *value* dword, one past the register's offset.
pub mod ctx {
    /// `CTX_CONTEXT_CONTROL`.
    pub const CONTEXT_CONTROL: usize = 0x02 + 1;
    /// `CTX_RING_HEAD`.
    pub const RING_HEAD: usize = 0x04 + 1;
    /// `CTX_RING_TAIL`.
    pub const RING_TAIL: usize = 0x06 + 1;
    /// `CTX_RING_START`.
    pub const RING_START: usize = 0x08 + 1;
    /// `CTX_RING_CTL`.
    pub const RING_CTL: usize = 0x0a + 1;
    /// `CTX_BB_STATE`.
    pub const BB_STATE: usize = 0x10 + 1;
    /// `CTX_TIMESTAMP`.
    pub const TIMESTAMP: usize = 0x22 + 1;
    /// `CTX_PDP3_UDW`.
    pub const PDP3_UDW: usize = 0x24 + 1;
    /// `CTX_PDP3_LDW`.
    pub const PDP3_LDW: usize = 0x26 + 1;
    /// `CTX_PDP2_UDW`.
    pub const PDP2_UDW: usize = 0x28 + 1;
    /// `CTX_PDP2_LDW`.
    pub const PDP2_LDW: usize = 0x2a + 1;
    /// `CTX_PDP1_UDW`.
    pub const PDP1_UDW: usize = 0x2c + 1;
    /// `CTX_PDP1_LDW`.
    pub const PDP1_LDW: usize = 0x2e + 1;
    /// `CTX_PDP0_UDW`.
    pub const PDP0_UDW: usize = 0x30 + 1;
    /// `CTX_PDP0_LDW`.
    pub const PDP0_LDW: usize = 0x32 + 1;
    /// `CTX_R_PWR_CLK_STATE`.
    pub const R_PWR_CLK_STATE: usize = 0x42 + 1;
}

/// `lrc_ring_mi_mode` for Gen12.0: the index of `RING_MI_MODE`'s offset
/// in the register state, its value one after.
#[must_use]
pub const fn lrc_ring_mi_mode() -> usize {
    0x60
}

/// The engine classes whose register state has a table of its own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OffsetsTable {
    /// `I915_ENGINE_HAS_RCS_REG_STATE`: `gen12_rcs_offsets`.
    Rcs,
    /// Every other engine: `gen12_xcs_offsets`.
    Xcs,
}

/// `reg_offsets` for Gen12.0.
#[must_use]
pub fn reg_offsets(table: OffsetsTable) -> &'static [u8] {
    match table {
        OffsetsTable::Rcs => &GEN12_RCS_OFFSETS,
        OffsetsTable::Xcs => &GEN12_XCS_OFFSETS,
    }
}

/// Why a table could not be applied.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OffsetsError {
    /// The table ran past the end of the image's register state.
    TooSmall,
    /// The table ended without `END`, or inside an offset.
    Truncated,
    /// `LRI(0, ...)`, which i915's `GEM_BUG_ON(!count)` refuses.
    EmptyLri,
}

/// `set_offsets`: write the `MI_LOAD_REGISTER_IMM` headers and the register
/// offsets (the engine's `base` plus the table's offset) into `regs`,
/// skipping the dwords `NOP(n)` names and leaving each value dword as it was.
/// With `close`, end with `MI_BATCH_BUFFER_END | BIT(0)`, as
/// `live_lrc_layout` wants. Returns the dwords used.
pub fn set_offsets(
    regs: &mut [u32],
    data: &[u8],
    base: u32,
    close: bool,
) -> Result<usize, OffsetsError> {
    let mut at = 0_usize;
    let mut bytes = data.iter().copied();
    loop {
        let first = bytes.next().ok_or(OffsetsError::Truncated)?;
        if first == 0 {
            break;
        }
        if first & 0x80 != 0 {
            at += usize::from(first & 0x7f);
            continue;
        }
        let count = first & 0x3f;
        let flags = first >> 6;
        if count == 0 {
            return Err(OffsetsError::EmptyLri);
        }
        let mut header = mi_load_register_imm(u32::from(count));
        if flags & 1 != 0 {
            header |= MI_LRI_FORCE_POSTED;
        }
        // GRAPHICS_VER >= 11
        header |= MI_LRI_LRM_CS_MMIO;
        *regs.get_mut(at).ok_or(OffsetsError::TooSmall)? = header;
        at += 1;
        for _ in 0..count {
            let mut offset: u32 = 0;
            loop {
                let byte = bytes.next().ok_or(OffsetsError::Truncated)?;
                offset = (offset << 7) | u32::from(byte & 0x7f);
                if byte & 0x80 == 0 {
                    break;
                }
            }
            *regs.get_mut(at).ok_or(OffsetsError::TooSmall)? = base + (offset << 2);
            at += 2;
        }
    }
    if close {
        *regs.get_mut(at).ok_or(OffsetsError::TooSmall)? = MI_BATCH_BUFFER_END | 1;
        at += 1;
    }
    Ok(at)
}

/// `LRC_PPHWSP_PN`: the per-process status page is the image's first page.
pub const LRC_PPHWSP_PN: u32 = 0;
/// `LRC_STATE_PN`: the register state is the second.
pub const LRC_STATE_PN: u32 = 1;
/// `LRC_STATE_OFFSET`.
pub const LRC_STATE_OFFSET: u32 = LRC_STATE_PN * 4096;
/// `GEN11_LR_CONTEXT_RENDER_SIZE`, which Gen12 uses: 14 pages, the status
/// page included.
pub const GEN11_LR_CONTEXT_RENDER_SIZE: u32 = 14 * 4096;
/// `GEN8_LR_CONTEXT_OTHER_SIZE`.
pub const GEN8_LR_CONTEXT_OTHER_SIZE: u32 = 2 * 4096;
/// `CACHELINE_BYTES`.
pub const CACHELINE_BYTES: u32 = 64;

/// `intel_engine_context_size` for Gen12.
#[must_use]
pub const fn context_size(table: OffsetsTable) -> u32 {
    match table {
        OffsetsTable::Rcs => GEN11_LR_CONTEXT_RENDER_SIZE,
        OffsetsTable::Xcs => GEN8_LR_CONTEXT_OTHER_SIZE,
    }
}

/// `__lrc_alloc_state` on Gen12: the context's size and the page its
/// indirect-context batch starts on (`ce->wa_bb_page`); "`INDIRECT_CTX` and
/// `PER_CTX_BB` need separate pages".
#[must_use]
pub const fn alloc_state_size(table: OffsetsTable) -> (u32, u32) {
    let size = context_size(table);
    (size + 2 * 4096, size / 4096)
}

/// `CTX_CTRL_ENGINE_CTX_RESTORE_INHIBIT`.
pub const CTX_CTRL_ENGINE_CTX_RESTORE_INHIBIT: u32 = 1 << 0;
/// `CTX_CTRL_INHIBIT_SYN_CTX_SWITCH`.
pub const CTX_CTRL_INHIBIT_SYN_CTX_SWITCH: u32 = 1 << 3;
/// `RING_VALID`.
pub const RING_VALID: u32 = 1;
/// `STOP_RING`, in `RING_MI_MODE`.
pub const STOP_RING: u32 = 1 << 8;
/// `GEN12_CTX_RCS_INDIRECT_CTX_OFFSET_DEFAULT`.
pub const GEN12_CTX_RCS_INDIRECT_CTX_OFFSET_DEFAULT: u32 = 0xd;

/// `RING_CTL_SIZE(size)`: bytes to the register's encoding.
#[must_use]
pub const fn ring_ctl_size(size: u32) -> u32 {
    size - 4096
}

/// `lrc_ring_wa_bb_per_ctx` on Gen12.
pub const LRC_RING_WA_BB_PER_CTX: usize = 0x12;
/// `lrc_ring_indirect_ptr`.
pub const LRC_RING_INDIRECT_PTR: usize = LRC_RING_WA_BB_PER_CTX + 2;
/// `lrc_ring_indirect_offset`.
pub const LRC_RING_INDIRECT_OFFSET: usize = LRC_RING_INDIRECT_PTR + 2;
/// `lrc_ring_bb_offset` on Gen12.
pub const LRC_RING_BB_OFFSET: usize = 0x70;
/// `lrc_ring_gpr0` on Gen12.
pub const LRC_RING_GPR0: usize = 0x74;
/// `lrc_ring_cmd_buf_cctl` on Gen12, render only.
pub const LRC_RING_CMD_BUF_CCTL: usize = 0xb6;

/// `init_common_regs`: context control (synchronous switches inhibited;
/// restore inhibited for a first image, as `inhibit` says), the timestamp,
/// and the batch-buffer offset cleared. `last_runtime` is
/// `ce->stats.runtime.last`, 0 for a new context.
pub fn init_common_regs(regs: &mut [u32], inhibit: bool, last_runtime: u32) {
    use crate::reg::{masked_bit_disable, masked_bit_enable};
    let mut ctl = masked_bit_enable(CTX_CTRL_INHIBIT_SYN_CTX_SWITCH);
    ctl |= masked_bit_disable(CTX_CTRL_ENGINE_CTX_RESTORE_INHIBIT);
    if inhibit {
        ctl |= CTX_CTRL_ENGINE_CTX_RESTORE_INHIBIT;
    }
    put(regs, ctx::CONTEXT_CONTROL, ctl);
    put(regs, ctx::TIMESTAMP, last_runtime);
    put(regs, LRC_RING_BB_OFFSET + 1, 0);
}

/// `init_ppgtt_regs` with `ASSIGN_CTX_PML4`: a 4-level VM's PML4 address in
/// `PDP0`, the other descriptors ignored.
pub fn init_ppgtt_regs(regs: &mut [u32], pml4: u64) {
    put(regs, ctx::PDP0_UDW, (pml4 >> 32) as u32);
    put(regs, ctx::PDP0_LDW, pml4 as u32);
}

/// `__reset_stop_ring`: the image's `RING_MI_MODE` with `STOP_RING` cleared.
pub fn reset_stop_ring(regs: &mut [u32]) {
    let at = lrc_ring_mi_mode() + 1;
    if let Some(value) = regs.get_mut(at) {
        *value &= !STOP_RING;
        *value |= STOP_RING << 16;
    }
}

/// `__lrc_init_regs` for a 4-level VM, without a global indirect batch
/// (`init_wa_bb_regs` has nothing on Gen11+: `lrc_init_wa_ctx` returns).
/// `regs` is the register-state page.
pub fn lrc_init_regs(
    regs: &mut [u32],
    table: OffsetsTable,
    base: u32,
    inhibit: bool,
    pml4: u64,
) -> Result<(), OffsetsError> {
    if inhibit {
        regs.fill(0);
    }
    let _ = set_offsets(regs, reg_offsets(table), base, inhibit)?;
    init_common_regs(regs, inhibit, 0);
    init_ppgtt_regs(regs, pml4);
    reset_stop_ring(regs);
    Ok(())
}

/// `intel_sseu_make_rpcs` for Gen12 with every slice requested: only
/// slice power gating (`has_slice_pg`); no subslice or EU gating.
#[must_use]
pub const fn make_rpcs(slices: u32) -> u32 {
    const GEN8_RPCS_ENABLE: u32 = 1 << 31;
    const GEN8_RPCS_S_CNT_ENABLE: u32 = 1 << 18;
    const GEN11_RPCS_S_CNT_SHIFT: u32 = 12;
    const GEN11_RPCS_S_CNT_MASK: u32 = 0x3f << GEN11_RPCS_S_CNT_SHIFT;
    GEN8_RPCS_ENABLE
        | GEN8_RPCS_S_CNT_ENABLE
        | ((slices << GEN11_RPCS_S_CNT_SHIFT) & GEN11_RPCS_S_CNT_MASK)
}

/// A context's ring, as `lrc_update_regs` writes it into the image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RingState {
    /// `i915_ggtt_offset(ring->vma)`.
    pub start: u32,
    /// The head to resume at.
    pub head: u32,
    /// `ring->tail`.
    pub tail: u32,
    /// `ring->size` in bytes.
    pub size: u32,
}

/// `lrc_update_regs`: the ring (GGTT address, head, tail, size) and, on the
/// render engine, `R_PWR_CLK_STATE`; then `lrc_setup_indirect_ctx` for the
/// Gen12 indirect batch at `indirect_bb` (GGTT) of `indirect_size` bytes.
pub fn lrc_update_regs(
    regs: &mut [u32],
    ring: RingState,
    render: bool,
    indirect_bb: u32,
    indirect_size: u32,
) {
    put(regs, ctx::RING_START, ring.start);
    put(regs, ctx::RING_HEAD, ring.head);
    put(regs, ctx::RING_TAIL, ring.tail);
    put(regs, ctx::RING_CTL, ring_ctl_size(ring.size) | RING_VALID);
    if render {
        put(regs, ctx::R_PWR_CLK_STATE, make_rpcs(1));
    }
    // lrc_setup_indirect_ctx
    put(
        regs,
        LRC_RING_INDIRECT_PTR + 1,
        indirect_bb | (indirect_size / CACHELINE_BYTES),
    );
    put(
        regs,
        LRC_RING_INDIRECT_OFFSET + 1,
        GEN12_CTX_RCS_INDIRECT_CTX_OFFSET_DEFAULT << 6,
    );
}

fn put(regs: &mut [u32], at: usize, value: u32) {
    if let Some(slot) = regs.get_mut(at) {
        *slot = value;
    }
}

/// `DG2_PREDICATE_RESULT_WA`: the dword the predicate workaround stores to,
/// the indirect page's last qword.
pub const DG2_PREDICATE_RESULT_WA: u32 = 4096 - 8;
/// `DG2_PREDICATE_RESULT_BB`: where in the indirect page its batch starts.
pub const DG2_PREDICATE_RESULT_BB: u32 = 2048;
/// `RING_CTX_TIMESTAMP(0)`, relative (`MI_LRI_LRM_CS_MMIO`).
const RING_CTX_TIMESTAMP_0: u32 = 0x3a8;
/// `RING_CMD_BUF_CCTL(0)`.
const RING_CMD_BUF_CCTL_0: u32 = 0x84;
/// `GEN8_RING_CS_GPR(0, 0)`.
const GEN8_RING_CS_GPR_0_0: u32 = 0x600;
/// `GEN12_CS_DEBUG_MODE2`.
const GEN12_CS_DEBUG_MODE2: u32 = 0x20d8;
/// `INSTRUCTION_STATE_CACHE_INVALIDATE`.
const INSTRUCTION_STATE_CACHE_INVALIDATE: u32 = 1 << 6;

/// `setup_indirect_ctx_bb` with `gen12_emit_indirect_ctx_rcs` or `_xcs`, for
/// a context whose image is at `ctx_ggtt` in the GGTT: the batch the engine
/// runs on every restore of this context, written into `page` (the
/// image's indirect page, `context_wabb(ce, false)`), padded to a cache line;
/// and `setup_predicate_disable_wa` at its byte 2048, which i915 writes for
/// every Gen12 context. Returns the batch's size in bytes, for
/// [`lrc_update_regs`].
pub fn setup_indirect_ctx_bb(
    page: &mut [u32],
    engine: &crate::engine::EngineInfo,
    ctx_ggtt: u32,
) -> Option<u32> {
    use crate::commands::{Cs, gpu};
    use crate::reg::masked_bit_enable;
    let render = engine.class == crate::engine::RENDER_CLASS;
    let state = ctx_ggtt + LRC_STATE_OFFSET;
    let indirect_bb = ctx_ggtt
        + (alloc_state_size(if render {
            OffsetsTable::Rcs
        } else {
            OffsetsTable::Xcs
        })
        .1 * 4096);
    let lrm = gpu::MI_LOAD_REGISTER_MEM_GEN8 | gpu::MI_SRM_LRM_GLOBAL_GTT | MI_LRI_LRM_CS_MMIO;
    let lrr = gpu::MI_LOAD_REGISTER_REG | gpu::MI_LRR_SOURCE_CS_MMIO | MI_LRI_LRM_CS_MMIO;
    let (head, tail) = page.split_at_mut((DG2_PREDICATE_RESULT_BB / 4) as usize);
    let mut cs = Cs::new(head);
    // gen12_emit_timestamp_wa
    for dword in [
        lrm,
        GEN8_RING_CS_GPR_0_0,
        state + (ctx::TIMESTAMP as u32) * 4,
        0,
    ] {
        cs.push(dword);
    }
    for _ in 0..2 {
        for dword in [lrr, GEN8_RING_CS_GPR_0_0, RING_CTX_TIMESTAMP_0] {
            cs.push(dword);
        }
    }
    if render {
        // gen12_emit_cmd_buf_wa
        for dword in [
            lrm,
            GEN8_RING_CS_GPR_0_0,
            state + (LRC_RING_CMD_BUF_CCTL as u32 + 1) * 4,
            0,
        ] {
            cs.push(dword);
        }
        for dword in [lrr, GEN8_RING_CS_GPR_0_0, RING_CMD_BUF_CCTL_0] {
            cs.push(dword);
        }
    }
    // gen12_emit_restore_scratch
    for dword in [
        lrm,
        GEN8_RING_CS_GPR_0_0,
        state + (LRC_RING_GPR0 as u32 + 1) * 4,
        0,
    ] {
        cs.push(dword);
    }
    crate::engine_cs::gen12_emit_aux_table_inv(engine, &mut cs);
    if render {
        // Wa_18022495364: IP 12.0 to 12.10, gen12_invalidate_state_cache
        cs.push(mi_load_register_imm(1));
        cs.push(GEN12_CS_DEBUG_MODE2);
        cs.push(masked_bit_enable(INSTRUCTION_STATE_CACHE_INVALIDATE));
    }
    while !(cs.len() * 4).is_multiple_of(CACHELINE_BYTES as usize) {
        cs.push(gpu::MI_NOOP);
    }
    if cs.overflowed() {
        return None;
    }
    let size = (cs.len() * 4) as u32;
    // setup_predicate_disable_wa
    let mut wa = Cs::new(tail);
    for dword in [
        gpu::MI_STORE_DWORD_IMM_GEN4 | gpu::MI_USE_GGTT | (4 - 2),
        indirect_bb + DG2_PREDICATE_RESULT_WA,
        0,
        0, // "No predication"
        gpu::MI_BATCH_BUFFER_END | (1 << 15),
        gpu::MI_SET_PREDICATE | gpu::MI_SET_PREDICATE_DISABLE,
        gpu::MI_STORE_DWORD_IMM_GEN4 | gpu::MI_USE_GGTT | (4 - 2),
        indirect_bb + DG2_PREDICATE_RESULT_WA,
        0,
        1, // "enable predication before the next BB"
        gpu::MI_BATCH_BUFFER_END,
    ] {
        wa.push(dword);
    }
    (!wa.overflowed()).then_some(size)
}

/// Dwords in a page.
const PAGE_DWORDS: usize = 1024;

/// `lrc_init_state` for a context object of [`alloc_state_size`] bytes,
/// `image` as dwords: the recorded default image copied in when there is
/// one (`ce->default_state`, `engine->context_size` bytes), the per-process
/// status page and the indirect-context page cleared, then
/// [`lrc_init_regs`] on the register-state page, restore inhibited only
/// when there was no default ("The second page of the context object
/// contains some registers which must be set up prior to the first
/// execution"). Returns whether restore was inhibited.
///
/// # Errors
///
/// [`OffsetsError::TooSmall`] for an image or default shorter than the
/// engine's; the table's own errors.
pub fn lrc_init_state(
    image: &mut [u32],
    default: Option<&[u32]>,
    table: OffsetsTable,
    base: u32,
    pml4: u64,
) -> Result<bool, OffsetsError> {
    let (size, wa_bb_page) = alloc_state_size(table);
    let context_dwords = (context_size(table) / 4) as usize;
    let image = image
        .get_mut(..(size / 4) as usize)
        .ok_or(OffsetsError::TooSmall)?;
    let inhibit = match default {
        Some(default) => {
            let from = default
                .get(..context_dwords)
                .ok_or(OffsetsError::TooSmall)?;
            image
                .get_mut(..context_dwords)
                .ok_or(OffsetsError::TooSmall)?
                .copy_from_slice(from);
            false
        }
        None => true,
    };
    // "Clear the ppHWSP (inc. per-context counters)"
    image
        .get_mut(..PAGE_DWORDS)
        .ok_or(OffsetsError::TooSmall)?
        .fill(0);
    // "Clear the indirect wa and storage"
    let wa = wa_bb_page as usize * PAGE_DWORDS;
    image
        .get_mut(wa..wa + PAGE_DWORDS)
        .ok_or(OffsetsError::TooSmall)?
        .fill(0);
    let state = LRC_STATE_PN as usize * PAGE_DWORDS;
    let regs = image
        .get_mut(state..state + PAGE_DWORDS)
        .ok_or(OffsetsError::TooSmall)?;
    lrc_init_regs(regs, table, base, inhibit, pml4)?;
    Ok(inhibit)
}

/// The image's register-state page, as dwords: where [`lrc_update_regs`]
/// and the submission write `CTX_RING_TAIL` (`ce->lrc_reg_state`).
#[must_use]
pub fn reg_state(image: &mut [u32]) -> Option<&mut [u32]> {
    let state = LRC_STATE_PN as usize * PAGE_DWORDS;
    image.get_mut(state..state + PAGE_DWORDS)
}

/// `__engines_record_defaults`' copy: once the first context on an engine
/// has run (its image restore-inhibited, its first request carrying the
/// context workarounds) and the engine is idle, the context's saved image,
/// `engine->context_size` bytes from its start, is the engine's default
/// state, which every later context starts from.
///
/// # Errors
///
/// [`OffsetsError::TooSmall`] for an image or a destination shorter than
/// the engine's context.
pub fn record_default_state(
    image: &[u32],
    default: &mut [u32],
    table: OffsetsTable,
) -> Result<(), OffsetsError> {
    let context_dwords = (context_size(table) / 4) as usize;
    let from = image.get(..context_dwords).ok_or(OffsetsError::TooSmall)?;
    default
        .get_mut(..context_dwords)
        .ok_or(OffsetsError::TooSmall)?
        .copy_from_slice(from);
    Ok(())
}

/// The ring half of [`lrc_update_regs`], which `execlists_reset_active`
/// needs to restart a context's ring at a request's head after a reset.
pub fn lrc_update_regs_ring(regs: &mut [u32], ring: RingState) {
    put(regs, ctx::RING_START, ring.start);
    put(regs, ctx::RING_HEAD, ring.head);
    put(regs, ctx::RING_TAIL, ring.tail);
    put(regs, ctx::RING_CTL, ring_ctl_size(ring.size) | RING_VALID);
}
