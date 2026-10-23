//! Lighting a port the firmware did not light (`docs/DESKTOP.md` §3.5):
//! the monitor on the combo port the firmware left dark -- B where it lit
//! A, A where it lit B -- on transcoder and pipe B, showing a surface of
//! its own, `card0`'s second scanout.
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
pub(crate) enum Asked {
    Nothing,
    Dry,
    On,
}

pub(crate) fn asked() -> Asked {
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

/// Print the steps that would light `port`, writing none
/// (`ferrix.intelgpu.modeset=dry`).
pub(crate) fn dry(regs: &mut Regs, port: Port) {
    let tag = "modeset dry";
    let Some(light) = plan(regs, tag, port, 0) else {
        return;
    };
    if let Err(why) = light::preflight(regs, &light) {
        say(format_args!(
            "intel: {tag}: port {} would not be lit: {why:?}",
            port.name()
        ));
        return;
    }
    let mut taken = 0_u32;
    let _ = light::steps(&light, &mut |step| {
        taken += 1;
        show(tag, step, None);
        true
    });
    say(format_args!("intel: {tag}: {taken} steps, none taken"));
}

/// Light `port` on pipe B showing the surface at GGTT `surface`
/// (`ferrix.intelgpu.modeset=on`): its mode, once lit; `None`, said, when
/// it is not.
pub(crate) fn light_port(regs: &mut Regs, port: Port, surface: u32) -> Option<Mode> {
    let tag = "modeset";
    let light = plan(regs, tag, port, surface)?;
    if let Err(why) = light::preflight(regs, &light) {
        say(format_args!(
            "intel: {tag}: port {} is not lit: {why:?} (nothing written)",
            port.name()
        ));
        return None;
    }
    let mut clock = super::hotplug::Monotonic;
    let mut taken = 0_u32;
    let all = light::steps(&light, &mut |step| {
        taken += 1;
        let result = light::take(regs, &mut clock, step);
        show(tag, step, Some(result));
        result.is_ok()
    });
    if all {
        say(format_args!(
            "intel: modeset: port {} lit, {}x{} on pipe B ({taken} steps)",
            port.name(),
            light.mode.h[0],
            light.mode.v[0]
        ));
        Some(light.mode)
    } else {
        say(format_args!(
            "intel: modeset: stopped at step {taken}; what was done stays as it is"
        ));
        None
    }
}

/// Point a pipe the firmware runs at the surface at GGTT `surface`: the
/// boot where it cloned one surface onto both pipes.
pub(crate) fn repoint(regs: &mut Regs, pipe: u32, surface: u32, size: (u32, u32)) -> bool {
    let mut clock = super::hotplug::Monotonic;
    light::repoint(
        pipe,
        surface,
        light::SURFACE_PITCH / 64,
        size,
        &mut |step| {
            let result = light::take(regs, &mut clock, step);
            show("modeset", step, Some(result));
            result.is_ok()
        },
    )
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

/// What lighting `port` takes, read from the display: its sink and EDID,
/// the mode (the preferred one, else the first listed at most 148.5 MHz),
/// whether it is HDMI, a free PLL, the reference clock, and `surface`,
/// the second surface's GGTT offset. `None`, having said why, when there is no sink, no
/// reference clock or no free PLL.
fn plan(regs: &mut Regs, tag: &str, port: Port, surface: u32) -> Option<Light> {
    let live = port
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
            "intel: {tag}: port {} has no sink (SDEISR); nothing done",
            port.name()
        ));
        return None;
    }
    let (mode, hdmi) = sink_mode(regs, tag, port);
    let Some(pll) = light::free_pll(regs) else {
        say(format_args!(
            "intel: {tag}: both combo PLLs are in use; nothing done"
        ));
        return None;
    };
    let light = Light {
        port,
        pipe: 1,
        mode,
        hdmi,
        pll,
        nssc_khz: nssc,
        surface,
        stride: light::SURFACE_PITCH / 64,
        surface_size: light::SURFACE_SIZE,
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

/// `port`'s mode and whether its sink is HDMI, from its EDID: the preferred
/// mode, else the first listed at most 148.5 MHz; CEA 1080p60 as DVI where
/// no EDID answers.
fn sink_mode(regs: &mut Regs, tag: &str, port: Port) -> (Mode, bool) {
    let mut edid = [0_u8; 2 * gmbus::EDID_BLOCK];
    match gmbus::read_edid(
        regs,
        &mut super::hotplug::Monotonic,
        gmbus::pin_of(port),
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
                "intel: {tag}: port {}'s EDID, {length} bytes, {} modes, {}",
                port.name(),
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
                "intel: {tag}: no EDID on port {} ({why:?}); CEA 1920x1080@60 as DVI",
                port.name()
            ));
            (Mode::CEA_1080P60, false)
        }
    }
}
