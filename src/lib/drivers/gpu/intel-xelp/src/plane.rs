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

// From Linux v7.0's drivers/gpu/drm/i915/display/intel_display_regs.h and
// intel_pps_regs.h, under the same notice: what drives pipe A's picture to
// a socket, read only, to name the connector (`docs/DISPLAY.md` §9.4).

/// `_TRANS_DDI_FUNC_CTL_A`: transcoder A's DDI function control.
pub const TRANS_DDI_FUNC_CTL_A: u32 = 0x6_0400;
/// `TRANS_DDI_FUNC_ENABLE`.
pub const TRANS_DDI_FUNC_ENABLE: u32 = 1 << 31;
/// `TRANS_DDI_MODE_SELECT_MASK`'s shift: bits 26:24.
pub const TRANS_DDI_MODE_SELECT_SHIFT: u32 = 24;
/// `TRANS_DDI_MODE_SELECT_HDMI`, `_DVI`, `_DP_SST` and `_DP_MST`.
pub const TRANS_DDI_MODE_HDMI: u32 = 0;
/// .
pub const TRANS_DDI_MODE_DVI: u32 = 1;
/// .
pub const TRANS_DDI_MODE_DP_SST: u32 = 2;
/// .
pub const TRANS_DDI_MODE_DP_MST: u32 = 3;
/// `PP_STATUS` at the PCH's panel power sequencer, `PCH_PPS_BASE`
/// (0xC7200), which ICL and later have.
pub const PCH_PP_STATUS: u32 = 0xC_7200;
/// `PP_ON`: the panel is powered.
pub const PP_ON: u32 = 1 << 31;

/// DRM's connector types the output can be (`DRM_MODE_CONNECTOR_*`).
pub const DRM_CONNECTOR_DVID: u32 = 3;
/// .
pub const DRM_CONNECTOR_DISPLAYPORT: u32 = 10;
/// .
pub const DRM_CONNECTOR_HDMIA: u32 = 11;
/// .
pub const DRM_CONNECTOR_EDP: u32 = 14;

/// What drives pipe A's picture out, as read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Output {
    /// `TRANS_DDI_FUNC_CTL` of transcoder A.
    pub ddi_func_ctl: u32,
    /// The PCH panel power sequencer's `PP_STATUS`.
    pub pp_status: u32,
}

impl Output {
    /// DRM's connector type for the socket the lit picture goes to: HDMI,
    /// DVI, or `DisplayPort` -- eDP where the panel power sequencer has a
    /// panel on, which only an embedded panel has -- or 0 where the
    /// transcoder is off or in a mode none of these is, which leaves the
    /// display core its own choice.
    #[must_use]
    pub const fn drm_connector(&self) -> u32 {
        if self.ddi_func_ctl & TRANS_DDI_FUNC_ENABLE == 0 {
            return 0;
        }
        match (self.ddi_func_ctl >> TRANS_DDI_MODE_SELECT_SHIFT) & 0x7 {
            TRANS_DDI_MODE_HDMI => DRM_CONNECTOR_HDMIA,
            TRANS_DDI_MODE_DVI => DRM_CONNECTOR_DVID,
            TRANS_DDI_MODE_DP_SST | TRANS_DDI_MODE_DP_MST if self.pp_status & PP_ON != 0 => {
                DRM_CONNECTOR_EDP
            }
            TRANS_DDI_MODE_DP_SST | TRANS_DDI_MODE_DP_MST => DRM_CONNECTOR_DISPLAYPORT,
            _ => 0,
        }
    }
}

/// Read what drives pipe A's picture out. Only while the firmware has pipe
/// A lit, which is when its power well is on (`read_plane1`'s rule).
pub fn read_output<M: Mmio + ?Sized>(mmio: &mut M) -> Output {
    Output {
        ddi_func_ctl: mmio.read32(Reg(TRANS_DDI_FUNC_CTL_A).offset()),
        pp_status: mmio.read32(Reg(PCH_PP_STATUS).offset()),
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;

    #[test]
    fn the_lit_output_is_named_by_its_ddi_mode_and_the_panel_power() {
        let output = |mode: u32, enabled: bool, panel: bool| Output {
            ddi_func_ctl: (mode << TRANS_DDI_MODE_SELECT_SHIFT)
                | if enabled { TRANS_DDI_FUNC_ENABLE } else { 0 },
            pp_status: if panel { PP_ON } else { 0 },
        };
        assert_eq!(
            output(TRANS_DDI_MODE_HDMI, true, false).drm_connector(),
            DRM_CONNECTOR_HDMIA
        );
        assert_eq!(
            output(TRANS_DDI_MODE_DVI, true, false).drm_connector(),
            DRM_CONNECTOR_DVID
        );
        assert_eq!(
            output(TRANS_DDI_MODE_DP_SST, true, false).drm_connector(),
            DRM_CONNECTOR_DISPLAYPORT
        );
        assert_eq!(
            output(TRANS_DDI_MODE_DP_SST, true, true).drm_connector(),
            DRM_CONNECTOR_EDP
        );
        assert_eq!(output(TRANS_DDI_MODE_HDMI, false, false).drm_connector(), 0);
        assert_eq!(output(4, true, false).drm_connector(), 0);
    }
}
