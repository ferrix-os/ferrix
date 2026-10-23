//! A monitor plugged into or pulled out of the lit port (`docs/DESKTOP.md`
//! §3.4, step 3f): the port's live hot-plug state read every
//! [`POLL_MS`](ferrix_intel_xelp::display::hotplug::POLL_MS) -- no MSI has
//! reached this driver on the box -- debounced, and each settled change
//! told to the display core as virtio-gpu tells it, by `MODES` with the
//! scanout enabled or not and, on a plug, the mode list again. The core
//! turns either into a connector change, and hyprix adds or removes the
//! screen.
//!
//! The port the firmware lit and, with a second pipe, the other combo port
//! (`docs/DESKTOP.md` §3.5); only combo PHY ports: a Type-C port's live
//! state is in the north display and the FIA. An HDMI or DVI
//! sink plugged back in is shown again with no programming, the transcoder
//! having run on; a `DisplayPort` one needs its link trained again, which
//! this driver does not do (step 3h), so it stays reported disconnected.

use ferrix_intel_xelp::display::gmbus;
use ferrix_intel_xelp::display::hotplug::{
    Debounce, POLL_MS, Port, SDEISR, SETTLE_MS, SHOTPLUG_CTL_DDI, TRANSCONF_A, TRANSCONF_ENABLE,
};
use ferrix_intel_xelp::io::{Clock, Mmio};
use ferrix_intel_xelp::plane::{
    TRANS_DDI_FUNC_CTL_A, TRANS_DDI_FUNC_ENABLE, TRANS_DDI_MODE_DP_MST, TRANS_DDI_MODE_DP_SST,
    TRANS_DDI_MODE_SELECT_SHIFT,
};

use super::regs::Regs;
use crate::say;

/// A combo port's hot-plug state, and the pipe that shows it.
pub(crate) struct Watch {
    port: Port,
    /// The transcoder and pipe that drive it: A is 0.
    pub(crate) pipe: u32,
    bit: u32,
    debounce: Debounce,
    /// Whether the transcoder drives `DisplayPort`, which a replug cannot
    /// bring back without training the link.
    display_port: bool,
}

impl Watch {
    /// Start watching the port `TRANS_DDI_FUNC_CTL_A` drives, as pipe A's.
    /// `None`, said, for a port this cannot follow.
    pub(crate) fn start(regs: &mut Regs) -> Option<Self> {
        let function = regs.read32(TRANS_DDI_FUNC_CTL_A);
        let Some(port) = Port::of_ddi_func_ctl(function) else {
            say(format_args!(
                "intel: card0: no hotplug: TRANS_DDI_FUNC_CTL_A {function:#010x} names no port"
            ));
            return None;
        };
        Self::on(regs, port, 0)
    }

    /// Start watching `port` as `pipe`'s: its detection enabled in
    /// `SHOTPLUG_CTL_DDI` if the firmware left it off (one bit set, the
    /// others kept, as i915's `icp_ddi_hpd_enable_detection` does), and
    /// the state it is in now taken as the starting one. `None`, said, for
    /// a Type-C port.
    pub(crate) fn on(regs: &mut Regs, port: Port, pipe: u32) -> Option<Self> {
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
        let function = regs.read32(TRANS_DDI_FUNC_CTL_A + pipe * 0x1000);
        let mode = (function >> TRANS_DDI_MODE_SELECT_SHIFT) & 0x7;
        let display_port = function & TRANS_DDI_FUNC_ENABLE != 0
            && (mode == TRANS_DDI_MODE_DP_SST || mode == TRANS_DDI_MODE_DP_MST);
        let live = regs.read32(SDEISR) & bit != 0;
        say(format_args!(
            "intel: card0: watching port {} for hotplug (SDEISR bit {}), {} now",
            port.name(),
            bit.trailing_zeros(),
            if live { "connected" } else { "disconnected" }
        ));
        Some(Self {
            port,
            pipe,
            bit,
            debounce: Debounce::new(live),
            display_port,
        })
    }

    /// The port watched.
    pub(crate) const fn port(&self) -> Port {
        self.port
    }

    /// The monitor's EDID, read over GMBUS on the port's DDC pins (step
    /// 3g): the base block and its first extension. `None`, said, when
    /// nothing answers.
    pub(crate) fn read_edid(
        &self,
        regs: &mut Regs,
    ) -> Option<([u8; 2 * gmbus::EDID_BLOCK], usize)> {
        let mut out = [0_u8; 2 * gmbus::EDID_BLOCK];
        let pin = gmbus::pin_of(self.port);
        match gmbus::read_edid(regs, &mut Monotonic, pin, &mut out) {
            Ok(length) => {
                say(format_args!(
                    "intel: card0: port {}'s monitor's EDID read over GMBUS pin {pin}: {length} bytes",
                    self.port.name()
                ));
                Some((out, length))
            }
            Err(why) => {
                say(format_args!(
                    "intel: card0: no EDID from port {} over GMBUS pin {pin}: {why:?}",
                    self.port.name()
                ));
                None
            }
        }
    }

    /// Read the live state at `now_ms`: the settled change, if one, and when
    /// to read it next. A plug is reported only where the pipe's
    /// transcoder runs (`running` says whether it may be lit first).
    pub(crate) fn poll(&mut self, regs: &mut Regs, now_ms: u64) -> (Option<bool>, u64) {
        let live = regs.read32(SDEISR) & self.bit != 0;
        let changed = self.debounce.feed(live, now_ms);
        let next = if live == self.debounce.connected {
            POLL_MS
        } else {
            SETTLE_MS
        };
        let Some(connected) = changed else {
            return (None, now_ms + next);
        };
        say(format_args!(
            "intel: card0: port {} {}",
            self.port.name(),
            if connected {
                "has a sink"
            } else {
                "disconnected"
            }
        ));
        (Some(connected), now_ms + next)
    }

    /// Whether a sink plugged back in is shown with no programming: the
    /// pipe's transcoder and function still run, and not for `DisplayPort`,
    /// whose link is not trained here.
    pub(crate) fn shown_again(&self, regs: &mut Regs) -> bool {
        let at = self.pipe * 0x1000;
        let transcoder = regs.read32(TRANSCONF_A + at) & TRANSCONF_ENABLE != 0;
        let function = regs.read32(TRANS_DDI_FUNC_CTL_A + at) & TRANS_DDI_FUNC_ENABLE != 0;
        if self.display_port || !transcoder || !function {
            say(format_args!(
                "intel: card0: port {} has a sink again, but {}",
                self.port.name(),
                if self.display_port {
                    "a DisplayPort link is not trained again by this driver yet"
                } else {
                    "its transcoder does not run"
                }
            ));
            return false;
        }
        true
    }
}

/// `CLOCK_MONOTONIC` in microseconds, and a busy wait on it: what a GMBUS
/// transfer polls by.
pub(crate) struct Monotonic;

impl Clock for Monotonic {
    fn now_us(&mut self) -> u64 {
        ferrix_rt::linux::monotonic_nanos().unwrap_or(0) / 1_000
    }

    fn delay_us(&mut self, us: u32) {
        let until = self.now_us().saturating_add(u64::from(us));
        while self.now_us() < until {
            core::hint::spin_loop();
        }
    }
}
