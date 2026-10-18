// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0,
// drivers/gpu/drm/i915/display/skl_universal_plane_regs.h:
// Copyright © 2024 Intel Corporation
// Under the MIT notice in this crate's i915/NOTICE.

//! Render-only touches no display register but these, and only reads them:
//! where the firmware's lit plane scans out from, so the GGTT entries
//! behind it are found and left alone (`docs/INTEL-GPU.md` §3.4, §4.3).

use crate::io::Mmio;
use crate::reg::Reg;

/// `_PLANE_CTL_1_A`.
pub const PLANE_CTL_1_A: u32 = 0x7_0180;
/// `_PLANE_STRIDE_1_A`.
pub const PLANE_STRIDE_1_A: u32 = 0x7_0188;
/// `_PLANE_SIZE_1_A`.
pub const PLANE_SIZE_1_A: u32 = 0x7_0190;
/// `_PLANE_SURF_1_A`.
pub const PLANE_SURF_1_A: u32 = 0x7_019c;
/// `PLANE_CTL_ENABLE`.
pub const PLANE_CTL_ENABLE: u32 = 1 << 31;
/// `_PIPE(pipe, _PIPEA, _PIPEB)`'s step for the universal planes: pipe B's
/// plane 1 is 0x1000 after pipe A's (`_PLANE_CTL_1_B` is 0x71180).
pub const PIPE_STEP: u32 = 0x1000;
/// The pipes ADL-P has.
pub const PIPES: u32 = 4;

/// Plane 1 of a pipe, as read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Plane {
    /// The pipe, 0 for A.
    pub pipe: u32,
    /// `PLANE_CTL`.
    pub ctl: u32,
    /// `PLANE_STRIDE`.
    pub stride: u32,
    /// `PLANE_SIZE`: (height - 1) << 16 | (width - 1).
    pub size: u32,
    /// `PLANE_SURF`: the surface's GGTT offset in bits 31:12.
    pub surf: u32,
}

impl Plane {
    /// Whether it is scanning out.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.ctl & PLANE_CTL_ENABLE != 0
    }

    /// The surface's GGTT offset.
    #[must_use]
    pub const fn ggtt_offset(&self) -> u32 {
        self.surf & !0xfff
    }

    /// Width and height in pixels.
    #[must_use]
    pub const fn dimensions(&self) -> (u32, u32) {
        ((self.size & 0xffff) + 1, (self.size >> 16) + 1)
    }
}

/// Read plane 1 of `pipe`.
pub fn read_plane1<M: Mmio + ?Sized>(mmio: &mut M, pipe: u32) -> Plane {
    let at = |base: u32| Reg(base + pipe * PIPE_STEP).offset();
    Plane {
        pipe,
        ctl: mmio.read32(at(PLANE_CTL_1_A)),
        stride: mmio.read32(at(PLANE_STRIDE_1_A)),
        size: mmio.read32(at(PLANE_SIZE_1_A)),
        surf: mmio.read32(at(PLANE_SURF_1_A)),
    }
}
