//! Which listed mode a program's `SETCRTC` names (`docs/DISPLAY.md` §9.3).
//!
//! The kernel's whole share of a mode list: it holds the list the driver
//! sent, finds the entry a request names, and sends that entry back in
//! `SET_MODE`. It makes no timing and reads no EDID; the drivers do
//! (`ferrix-display-modes`).

use crate::message::{Listed, Timing};

/// The listed timing a `SETCRTC`'s mode asks for (`docs/DISPLAY.md` §9.3):
/// the first entry equal to it in size, clock, totals, sync spans and sync
/// polarity; else the first of its size whose rounded refresh is the
/// request's `vrefresh`; else `None`, which the core answers `EINVAL`. What
/// is sent the driver is always the listed entry, never the request's
/// bytes (the consultant's condition 3, ledger 3117).
#[must_use]
pub fn listed_match(list: &[Listed], wanted: &ferrix_linux_abi::drm::ModeInfo) -> Option<Timing> {
    use ferrix_linux_abi::drm::{MODE_FLAG_PHSYNC, MODE_FLAG_PVSYNC};
    let exact = |timing: &Timing| {
        timing.clock_khz == wanted.clock
            && [
                timing.hdisplay,
                timing.hsync_start,
                timing.hsync_end,
                timing.htotal,
                timing.vdisplay,
                timing.vsync_start,
                timing.vsync_end,
                timing.vtotal,
            ] == [
                wanted.hdisplay,
                wanted.hsync_start,
                wanted.hsync_end,
                wanted.htotal,
                wanted.vdisplay,
                wanted.vsync_start,
                wanted.vsync_end,
                wanted.vtotal,
            ]
            && timing.hsync_high == (wanted.flags & MODE_FLAG_PHSYNC != 0)
            && timing.vsync_high == (wanted.flags & MODE_FLAG_PVSYNC != 0)
    };
    let sized =
        |timing: &&Timing| (timing.hdisplay, timing.vdisplay) == (wanted.hdisplay, wanted.vdisplay);
    let timings = || list.iter().map(|listed| &listed.timing);
    timings()
        .find(|timing| exact(timing))
        .or_else(|| {
            timings()
                .filter(sized)
                .find(|timing| timing.refresh_hz() == wanted.vrefresh)
        })
        .copied()
}
