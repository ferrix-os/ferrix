// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0,
// drivers/gpu/drm/i915/display/intel_display_power_well.c (hsw_power_well_enable),
// intel_display_power_map.c (xelpd_power_wells_main),
// intel_dpll_mgr.c (combo_pll_enable, icl_dpll_write),
// intel_ddi.c (icl_ddi_combo_enable_clock, intel_ddi_enable_transcoder_clock,
// icl_combo_phy_set_signal_levels, icl_ddi_combo_vswing_program,
// intel_ddi_enable_hdmi), intel_combo_phy.c (intel_combo_phy_power_up_lanes),
// skl_watermark.c (pipe_mbus_dbox_ctl, adlp_allowed_dbufs),
// intel_display.c (hsw_crtc_enable), and the register headers beside them:
// Copyright © 2006-2025 Intel Corporation
// Under the MIT notice in this crate's i915/NOTICE.

//! Lighting a combo port the firmware did not light, written (`docs/DESKTOP.md`
//! §3.5, h2): HDMI or DVI on port A or B -- whichever the firmware left
//! dark -- on transcoder and pipe B, a universal plane
//! scanning out a surface that is already in the GGTT -- the lit pipe's own,
//! so the new monitor mirrors the top-left of the first until `card0` has a
//! second scanout of its own.
//!
//! The steps are i915's, in `hsw_crtc_enable`'s order: the pipe's power
//! wells, the DBUF slices pipe B has beside pipe A (`adlp_allowed_dbufs`),
//! the combo PLL, the port's clock and its I/O power well, the pipe and
//! the transcoder with the plane's DBUF share and watermarks, the
//! transcoder's function and the transcoder on, the PHY's signal levels and
//! the port's buffer, and once the pipe runs, the plane. Each is a [`Step`] handed to a caller in order -- the driver
//! does them, `modeset=dry` prints them, a host model checks them.
//!
//! What the steps take from the ground truth rather than from arithmetic
//! (`~/.local/share/ferrix/n150-linux/evidence/3h-dump-2026-10-23`, the
//! box's inspection Linux with a monitor on each HDMI port): the PHY's HDMI
//! signal level, which i915 reads from the VBT and which Linux programmed
//! identically on both PHYs (swing select 0xA, N scalar 0x60, cursor 0x3F,
//! no post-cursor); and pipe B's DBUF share and watermarks, i915's for
//! 1920x1080 at 148.5 MHz there, which hold for every mode up to that
//! clock and are refused above it.

use super::hotplug::Port;
use super::modeset::{Mode, Pipe, ddi_func_ctl, pipe_src, pll_words, transcoder_timings};
use crate::io::Mmio;

/// One thing done to a register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Write `value`.
    Write {
        /// The register.
        offset: u32,
        /// What is written.
        value: u32,
        /// Its i915 name.
        name: &'static str,
    },
    /// Read, clear `clear`, set `set`, write back (`intel_de_rmw`).
    Rmw {
        /// The register.
        offset: u32,
        /// Bits cleared.
        clear: u32,
        /// Bits set.
        set: u32,
        /// Its i915 name.
        name: &'static str,
    },
    /// Read `from`, clear and set, write to `to`: a combo PHY's group
    /// register written from lane 0's value, as i915 does.
    Copy {
        /// Read from.
        from: u32,
        /// Written to.
        to: u32,
        /// Bits cleared.
        clear: u32,
        /// Bits set.
        set: u32,
        /// Its i915 name.
        name: &'static str,
    },
    /// Wait until `(read & mask) == value`, for at most `timeout_us`.
    Wait {
        /// The register.
        offset: u32,
        /// Bits looked at.
        mask: u32,
        /// What they must be.
        value: u32,
        /// How long, in microseconds.
        timeout_us: u64,
        /// What is waited for.
        name: &'static str,
    },
}

/// What lighting needs to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Light {
    /// The combo port lit, A or B: whichever the firmware left dark.
    pub port: Port,
    /// The transcoder and pipe: 1, B.
    pub pipe: Pipe,
    /// The monitor's mode.
    pub mode: Mode,
    /// Whether the sink takes HDMI (an HDMI vendor block in its EDID);
    /// DVI otherwise, which every HDMI sink takes too.
    pub hdmi: bool,
    /// The combo PLL: 0 or 1.
    pub pll: u32,
    /// The display's reference clock in kHz.
    pub nssc_khz: u32,
    /// The surface the plane shows: its GGTT offset (`PLANE_SURF`), its
    /// stride in 64-byte units and its size, the lit pipe's.
    pub surface: u32,
    /// `PLANE_STRIDE`.
    pub stride: u32,
    /// The surface's width and height.
    pub surface_size: (u32, u32),
}

/// Why a port is not lit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not a combo port, or not pipe B: the DBUF share is known for pipe B
    /// alone, and the signal level for the combo PHYs.
    Unsupported,
    /// The pipe's transcoder is on already.
    PipeOn,
    /// The port's buffer is on already.
    PortOn,
    /// The firmware joined MBUS for one pipe: pipe A holds every DBUF slice,
    /// and lighting B would mean moving A's share while it shows.
    MbusJoined,
    /// Display C-states are on, which a modeset must turn off first.
    DcStates,
    /// Both combo PLLs are in use.
    NoPll,
    /// A clock above 148.5 MHz, whose watermarks the ground truth does
    /// not give, or one no PLL divider reaches.
    Clock,
    /// The port's combo PHY was never initialised (`icl_combo_phy_enabled`:
    /// its compensation powered down or not run), which i915 does at load
    /// and this driver does not yet.
    PhyNotReady,
}

/// `HSW_PWR_WELL_CTL2`, the driver's power-well requests.
pub const PWR_WELL_CTL2: u32 = 0x4_5404;
/// `ICL_PWR_WELL_CTL_DDI2`.
pub const PWR_WELL_CTL_DDI2: u32 = 0x4_5454;
/// `SKL_FUSE_STATUS`.
pub const FUSE_STATUS: u32 = 0x4_2000;
/// `ICL_PW_CTL_IDX_PW_2` and `XELPD_PW_CTL_IDX_PW_B`.
const PW_2: u32 = 1;
const PW_B: u32 = 6;
/// `DBUF_CTL_S(2)` and `DBUF_CTL_S(3)`: slices S3 and S4, pipe B's when
/// pipes A and B run.
pub const DBUF_CTL_S3_S4: [u32; 2] = [0x4_4300, 0x4_4304];
/// `MBUS_CTL`; `MBUS_JOIN` is bit 31.
pub const MBUS_CTL: u32 = 0x4_438C;
/// `DC_STATE_EN`.
pub const DC_STATE_EN: u32 = 0x4_5504;
/// `ICL_DPCLKA_CFGCR0`.
pub const DPCLKA_CFGCR0: u32 = 0x16_4280;
/// The combo PLLs' enable registers (`DPLL_ENABLE(0)`, `(1)`).
pub const DPLL_ENABLE: [u32; 2] = [0x4_6010, 0x4_6014];
/// `PLL_ENABLE`, `PLL_LOCK`, `PLL_POWER_ENABLE`, `PLL_POWER_STATE`.
const PLL_ENABLE: u32 = 1 << 31;
const PLL_LOCK: u32 = 1 << 30;
const PLL_POWER_ENABLE: u32 = 1 << 27;
const PLL_POWER_STATE: u32 = 1 << 26;
/// `_TRANS_CLK_SEL_A`.
const TRANS_CLK_SEL_A: u32 = 0x4_6140;
/// `_TRANSACONF`, `_PIPE_MISC_A`, `_PIPEA_MBUS_DBOX_CTL`: each pipe 0x1000 on.
pub const TRANSCONF_A: u32 = 0x7_0008;
const PIPE_MISC_A: u32 = 0x7_0030;
const PIPE_MBUS_DBOX_CTL_A: u32 = 0x7_003C;
/// `_PIPEA_CHICKEN`, `_TRANS_MULT_A`; each 0x1000 on.
const PIPE_CHICKEN_A: u32 = 0x7_0038;
const TRANS_MULT_A: u32 = 0x6_002C;
/// `_CHICKEN_TRANS_A` and `_WM_LINETIME_A`; each transcoder or pipe 4 on.
const CHICKEN_TRANS_A: u32 = 0x4_20C0;
const WM_LINETIME_A: u32 = 0x4_5270;
/// `_DDI_BUF_CTL_A`; each port 0x100 on. `DDI_BUF_IS_IDLE` is bit 7.
pub const DDI_BUF_CTL_A: u32 = 0x6_4000;
/// `_PLANE_WM_1_A_0`, `_PLANE_WM_TRANS_1_A`, `_PLANE_BUF_CFG_1_A`.
const PLANE_WM_1_A_0: u32 = 0x7_0240;
const PLANE_WM_TRANS_1_A: u32 = 0x7_0268;
const PLANE_BUF_CFG_1_A: u32 = 0x7_027C;
/// The plane's registers, as `modeset::primary_plane` has them.
const PLANE_CTL_1_A: u32 = 0x7_0180;
const PLANE_STRIDE_1_A: u32 = 0x7_0188;
const PLANE_POS_1_A: u32 = 0x7_018C;
const PLANE_SIZE_1_A: u32 = 0x7_0190;
const PLANE_SURF_1_A: u32 = 0x7_019C;
/// `_ICL_PHY_MISC_A`; B 4 on.
const PHY_MISC_A: u32 = 0x6_4C00;
/// The combo PHYs (`_ICL_COMBOPHY_A`, `_B`).
const COMBOPHY: [u32; 2] = [0x16_2000, 0x6_C000];

/// The highest clock lit: the one the ground truth's watermarks are for.
pub const MAX_CLOCK_KHZ: u32 = 148_500;

/// Pipe B's DBUF share and watermarks, i915's for 1920x1080 at 148.5 MHz
/// on the box (the dump's `PLANE_BUF_CFG_1_B`, `PLANE_WM_1_B_0..7`,
/// `PLANE_WM_TRANS_1_B`): blocks 0 to 2011 of slices S3 and S4.
const PIPE_B_BUF_CFG: u32 = 0x07DB_0000;
const PIPE_B_WM: [u32; 8] = [
    0x8000_4010,
    0x8001_0041,
    0x8001_8061,
    0x8001_C071,
    0x8002_80A1,
    0x8002_80A1,
    0x8000_C031,
    0x8000_003F,
];
const PIPE_B_WM_TRANS: u32 = 0x8000_001E;

/// `PIPE_MISC` as Linux runs both pipes: `PIPE_MISC_HDR_MODE_PRECISION`
/// and `PIPE_MISC_PIXEL_ROUNDING_TRUNC`.
const PIPE_MISC: u32 = (1 << 23) | (1 << 8);

/// `pipe_mbus_dbox_ctl` on Alder Lake-P/N with MBUS not joined:
/// B2B transactions max 16, delay 1, regulated; A credit 4 (`Wa_22010947358`);
/// BW credit 2; B credit 8.
const MBUS_DBOX: u32 = (16 << 20) | (1 << 17) | (1 << 16) | (2 << 14) | (8 << 8) | 4;

/// Whether `light` can be done on a display whose registers read as
/// `mmio` does now. Reads only.
///
/// # Errors
///
/// The [`Refusal`] that stops it.
pub fn preflight<M: Mmio + ?Sized>(mmio: &mut M, light: &Light) -> Result<(), Refusal> {
    let Port::Combo(phy @ 0..=1) = light.port else {
        return Err(Refusal::Unsupported);
    };
    if light.pipe != 1 || light.pll > 1 {
        return Err(Refusal::Unsupported);
    }
    if light.mode.clock_khz > MAX_CLOCK_KHZ
        || pll_words(light.pll, &light.mode, light.nssc_khz).is_none()
    {
        return Err(Refusal::Clock);
    }
    if mmio.read32(TRANSCONF_A + light.pipe * 0x1000) & (1 << 31) != 0 {
        return Err(Refusal::PipeOn);
    }
    if mmio.read32(DDI_BUF_CTL_A + u32::from(phy) * 0x100) & (1 << 31) != 0 {
        return Err(Refusal::PortOn);
    }
    // `ICL_PHY_MISC(phy)`'s `DE_IO_COMP_PWR_DOWN` (bit 23) clear and
    // `ICL_PORT_COMP_DW0(phy)`'s `COMP_INIT` (bit 31) set.
    let base = COMBOPHY.get(usize::from(phy)).copied().unwrap_or(0);
    if mmio.read32(PHY_MISC_A + u32::from(phy) * 4) & (1 << 23) != 0
        || mmio.read32(base + 0x100) & (1 << 31) == 0
    {
        return Err(Refusal::PhyNotReady);
    }
    if mmio.read32(MBUS_CTL) & (1 << 31) != 0 {
        return Err(Refusal::MbusJoined);
    }
    // DC_STATE_EN_UPTO_DC5, _DC6, _DC9 (bits 0, 1, 3).
    if mmio.read32(DC_STATE_EN) & 0b1011 != 0 {
        return Err(Refusal::DcStates);
    }
    if !pll_free(mmio, light.pll) {
        return Err(Refusal::NoPll);
    }
    Ok(())
}

/// Whether combo PLL `pll` is off and no running combo port takes its
/// clock from it.
pub fn pll_free<M: Mmio + ?Sized>(mmio: &mut M, pll: u32) -> bool {
    let Some(&enable) = DPLL_ENABLE.get(pll as usize) else {
        return false;
    };
    if mmio.read32(enable) & PLL_ENABLE != 0 {
        return false;
    }
    let clocks = mmio.read32(DPCLKA_CFGCR0);
    (0..2_u32).all(|phy| {
        let on = clocks & clock_off(phy) == 0;
        let from = (clocks >> (phy * 2)) & 3;
        !(on && from == pll)
    })
}

/// The first combo PLL [`pll_free`] says is free.
pub fn free_pll<M: Mmio + ?Sized>(mmio: &mut M) -> Option<u32> {
    (0..2).find(|&pll| pll_free(mmio, pll))
}

/// `ICL_DPCLKA_CFGCR0_DDI_CLK_OFF(phy)` for the combo PHYs.
const fn clock_off(phy: u32) -> u32 {
    1 << (10 + phy)
}

/// `PLANE_CTL` for a linear `XRGB8888` surface on display 13:
/// `PLANE_CTL_ENABLE`, `PLANE_CTL_ARB_SLOTS(1)` (`adlp_plane_ctl_arb_slots`
/// for four bytes a pixel), `PLANE_CTL_FORMAT_XRGB_8888` in ICL's field.
pub const PLANE_CTL: u32 = (1 << 31) | (1 << 28) | ((4 << 1) << 23);

/// Hand each step of lighting `light` to `step`, in order; stop at the first
/// it answers `false` to. Gives whether every step was taken.
pub fn steps(light: &Light, step: &mut dyn FnMut(Step) -> bool) -> bool {
    let Port::Combo(phy) = light.port else {
        return false;
    };
    let phy = u32::from(phy);
    let Some(&enable) = DPLL_ENABLE.get(light.pll as usize) else {
        return false;
    };
    let Some(&base) = COMBOPHY.get(phy as usize) else {
        return false;
    };
    let Some(pll) = pll_words(light.pll, &light.mode, light.nssc_khz) else {
        return false;
    };
    let mut ok = true;
    let mut push = |made: Step| {
        if ok {
            ok = step(made);
        }
    };
    power(light.pipe, &mut push);
    combo_pll(enable, &pll, &mut push);
    port_clock(light, phy, &mut push);
    transcoder(light, &mut push);
    transcoder_on(light, &mut push);
    port_on(phy, base, &mut push);
    plane(light, &mut push);
    ok
}

/// The pipe's power wells and DBUF slices, and its MBUS credits.
fn power(pipe: Pipe, push: &mut dyn FnMut(Step)) {
    let at = pipe * 0x1000;
    // The pipe's power: PW2, then pipe B's own (`xelpd_power_wells_main`),
    // each requested, its state awaited, then its fuses' distribution
    // (`SKL_FUSE_PG_DIST_STATUS(pg)`, pg = idx + 1).
    for (index, name) in [(PW_2, "PW_2"), (PW_B, "PW_B")] {
        push(Step::Rmw {
            offset: PWR_WELL_CTL2,
            clear: 0,
            set: 1 << (index * 2 + 1),
            name,
        });
        push(Step::Wait {
            offset: PWR_WELL_CTL2,
            mask: 1 << (index * 2),
            value: 1 << (index * 2),
            timeout_us: 1_000,
            name,
        });
        push(Step::Wait {
            offset: FUSE_STATUS,
            mask: 1 << (27 - (index + 1)),
            value: 1 << (27 - (index + 1)),
            timeout_us: 1_000,
            name: "SKL_FUSE_PG_DIST_STATUS",
        });
    }
    // Pipe B's DBUF slices, S3 and S4, with the tracker service level
    // `gen12_dbuf_slices_config` gives every slice.
    for slice in DBUF_CTL_S3_S4 {
        push(Step::Rmw {
            offset: slice,
            clear: 0x1F << 19,
            set: (1 << 31) | (8 << 19),
            name: "DBUF_CTL_S",
        });
        push(Step::Wait {
            offset: slice,
            mask: 1 << 30,
            value: 1 << 30,
            timeout_us: 100,
            name: "DBUF_POWER_STATE",
        });
    }
    push(Step::Write {
        offset: PIPE_MBUS_DBOX_CTL_A + at,
        value: MBUS_DBOX,
        name: "PIPE_MBUS_DBOX_CTL",
    });
}

/// The combo PLL: power, dividers, enable, lock.
fn combo_pll(enable: u32, pll: &[super::modeset::Write; 2], push: &mut dyn FnMut(Step)) {
    // The combo PLL: power, dividers, enable, lock (`combo_pll_enable`).
    push(Step::Rmw {
        offset: enable,
        clear: 0,
        set: PLL_POWER_ENABLE,
        name: "DPLL_ENABLE power",
    });
    push(Step::Wait {
        offset: enable,
        mask: PLL_POWER_STATE,
        value: PLL_POWER_STATE,
        timeout_us: 1_000,
        name: "PLL_POWER_STATE",
    });
    for word in pll {
        push(Step::Write {
            offset: word.offset,
            value: word.value,
            name: word.name,
        });
    }
    push(Step::Rmw {
        offset: enable,
        clear: 0,
        set: PLL_ENABLE,
        name: "DPLL_ENABLE",
    });
    push(Step::Wait {
        offset: enable,
        mask: PLL_LOCK,
        value: PLL_LOCK,
        timeout_us: 5_000,
        name: "PLL_LOCK",
    });
}

/// The port's clock and its I/O power, and the transcoder's clock.
fn port_clock(light: &Light, phy: u32, push: &mut dyn FnMut(Step)) {
    let pipe = light.pipe;
    // The port's clock from the PLL, then ungated, in two writes
    // (`_icl_ddi_enable_clock`); its I/O power well; the transcoder's
    // clock from the port (`TGL_TRANS_CLK_SEL_PORT(phy)` on display 13).
    push(Step::Rmw {
        offset: DPCLKA_CFGCR0,
        clear: 3 << (phy * 2),
        set: light.pll << (phy * 2),
        name: "DPCLKA_CFGCR0 select",
    });
    push(Step::Rmw {
        offset: DPCLKA_CFGCR0,
        clear: clock_off(phy),
        set: 0,
        name: "DPCLKA_CFGCR0 on",
    });
    push(Step::Rmw {
        offset: PWR_WELL_CTL_DDI2,
        clear: 0,
        set: 1 << (phy * 2 + 1),
        name: "DDI_IO",
    });
    push(Step::Wait {
        offset: PWR_WELL_CTL_DDI2,
        mask: 1 << (phy * 2),
        value: 1 << (phy * 2),
        timeout_us: 1_000,
        name: "DDI_IO",
    });
    push(Step::Write {
        offset: TRANS_CLK_SEL_A + pipe * 4,
        value: (phy + 1) << 28,
        name: "TRANS_CLK_SEL",
    });
}

/// The pipe and the transcoder as `hsw_crtc_enable` sets them before
/// anything is on: the pipe's source and misc
/// (`intel_set_pipe_src_size`, `bdw_set_pipe_misc`), the transcoder's
/// timings, multiplier, frame-start delay and configuration with the enable
/// off (`hsw_configure_cpu_transcoder`), the line time
/// (`hsw_set_linetime_wm`, `skl_linetime_wm`) and the pipe's chicken bits
/// (`icl_set_pipe_chicken`), then the plane's DBUF share and watermarks
/// (`intel_initial_watermarks`).
fn transcoder(light: &Light, push: &mut dyn FnMut(Step)) {
    let pipe = light.pipe;
    let at = pipe * 0x1000;
    let source = pipe_src(pipe, &light.mode);
    push(Step::Write {
        offset: source.offset,
        value: source.value,
        name: source.name,
    });
    push(Step::Write {
        offset: PIPE_MISC_A + at,
        value: PIPE_MISC,
        name: "PIPE_MISC",
    });
    for word in transcoder_timings(pipe, &light.mode) {
        push(Step::Write {
            offset: word.offset,
            value: word.value,
            name: word.name,
        });
    }
    push(Step::Write {
        offset: TRANS_MULT_A + at,
        value: 0,
        name: "TRANS_MULT",
    });
    // `HSW_FRAME_START_DELAY(framestart_delay - 1)`, the delay being 1.
    push(Step::Rmw {
        offset: CHICKEN_TRANS_A + pipe * 4,
        clear: 0b11 << 27,
        set: 0,
        name: "CHICKEN_TRANS",
    });
    push(Step::Write {
        offset: TRANSCONF_A + at,
        value: 0,
        name: "TRANSCONF",
    });
    push(Step::Write {
        offset: WM_LINETIME_A + pipe * 4,
        value: linetime(&light.mode),
        name: "WM_LINETIME",
    });
    // `PER_PIXEL_ALPHA_BYPASS_EN`, `PIXEL_ROUNDING_TRUNC_FB_PASSTHRU`,
    // `UNDERRUN_RECOVERY_DISABLE_ADLP`.
    push(Step::Rmw {
        offset: PIPE_CHICKEN_A + at,
        clear: 0,
        set: (1 << 7) | (1 << 15) | (1 << 30),
        name: "PIPE_CHICKEN",
    });
    push(Step::Write {
        offset: PLANE_BUF_CFG_1_A + at,
        value: PIPE_B_BUF_CFG,
        name: "PLANE_BUF_CFG",
    });
    for (level, value) in (0_u32..).zip(PIPE_B_WM) {
        push(Step::Write {
            offset: PLANE_WM_1_A_0 + at + level * 4,
            value,
            name: "PLANE_WM",
        });
    }
    push(Step::Write {
        offset: PLANE_WM_TRANS_1_A + at,
        value: PIPE_B_WM_TRANS,
        name: "PLANE_WM_TRANS",
    });
}

/// `skl_linetime_wm`: a line's time in eighths of a microsecond, rounded
/// up, at most 0x1FF.
#[must_use]
pub fn linetime(mode: &Mode) -> u32 {
    (mode.h[3] * 8_000)
        .div_ceil(mode.clock_khz.max(1))
        .min(0x1FF)
}

/// The transcoder's function and the transcoder on (`intel_ddi_enable`:
/// `intel_ddi_enable_transcoder_func`, `intel_enable_transcoder`).
fn transcoder_on(light: &Light, push: &mut dyn FnMut(Step)) {
    let pipe = light.pipe;
    let at = pipe * 0x1000;
    if let Some(function) = ddi_func_ctl(pipe, light.port, &light.mode) {
        // `TRANS_DDI_MODE_SELECT_DVI` for a sink without HDMI.
        let mode = if light.hdmi { 0 } else { 1 << 24 };
        push(Step::Write {
            offset: function.offset,
            value: function.value | mode,
            name: function.name,
        });
    }
    push(Step::Rmw {
        offset: TRANSCONF_A + at,
        clear: 0,
        set: 1 << 31,
        name: "TRANSCONF enable",
    });
    push(Step::Wait {
        offset: TRANSCONF_A + at,
        mask: 1 << 30,
        value: 1 << 30,
        timeout_us: 100_000,
        name: "TRANSCONF_STATE_ENABLE",
    });
}

/// The primary plane, once the pipe runs: its surface last, which arms the
/// rest. It is no larger than the surface; the pipe's background shows
/// round a smaller one.
fn plane(light: &Light, push: &mut dyn FnMut(Step)) {
    let at = light.pipe * 0x1000;
    let (width, height) = (
        light.mode.h[0].min(light.surface_size.0),
        light.mode.v[0].min(light.surface_size.1),
    );
    for (offset, value, name) in [
        (PLANE_STRIDE_1_A, light.stride, "PLANE_STRIDE"),
        (PLANE_POS_1_A, 0, "PLANE_POS"),
        (
            PLANE_SIZE_1_A,
            ((height.max(1) - 1) << 16) | (width.max(1) - 1),
            "PLANE_SIZE",
        ),
        (PLANE_CTL_1_A, PLANE_CTL, "PLANE_CTL"),
        (PLANE_SURF_1_A, light.surface, "PLANE_SURF"),
    ] {
        push(Step::Write {
            offset: offset + at,
            value,
            name,
        });
    }
}

/// The port: the PHY's signal levels, its lanes up, its buffer on.
fn port_on(phy: u32, base: u32, push: &mut dyn FnMut(Step)) {
    // The port: the PHY's signal levels for HDMI
    // (`icl_combo_phy_set_signal_levels`), its lanes up, its buffer on.
    let lane = |dw: u32, ln: u32| base + 0x880 + ln * 0x100 + dw * 4;
    let group = |dw: u32| base + 0x680 + dw * 4;
    // 1. PORT_PCS_DW1: COMMON_KEEPER_EN off for HDMI.
    push(Step::Copy {
        from: base + 0x800 + 4,
        to: base + 0x600 + 4,
        clear: 1 << 26,
        set: 0,
        name: "PORT_PCS_DW1",
    });
    // 2. Loadgen select, four lanes at a TMDS rate below 6 GHz: lanes 1-3.
    for ln in 0..4 {
        push(Step::Rmw {
            offset: lane(4, ln),
            clear: 1 << 31,
            set: if ln >= 1 { 1 << 31 } else { 0 },
            name: "PORT_TX_DW4 loadgen",
        });
    }
    // 3. PORT_CL_DW5's SUS clock configuration.
    push(Step::Rmw {
        offset: base + 0x14,
        clear: 0,
        set: 0b11,
        name: "PORT_CL_DW5",
    });
    // 4. Training off.
    push(Step::Copy {
        from: lane(5, 0),
        to: group(5),
        clear: 1 << 31,
        set: 0,
        name: "PORT_TX_DW5 training off",
    });
    // 5. Swing and de-emphasis (`icl_ddi_combo_vswing_program`).
    push(Step::Copy {
        from: lane(5, 0),
        to: group(5),
        clear: (0b111 << 18) | (0b111 << 3) | (1 << 25) | (1 << 26) | (1 << 30) | (1 << 29),
        set: (2 << 18) | (6 << 3) | (1 << 29),
        name: "PORT_TX_DW5",
    });
    for ln in 0..4 {
        // Swing select 0xA: its bit 3 in bit 15, its low three in 13:11.
        push(Step::Rmw {
            offset: lane(2, ln),
            clear: (1 << 15) | (0b111 << 11) | 0xFF,
            set: (1 << 15) | (0x2 << 11) | 0x98,
            name: "PORT_TX_DW2",
        });
    }
    for ln in 0..4 {
        push(Step::Rmw {
            offset: lane(4, ln),
            clear: (0x3F << 12) | (0x3F << 6) | 0x3F,
            set: 0x3F,
            name: "PORT_TX_DW4",
        });
    }
    for ln in 0..4 {
        push(Step::Rmw {
            offset: lane(7, ln),
            clear: 0x7F << 24,
            set: 0x60 << 24,
            name: "PORT_TX_DW7",
        });
    }
    // 6. Training on, which loads the values.
    push(Step::Copy {
        from: lane(5, 0),
        to: group(5),
        clear: 0,
        set: 1 << 31,
        name: "PORT_TX_DW5 training on",
    });
    // Every lane up (`PWR_UP_ALL_LANES`).
    push(Step::Rmw {
        offset: base + 0x28,
        clear: 0xF << 4,
        set: 0,
        name: "PORT_CL_DW10",
    });
    // The buffer on; HDMI's port width is not used.
    push(Step::Write {
        offset: DDI_BUF_CTL_A + phy * 0x100,
        value: 1 << 31,
        name: "DDI_BUF_CTL",
    });
    push(Step::Wait {
        offset: DDI_BUF_CTL_A + phy * 0x100,
        mask: 1 << 7,
        value: 0,
        timeout_us: 1_000,
        name: "DDI_BUF_IS_IDLE clear",
    });
}

/// The second pipe's own surface (`docs/DESKTOP.md` §3.5): the largest a
/// monitor lit here can take -- 1920x1200, every mode up to
/// [`MAX_CLOCK_KHZ`] -- at one stride, in the GGTT just past what the
/// firmware mapped and below the driver's own GGTT window.
pub const SURFACE_SIZE: (u32, u32) = (1920, 1200);
/// Its stride in bytes: a 64-byte multiple, as a linear plane takes.
pub const SURFACE_PITCH: u32 = SURFACE_SIZE.0 * 4;
/// Its bytes.
pub const SURFACE_BYTES: u64 = SURFACE_PITCH as u64 * SURFACE_SIZE.1 as u64;
/// Where the second surface must end: below the GGTT window the driver's
/// M2 self-test and the serving back end bind from (256 MiB and above).
pub const SURFACE_GGTT_LIMIT: u64 = 256 << 20;

/// Where in the GGTT it may start: a 2 MiB boundary, which a linear plane's
/// alignment needs a fraction of and which keeps its GGTT entries a whole
/// number of pages.
pub const SURFACE_ALIGN: u64 = 2 << 20;

/// Where the second surface goes: the first [`SURFACE_ALIGN`] boundary at or
/// past `firmware_end` (the end of what the firmware mapped) whose
/// [`SURFACE_BYTES`] end below `limit` ([`SURFACE_GGTT_LIMIT`]). `None`
/// when it does not fit.
#[must_use]
pub const fn surface_place(firmware_end: u64, limit: u64) -> Option<u64> {
    let at = firmware_end.next_multiple_of(SURFACE_ALIGN);
    if at + SURFACE_BYTES <= limit {
        Some(at)
    } else {
        None
    }
}

/// A pipe the firmware already runs, as read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Running {
    /// The port its transcoder drives.
    pub port: Port,
    /// Its size, from `PIPESRC`.
    pub size: (u32, u32),
    /// Its `TRANS_DDI_FUNC_CTL`, which names the connector's kind.
    pub function: u32,
}

/// Whether `pipe` runs (its transcoder and its function on), and on what
/// and at what size. Reads only.
pub fn running<M: Mmio + ?Sized>(mmio: &mut M, pipe: Pipe) -> Option<Running> {
    let at = pipe * 0x1000;
    if mmio.read32(TRANSCONF_A + at) & (1 << 31) == 0 {
        return None;
    }
    let function = mmio.read32(0x6_0400 + at);
    if function & (1 << 31) == 0 {
        return None;
    }
    let port = Port::of_ddi_func_ctl(function)?;
    let source = mmio.read32(0x6_001C + at);
    Some(Running {
        port,
        size: (((source >> 16) & 0x1FFF) + 1, (source & 0x1FFF) + 1),
        function,
    })
}

/// A running pipe's primary plane pointed at a surface of its own: the
/// boot where the firmware cloned one surface onto both pipes. The stride
/// and size first, the surface last, which arms them at the next frame.
pub fn repoint(
    pipe: Pipe,
    surface: u32,
    stride: u32,
    size: (u32, u32),
    step: &mut dyn FnMut(Step) -> bool,
) -> bool {
    let at = pipe * 0x1000;
    let (width, height) = (size.0.min(SURFACE_SIZE.0), size.1.min(SURFACE_SIZE.1));
    [
        (PLANE_STRIDE_1_A, stride, "PLANE_STRIDE"),
        (PLANE_POS_1_A, 0, "PLANE_POS"),
        (
            PLANE_SIZE_1_A,
            ((height.max(1) - 1) << 16) | (width.max(1) - 1),
            "PLANE_SIZE",
        ),
        (PLANE_SURF_1_A, surface, "PLANE_SURF"),
    ]
    .into_iter()
    .all(|(offset, value, name)| {
        step(Step::Write {
            offset: offset + at,
            value,
            name,
        })
    })
}

/// Do one step on `mmio`, waiting on `clock`. Gives what the register read
/// after it, `Err` with the last value read for a wait that timed out.
///
/// # Errors
///
/// A wait that did not see its value in time.
pub fn take<M: Mmio + ?Sized, C: crate::io::Clock + ?Sized>(
    mmio: &mut M,
    clock: &mut C,
    step: Step,
) -> Result<u32, u32> {
    match step {
        Step::Write { offset, value, .. } => {
            mmio.write32(offset, value);
            Ok(mmio.read32(offset))
        }
        Step::Rmw {
            offset, clear, set, ..
        } => {
            let value = (mmio.read32(offset) & !clear) | set;
            mmio.write32(offset, value);
            Ok(mmio.read32(offset))
        }
        Step::Copy {
            from,
            to,
            clear,
            set,
            ..
        } => {
            let value = (mmio.read32(from) & !clear) | set;
            mmio.write32(to, value);
            Ok(value)
        }
        Step::Wait {
            offset,
            mask,
            value,
            timeout_us,
            ..
        } => crate::io::wait_for_register(mmio, clock, offset, mask, value, timeout_us),
    }
}

/// Whether an EDID names an HDMI sink: a CEA-861 extension (tag 2) with an
/// HDMI vendor-specific data block (IEEE OUI 00-0C-03), as
/// `drm_detect_hdmi_monitor` looks.
#[must_use]
pub fn is_hdmi(edid: &[u8]) -> bool {
    edid.chunks_exact(128).skip(1).any(|block| {
        if block.first() != Some(&0x02) {
            return false;
        }
        // Byte 2: where the detailed timings start, so the data blocks end.
        let end = block.get(2).map_or(0, |&end| usize::from(end).min(127));
        let mut at = 4;
        while at < end {
            let Some(&header) = block.get(at) else {
                return false;
            };
            let (tag, length) = (header >> 5, usize::from(header & 0x1F));
            if tag == 3 && length >= 3 && block.get(at + 1..at + 4) == Some(&[0x03, 0x0C, 0x00][..])
            {
                return true;
            }
            at += 1 + length;
        }
        false
    })
}

#[cfg(test)]
mod tests;
