//! The steps against a model of the box's display registers: power wells,
//! PLLs and DBUF slices that answer their requests, the port and pipe as
//! the firmware left them, and every value checked against what Linux
//! wrote for the same monitor (the 3h dump, 2026-10-23).

extern crate std;

use std::collections::BTreeMap;
use std::vec::Vec;

use super::{
    DBUF_CTL_S3_S4, DDI_BUF_CTL_A, DPCLKA_CFGCR0, DPLL_ENABLE, FUSE_STATUS, Light, MBUS_CTL,
    PWR_WELL_CTL_DDI2, PWR_WELL_CTL2, Refusal, Step, TRANSCONF_A, free_pll, is_hdmi, preflight,
    steps, take,
};
use crate::display::hotplug::Port;
use crate::display::modeset::Mode;
use crate::io::{Clock, Mmio};

/// The display as the firmware leaves it on the box: pipe A lit on port A
/// from DPLL0, PW1, PW2 and `PW_A` on, port B's clock gated.
struct Display {
    regs: BTreeMap<u32, u32>,
    writes: Vec<(u32, u32)>,
}

impl Display {
    fn firmware() -> Self {
        let mut regs = BTreeMap::new();
        let _ = regs.insert(PWR_WELL_CTL2, 0x0000_0C0F);
        let _ = regs.insert(FUSE_STATUS, 0xFFFF_FFFF);
        let _ = regs.insert(PWR_WELL_CTL_DDI2, 0x3);
        let _ = regs.insert(DPLL_ENABLE[0], 0xCC00_0000);
        // Port A from DPLL0 and on; port B gated; the Type-C clocks off.
        let _ = regs.insert(DPCLKA_CFGCR0, 0x01E0_7800);
        let _ = regs.insert(TRANSCONF_A, 0xC000_0000);
        let _ = regs.insert(DDI_BUF_CTL_A, 0x8000_0000);
        // The PHY's lanes as they come up: training on, other fields
        // whatever they are.
        for phy in [0x16_2000_u32, 0x6_C000] {
            for ln in 0..4 {
                let _ = regs.insert(phy + 0x880 + ln * 0x100 + 0x14, 0x8000_0000);
                let _ = regs.insert(phy + 0x800 + ln * 0x100 + 0x4, 0x1C00_0004);
            }
            let _ = regs.insert(phy + 0x28, 0x0003_00F0);
        }
        Self {
            regs,
            writes: Vec::new(),
        }
    }

    fn value(&self, offset: u32) -> u32 {
        self.regs.get(&offset).copied().unwrap_or(0)
    }
}

impl Mmio for Display {
    fn read32(&mut self, offset: u32) -> u32 {
        self.value(offset)
    }

    fn write32(&mut self, offset: u32, value: u32) {
        self.writes.push((offset, value));
        let mut stored = value;
        // Requests answered: a power well's state follows its request bit,
        // a PLL's power and lock follow theirs, a DBUF slice's state its
        // request, a transcoder's state its enable, and a buffer is not
        // idle once on.
        if offset == PWR_WELL_CTL2 || offset == PWR_WELL_CTL_DDI2 {
            for index in 0..16 {
                if value & (1 << (index * 2 + 1)) != 0 {
                    stored |= 1 << (index * 2);
                }
            }
        }
        if DPLL_ENABLE.contains(&offset) {
            if value & (1 << 27) != 0 {
                stored |= 1 << 26;
            }
            if value & (1 << 31) != 0 {
                stored |= 1 << 30;
            }
        }
        if DBUF_CTL_S3_S4.contains(&offset) && value & (1 << 31) != 0 {
            stored |= 1 << 30;
        }
        if offset == TRANSCONF_A + 0x1000 && value & (1 << 31) != 0 {
            stored |= 1 << 30;
        }
        let _ = self.regs.insert(offset, stored);
        // A combo PHY's group registers write every lane.
        let groups = [0x16_2000_u32, 0x6_C000]
            .into_iter()
            .flat_map(|phy| [(phy + 0x600, phy + 0x800), (phy + 0x680, phy + 0x880)]);
        for (group, lanes) in groups {
            if (group..group + 0x40).contains(&offset) {
                for ln in 0..4 {
                    let _ = self.regs.insert(offset - group + lanes + ln * 0x100, value);
                }
            }
        }
    }
}

struct Instant(u64);

impl Clock for Instant {
    fn now_us(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }

    fn delay_us(&mut self, us: u32) {
        self.0 += u64::from(us);
    }
}

/// The small monitor of the dump: 1024x600 at 50.25 MHz, negative syncs,
/// DVI (its EDID is CEA revision 0, no HDMI block).
const SMALL: Mode = Mode {
    clock_khz: 50_250,
    h: [1024, 1068, 1156, 1344],
    v: [600, 603, 609, 625],
    phsync: false,
    pvsync: false,
};

fn light(mode: Mode, hdmi: bool) -> Light {
    Light {
        port: Port::Combo(1),
        pipe: 1,
        mode,
        hdmi,
        pll: 1,
        nssc_khz: 38_400,
        surface: 0x00B9_C000,
        stride: 0x78,
        surface_size: (1920, 1080),
    }
}

/// Every step taken on the firmware's display, and what each register holds
/// at the end.
fn lit(light: &Light) -> Display {
    let mut display = Display::firmware();
    preflight(&mut display, light).expect("the firmware's display can take it");
    let mut clock = Instant(0);
    let mut failed = None;
    let all = steps(
        light,
        &mut |step| match take(&mut display, &mut clock, step) {
            Ok(_) => true,
            Err(got) => {
                failed = Some((step, got));
                false
            }
        },
    );
    assert!(all, "a step failed: {failed:?}");
    display
}

/// The 1024x600 monitor on port B, pipe B, from PLL 1: the PLL's words,
/// the transcoder's, the function and the PHY's are the ones Linux wrote
/// for the same monitor (on its pipe A, so the transcoder's address is B's
/// here and its value A's there).
#[test]
fn the_small_monitor_is_lit_as_linux_lit_it() {
    let display = lit(&light(SMALL, false));
    // The PLL: the dump's DPLL1.
    assert_eq!(display.value(0x16_428C), 0x0018_01D7);
    assert_eq!(display.value(0x16_4290), 0x0000_2684);
    assert_eq!(display.value(DPLL_ENABLE[1]) & 0xCC00_0000, 0xCC00_0000);
    // Port B from DPLL1, ungated; port A's choice and clock untouched.
    let clocks = display.value(DPCLKA_CFGCR0);
    assert_eq!((clocks >> 2) & 3, 1);
    assert_eq!(clocks & (1 << 11), 0);
    assert_eq!(clocks & 3, 0);
    assert_eq!(clocks & (1 << 10), 0);
    // The transcoder: the dump's transcoder A words, at B.
    for (offset, want) in [
        (0x6_1000, 0x053F_03FF),
        (0x6_1004, 0x053F_03FF),
        (0x6_1008, 0x0483_042B),
        (0x6_100C, 0x0270_0257),
        (0x6_1014, 0x0260_025A),
        (0x6_101C, 0x03FF_0257),
    ] {
        assert_eq!(display.value(offset), want, "{offset:#x}");
    }
    // DVI on DDI B, negative syncs: the dump's 0x91000100 less bit 8.
    assert_eq!(display.value(0x6_1400), 0x9100_0000);
    assert_eq!(display.value(0x4_6144), 0x2000_0000, "TRANS_CLK_SEL port B");
    assert_eq!(display.value(0x7_1030), 0x0080_0100, "PIPE_MISC");
    assert_eq!(display.value(TRANSCONF_A + 0x1000), 0xC000_0000);
    // The plane: the dump's, pipe B's DBUF share and watermarks.
    assert_eq!(display.value(0x7_1180), 0x9400_0000);
    assert_eq!(display.value(0x7_1188), 0x78);
    assert_eq!(display.value(0x7_1190), 0x0257_03FF);
    assert_eq!(display.value(0x7_119C), 0x00B9_C000);
    assert_eq!(display.value(0x7_127C), 0x07DB_0000);
    assert_eq!(display.value(0x7_1268), 0x8000_001E);
    // The PHY: the dump's lane words.
    let phy = 0x6_C000;
    for ln in 0..4 {
        let lane = phy + 0x880 + ln * 0x100;
        assert_eq!(
            display.value(lane + 0x14) & 0xE61C_0038,
            0xA008_0030,
            "DW5 lane {ln}"
        );
        assert_eq!(display.value(lane + 0x8) & 0xB8FF, 0x9098, "DW2 lane {ln}");
        let dw4 = display.value(lane + 0x10);
        assert_eq!(dw4 & 0x3_FFFF, 0x3F, "DW4 lane {ln}");
        assert_eq!(dw4 >> 31, u32::from(ln >= 1), "loadgen lane {ln}");
        assert_eq!(display.value(lane + 0x1C), 0x6000_0000, "DW7 lane {ln}");
    }
    assert_eq!(
        display.value(phy + 0x600 + 4) & (1 << 26),
        0,
        "no common keeper"
    );
    assert_eq!(display.value(phy + 0x28) & 0xF0, 0, "every lane up");
    assert_eq!(display.value(DDI_BUF_CTL_A + 0x100), 0x8000_0000);
    // Pipe A's port and PLL were never written.
    assert!(
        display.writes.iter().all(|(offset, _)| ![
            DDI_BUF_CTL_A,
            TRANSCONF_A,
            DPLL_ENABLE[0],
            0x16_4284,
            0x6_0400
        ]
        .contains(offset)),
        "pipe A's registers untouched"
    );
}

/// The order i915 keeps: the PLL's dividers between its power and its
/// enable, the transcoder on before the port's buffer, the plane after
/// both with its surface last.
#[test]
fn the_steps_keep_i915s_order() {
    let display = lit(&light(Mode::CEA_1080P60, true));
    let at = |offset: u32| {
        display
            .writes
            .iter()
            .position(|(written, _)| *written == offset)
            .unwrap_or_else(|| panic!("{offset:#x} never written"))
    };
    assert!(at(DPLL_ENABLE[1]) < at(0x16_428C));
    let enabled = display
        .writes
        .iter()
        .rposition(|(offset, _)| *offset == DPLL_ENABLE[1])
        .expect("enabled");
    assert!(at(0x16_4290) < enabled);
    let on = display
        .writes
        .iter()
        .rposition(|(offset, _)| *offset == TRANSCONF_A + 0x1000)
        .expect("on");
    assert!(on < at(DDI_BUF_CTL_A + 0x100));
    assert!(at(DDI_BUF_CTL_A + 0x100) < at(0x7_1188));
    assert!(at(0x7_1188) < at(0x7_119C) && at(0x7_1180) < at(0x7_119C));
    assert_eq!(
        display.writes.last().map(|(offset, _)| *offset),
        Some(0x7_119C),
        "the plane's surface last"
    );
    // A line of 1080p60 is 2200 / 148.5 MHz = 14.8 us, 119 eighths.
    assert_eq!(display.value(0x4_5274), 119);
    // The TV's mode: the dump's DPLL0 words, here on DPLL1; HDMI.
    assert_eq!(display.value(0x16_428C), 0x0010_01D0);
    assert_eq!(display.value(0x16_4290), 0x0000_0E84);
    assert_eq!(display.value(0x6_1400), 0x8803_0000 + (1 << 27));
}

/// What stops it: a pipe or port already on, MBUS joined, display C-states,
/// no PLL free, a clock above 148.5 MHz, a port other than B.
#[test]
fn what_is_refused() {
    let small = light(SMALL, false);
    let refused = |change: &dyn Fn(&mut Display), light: &Light| {
        let mut display = Display::firmware();
        change(&mut display);
        preflight(&mut display, light)
    };
    assert_eq!(refused(&|_| {}, &small), Ok(()));
    assert_eq!(
        refused(&|d| d.write32(TRANSCONF_A + 0x1000, 1 << 31), &small),
        Err(Refusal::PipeOn)
    );
    assert_eq!(
        refused(&|d| d.write32(DDI_BUF_CTL_A + 0x100, 1 << 31), &small),
        Err(Refusal::PortOn)
    );
    assert_eq!(
        refused(&|d| d.write32(MBUS_CTL, 1 << 31), &small),
        Err(Refusal::MbusJoined)
    );
    assert_eq!(
        refused(&|d| d.write32(0x4_5504, 1), &small),
        Err(Refusal::DcStates)
    );
    assert_eq!(
        refused(&|d| d.write32(DPLL_ENABLE[1], 1 << 31), &small),
        Err(Refusal::NoPll)
    );
    let mut fast = small;
    fast.mode.clock_khz = 241_500;
    assert_eq!(refused(&|_| {}, &fast), Err(Refusal::Clock));
    let mut other = small;
    other.port = Port::Combo(0);
    assert_eq!(refused(&|_| {}, &other), Err(Refusal::Unsupported));
}

/// The free PLL is the one the lit port does not take its clock from.
#[test]
fn the_free_pll_is_the_other_one() {
    let mut display = Display::firmware();
    assert_eq!(free_pll(&mut display), Some(1));
    // Port A from DPLL1 instead, DPLL0 off: DPLL0 is free.
    display.write32(DPCLKA_CFGCR0, 0x01E0_7801);
    display.write32(DPLL_ENABLE[0], 0);
    let _ = display.regs.insert(DPLL_ENABLE[1], 0xCC00_0000);
    assert_eq!(free_pll(&mut display), Some(0));
}

/// A wait that never sees its value stops the steps there.
#[test]
fn a_power_well_that_never_comes_stops_it() {
    struct Dead(Display);
    impl Mmio for Dead {
        fn read32(&mut self, offset: u32) -> u32 {
            self.0.read32(offset)
        }
        fn write32(&mut self, offset: u32, value: u32) {
            // Requests go unanswered.
            let _ = self.0.regs.insert(offset, value);
            self.0.writes.push((offset, value));
        }
    }
    let mut dead = Dead(Display::firmware());
    let mut clock = Instant(0);
    let mut taken = 0;
    let all = steps(&light(SMALL, false), &mut |step| {
        taken += 1;
        take(&mut dead, &mut clock, step).is_ok() || !matches!(step, Step::Wait { .. })
    });
    assert!(!all);
    // PW2 is the firmware's already; PW_B's request, then its state, and
    // no further.
    assert_eq!(taken, 5);
}

/// The TV's EDID has an HDMI block; a DVI monitor's has none.
#[test]
fn an_hdmi_sink_is_known_by_its_vendor_block() {
    let mut edid = [0_u8; 256];
    edid[128] = 0x02;
    edid[130] = 12;
    // A video block of two bytes, then the HDMI VSDB.
    edid[132] = (2 << 5) | 2;
    edid[135] = (3 << 5) | 5;
    edid[136..139].copy_from_slice(&[0x03, 0x0C, 0x00]);
    assert!(is_hdmi(&edid));
    edid[136] = 0x04;
    assert!(!is_hdmi(&edid));
    assert!(!is_hdmi(&edid[..128]), "no extension");
}
