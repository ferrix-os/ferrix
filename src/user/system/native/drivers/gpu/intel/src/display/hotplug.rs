//! A monitor plugged into or pulled out of the lit port (`docs/DESKTOP.md`
//! §3.4, step 3f): the port's live hot-plug state read every
//! [`POLL_MS`](ferrix_intel_xelp::display::hotplug::POLL_MS) -- no MSI has
//! reached this driver on the box -- debounced, and each settled change
//! told to the display core as virtio-gpu tells it, by `MODES` with the
//! scanout enabled or not and, on a plug, the mode list again. The core
//! turns either into a connector change, and hyprix adds or removes the
//! screen.
//!
//! Only the port the firmware lit, and only a combo PHY port: a Type-C
//! port's live state is in the north display and the FIA. An HDMI or DVI
//! sink plugged back in is shown again with no programming, the transcoder
//! having run on; a `DisplayPort` one needs its link trained again, which
//! this driver does not do (step 3h), so it stays reported disconnected.

use ferrix_intel_xelp::display::hotplug::{
    Debounce, POLL_MS, Port, SDEISR, SETTLE_MS, SHOTPLUG_CTL_DDI, TRANSCONF_A, TRANSCONF_ENABLE,
};
use ferrix_intel_xelp::io::Mmio;
use ferrix_intel_xelp::plane::{
    TRANS_DDI_FUNC_CTL_A, TRANS_DDI_FUNC_ENABLE, TRANS_DDI_MODE_DP_MST, TRANS_DDI_MODE_DP_SST,
    TRANS_DDI_MODE_SELECT_SHIFT,
};

use super::regs::Regs;
use crate::say;

/// The lit port's hot-plug state.
pub(crate) struct Watch {
    regs: Regs,
    port: Port,
    bit: u32,
    debounce: Debounce,
    /// Whether the transcoder drives `DisplayPort`, which a replug cannot
    /// bring back without training the link.
    display_port: bool,
}

impl Watch {
    /// Start watching the port `TRANS_DDI_FUNC_CTL_A` drives: its detection
    /// enabled in `SHOTPLUG_CTL_DDI` if the firmware left it off (one bit
    /// set, the others kept, as i915's `icp_ddi_hpd_enable_detection`
    /// does), and the state it is in now taken as the starting one.
    /// `None`, said, for a port this cannot follow.
    pub(crate) fn start(mut regs: Regs) -> Option<Self> {
        let function = regs.read32(TRANS_DDI_FUNC_CTL_A);
        let Some(port) = Port::of_ddi_func_ctl(function) else {
            say(format_args!(
                "intel: card0: no hotplug: TRANS_DDI_FUNC_CTL_A {function:#010x} names no port"
            ));
            return None;
        };
        let Some(bit) = port.live_bit() else {
            say(format_args!(
                "intel: card0: no hotplug: port {} is Type-C, whose live state is not read yet",
                port.name()
            ));
            return None;
        };
        let control = regs.read32(SHOTPLUG_CTL_DDI);
        let enable = port.detect_enable();
        if control & enable == 0 {
            regs.write32(SHOTPLUG_CTL_DDI, control | enable);
            say(format_args!(
                "intel: card0: hotplug detection on port {} enabled (SHOTPLUG_CTL_DDI {control:#010x} -> {:#010x})",
                port.name(),
                regs.read32(SHOTPLUG_CTL_DDI)
            ));
        }
        let mode = (function >> TRANS_DDI_MODE_SELECT_SHIFT) & 0x7;
        let display_port = mode == TRANS_DDI_MODE_DP_SST || mode == TRANS_DDI_MODE_DP_MST;
        let live = regs.read32(SDEISR) & bit != 0;
        say(format_args!(
            "intel: card0: watching port {} for hotplug (SDEISR bit {}), {} now",
            port.name(),
            bit.trailing_zeros(),
            if live { "connected" } else { "disconnected" }
        ));
        Some(Self {
            regs,
            port,
            bit,
            debounce: Debounce::new(live),
            display_port,
        })
    }

    /// Read the live state at `now_ms`: the new state once a change has
    /// settled, and when to read it next.
    pub(crate) fn poll(&mut self, now_ms: u64) -> (Option<bool>, u64) {
        let live = self.regs.read32(SDEISR) & self.bit != 0;
        let changed = self.debounce.feed(live, now_ms);
        let next = if live == self.debounce.connected {
            POLL_MS
        } else {
            SETTLE_MS
        };
        let Some(connected) = changed else {
            return (None, now_ms + next);
        };
        if !connected {
            say(format_args!(
                "intel: card0: port {} disconnected",
                self.port.name()
            ));
            return (Some(false), now_ms + next);
        }
        // Plugged back in: shown again only where the transcoder still
        // runs, and not for DisplayPort, whose link is not trained here.
        let transcoder = self.regs.read32(TRANSCONF_A) & TRANSCONF_ENABLE != 0;
        let function = self.regs.read32(TRANS_DDI_FUNC_CTL_A) & TRANS_DDI_FUNC_ENABLE != 0;
        if self.display_port || !transcoder || !function {
            say(format_args!(
                "intel: card0: port {} has a sink again, but {}; it stays disconnected",
                self.port.name(),
                if self.display_port {
                    "a DisplayPort link is not trained again by this driver yet"
                } else {
                    "transcoder A no longer runs"
                }
            ));
            return (None, now_ms + next);
        }
        say(format_args!(
            "intel: card0: port {} connected",
            self.port.name()
        ));
        (Some(true), now_ms + next)
    }
}
