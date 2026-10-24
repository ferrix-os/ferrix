//! A model of an RTL8111H: its register window, the four indirect buses
//! behind it, its PHY, and "physical" memory its DMA reaches by address.
//!
//! It is written from the chip's side, not the driver's: the DMA finds the
//! rings by the descriptor address registers, walks them by device address,
//! reads a transmit descriptor's buffer address and length from its fields,
//! and writes a receive descriptor back as the chip would. The station
//! address and the configuration registers ignore writes while the
//! configuration lock is shut, as the chip does. So a driver that encoded a
//! descriptor or a register wrongly, or forgot the lock, meets a model that
//! reads it as the chip does.
//!
//! The knobs a test turns are the hostile ones: a reset that never ends, a
//! bus that never answers, another XID, a chip that has vanished, a
//! written-back length past the buffer, error and fragment bits, and a
//! system error.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::vec;
use std::vec::Vec;

use crate::desc::{self, DESCRIPTOR_BYTES};
use crate::regs::{self, phy};
use crate::{Clock, Memory, PAGE, Pages, Registers};

/// The identifier the RTL8111H's internal PHY answers: Realtek's generic
/// gigabit PHY.
pub(crate) const PHY_ID: u32 = 0x001C_C800;

/// The permanent address the model's ERI holds.
pub(crate) const PERMANENT: [u8; 6] = [0xBC, 0xFC, 0xE7, 0x52, 0x82, 0x8A];

/// How a written-back receive descriptor lies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Lie {
    /// A length past the buffer.
    Length(u32),
    /// The error summary set.
    Error,
    /// Only the first piece of a frame.
    Piece,
}

/// The chip.
#[derive(Debug)]
pub(crate) struct Model {
    /// The register window's plain bytes.
    bytes: [u8; 256],
    /// The XID the transmit configuration reports.
    pub(crate) xid: u32,
    /// The chip answers nothing: every read is all ones.
    pub(crate) vanished: bool,
    /// The chip's reset never ends.
    pub(crate) stuck_in_reset: bool,
    /// The ERI bus never answers.
    pub(crate) eri_dead: bool,
    /// The PHY's bus never answers.
    pub(crate) phy_dead: bool,
    /// Resets seen.
    pub(crate) resets: u32,
    /// ERI words by address.
    pub(crate) eri: BTreeMap<u16, u32>,
    /// MAC OCP words by address.
    pub(crate) mac_ocp: BTreeMap<u16, u16>,
    /// PHY OCP words by address.
    pub(crate) phy_ocp: BTreeMap<u16, u16>,
    /// EPHY registers.
    pub(crate) ephy: [u16; 32],
    /// Reads of BMCR still to answer with the reset bit set.
    phy_reset_reads: u32,
    /// The PHY was reset, then told to negotiate.
    pub(crate) negotiating: bool,
    /// What the link partner offers, as the PHY status register would
    /// read with the link up; `None` with no cable.
    pub(crate) partner: Option<u8>,
    /// Memory by page.
    memory: BTreeMap<u64, Vec<u8>>,
    /// The next transmit descriptor the DMA reads.
    tx_next: u32,
    /// The next receive descriptor the DMA writes.
    rx_next: u32,
    /// Frames sent, in order.
    pub(crate) sent: Vec<Vec<u8>>,
    /// Doorbells rung.
    pub(crate) doorbells: u32,
    /// The next written-back receive descriptor lies so.
    pub(crate) lie: Option<Lie>,
    /// Writes to the station address and configuration registers made
    /// while they were locked, which the chip ignored.
    pub(crate) locked_writes: u32,
}

impl Model {
    /// A chip as firmware leaves it: an RTL8111H, out of reset, with its
    /// permanent address in ERI and none in its receive filter.
    pub(crate) fn new() -> Self {
        let mut eri = BTreeMap::new();
        let [a, b, c, d, e, f] = PERMANENT;
        let _ = eri.insert(0xE0, u32::from_le_bytes([a, b, c, d]));
        let _ = eri.insert(0xE4, u32::from_le_bytes([e, f, 0, 0]));
        let mut phy_ocp = BTreeMap::new();
        let _ = phy_ocp.insert(
            phy_reg(phy::BMCR),
            phy::BMCR_ANENABLE | phy::BMCR_POWER_DOWN,
        );
        let _ = phy_ocp.insert(phy_reg(phy::PHYSID1), (PHY_ID >> 16) as u16);
        let _ = phy_ocp.insert(phy_reg(phy::PHYSID2), PHY_ID as u16);
        // The sawtooth count, page 0xC42 register 0x13.
        let _ = phy_ocp.insert(0xC426, 0x0400);
        let mut model = Model {
            bytes: [0; 256],
            xid: 0x541,
            vanished: false,
            stuck_in_reset: false,
            eri_dead: false,
            phy_dead: false,
            resets: 0,
            eri,
            mac_ocp: BTreeMap::new(),
            phy_ocp,
            ephy: [0; 32],
            phy_reset_reads: 0,
            negotiating: false,
            partner: None,
            memory: BTreeMap::new(),
            tx_next: 0,
            rx_next: 0,
            sent: Vec::new(),
            doorbells: 0,
            lie: None,
            locked_writes: 0,
        };
        model.set_u32(regs::MISC, regs::MISC_RXDV_GATE);
        model
    }

    fn page(&mut self, address: u64) -> &mut Vec<u8> {
        self.memory
            .entry(address / PAGE as u64)
            .or_insert_with(|| vec![0; PAGE])
    }

    pub(crate) fn read_byte(&mut self, address: u64) -> u8 {
        let within = (address % PAGE as u64) as usize;
        self.page(address)[within]
    }

    pub(crate) fn write_byte(&mut self, address: u64, value: u8) {
        let within = (address % PAGE as u64) as usize;
        self.page(address)[within] = value;
    }

    fn le32(&mut self, address: u64) -> u32 {
        u32::from_le_bytes([
            self.read_byte(address),
            self.read_byte(address + 1),
            self.read_byte(address + 2),
            self.read_byte(address + 3),
        ])
    }

    fn set_le32(&mut self, address: u64, value: u32) {
        for (at, byte) in value.to_le_bytes().into_iter().enumerate() {
            self.write_byte(address + at as u64, byte);
        }
    }

    /// The plain register bytes at `offset`, `width` of them.
    fn plain(&self, offset: u32, width: u32) -> u32 {
        (0..width).fold(0, |value, at| {
            value | (u32::from(self.bytes[((offset + at) & 0xFF) as usize]) << (8 * at))
        })
    }

    fn set_plain(&mut self, offset: u32, width: u32, value: u32) {
        for at in 0..width {
            self.bytes[((offset + at) & 0xFF) as usize] = (value >> (8 * at)) as u8;
        }
    }

    pub(crate) fn u8(&self, offset: u32) -> u8 {
        self.plain(offset, 1) as u8
    }

    pub(crate) fn u16(&self, offset: u32) -> u16 {
        self.plain(offset, 2) as u16
    }

    pub(crate) fn u32(&self, offset: u32) -> u32 {
        self.plain(offset, 4)
    }

    fn set_u32(&mut self, offset: u32, value: u32) {
        self.set_plain(offset, 4, value);
    }

    /// The PHY's link as the status register shows it.
    fn phy_status(&self) -> u8 {
        match self.partner {
            Some(status) if self.negotiating => status | regs::STATUS_LINK,
            _ => 0,
        }
    }

    pub(crate) fn read(&mut self, offset: u32, width: u32) -> u32 {
        if self.vanished {
            return u32::MAX >> (32 - 8 * width);
        }
        match (offset, width) {
            (regs::TX_CONFIG, 4) => {
                (self.u32(regs::TX_CONFIG) & !(0x7CF << 20)) | (self.xid << 20) | regs::TX_EMPTY
            }
            (regs::MCU, 1) => u32::from(
                (self.u8(regs::MCU) & regs::MCU_NOW_IS_OOB)
                    | regs::MCU_LINK_LIST_READY
                    | regs::MCU_TX_EMPTY
                    | regs::MCU_RX_EMPTY,
            ),
            (regs::PHY_STATUS, 1) => u32::from(self.phy_status()),
            (regs::ERIAR | regs::PHY_OCP | regs::EPHYAR, 4) => self.u32(offset),
            _ => self.plain(offset, width),
        }
    }

    pub(crate) fn write(&mut self, offset: u32, width: u32, value: u32) {
        if self.vanished {
            return;
        }
        let locked = self.u8(regs::CFG9346) != regs::CFG_UNLOCK;
        match (offset, width) {
            (regs::MAC0 | regs::MAC4 | regs::CONFIG2 | regs::CONFIG3 | regs::CONFIG5, _)
                if locked =>
            {
                self.locked_writes += 1;
            }
            (regs::CHIP_CMD, 1) => {
                if value as u8 & regs::CMD_RESET != 0 {
                    self.resets += 1;
                    self.tx_next = 0;
                    self.rx_next = 0;
                    let stuck = if self.stuck_in_reset {
                        regs::CMD_RESET
                    } else {
                        0
                    };
                    self.set_plain(offset, 1, u32::from(stuck));
                } else {
                    self.set_plain(offset, 1, value);
                }
            }
            (regs::TX_POLL, 1) => {
                if value as u8 & regs::POLL_NORMAL != 0 {
                    self.doorbells += 1;
                    self.transmit();
                }
            }
            (regs::INTR_STATUS, 2) => {
                let status = self.u16(offset) & !(value as u16);
                self.set_plain(offset, 2, u32::from(status));
            }
            (regs::ERIAR, 4) => self.eri_command(value),
            (regs::MAC_OCP, 4) => self.mac_ocp_command(value),
            (regs::PHY_OCP, 4) => self.phy_ocp_command(value),
            (regs::EPHYAR, 4) => self.ephy_command(value),
            _ => self.set_plain(offset, width, value),
        }
    }

    fn eri_command(&mut self, command: u32) {
        if self.eri_dead {
            self.set_u32(regs::ERIAR, command);
            return;
        }
        let address = (command & 0xFFC) as u16;
        let mask = (command >> regs::ERI_MASK_SHIFT) & 0xF;
        if command & regs::ACCESS_FLAG != 0 {
            let old = self.eri.get(&address).copied().unwrap_or(0);
            let data = self.u32(regs::ERIDR);
            let bytes = (0..4).fold(0_u32, |m, at| {
                if mask & (1 << at) != 0 {
                    m | (0xFF << (8 * at))
                } else {
                    m
                }
            });
            let _ = self.eri.insert(address, (old & !bytes) | (data & bytes));
            self.set_u32(regs::ERIAR, command & !regs::ACCESS_FLAG);
        } else {
            let value = self.eri.get(&address).copied().unwrap_or(0);
            self.set_u32(regs::ERIDR, value);
            self.set_u32(regs::ERIAR, command | regs::ACCESS_FLAG);
        }
    }

    fn mac_ocp_command(&mut self, command: u32) {
        let address = ((command >> regs::OCP_ADDRESS_SHIFT) & 0xFFFF) as u16;
        if command & regs::ACCESS_FLAG != 0 {
            let _ = self.mac_ocp.insert(address, command as u16);
            self.set_u32(regs::MAC_OCP, command);
        } else {
            let value = self.mac_ocp.get(&address).copied().unwrap_or(0);
            self.set_u32(regs::MAC_OCP, command | u32::from(value));
        }
    }

    fn phy_ocp_command(&mut self, command: u32) {
        if self.phy_dead {
            self.set_u32(regs::PHY_OCP, command);
            return;
        }
        let address = ((command >> regs::OCP_ADDRESS_SHIFT) & 0xFFFF) as u16;
        if command & regs::ACCESS_FLAG != 0 {
            self.phy_write(address, command as u16);
            self.set_u32(regs::PHY_OCP, command & !regs::ACCESS_FLAG);
        } else {
            let value = self.phy_read(address);
            self.set_u32(
                regs::PHY_OCP,
                (command & !0xFFFF) | regs::ACCESS_FLAG | u32::from(value),
            );
        }
    }

    fn phy_read(&mut self, address: u16) -> u16 {
        let mut value = self.phy_ocp.get(&address).copied().unwrap_or(0);
        if address == phy_reg(phy::BMCR) && self.phy_reset_reads > 0 {
            self.phy_reset_reads -= 1;
            value |= phy::BMCR_RESET;
        }
        value
    }

    fn phy_write(&mut self, address: u16, value: u16) {
        if address == phy_reg(phy::BMCR) {
            if value & phy::BMCR_RESET != 0 {
                self.phy_reset_reads = 2;
                self.negotiating = false;
                let _ = self.phy_ocp.insert(address, phy::BMCR_ANENABLE);
                return;
            }
            if value & phy::BMCR_ANRESTART != 0 {
                self.negotiating = true;
            }
            let _ = self.phy_ocp.insert(address, value & !phy::BMCR_ANRESTART);
            return;
        }
        if matches!(address, 0xA404 | 0xA406) {
            return;
        }
        let _ = self.phy_ocp.insert(address, value);
    }

    fn ephy_command(&mut self, command: u32) {
        let register = ((command >> regs::EPHY_REG_SHIFT) & 0x1F) as usize;
        if command & regs::ACCESS_FLAG != 0 {
            self.ephy[register] = command as u16;
            self.set_u32(regs::EPHYAR, command & !regs::ACCESS_FLAG);
        } else {
            let value = self.ephy[register];
            self.set_u32(
                regs::EPHYAR,
                (command & !0xFFFF) | regs::ACCESS_FLAG | u32::from(value),
            );
        }
    }

    /// Raise `bits` in the interrupt status.
    pub(crate) fn raise(&mut self, bits: u16) {
        let status = self.u16(regs::INTR_STATUS) | bits;
        self.set_plain(regs::INTR_STATUS, 2, u32::from(status));
    }

    /// Whether the chip's interrupt is asserted: a status bit the mask lets
    /// through.
    pub(crate) fn interrupt(&self) -> bool {
        self.u16(regs::INTR_STATUS) & self.u16(regs::INTR_MASK) != 0
    }

    fn ring(&self, low: u32, high: u32) -> u64 {
        u64::from(self.u32(low)) | (u64::from(self.u32(high)) << 32)
    }

    fn running(&self, bit: u8) -> bool {
        self.u8(regs::CHIP_CMD) & bit != 0
    }

    /// The DMA's walk of the transmit ring, from where it stopped, while
    /// the descriptor in front of it is its own.
    fn transmit(&mut self) {
        if !self.running(regs::CMD_TX_ENABLE) {
            return;
        }
        let base = self.ring(regs::TX_DESC_LOW, regs::TX_DESC_HIGH);
        let mut moved = false;
        for _ in 0..crate::MAX_ENTRIES {
            let at = base + u64::from(self.tx_next) * DESCRIPTOR_BYTES as u64;
            let word0 = self.le32(at);
            if word0 & desc::OWN == 0 {
                break;
            }
            let address = u64::from(self.le32(at + 8)) | (u64::from(self.le32(at + 12)) << 32);
            let len = word0 & desc::TX_LENGTH;
            let bytes = (0..u64::from(len))
                .map(|index| self.read_byte(address + index))
                .collect();
            self.sent.push(bytes);
            self.set_le32(at, word0 & !desc::OWN);
            moved = true;
            self.tx_next = if word0 & desc::RING_END != 0 {
                0
            } else {
                self.tx_next + 1
            };
        }
        if moved {
            self.raise(regs::INT_TX_OK);
        }
    }

    /// A frame off the wire: written into the next receive descriptor's
    /// buffer with a four-byte check sequence after it, if the chip owns
    /// that descriptor and the receiver is on. `false` if it was lost.
    pub(crate) fn inject(&mut self, frame: &[u8]) -> bool {
        if !self.running(regs::CMD_RX_ENABLE)
            || self.u32(regs::RX_CONFIG) & regs::RX_ACCEPT_MINE == 0
        {
            return false;
        }
        let base = self.ring(regs::RX_DESC_LOW, regs::RX_DESC_HIGH);
        let at = base + u64::from(self.rx_next) * DESCRIPTOR_BYTES as u64;
        let word0 = self.le32(at);
        if word0 & desc::OWN == 0 {
            self.raise(regs::INT_RX_OVERFLOW);
            return false;
        }
        let address = u64::from(self.le32(at + 8)) | (u64::from(self.le32(at + 12)) << 32);
        let size = word0 & desc::RX_LENGTH;
        let len = frame.len() as u32 + desc::FCS_BYTES;
        assert!(len <= size, "the model writes only frames that fit");
        for (index, byte) in frame.iter().chain([0xFC_u8; 4].iter()).enumerate() {
            self.write_byte(address + index as u64, *byte);
        }
        let mut back = (word0 & desc::RING_END) | desc::FIRST | desc::LAST | len;
        match self.lie.take() {
            Some(Lie::Length(lie)) => back = (back & !desc::RX_LENGTH) | (lie & desc::RX_LENGTH),
            Some(Lie::Error) => back |= desc::RX_ERROR,
            Some(Lie::Piece) => back &= !desc::LAST,
            None => {}
        }
        self.set_le32(at + 4, 0);
        self.set_le32(at, back);
        self.rx_next = if word0 & desc::RING_END != 0 {
            0
        } else {
            self.rx_next + 1
        };
        self.raise(regs::INT_RX_OK);
        true
    }

    /// Word 0 of receive descriptor `index`.
    pub(crate) fn rx_word0(&mut self, index: u32) -> u32 {
        let base = self.ring(regs::RX_DESC_LOW, regs::RX_DESC_HIGH);
        self.le32(base + u64::from(index) * DESCRIPTOR_BYTES as u64)
    }

    /// The buffer address of receive descriptor `index`.
    pub(crate) fn rx_address(&mut self, index: u32) -> u64 {
        let base = self.ring(regs::RX_DESC_LOW, regs::RX_DESC_HIGH);
        let at = base + u64::from(index) * DESCRIPTOR_BYTES as u64;
        u64::from(self.le32(at + 8)) | (u64::from(self.le32(at + 12)) << 32)
    }
}

/// Where page 0's register `register` is on the PHY's OCP bus.
pub(crate) const fn phy_reg(register: u8) -> u16 {
    phy::STANDARD_BASE + 2 * register as u16
}

pub(crate) type Shared = Rc<RefCell<Model>>;

/// The register window, as the driver reaches it.
#[derive(Debug)]
pub(crate) struct Regs(pub(crate) Shared);

impl Registers for Regs {
    fn read8(&self, offset: u32) -> u8 {
        self.0.borrow_mut().read(offset, 1) as u8
    }

    fn read16(&self, offset: u32) -> u16 {
        self.0.borrow_mut().read(offset, 2) as u16
    }

    fn read32(&self, offset: u32) -> u32 {
        self.0.borrow_mut().read(offset, 4)
    }

    fn write8(&mut self, offset: u32, value: u8) {
        self.0.borrow_mut().write(offset, 1, u32::from(value));
    }

    fn write16(&mut self, offset: u32, value: u16) {
        self.0.borrow_mut().write(offset, 2, u32::from(value));
    }

    fn write32(&mut self, offset: u32, value: u32) {
        self.0.borrow_mut().write(offset, 4, value);
    }
}

/// Memory the model's DMA reaches, at the device addresses its pages are
/// given.
#[derive(Debug)]
pub(crate) struct Mem {
    model: Shared,
    pages: Vec<u64>,
}

impl Mem {
    /// `count` pages from `base` on.
    pub(crate) fn new(model: &Shared, base: u64, count: usize) -> Self {
        Mem::at(
            model,
            (0..count as u64)
                .map(|page| base + page * PAGE as u64)
                .collect(),
        )
    }

    /// Pages at the addresses given.
    pub(crate) fn at(model: &Shared, pages: Vec<u64>) -> Self {
        Mem {
            model: Rc::clone(model),
            pages,
        }
    }

    /// The device address of byte `offset`.
    pub(crate) fn address(&self, offset: u64) -> u64 {
        self.pages[(offset / PAGE as u64) as usize] + offset % PAGE as u64
    }

    /// Write `bytes` at `offset`.
    pub(crate) fn put(&self, offset: u64, bytes: &[u8]) {
        let mut model = self.model.borrow_mut();
        for (index, byte) in bytes.iter().enumerate() {
            model.write_byte(self.address(offset + index as u64), *byte);
        }
    }

    /// `len` bytes from `offset`.
    pub(crate) fn get(&self, offset: u64, len: usize) -> Vec<u8> {
        let mut model = self.model.borrow_mut();
        (0..len as u64)
            .map(|index| model.read_byte(self.address(offset + index)))
            .collect()
    }
}

impl Pages for Mem {
    fn device_pages(&self) -> &[u64] {
        &self.pages
    }
}

impl Memory for Mem {
    fn read_u32(&self, offset: usize) -> u32 {
        let address = self.address(offset as u64);
        self.model.borrow_mut().le32(address)
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        let address = self.address(offset as u64);
        self.model.borrow_mut().set_le32(address, value);
    }

    fn barrier(&self) {}
}

/// A clock a pause moves.
#[derive(Debug)]
pub(crate) struct FakeClock(pub(crate) Rc<Cell<u64>>);

impl Clock for FakeClock {
    fn now_nanos(&self) -> u64 {
        // Every look takes a microsecond, so a wait that never pauses
        // still reaches its deadline.
        let now = self.0.get() + 1_000;
        self.0.set(now);
        now
    }

    fn pause(&mut self, nanos: u64) {
        self.0.set(self.0.get() + nanos);
    }
}
