// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0,
// drivers/gpu/drm/i915/display/intel_display_regs.h:
// Copyright © 2025 Intel Corporation
// drivers/gpu/drm/i915/display/intel_dpll_mgr.c (icl_calc_wrpll,
// icl_wrpll_get_multipliers, icl_wrpll_params_populate, icl_calc_dpll_state):
// Copyright © 2006-2016 Intel Corporation
// drivers/gpu/drm/i915/display/intel_display.c (intel_set_transcoder_timings,
// intel_set_pipe_src_size):
// Copyright © 2006-2007 Intel Corporation
// Under the MIT notice in this crate's i915/NOTICE.

//! Lighting a port the firmware did not light: the planner (`docs/DESKTOP.md`
//! §3.5, step 3h's first part, h1).
//!
//! Every register value an HDMI mode on a combo PHY port needs, worked out
//! from the mode and nothing else, in the order i915's `hsw_crtc_enable`
//! writes them -- and written by nobody yet. The driver prints the plan
//! under `ferrix.intelgpu.modeset=dry`, and the values are compared with a
//! dump of the same registers while Linux drives the same monitor on the
//! same port (the ground truth) before any of them is written (h2).
//!
//! What is here: the combo PLL's dividers (`icl_calc_wrpll`) and the two
//! words they become, with display 12+'s half-fraction workaround at a
//! 38.4 MHz reference; the transcoder's timings; `TRANS_DDI_FUNC_CTL` for
//! HDMI; the pipe's source size; the primary plane. What is not yet: power
//! wells, `DPCLKA_CFGCR0`'s mapping, the PHY's buffer translations, DBUF and
//! watermarks -- each waits for the dump to say what the box runs.

use super::hotplug::Port;

/// `ICL_DSSM_CDCLK_PLL_REFCLK_MASK` of `SKL_DSSM` (`0x51004`).
pub const SKL_DSSM: u32 = 0x5_1004;

/// The display's reference clock in kHz, from `SKL_DSSM`'s bits 31:29:
/// 24, 19.2 or 38.4 MHz (`ICL_DSSM_CDCLK_PLL_REFCLK_*`).
#[must_use]
pub const fn reference_khz(dssm: u32) -> Option<u32> {
    match (dssm >> 29) & 0x7 {
        0 => Some(24_000),
        1 => Some(19_200),
        2 => Some(38_400),
        _ => None,
    }
}

/// A combo PLL's settings: `skl_wrpll_params` as `icl_calc_wrpll` fills it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wrpll {
    /// The DCO frequency in kHz.
    pub dco_khz: u32,
    /// `dco_integer`.
    pub dco_integer: u32,
    /// `dco_fraction`, before display 12's workaround halves it.
    pub dco_fraction: u32,
    /// `pdiv`, as the register field holds it (1, 2, 4 or 8).
    pub pdiv: u32,
    /// `kdiv`, as the register field holds it (1, 2 or 4).
    pub kdiv: u32,
    /// `qdiv_ratio`.
    pub qdiv_ratio: u32,
    /// `qdiv_mode`.
    pub qdiv_mode: u32,
}

/// `icl_calc_wrpll`: the combo PLL for a `port_clock_khz` (an HDMI mode's
/// pixel clock at 8 bits a colour) from a `nssc_khz` reference. `None` for
/// a clock no divider reaches.
#[must_use]
pub fn wrpll(port_clock_khz: u32, nssc_khz: u32) -> Option<Wrpll> {
    // `icl_wrpll_ref_clock`: a 38.4 MHz reference is divided by 2 in the PLL.
    let reference = if nssc_khz == 38_400 { 19_200 } else { nssc_khz };
    let afe = u64::from(port_clock_khz) * 5;
    let (dco_min, dco_max) = (7_998_000_u64, 10_000_000_u64);
    let dco_mid = (dco_min + dco_max) / 2;
    const DIVIDERS: [u64; 46] = [
        2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 24, 28, 30, 32, 36, 40, 42, 44, 48, 50, 52, 54, 56, 60,
        64, 66, 68, 70, 72, 76, 78, 80, 84, 88, 90, 92, 96, 98, 100, 102, 3, 5, 7, 9, 15, 21,
    ];
    let mut best: Option<(u64, u64, u64)> = None; // (centrality, divider, dco)
    for divider in DIVIDERS {
        let dco = afe * divider;
        if (dco_min..=dco_max).contains(&dco) {
            let centrality = dco.abs_diff(dco_mid);
            if best.is_none_or(|(held, _, _)| centrality < held) {
                best = Some((centrality, divider, dco));
            }
        }
    }
    let (_, divider, dco) = best?;
    let (pdiv, qdiv, kdiv) = multipliers(divider)?;
    let kdiv_field = match kdiv {
        1 => 1,
        2 => 2,
        3 => 4,
        _ => return None,
    };
    let pdiv_field = match pdiv {
        2 => 1,
        3 => 2,
        5 => 4,
        7 => 8,
        _ => return None,
    };
    let scaled = (dco << 15) / u64::from(reference);
    Some(Wrpll {
        dco_khz: u32::try_from(dco).ok()?,
        dco_integer: u32::try_from(scaled >> 15).ok()?,
        dco_fraction: u32::try_from(scaled & 0x7FFF).ok()?,
        pdiv: pdiv_field,
        kdiv: kdiv_field,
        qdiv_ratio: u32::try_from(qdiv).ok()?,
        qdiv_mode: u32::from(qdiv != 1),
    })
}

/// `icl_wrpll_get_multipliers`: `(pdiv, qdiv, kdiv)` for a divider.
fn multipliers(divider: u64) -> Option<(u64, u64, u64)> {
    if divider.is_multiple_of(2) {
        if divider == 2 {
            Some((2, 1, 1))
        } else if divider.is_multiple_of(4) {
            Some((2, divider / 4, 2))
        } else if divider.is_multiple_of(6) {
            Some((3, divider / 6, 2))
        } else if divider.is_multiple_of(5) {
            Some((5, divider / 10, 2))
        } else if divider.is_multiple_of(14) {
            Some((7, divider / 14, 2))
        } else {
            None
        }
    } else if matches!(divider, 3 | 5 | 7) {
        Some((divider, 1, 1))
    } else {
        Some((divider / 3, 1, 3))
    }
}

/// `icl_calc_dpll_state` on display 12+: `(DPLL_CFGCR0, DPLL_CFGCR1)`. The
/// fraction is halved, rounded, at a 38.4 MHz reference
/// (`ehl_combo_pll_div_frac_wa_needed`, Display WA #22010492432);
/// `TGL_DPLL_CFGCR1_CFSELOVRD_NORMAL_XTAL` is 0.
#[must_use]
pub const fn dpll_cfgcr(pll: &Wrpll, nssc_khz: u32) -> (u32, u32) {
    let fraction = if nssc_khz == 38_400 {
        // DIV_ROUND_CLOSEST(fraction, 2).
        pll.dco_fraction.div_ceil(2)
    } else {
        pll.dco_fraction
    };
    let cfgcr0 = (fraction << 10) | pll.dco_integer;
    let cfgcr1 = (pll.qdiv_ratio << 10) | (pll.qdiv_mode << 9) | (pll.kdiv << 6) | (pll.pdiv << 2);
    (cfgcr0, cfgcr1)
}

/// A mode: what a monitor's EDID gives, in DRM's terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    /// The pixel clock in kHz.
    pub clock_khz: u32,
    /// `hdisplay`, `hsync_start`, `hsync_end`, `htotal`.
    pub h: [u32; 4],
    /// `vdisplay`, `vsync_start`, `vsync_end`, `vtotal`.
    pub v: [u32; 4],
    /// Positive horizontal sync.
    pub phsync: bool,
    /// Positive vertical sync.
    pub pvsync: bool,
}

impl Mode {
    /// CEA 1920x1080@60, VIC 16: what the box's monitor runs.
    pub const CEA_1080P60: Self = Self {
        clock_khz: 148_500,
        h: [1920, 2008, 2052, 2200],
        v: [1080, 1084, 1089, 1125],
        phsync: true,
        pvsync: true,
    };
}

/// One register write the plan makes: where, what, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Write {
    /// The register.
    pub offset: u32,
    /// The value.
    pub value: u32,
    /// Its i915 name.
    pub name: &'static str,
}

/// A transcoder or pipe: A is 0.
pub type Pipe = u32;

/// `_TRANS_HTOTAL_A`; each transcoder `0x1000` on.
const TRANS_HTOTAL_A: u32 = 0x6_0000;
/// `_PIPEASRC`.
const PIPEASRC: u32 = 0x6_001C;
/// `_TRANS_DDI_FUNC_CTL_A`.
const TRANS_DDI_FUNC_CTL_A: u32 = 0x6_0400;
/// `_PLANE_CTL_1_A`, `_PLANE_STRIDE_1_A`, `_PLANE_POS_1_A`,
/// `_PLANE_SIZE_1_A`, `_PLANE_SURF_1_A`; each pipe `0x1000` on.
const PLANE_CTL_1_A: u32 = 0x7_0180;
const PLANE_STRIDE_1_A: u32 = 0x7_0188;
const PLANE_POS_1_A: u32 = 0x7_018C;
const PLANE_SIZE_1_A: u32 = 0x7_0190;
const PLANE_SURF_1_A: u32 = 0x7_019C;
/// `_TGL_DPLL0_CFGCR0`, `_TGL_DPLL1_CFGCR0`, and CFGCR1 four bytes on.
const TGL_DPLL_CFGCR0: [u32; 2] = [0x16_4284, 0x16_428C];

/// The transcoder timing words `intel_set_transcoder_timings` writes on
/// display 13: `HTOTAL`, `HBLANK`, `HSYNC`, `VTOTAL`, `VBLANK` (whose start
/// is unused and written as 1), `VSYNC`.
#[must_use]
pub fn transcoder_timings(pipe: Pipe, mode: &Mode) -> [Write; 6] {
    let base = TRANS_HTOTAL_A + pipe * 0x1000;
    let pair = |low: u32, high: u32| (low - 1) | ((high - 1) << 16);
    let [hdisplay, hsync_start, hsync_end, htotal] = mode.h;
    let [vdisplay, vsync_start, vsync_end, vtotal] = mode.v;
    [
        Write {
            offset: base,
            value: pair(hdisplay, htotal),
            name: "TRANS_HTOTAL",
        },
        Write {
            offset: base + 0x4,
            value: pair(hdisplay, htotal),
            name: "TRANS_HBLANK",
        },
        Write {
            offset: base + 0x8,
            value: pair(hsync_start, hsync_end),
            name: "TRANS_HSYNC",
        },
        Write {
            offset: base + 0xC,
            value: pair(vdisplay, vtotal),
            name: "TRANS_VTOTAL",
        },
        Write {
            offset: base + 0x10,
            value: pair(1, vtotal),
            name: "TRANS_VBLANK",
        },
        Write {
            offset: base + 0x14,
            value: pair(vsync_start, vsync_end),
            name: "TRANS_VSYNC",
        },
    ]
}

/// `TRANS_DDI_FUNC_CTL` for HDMI at 8 bits a colour on `port`, enabled.
#[must_use]
pub fn ddi_func_ctl(pipe: Pipe, port: Port, mode: &Mode) -> Option<Write> {
    let select = match port {
        Port::Combo(index) => u32::from(index) + 1,
        Port::TypeC(number) => u32::from(number) + 3,
    };
    let value = (1 << 31)
        | (select << 27)
        | (u32::from(mode.pvsync) << 17)
        | (u32::from(mode.phsync) << 16);
    Some(Write {
        offset: TRANS_DDI_FUNC_CTL_A + pipe * 0x1000,
        value,
        name: "TRANS_DDI_FUNC_CTL",
    })
}

/// `PIPESRC`: the pipe's source size.
#[must_use]
pub fn pipe_src(pipe: Pipe, mode: &Mode) -> Write {
    Write {
        offset: PIPEASRC + pipe * 0x1000,
        value: ((mode.h[0] - 1) << 16) | (mode.v[0] - 1),
        name: "PIPESRC",
    }
}

/// The primary plane showing a linear `XRGB8888` surface at GGTT offset
/// `surface` with `pitch` bytes a line: `PLANE_STRIDE`, `PLANE_POS`,
/// `PLANE_SIZE`, `PLANE_CTL` and, last, `PLANE_SURF`, which arms the rest.
#[must_use]
pub fn primary_plane(pipe: Pipe, mode: &Mode, surface: u32, pitch: u32) -> [Write; 5] {
    let at = pipe * 0x1000;
    [
        Write {
            offset: PLANE_STRIDE_1_A + at,
            value: pitch / 64,
            name: "PLANE_STRIDE",
        },
        Write {
            offset: PLANE_POS_1_A + at,
            value: 0,
            name: "PLANE_POS",
        },
        Write {
            offset: PLANE_SIZE_1_A + at,
            value: ((mode.v[0] - 1) << 16) | (mode.h[0] - 1),
            name: "PLANE_SIZE",
        },
        Write {
            // PLANE_CTL_ENABLE, PLANE_CTL_FORMAT_XRGB_8888 (4 << 24 >> 1 on
            // ICL+: bits 27:23 = 4 << 1), linear.
            offset: PLANE_CTL_1_A + at,
            value: (1 << 31) | ((4 << 1) << 23),
            name: "PLANE_CTL",
        },
        Write {
            offset: PLANE_SURF_1_A + at,
            value: surface,
            name: "PLANE_SURF",
        },
    ]
}

/// The combo PLL `pll` (0 or 1) at `mode`'s clock: `DPLL_CFGCR0` and
/// `DPLL_CFGCR1`. `None` for a clock no divider reaches.
#[must_use]
pub fn pll_words(pll: u32, mode: &Mode, nssc_khz: u32) -> Option<[Write; 2]> {
    let params = wrpll(mode.clock_khz, nssc_khz)?;
    let (cfgcr0, cfgcr1) = dpll_cfgcr(&params, nssc_khz);
    let base = *TGL_DPLL_CFGCR0.get(pll as usize)?;
    Some([
        Write {
            offset: base,
            value: cfgcr0,
            name: "DPLL_CFGCR0",
        },
        Write {
            offset: base + 4,
            value: cfgcr1,
            name: "DPLL_CFGCR1",
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::{
        Mode, Port, ddi_func_ctl, dpll_cfgcr, pipe_src, reference_khz, transcoder_timings, wrpll,
    };

    /// 148.5 MHz from a 38.4 MHz reference: the DCO nearest the middle of
    /// 7.998 to 10 GHz is 742.5 MHz times 12 = 8.91 GHz, and divider 12 is
    /// pdiv 2, qdiv 3, kdiv 2 (fields 1, 3 in mode 1, 2).
    #[test]
    fn hdmi_1080p60s_pll() {
        let pll = wrpll(148_500, 38_400).expect("a divider");
        assert_eq!(pll.dco_khz, 8_910_000);
        assert_eq!(
            (pll.pdiv, pll.qdiv_ratio, pll.qdiv_mode, pll.kdiv),
            (1, 3, 1, 2)
        );
        // 8 910 000 / 19 200 = 464.0625: integer 464, fraction 0.0625 * 2^15.
        assert_eq!(pll.dco_integer, 464);
        assert_eq!(pll.dco_fraction, 2048);
        let (cfgcr0, cfgcr1) = dpll_cfgcr(&pll, 38_400);
        assert_eq!(
            cfgcr0,
            (1024 << 10) | 464,
            "the fraction halved at 38.4 MHz"
        );
        assert_eq!(cfgcr1, (3 << 10) | (1 << 9) | (2 << 6) | (1 << 2));
        assert_eq!(reference_khz(2 << 29), Some(38_400));
    }

    /// The transcoder's words for CEA 1080p60, as `intel_set_transcoder_timings`
    /// writes them on display 13.
    #[test]
    fn the_transcoders_timings() {
        let words = transcoder_timings(1, &Mode::CEA_1080P60);
        assert_eq!(words[0].offset, 0x6_1000);
        assert_eq!(words[0].value, (2199 << 16) | 1919);
        assert_eq!(words[2].value, (2051 << 16) | 2007);
        assert_eq!(words[3].value, (1124 << 16) | 1079);
        assert_eq!(
            words[4].value,
            1124 << 16,
            "VBLANK_START unused, written as 1"
        );
        assert_eq!(words[5].value, (1088 << 16) | 1083);
        assert_eq!(pipe_src(1, &Mode::CEA_1080P60).value, (1919 << 16) | 1079);
        let function = ddi_func_ctl(1, Port::Combo(1), &Mode::CEA_1080P60).expect("a port");
        assert_eq!(function.offset, 0x6_1400);
        assert_eq!(
            function.value,
            (1 << 31) | (2 << 27) | (1 << 17) | (1 << 16)
        );
    }
}
