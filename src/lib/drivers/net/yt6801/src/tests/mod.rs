//! The chip's own registers against a model of them.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::vec;
use std::vec::Vec;

use ferrix_dwmac::{Clock, Registers, regs};

use crate::{
    Efuse, GMAC_OFFSET, Gmac, MacSource, enable_interrupts, generated, glue, options, prepare,
    valid_unicast,
};

/// BAR 0 of a YT6801: the glue's registers, an eFuse, and the core's
/// address registers as plain storage.
#[derive(Debug, Default)]
struct Chip {
    registers: BTreeMap<u32, u32>,
    efuse: Vec<u8>,
    /// Reads of the eFuse still to answer with zeroes after a reset, as the
    /// eFuse loads.
    loading: u32,
    /// The time the last reset happened, by the clock.
    reset_at: Option<u64>,
    clock: Rc<Cell<u64>>,
    /// The eFuse never finishes a read.
    efuse_stuck: bool,
    resets: u32,
    /// Every write, in order.
    writes: Vec<(u32, u32)>,
}

impl Chip {
    fn new(clock: &Rc<Cell<u64>>) -> Self {
        Chip {
            efuse: vec![0; 256],
            clock: Rc::clone(clock),
            ..Chip::default()
        }
    }

    /// Patch `index` of the eFuse: `address`, `value`.
    fn patch(&mut self, index: usize, address: u16, value: u32) {
        let at = 18 + 6 * index;
        self.efuse[at..at + 2].copy_from_slice(&address.to_le_bytes());
        self.efuse[at + 2..at + 6].copy_from_slice(&value.to_le_bytes());
    }

    fn reg(&self, offset: u32) -> u32 {
        self.registers.get(&offset).copied().unwrap_or(0)
    }
}

#[derive(Debug)]
struct Bar(Rc<std::cell::RefCell<Chip>>);

impl Registers for Bar {
    fn read(&self, offset: u32) -> u32 {
        assert_eq!(offset % 4, 0);
        assert!(
            !(glue::MSIX_TABLE..glue::MSIX_PBA + 0x100).contains(&offset),
            "the driver never touches the MSI-X table or PBA: {offset:#x}"
        );
        let chip = self.0.borrow();
        chip.reg(offset)
    }

    fn write(&mut self, offset: u32, value: u32) {
        assert_eq!(offset % 4, 0);
        assert!(
            !(glue::MSIX_TABLE..glue::MSIX_PBA + 0x100).contains(&offset),
            "the driver never touches the MSI-X table or PBA: {offset:#x}"
        );
        let mut chip = self.0.borrow_mut();
        chip.writes.push((offset, value));
        match offset {
            glue::SYS_RESET => {
                let before = chip.reg(glue::SYS_RESET);
                if before & glue::SYS_RESET_BIT == 0 && value & glue::SYS_RESET_BIT != 0 {
                    // The whole chip resets: the core's registers, the
                    // PHY's release, and the eFuse starts loading.
                    chip.registers.clear();
                    chip.resets += 1;
                    chip.loading = 3;
                    chip.reset_at = Some(chip.clock.get());
                }
                let _ = chip.registers.insert(offset, value);
            }
            glue::EFUSE_OP_CTRL_0 => {
                let _ = chip.registers.insert(offset, value);
                if chip.efuse_stuck || value & glue::EFUSE_START == 0 {
                    let _ = chip.registers.insert(glue::EFUSE_OP_CTRL_1, 0);
                    return;
                }
                assert_eq!(value & 0b11, glue::EFUSE_ROW_READ);
                let address = ((value >> 8) & 0xFF) as usize;
                // Read too soon after a reset, the eFuse answers zeroes.
                let soon = chip
                    .reset_at
                    .is_some_and(|at| chip.clock.get() < at + 2_000_000);
                let byte = if soon { 0 } else { chip.efuse[address] };
                let _ = chip.registers.insert(
                    glue::EFUSE_OP_CTRL_1,
                    glue::EFUSE_DONE | (u32::from(byte) << 24),
                );
            }
            _ => {
                let _ = chip.registers.insert(offset, value);
            }
        }
    }
}

#[derive(Debug)]
struct FakeClock(Rc<Cell<u64>>);

impl Clock for FakeClock {
    fn now_nanos(&self) -> u64 {
        self.0.get()
    }

    fn pause(&mut self, nanos: u64) {
        self.0.set(self.0.get() + nanos.max(1));
    }
}

struct Rig {
    chip: Rc<std::cell::RefCell<Chip>>,
    bar: Bar,
    clock: FakeClock,
}

impl Rig {
    fn new() -> Self {
        let time = Rc::new(Cell::new(0));
        let chip = Rc::new(std::cell::RefCell::new(Chip::new(&time)));
        Rig {
            bar: Bar(Rc::clone(&chip)),
            chip,
            clock: FakeClock(time),
        }
    }
}

const MAC: [u8; 6] = [0x00, 0x0A, 0x35, 0xAB, 0xCD, 0xEF];

#[test]
fn the_efuse_address_wins_and_is_read_after_the_reset_has_settled() {
    let mut rig = Rig::new();
    {
        let mut chip = rig.chip.borrow_mut();
        chip.patch(0, 0x1004, 0xDEAD_BEEF);
        chip.patch(1, glue::PATCH_MACA0HR, 0x0000_000A);
        chip.patch(2, glue::PATCH_MACA0LR, 0x35AB_CDEF);
    }
    let prepared = prepare(&mut rig.bar, &mut rig.clock, [1, 2, 3]);
    assert_eq!(prepared.efuse, Efuse::Found(MAC));
    assert_eq!(prepared.mac, MAC);
    assert_eq!(prepared.source, MacSource::Efuse);
    let chip = rig.chip.borrow();
    assert_eq!(chip.resets, 1);
    assert_ne!(
        chip.reg(glue::EPHY_CTRL) & glue::EPHY_RELEASE,
        0,
        "PHY released"
    );
    // The reset: the bit cleared, then set.
    let resets: Vec<u32> = chip
        .writes
        .iter()
        .filter(|(offset, _)| *offset == glue::SYS_RESET)
        .map(|(_, value)| *value & glue::SYS_RESET_BIT)
        .collect();
    assert_eq!(resets, [0, glue::SYS_RESET_BIT]);
    // The PHY was released after the reset, not before.
    let reset = chip.writes.iter().rposition(|(o, _)| *o == glue::SYS_RESET);
    let release = chip.writes.iter().position(|(o, _)| *o == glue::EPHY_CTRL);
    assert!(release > reset);
}

#[test]
fn an_empty_efuse_falls_back_to_what_firmware_left_in_the_core() {
    let mut rig = Rig::new();
    {
        let mut chip = rig.chip.borrow_mut();
        let _ = chip
            .registers
            .insert(GMAC_OFFSET + regs::MAC_ADDRESS0_LOW, 0xAB35_0A00);
        let _ = chip
            .registers
            .insert(GMAC_OFFSET + regs::MAC_ADDRESS0_HIGH, 0x8000_EFCD);
    }
    let prepared = prepare(&mut rig.bar, &mut rig.clock, [1, 2, 3]);
    assert_eq!(prepared.efuse, Efuse::Absent);
    assert_eq!(prepared.firmware, MAC);
    assert_eq!(prepared.mac, MAC);
    assert_eq!(prepared.source, MacSource::Firmware);
}

#[test]
fn with_neither_an_address_is_made_up_locally_administered() {
    let mut rig = Rig::new();
    {
        // The core's reset value: all ones, a group address.
        let mut chip = rig.chip.borrow_mut();
        let _ = chip
            .registers
            .insert(GMAC_OFFSET + regs::MAC_ADDRESS0_LOW, u32::MAX);
        let _ = chip
            .registers
            .insert(GMAC_OFFSET + regs::MAC_ADDRESS0_HIGH, 0xFFFF);
    }
    let prepared = prepare(&mut rig.bar, &mut rig.clock, [7, 8, 9]);
    assert_eq!(prepared.mac, [0x02, 0x1F, 0x0A, 7, 8, 9]);
    assert_eq!(prepared.source, MacSource::Generated);
    assert!(valid_unicast(prepared.mac));
}

#[test]
fn an_efuse_address_that_is_not_unicast_is_not_believed() {
    for (high, low) in [(0x0100, 0), (0, 0), (0xFFFF, u32::MAX)] {
        let mut rig = Rig::new();
        {
            let mut chip = rig.chip.borrow_mut();
            chip.patch(0, glue::PATCH_MACA0LR, low);
            chip.patch(1, glue::PATCH_MACA0HR, high);
        }
        let prepared = prepare(&mut rig.bar, &mut rig.clock, [1, 2, 3]);
        assert_eq!(prepared.efuse, Efuse::Absent, "{high:#x} {low:#x}");
        assert_eq!(prepared.source, MacSource::Generated);
    }
}

#[test]
fn half_an_address_is_none() {
    let mut rig = Rig::new();
    rig.chip
        .borrow_mut()
        .patch(0, glue::PATCH_MACA0LR, 0x35AB_CDEF);
    let prepared = prepare(&mut rig.bar, &mut rig.clock, [1, 2, 3]);
    assert_eq!(prepared.efuse, Efuse::Absent);
}

#[test]
fn a_full_patch_region_without_the_address_ends() {
    let mut rig = Rig::new();
    {
        let mut chip = rig.chip.borrow_mut();
        for index in 0..39 {
            chip.patch(index, 0x1000 + index as u16 * 4, index as u32);
        }
        // The region's last byte is the eFuse's 252nd: no 40th patch.
    }
    let prepared = prepare(&mut rig.bar, &mut rig.clock, [1, 2, 3]);
    assert_eq!(prepared.efuse, Efuse::Absent);
}

#[test]
fn an_efuse_that_never_answers_times_out_and_falls_back() {
    let mut rig = Rig::new();
    rig.chip.borrow_mut().efuse_stuck = true;
    let before = rig.clock.0.get();
    let prepared = prepare(&mut rig.bar, &mut rig.clock, [1, 2, 3]);
    assert_eq!(prepared.efuse, Efuse::Timeout);
    assert_eq!(prepared.source, MacSource::Generated);
    // One byte's patience, not 39 patches' worth.
    assert!(rig.clock.0.get() - before < 30_000_000);
}

#[test]
fn interrupts_are_let_out() {
    let mut rig = Rig::new();
    let _ = rig
        .chip
        .borrow_mut()
        .registers
        .insert(glue::MGMT_INT_CTRL0, 0xFFFF_0000);
    enable_interrupts(&mut rig.bar);
    let chip = rig.chip.borrow();
    assert_eq!(chip.reg(glue::MGMT_INT_CTRL0), 0);
    assert_eq!(chip.reg(glue::OOB_WOL_CTRL) & glue::OOB_WOL_DISABLE, 1);
    assert_eq!(chip.reg(glue::INT_MODERATION), 200 | (200 << 16));
}

#[test]
fn the_core_is_reached_at_its_offset() {
    let rig = Rig::new();
    let mut gmac = Gmac(rig.bar);
    gmac.write(regs::DMA_MODE, 5);
    assert_eq!(rig.chip.borrow().reg(0x3000), 5);
    assert_eq!(gmac.read(regs::DMA_MODE), 5);
}

#[test]
fn the_options_are_one_vector_s() {
    let options = options(MAC);
    assert_eq!(options.interrupt_mode, 0);
    assert_eq!(options.bus.phy, 0);
    assert_eq!(options.bus.clock_range, 2);
    assert!(options.bus.preamble_suppression);
    assert_eq!(options.mac, MAC);
    assert_eq!(generated([0, 0, 0])[0] & 0b11, 0b10, "local, unicast");
}
