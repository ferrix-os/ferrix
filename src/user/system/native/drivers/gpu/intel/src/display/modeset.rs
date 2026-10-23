//! Lighting a port the firmware did not light, first as a plan
//! (`docs/DESKTOP.md` §3.5, h1): under `ferrix.intelgpu.modeset=dry` the
//! card prints every register value it would write to show the monitor on
//! the second combo port (port B, transcoder and pipe B) and writes none of
//! them. The values are compared with what Linux writes to the same
//! registers for the same monitor before h2 writes anything.

use ferrix_display_modes::{ModeList, edid_modes};
use ferrix_intel_xelp::display::gmbus;
use ferrix_intel_xelp::display::hotplug::{Port, SDEISR};
use ferrix_intel_xelp::display::modeset::{
    Mode, SKL_DSSM, Write, ddi_func_ctl, pipe_src, pll_words, primary_plane, reference_khz,
    transcoder_timings,
};
use ferrix_intel_xelp::io::Mmio;

use super::regs::Regs;
use crate::say;

/// The kernel command line's word that asks for the plan.
const WORD: &[u8] = b"ferrix.intelgpu.modeset=dry";

/// Whether the command line asks for the plan.
pub(crate) fn asked() -> bool {
    let mut line = [0_u8; 4096];
    crate::steps::read_cmdline(&mut line).is_some_and(|length| {
        line.get(..length)
            .unwrap_or_default()
            .split(u8::is_ascii_whitespace)
            .any(|word| word == WORD)
    })
}

/// Print the plan for port B on pipe B: its sink's hot-plug state and
/// EDID, the mode it prefers (CEA 1080p60 where no EDID answers), and each
/// write in `hsw_crtc_enable`'s order as far as the planner goes. Nothing
/// is written but the GMBUS transfer that reads the EDID.
pub(crate) fn print_plan(regs: &mut Regs) {
    const PORT: Port = Port::Combo(1);
    const PIPE: u32 = 1;
    let live = PORT
        .live_bit()
        .is_some_and(|bit| regs.read32(SDEISR) & bit != 0);
    let dssm = regs.read32(SKL_DSSM);
    let Some(nssc) = reference_khz(dssm) else {
        say(format_args!(
            "intel: modeset dry: SKL_DSSM {dssm:#010x} names no reference clock; no plan"
        ));
        return;
    };
    say(format_args!(
        "intel: modeset dry: port B {} (SDEISR), reference {nssc} kHz (SKL_DSSM {dssm:#010x})",
        if live { "has a sink" } else { "has no sink" }
    ));
    let mut edid = [0_u8; 2 * gmbus::EDID_BLOCK];
    let mode = match gmbus::read_edid(
        regs,
        &mut super::hotplug::Monotonic,
        gmbus::pin_of(PORT),
        &mut edid,
    ) {
        Ok(length) => {
            let mut list = ModeList::new();
            edid_modes(edid.get(..length).unwrap_or_default(), &mut list);
            let preferred = list
                .as_slice()
                .iter()
                .find(|listed| listed.preferred)
                .or_else(|| list.as_slice().first())
                .map(|listed| listed.timing);
            say(format_args!(
                "intel: modeset dry: port B's EDID, {length} bytes, {} modes",
                list.len()
            ));
            preferred.map_or(Mode::CEA_1080P60, |timing| Mode {
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
            })
        }
        Err(why) => {
            say(format_args!(
                "intel: modeset dry: no EDID on port B ({why:?}); planning CEA 1920x1080@60"
            ));
            Mode::CEA_1080P60
        }
    };
    say(format_args!(
        "intel: modeset dry: mode {}x{} at {} kHz, h {:?} v {:?}, sync {}{}",
        mode.h[0],
        mode.v[0],
        mode.clock_khz,
        mode.h,
        mode.v,
        if mode.phsync { '+' } else { '-' },
        if mode.pvsync { '+' } else { '-' }
    ));
    let mut planned = 0;
    let mut show = |writes: &[Write]| {
        for write in writes {
            say(format_args!(
                "intel: modeset dry: {:<20} {:#08x} <- {:#010x}",
                write.name, write.offset, write.value
            ));
        }
        planned += writes.len();
    };
    match pll_words(1, &mode, nssc) {
        Some(words) => show(&words),
        None => say(format_args!(
            "intel: modeset dry: no combo PLL divider reaches {} kHz",
            mode.clock_khz
        )),
    }
    show(&transcoder_timings(PIPE, &mode));
    show(&[pipe_src(PIPE, &mode)]);
    if let Some(function) = ddi_func_ctl(PIPE, PORT, &mode) {
        show(&[function]);
    }
    show(&primary_plane(PIPE, &mode, 0, mode.h[0] * 4));
    say(format_args!(
        "intel: modeset dry: {planned} writes planned, none made; power wells, DPCLKA, the PHY's buffer translations, DBUF and watermarks wait for the ground truth",
    ));
}
