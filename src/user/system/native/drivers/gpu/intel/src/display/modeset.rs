//! Lighting a port the firmware did not light (`docs/DESKTOP.md` §3.5):
//! the monitor on the second combo port (port B, transcoder and pipe B),
//! showing the lit pipe's own surface -- a mirror of its top-left corner
//! until `card0` has a second scanout.
//!
//! `ferrix.intelgpu.modeset=dry` (h1) prints every step and writes none;
//! `ferrix.intelgpu.modeset=on` (h2) checks the display is as the steps
//! expect (`light::preflight`, reads only) and then takes them, saying each
//! and stopping at the first wait that does not end. Nothing is written
//! without the word.

use ferrix_display_modes::{ModeList, edid_modes};
use ferrix_intel_xelp::display::gmbus;
use ferrix_intel_xelp::display::hotplug::{Port, SDEISR};
use ferrix_intel_xelp::display::light::{self, Light, MAX_CLOCK_KHZ, Step};
use ferrix_intel_xelp::display::modeset::{Mode, SKL_DSSM, reference_khz};
use ferrix_intel_xelp::io::Mmio;

use super::regs::Regs;
use crate::say;

/// The kernel command line's words: the plan printed, or taken.
const DRY: &[u8] = b"ferrix.intelgpu.modeset=dry";
const ON: &[u8] = b"ferrix.intelgpu.modeset=on";

/// What the command line asks for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Asked {
    Nothing,
    Dry,
    On,
}

fn asked() -> Asked {
    let mut line = [0_u8; 4096];
    let Some(length) = crate::steps::read_cmdline(&mut line) else {
        return Asked::Nothing;
    };
    let words = || {
        line.get(..length)
            .unwrap_or_default()
            .split(u8::is_ascii_whitespace)
    };
    if words().any(|word| word == ON) {
        Asked::On
    } else if words().any(|word| word == DRY) {
        Asked::Dry
    } else {
        Asked::Nothing
    }
}

/// The lit pipe's primary plane: `PLANE_SURF_1_A`, `PLANE_STRIDE_1_A`,
/// `PLANE_SIZE_1_A`.
const LIT_SURF: u32 = 0x7_019C;
const LIT_STRIDE: u32 = 0x7_0188;
const LIT_SIZE: u32 = 0x7_0190;

/// Light port B as the command line asks: print the steps under `dry`,
/// take them under `on`, nothing otherwise.
pub(crate) fn run(regs: &mut Regs) {
    let asked = asked();
    if asked == Asked::Nothing {
        return;
    }
    let tag = if asked == Asked::On {
        "modeset"
    } else {
        "modeset dry"
    };
    let Some(light) = plan(regs, tag) else {
        return;
    };
    if let Err(why) = light::preflight(regs, &light) {
        say(format_args!(
            "intel: {tag}: port B is not lit: {why:?} (nothing written)"
        ));
        return;
    }
    let mut clock = super::hotplug::Monotonic;
    let mut taken = 0_u32;
    let all = light::steps(&light, &mut |step| {
        taken += 1;
        if asked != Asked::On {
            show(tag, step, None);
            return true;
        }
        let result = light::take(regs, &mut clock, step);
        show(tag, step, Some(result));
        result.is_ok()
    });
    if asked != Asked::On {
        say(format_args!("intel: {tag}: {taken} steps, none taken"));
    } else if all {
        say(format_args!(
            "intel: modeset: port B lit, {}x{} on pipe B, mirroring pipe A's surface ({taken} steps)",
            light.mode.h[0], light.mode.v[0]
        ));
    } else {
        say(format_args!(
            "intel: modeset: stopped at step {taken}; what was done stays as it is"
        ));
    }
}

/// One step, and what came of it.
fn show(tag: &str, step: Step, result: Option<Result<u32, u32>>) {
    let after = match result {
        None => "",
        Some(Ok(_)) => " ok",
        Some(Err(_)) => " TIMED OUT",
    };
    let read = match result {
        Some(Ok(value) | Err(value)) => value,
        None => 0,
    };
    match step {
        Step::Write {
            offset,
            value,
            name,
        } => say(format_args!(
            "intel: {tag}: {name:<24} {offset:#08x} <- {value:#010x}{after} (reads {read:#010x})"
        )),
        Step::Rmw {
            offset,
            clear,
            set,
            name,
        } => say(format_args!(
            "intel: {tag}: {name:<24} {offset:#08x} &~{clear:#010x} |{set:#010x}{after} (reads {read:#010x})"
        )),
        Step::Copy {
            from,
            to,
            clear,
            set,
            name,
        } => say(format_args!(
            "intel: {tag}: {name:<24} {to:#08x} <- ({from:#08x} &~{clear:#010x} |{set:#010x}){after} (wrote {read:#010x})"
        )),
        Step::Wait {
            offset,
            mask,
            value,
            timeout_us,
            name,
        } => say(format_args!(
            "intel: {tag}: wait {name:<19} {offset:#08x} & {mask:#010x} == {value:#010x} within {timeout_us} us{after} (reads {read:#010x})"
        )),
    }
}

/// What lighting port B takes, read from the display: its sink and EDID,
/// the mode (the preferred one, else the first listed at most 148.5 MHz),
/// whether it is HDMI, a free PLL, the reference clock and the lit
/// plane's surface. `None`, having said why, when there is no sink, no
/// reference clock or no free PLL.
fn plan(regs: &mut Regs, tag: &str) -> Option<Light> {
    const PORT: Port = Port::Combo(1);
    let live = PORT
        .live_bit()
        .is_some_and(|bit| regs.read32(SDEISR) & bit != 0);
    let dssm = regs.read32(SKL_DSSM);
    let Some(nssc) = reference_khz(dssm) else {
        say(format_args!(
            "intel: {tag}: SKL_DSSM {dssm:#010x} names no reference clock; nothing done"
        ));
        return None;
    };
    if !live {
        say(format_args!(
            "intel: {tag}: port B has no sink (SDEISR); nothing done"
        ));
        return None;
    }
    let (mode, hdmi) = sink_mode(regs, tag);
    let Some(pll) = light::free_pll(regs) else {
        say(format_args!(
            "intel: {tag}: both combo PLLs are in use; nothing done"
        ));
        return None;
    };
    let size = regs.read32(LIT_SIZE);
    let light = Light {
        port: PORT,
        pipe: 1,
        mode,
        hdmi,
        pll,
        nssc_khz: nssc,
        surface: regs.read32(LIT_SURF),
        stride: regs.read32(LIT_STRIDE),
        surface_size: ((size & 0x1FFF) + 1, ((size >> 16) & 0x1FFF) + 1),
    };
    say(format_args!(
        "intel: {tag}: {}x{} at {} kHz, h {:?} v {:?}, sync {}{}, PLL {pll}, reference {nssc} kHz, surface {:#x} ({}x{}, stride {} x 64)",
        mode.h[0],
        mode.v[0],
        mode.clock_khz,
        mode.h,
        mode.v,
        if mode.phsync { '+' } else { '-' },
        if mode.pvsync { '+' } else { '-' },
        light.surface,
        light.surface_size.0,
        light.surface_size.1,
        light.stride,
    ));
    Some(light)
}

/// Port B's mode and whether its sink is HDMI, from its EDID: the preferred
/// mode, else the first listed at most 148.5 MHz; CEA 1080p60 as DVI where
/// no EDID answers.
fn sink_mode(regs: &mut Regs, tag: &str) -> (Mode, bool) {
    const PORT: Port = Port::Combo(1);
    let mut edid = [0_u8; 2 * gmbus::EDID_BLOCK];
    match gmbus::read_edid(
        regs,
        &mut super::hotplug::Monotonic,
        gmbus::pin_of(PORT),
        &mut edid,
    ) {
        Ok(length) => {
            let bytes = edid.get(..length).unwrap_or_default();
            let mut list = ModeList::new();
            edid_modes(bytes, &mut list);
            let listed = list.as_slice();
            let chosen = listed
                .iter()
                .filter(|listed| listed.timing.clock_khz <= MAX_CLOCK_KHZ)
                .find(|listed| listed.preferred)
                .or_else(|| {
                    listed
                        .iter()
                        .find(|listed| listed.timing.clock_khz <= MAX_CLOCK_KHZ)
                })
                .map(|listed| listed.timing);
            say(format_args!(
                "intel: {tag}: port B's EDID, {length} bytes, {} modes, {}",
                list.len(),
                if light::is_hdmi(bytes) { "HDMI" } else { "DVI" }
            ));
            (
                chosen.map_or(Mode::CEA_1080P60, |timing| Mode {
                    clock_khz: timing.clock_khz,
                    h: [
                        u32::from(timing.hdisplay),
                        u32::from(timing.hsync_start),
                        u32::from(timing.hsync_end),
                        u32::from(timing.htotal),
                    ],
                    v: [
                        u32::from(timing.vdisplay),
                        u32::from(timing.vsync_start),
                        u32::from(timing.vsync_end),
                        u32::from(timing.vtotal),
                    ],
                    phsync: timing.hsync_high,
                    pvsync: timing.vsync_high,
                }),
                light::is_hdmi(bytes),
            )
        }
        Err(why) => {
            say(format_args!(
                "intel: {tag}: no EDID on port B ({why:?}); CEA 1920x1080@60 as DVI"
            ));
            (Mode::CEA_1080P60, false)
        }
    }
}
