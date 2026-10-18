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
