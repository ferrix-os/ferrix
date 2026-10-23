// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0,
// drivers/gpu/drm/i915/display/intel_display_regs.h:
// Copyright © 2025 Intel Corporation
// drivers/gpu/drm/i915/display/intel_hotplug_irq.c, intel_ddi.c:
// Copyright © 2023, © 2014 Intel Corporation
// Under the MIT notice in this crate's i915/NOTICE.

//! A monitor plugged into, or pulled out of, the port the firmware lit
//! (`docs/DESKTOP.md` §3.4, step 3f).
//!
//! On Alder Lake-N (display version 13, a PCH of the ICP family's south
//! display) a combo PHY port's hot-plug detect is the PCH's: its live
//! state is a bit of `SDEISR`, which reads as long as the pin's detection
//! is enabled in `SHOTPLUG_CTL_DDI`. i915 reads exactly that bit to say a
//! combo port is connected (`icp_digital_port_connected`, `intel_ddi.c`),
//! and polls it when it cannot take the interrupt (`intel_hpd_poll`). The
//! port is the one `TRANS_DDI_FUNC_CTL_A` drives; its pin is
//! `xelpd_hpd_pin`'s. Type-C ports keep their live state in the north
//! display and the FIA, which this does not read yet: such a port is
//! reported as one hotplug cannot follow.

/// `SDEISR`: the south display engine's live interrupt state.
pub const SDEISR: u32 = 0xC_4000;
/// `SHOTPLUG_CTL_DDI`: the combo ports' hot-plug detection control.
pub const SHOTPLUG_CTL_DDI: u32 = 0xC_4030;
/// `_TRANSACONF`: transcoder A's configuration.
pub const TRANSCONF_A: u32 = 0x7_0008;
/// `TRANSCONF_ENABLE`.
pub const TRANSCONF_ENABLE: u32 = 1 << 31;
/// `TGL_TRANS_DDI_PORT_SHIFT`: which port a transcoder drives, plus one.
pub const TGL_TRANS_DDI_PORT_SHIFT: u32 = 27;
/// `TGL_TRANS_DDI_PORT_MASK`.
pub const TGL_TRANS_DDI_PORT_MASK: u32 = 0xF << TGL_TRANS_DDI_PORT_SHIFT;

/// A display port of Alder Lake-P/N: `PORT_A` and `PORT_B` are combo PHYs,
/// `PORT_TC1` to `PORT_TC4` Type-C (`xe_lpd_display`'s `port_mask`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Port {
    /// A combo PHY port: `PORT_A` is 0, `PORT_B` 1.
    Combo(u8),
    /// A Type-C port: `PORT_TC1` is 1.
    TypeC(u8),
}

impl Port {
    /// The port a `TRANS_DDI_FUNC_CTL` value drives (`TGL_TRANS_DDI_PORT_MASK`
    /// holds the port plus one; `PORT_TC1` is `PORT_D`, 3). `None` for no
    /// port or one Alder Lake-P/N does not have.
    #[must_use]
    pub const fn of_ddi_func_ctl(value: u32) -> Option<Self> {
        let field = (value & TGL_TRANS_DDI_PORT_MASK) >> TGL_TRANS_DDI_PORT_SHIFT;
        match field {
            1 => Some(Self::Combo(0)),
            2 => Some(Self::Combo(1)),
            4..=7 => Some(Self::TypeC((field - 3) as u8)),
            _ => None,
        }
    }

    /// The port's name as i915 says it: `A`, `B`, `TC1`...
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Combo(0) => "A",
            Self::Combo(1) => "B",
            Self::Combo(_) => "?",
            Self::TypeC(1) => "TC1",
            Self::TypeC(2) => "TC2",
            Self::TypeC(3) => "TC3",
            Self::TypeC(4) => "TC4",
            Self::TypeC(_) => "TC?",
        }
    }

    /// The live-state bit of this port's pin in `SDEISR`:
    /// `SDE_DDI_HOTPLUG_ICP(pin)`, bit 16 plus the pin's place among the
    /// DDI pins, for a combo port; `None` for a Type-C one.
    #[must_use]
    pub const fn live_bit(self) -> Option<u32> {
        match self {
            Self::Combo(index) if index < 4 => Some(1 << (16 + index as u32)),
            _ => None,
        }
    }

    /// `SHOTPLUG_CTL_DDI_HPD_ENABLE(pin)` for a combo port, 0 for a Type-C
    /// one.
    #[must_use]
    pub const fn detect_enable(self) -> u32 {
        match self {
            Self::Combo(index) if index < 4 => 0x8 << (index as u32 * 4),
            _ => 0,
        }
    }
}

/// How long a new live state must hold before it is believed: a cable
/// half in bounces, and i915 waits out the same with its storm detection.
pub const SETTLE_MS: u64 = 100;

/// How often the live state is read while no interrupt says it changed:
/// i915 polls connectors it cannot take an interrupt for every 10 seconds
/// (`INTEL_HPD_POLL`), which is too slow for a person pulling a cable out
/// and plugging it back in; a register read every half second costs
/// nothing.
pub const POLL_MS: u64 = 500;

/// The live state read again and again, and a change said once it has held
/// for [`SETTLE_MS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Debounce {
    /// What was last said.
    pub connected: bool,
    /// A different state seen, and since when, not yet held long enough.
    pending: Option<(bool, u64)>,
}

impl Debounce {
    /// Starting from `connected`, the state the port was found in.
    #[must_use]
    pub const fn new(connected: bool) -> Self {
        Self {
            connected,
            pending: None,
        }
    }

    /// The live state read `live` at `now_ms`: the new state, once, when a
    /// change has held for [`SETTLE_MS`].
    pub fn feed(&mut self, live: bool, now_ms: u64) -> Option<bool> {
        if live == self.connected {
            self.pending = None;
            return None;
        }
        match self.pending {
            Some((state, since)) if state == live => (now_ms.saturating_sub(since) >= SETTLE_MS)
                .then(|| {
                    self.connected = live;
                    self.pending = None;
                    live
                }),
            _ => {
                self.pending = Some((live, now_ms));
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Debounce, Port, SETTLE_MS};

    /// The N150's lit port: `TRANS_DDI_FUNC_CTL_A` driving `PORT_A`, whose
    /// pin is bit 16 and whose detection is bit 3 of `SHOTPLUG_CTL_DDI`.
    #[test]
    fn the_ports_and_their_bits() {
        let port_a = 0x8000_0000 | (1 << 27);
        assert_eq!(Port::of_ddi_func_ctl(port_a), Some(Port::Combo(0)));
        assert_eq!(Port::Combo(0).live_bit(), Some(1 << 16));
        assert_eq!(Port::Combo(0).detect_enable(), 0x8);
        assert_eq!(Port::of_ddi_func_ctl(2 << 27), Some(Port::Combo(1)));
        assert_eq!(Port::Combo(1).live_bit(), Some(1 << 17));
        assert_eq!(Port::Combo(1).detect_enable(), 0x80);
        assert_eq!(Port::of_ddi_func_ctl(4 << 27), Some(Port::TypeC(1)));
        assert_eq!(Port::TypeC(1).live_bit(), None);
        assert_eq!(Port::of_ddi_func_ctl(0), None);
        assert_eq!(Port::of_ddi_func_ctl(3 << 27), None, "no PORT_C on ADL-P/N");
    }

    /// A change is said once it has held, and a bounce is not.
    #[test]
    fn a_change_is_said_once_it_has_held() {
        let mut debounce = Debounce::new(true);
        assert_eq!(debounce.feed(true, 0), None);
        assert_eq!(debounce.feed(false, 10), None, "just seen");
        assert_eq!(debounce.feed(true, 20), None, "a bounce");
        assert_eq!(debounce.feed(false, 30), None);
        assert_eq!(debounce.feed(false, 30 + SETTLE_MS - 1), None);
        assert_eq!(debounce.feed(false, 30 + SETTLE_MS), Some(false));
        assert_eq!(debounce.feed(false, 1000), None, "said once");
        assert_eq!(debounce.feed(true, 2000), None);
        assert_eq!(debounce.feed(true, 2000 + SETTLE_MS), Some(true));
    }
}
