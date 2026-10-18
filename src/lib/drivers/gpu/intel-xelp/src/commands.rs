// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_gpu_commands.h,
// gt/gen8_engine_cs.h and gt/intel_migrate.c:
// Copyright © 2003-2018 Intel Corporation (intel_gpu_commands.h)
// Copyright © 2014 Intel Corporation (gen8_engine_cs.h)
// Copyright © 2020 Intel Corporation (intel_migrate.c)
// Under the MIT notice in this crate's i915/NOTICE.

//! Command streams: a writer over a ring's or a batch's dwords (i915's
//! `u32 *cs` with `intel_ring_begin`'s bound), the function-like command
//! macros of `intel_gpu_commands.h`, and `gen8_engine_cs.h`'s inline
//! emitters. Object-like constants are generated
//! ([`crate::generated::intel_gpu_commands`]).

pub use crate::generated::intel_gpu_commands as gpu;

/// `u32 *cs` over a reserved span: every emit goes through here, so an
/// emitter that writes more than its span is caught, not a ring overrun.
#[derive(Debug)]
pub struct Cs<'a> {
    dwords: &'a mut [u32],
    at: usize,
    overflowed: bool,
}

impl<'a> Cs<'a> {
    /// A writer over `dwords` (what `intel_ring_begin` reserved).
    pub fn new(dwords: &'a mut [u32]) -> Self {
        Self {
            dwords,
            at: 0,
            overflowed: false,
        }
    }

    /// `*cs++ = dword`.
    pub fn push(&mut self, dword: u32) {
        match self.dwords.get_mut(self.at) {
            Some(slot) => *slot = dword,
            None => self.overflowed = true,
        }
        self.at += 1;
    }

    /// Dwords written.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.at
    }

    /// Nothing written yet.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.at == 0
    }

    /// Whether more was written than the span holds.
    #[must_use]
    pub const fn overflowed(&self) -> bool {
        self.overflowed
    }
}

/// `MI_INSTR(opcode, flags)`.
#[must_use]
pub const fn mi_instr(opcode: u32, flags: u32) -> u32 {
    (opcode << 23) | flags
}

/// `GFX_OP_PIPE_CONTROL(len)`.
#[must_use]
pub const fn gfx_op_pipe_control(len: u32) -> u32 {
    (0x3 << 29) | (0x3 << 27) | (0x2 << 24) | (len - 2)
}

/// `__gen8_emit_pipe_control`: six dwords, the post-sync data zero.
pub fn gen12_emit_pipe_control(cs: &mut Cs<'_>, bit_group_0: u32, bit_group_1: u32, offset: u32) {
    cs.push(gfx_op_pipe_control(6) | bit_group_0);
    cs.push(bit_group_1);
    cs.push(offset);
    for _ in 0..3 {
        cs.push(0);
    }
}

/// `gen8_emit_pipe_control`.
pub fn gen8_emit_pipe_control(cs: &mut Cs<'_>, bit_group_1: u32, offset: u32) {
    gen12_emit_pipe_control(cs, 0, bit_group_1, offset);
}

/// `__gen8_emit_write_rcs`: a `PIPE_CONTROL` that writes `value`.
fn gen8_emit_write_rcs(cs: &mut Cs<'_>, value: u32, offset: u32, flags0: u32, flags1: u32) {
    cs.push(gfx_op_pipe_control(6) | flags0);
    cs.push(flags1 | gpu::PIPE_CONTROL_QW_WRITE);
    cs.push(offset);
    cs.push(0);
    cs.push(value);
    cs.push(0); // "We're thrashing one extra dword."
}

/// `gen12_emit_ggtt_write_rcs`: `value` to a GGTT address, qword-aligned.
pub fn gen12_emit_ggtt_write_rcs(
    cs: &mut Cs<'_>,
    value: u32,
    gtt_offset: u32,
    flags0: u32,
    flags1: u32,
) {
    gen8_emit_write_rcs(
        cs,
        value,
        gtt_offset,
        flags0,
        flags1 | gpu::PIPE_CONTROL_GLOBAL_GTT_IVB,
    );
}

/// `__gen8_emit_flush_dw`.
pub fn gen8_emit_flush_dw(cs: &mut Cs<'_>, value: u32, gtt_offset: u32, flags: u32) {
    cs.push((gpu::MI_FLUSH_DW + 1) | flags);
    cs.push(gtt_offset);
    cs.push(0);
    cs.push(value);
}

/// `gen8_emit_ggtt_write`: `MI_FLUSH_DW` with a store; "bit 5 needs to be
/// zero for `MI_FLUSH_DW` address", and the offset qword-aligned.
pub fn gen8_emit_ggtt_write(cs: &mut Cs<'_>, value: u32, gtt_offset: u32, flags: u32) {
    gen8_emit_flush_dw(
        cs,
        value,
        gtt_offset | gpu::MI_FLUSH_DW_USE_GTT,
        flags | gpu::MI_FLUSH_DW_OP_STOREDW,
    );
}

/// `MI_STORE_DWORD_IMM_GEN4` to a PPGTT address (no `MI_USE_GGTT`): the
/// first command M2 runs from a batch.
pub fn emit_store_dword(cs: &mut Cs<'_>, address: u64, value: u32) {
    cs.push(gpu::MI_STORE_DWORD_IMM_GEN4);
    cs.push(address as u32);
    cs.push((address >> 32) as u32);
    cs.push(value);
}

/// `emit_copy` of `intel_migrate.c` for Gen9+: `size` bytes, whole pages,
/// as a 32-bpp surface one page wide, by `XY_FAST_COPY_BLT` on the copy
/// engine.
pub fn emit_fast_copy(cs: &mut Cs<'_>, dst: u64, src: u64, size: u32) {
    const PAGE_SIZE: u32 = 4096;
    cs.push(gpu::GEN9_XY_FAST_COPY_BLT_CMD | (10 - 2));
    cs.push(gpu::BLT_DEPTH_32 | PAGE_SIZE);
    cs.push(0);
    cs.push(((size / PAGE_SIZE) << 16) | (PAGE_SIZE / 4));
    cs.push(dst as u32);
    cs.push((dst >> 32) as u32);
    cs.push(0);
    cs.push(PAGE_SIZE);
    cs.push(src as u32);
    cs.push((src >> 32) as u32);
}

/// `XY_FAST_COPY_BLT` of a linear 32-bpp rectangle, `width` by `height`
/// pixels, each surface addressed at the rectangle's first pixel with a
/// pitch of its own in bytes: `emit_copy`'s layout, for a rectangle of a
/// larger surface (a plane's).
pub fn emit_rect_copy(
    cs: &mut Cs<'_>,
    dst: u64,
    dst_pitch: u32,
    src: u64,
    src_pitch: u32,
    width: u32,
    height: u32,
) {
    cs.push(gpu::GEN9_XY_FAST_COPY_BLT_CMD | (10 - 2));
    cs.push(gpu::BLT_DEPTH_32 | (dst_pitch & 0xffff));
    cs.push(0);
    cs.push((height << 16) | (width & 0xffff));
    cs.push(dst as u32);
    cs.push((dst >> 32) as u32);
    cs.push(0);
    cs.push(src_pitch & 0xffff);
    cs.push(src as u32);
    cs.push((src >> 32) as u32);
}
