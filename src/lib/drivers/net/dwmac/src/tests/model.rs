//! A model of a `DesignWare` Ethernet `QoS` core, its MDIO bus and one
//! clause-22 PHY on it, with "physical" memory its DMA reaches by address.
//!
//! It is written from the databook's side, not the driver's: the DMA finds
//! the rings by the list address and ring length registers, walks them by
//! device address, reads a transmit descriptor's buffer address and length
//! from its fields, and writes a receive descriptor back as the core would.
//! So a driver that encoded a descriptor or a register wrongly meets a
//! model that reads it as the core does.
//!
//! The knobs a test turns are the hostile ones: a reset that never ends, an
//! MDIO bus that stays busy, no PHY, a written-back length past the buffer,
//! error summaries, context descriptors, frames split across descriptors,
//! and a fatal bus error.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::vec;
use std::vec::Vec;

use crate::desc::{self, DESCRIPTOR_BYTES};
use crate::regs::{self, phy};
use crate::{Clock, Memory, PAGE, Pages, Registers};

/// The YT8531S's identifier, which the YT6801's integrated PHY answers.
pub(crate) const PHY_ID: u32 = 0x4F51_E91A;

/// A clause-22 PHY.
#[derive(Debug)]
pub(crate) struct Phy {
    pub(crate) regs: [u16; 32],
    /// Reads of BMCR still to answer with the reset bit set.
    reset_reads: u32,
    /// Never leaves its reset.
    pub(crate) stuck_in_reset: bool,
    /// What the link partner advertises, or `None` with no cable.
    pub(crate) partner: Option<(u16, u16)>,
    /// Reads of BMSR still to answer before auto-negotiation completes.
    pub(crate) negotiation_reads: u32,
    /// The link dropped since BMSR was last read.
    latched_low: bool,
    up: bool,
}

impl Phy {
    fn new() -> Self {
        let mut regs = [0_u16; 32];
        regs[usize::from(phy::BMCR)] = phy::BMCR_ANENABLE;
        regs[usize::from(phy::BMSR)] = 0x7949;
        regs[usize::from(phy::PHYSID1)] = (PHY_ID >> 16) as u16;
        regs[usize::from(phy::PHYSID2)] = PHY_ID as u16;
        Phy {
            regs,
            reset_reads: 0,
            stuck_in_reset: false,
            partner: None,
            negotiation_reads: 2,
            latched_low: false,
            up: false,
        }
    }

    fn read(&mut self, register: u8) -> u16 {
        let register = register & 0x1F;
        match register {
            phy::BMCR => {
                let mut value = self.regs[usize::from(phy::BMCR)];
                if self.reset_reads > 0 || self.stuck_in_reset {
                    self.reset_reads = self.reset_reads.saturating_sub(1);
                    value |= phy::BMCR_RESET;
                }
                value
            }
            phy::BMSR => {
                if !self.up && self.partner.is_some() && self.negotiating() {
                    if self.negotiation_reads == 0 {
                        self.up = true;
                    } else {
                        self.negotiation_reads -= 1;
                    }
                }
                let mut value = self.regs[usize::from(phy::BMSR)];
                let shown = self.up && !self.latched_low;
                self.latched_low = false;
                if shown {
                    value |= phy::BMSR_LINK | phy::BMSR_ANEGCOMPLETE;
                }
                value
            }
            phy::LPA => self.partner.filter(|_| self.up).map_or(0, |(lpa, _)| lpa),
            phy::STAT1000 => self.partner.filter(|_| self.up).map_or(0, |(_, stat)| stat),
            _ => self.regs[usize::from(register)],
        }
    }

    /// Whether auto-negotiation was started since the last reset.
    fn negotiating(&self) -> bool {
        self.regs[usize::from(phy::BMCR)] & phy::BMCR_ANENABLE != 0 && self.regs[31] != 0
    }

    fn write(&mut self, register: u8, value: u16) {
        let register = register & 0x1F;
        if register == phy::BMCR {
            if value & phy::BMCR_RESET != 0 {
                self.reset_reads = 3;
                self.regs[usize::from(phy::BMCR)] = phy::BMCR_ANENABLE;
                self.regs[usize::from(phy::ADVERTISE)] = 0x01E1;
                self.regs[usize::from(phy::CTRL1000)] = 0x0300;
                self.regs[31] = 0;
                self.drop_link();
                return;
            }
            if value & phy::BMCR_ANRESTART != 0 {
                // Register 31 is vendor space here; the model marks with it
                // that negotiation was started.
                self.regs[31] = 1;
                self.drop_link();
            }
            self.regs[usize::from(phy::BMCR)] = value & !phy::BMCR_ANRESTART;
            return;
        }
        if matches!(register, phy::BMSR | phy::PHYSID1 | phy::PHYSID2) {
            return;
        }
        self.regs[usize::from(register)] = value;
    }

    fn drop_link(&mut self) {
        if self.up {
            self.latched_low = true;
        }
        self.up = false;
    }

    /// Pull the cable.
    pub(crate) fn unplug(&mut self) {
        self.partner = None;
        self.drop_link();
    }

    /// Plug in a partner advertising `lpa` and `stat1000`; it negotiates
    /// again.
    pub(crate) fn plug(&mut self, lpa: u16, stat1000: u16) {
        self.partner = Some((lpa, stat1000));
        self.negotiation_reads = 0;
    }
}

/// What the next receive write-back is made to say instead of the truth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lie {
    /// A length of this many bytes.
    Length(u32),
    /// The error summary set.
    Error,
    /// A context descriptor.
    Context,
    /// The first descriptor of a frame that goes on in the next.
    FirstOnly,
}

/// The core.
#[derive(Debug)]
pub(crate) struct Model {
    memory: BTreeMap<u64, Vec<u8>>,
    registers: BTreeMap<u32, u32>,
    pub(crate) phy: Phy,
    pub(crate) phy_present: bool,
    /// Reads of `DMA_MODE` still to answer with `SWR` set.
    reset_reads: u32,
    /// `SWR` never clears.
    pub(crate) stuck_reset: bool,
    /// The MDIO busy bit never clears.
    pub(crate) mdio_stuck: bool,
    /// Every MDIO command the driver gave, as written.
    pub(crate) mdio_commands: Vec<u32>,
    pub(crate) version: u32,
    pub(crate) feature1: u32,
    /// Index of the transmit descriptor the DMA looks at next.
    tx_current: u32,
    /// Index of the receive descriptor the DMA looks at next.
    rx_current: u32,
    /// Frames the core put on the wire.
    pub(crate) sent: Vec<Vec<u8>>,
    /// Frames sent with the error summary written back.
    pub(crate) fail_sends: u32,
    /// Lies for the next receive write-backs, first first.
    pub(crate) lies: Vec<Lie>,
    /// Frames that found no descriptor of the DMA's own.
    pub(crate) missed: u32,
    /// The DMA stopped on a fatal bus error.
    pub(crate) bus_error: bool,
    pub(crate) resets: u32,
    /// What PMT control and status holds after a software reset, which does
    /// not clear it: the N150 box's YT6801 comes up with 0x3, power-down
    /// with magic packets armed.
    pub(crate) pmt_after_reset: u32,
    /// Every tail pointer write, (register, value).
    pub(crate) tails: Vec<(u32, u32)>,
}

impl Model {
    /// A core of release 5.10 with 16 KiB FIFOs each way, a 40-bit DMA and
    /// the PHY at address 0.
    pub(crate) fn new() -> Self {
        Model {
            memory: BTreeMap::new(),
            registers: BTreeMap::new(),
            phy: Phy::new(),
            phy_present: true,
            reset_reads: 0,
            stuck_reset: false,
            mdio_stuck: false,
            mdio_commands: Vec::new(),
            version: 0x0000_1051,
            // RXFIFOSIZE 7 and TXFIFOSIZE 7 (16 KiB), ADDR64 40 bits.
            feature1: 7 | (7 << 6) | (1 << 14),
            tx_current: 0,
            rx_current: 0,
            sent: Vec::new(),
            fail_sends: 0,
            lies: Vec::new(),
            missed: 0,
            bus_error: false,
            resets: 0,
            pmt_after_reset: 0,
            tails: Vec::new(),
        }
    }

    fn page(&mut self, address: u64) -> &mut Vec<u8> {
        self.memory
            .entry(address & !(PAGE as u64 - 1))
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
        let mut bytes = [0; 4];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = self.read_byte(address + index as u64);
        }
        u32::from_le_bytes(bytes)
    }

    fn set_le32(&mut self, address: u64, value: u32) {
        for (index, byte) in value.to_le_bytes().iter().enumerate() {
            self.write_byte(address + index as u64, *byte);
        }
    }

    /// A register as last written, or zero.
    pub(crate) fn reg(&self, offset: u32) -> u32 {
        self.registers.get(&offset).copied().unwrap_or(0)
    }

    fn set_reg(&mut self, offset: u32, value: u32) {
        let _ = self.registers.insert(offset, value);
    }

    fn reset(&mut self) {
        self.resets += 1;
        self.registers.clear();
        self.tx_current = 0;
        self.rx_current = 0;
        self.reset_reads = 2;
        self.set_reg(regs::DMA_MODE, regs::DMA_SWR);
        self.set_reg(regs::MAC_PMT_CONTROL_STATUS, self.pmt_after_reset);
    }

    pub(crate) fn register_read(&mut self, offset: u32) -> u32 {
        match offset {
            regs::DMA_MODE => {
                let value = self.reg(regs::DMA_MODE);
                if self.stuck_reset {
                    return value | regs::DMA_SWR;
                }
                if self.reset_reads > 0 {
                    self.reset_reads -= 1;
                    if self.reset_reads == 0 {
                        self.set_reg(regs::DMA_MODE, value & !regs::DMA_SWR);
                    }
                    return value | regs::DMA_SWR;
                }
                value
            }
            regs::MAC_VERSION => self.version,
            regs::MAC_HW_FEATURE1 => self.feature1,
            regs::MDIO_ADDRESS => {
                let value = self.reg(regs::MDIO_ADDRESS);
                if self.mdio_stuck {
                    value | regs::MDIO_GB
                } else {
                    value
                }
            }
            _ => self.reg(offset),
        }
    }

    pub(crate) fn register_write(&mut self, offset: u32, value: u32) {
        match offset {
            regs::DMA_MODE => {
                if value & regs::DMA_SWR != 0 {
                    self.reset();
                } else {
                    self.set_reg(regs::DMA_MODE, value);
                }
            }
            regs::MDIO_ADDRESS => {
                self.mdio_commands.push(value);
                if value & regs::MDIO_GB != 0 {
                    self.mdio(value);
                }
                self.set_reg(regs::MDIO_ADDRESS, value & !regs::MDIO_GB);
            }
            regs::CH_STATUS => {
                let now = self.reg(regs::CH_STATUS) & !value;
                self.set_reg(regs::CH_STATUS, now);
                self.summarize();
            }
            regs::CH_TX_TAIL | regs::CH_RX_TAIL => {
                self.tails.push((offset, value));
                self.set_reg(offset, value);
                self.run();
            }
            regs::CH_TX_CONTROL | regs::CH_RX_CONTROL | regs::MAC_CONFIGURATION => {
                self.set_reg(offset, value);
                self.run();
            }
            _ => self.set_reg(offset, value),
        }
    }

    fn mdio(&mut self, command: u32) {
        let operation = command & (0b11 << 2);
        let address = ((command >> regs::MDIO_PA_SHIFT) & 0x1F) as u8;
        let register = ((command >> regs::MDIO_RDA_SHIFT) & 0x1F) as u8;
        let present = self.phy_present && address == 0;
        if operation == regs::MDIO_GOC_READ {
            let value = if present {
                self.phy.read(register)
            } else {
                0xFFFF
            };
            self.set_reg(regs::MDIO_DATA, u32::from(value));
        } else if operation == regs::MDIO_GOC_WRITE && present {
            let value = (self.reg(regs::MDIO_DATA) & 0xFFFF) as u16;
            self.phy.write(register, value);
        }
    }

    fn raise(&mut self, bits: u32) {
        let now = self.reg(regs::CH_STATUS) | bits;
        self.set_reg(regs::CH_STATUS, now);
        self.summarize();
    }

    fn summarize(&mut self) {
        let mut status = self.reg(regs::CH_STATUS) & !(regs::CH_NIS | regs::CH_AIS);
        if status & (regs::CH_TI | regs::CH_RI) != 0 {
            status |= regs::CH_NIS;
        }
        if status & (regs::CH_RBU | regs::CH_FBE) != 0 {
            status |= regs::CH_AIS;
        }
        self.set_reg(regs::CH_STATUS, status);
    }

    /// Whether the channel's interrupt is asserted.
    pub(crate) fn interrupt(&self) -> bool {
        self.reg(regs::CH_STATUS) & self.reg(regs::CH_INTERRUPT_ENABLE) != 0
    }

    fn list(&self, high: u32, low: u32) -> u64 {
        (u64::from(self.reg(high)) << 32) | u64::from(self.reg(low))
    }

    fn descriptor(&self, high: u32, low: u32, index: u32) -> u64 {
        self.list(high, low) + u64::from(index) * DESCRIPTOR_BYTES as u64
    }

    fn transmitting(&self) -> bool {
        !self.bus_error
            && self.reg(regs::CH_TX_CONTROL) & regs::CH_TX_ST != 0
            && self.reg(regs::MAC_CONFIGURATION) & regs::MAC_TE != 0
    }

    fn receiving(&self) -> bool {
        !self.bus_error
            && self.reg(regs::CH_RX_CONTROL) & regs::CH_RX_SR != 0
            && self.reg(regs::MAC_CONFIGURATION) & regs::MAC_RE != 0
    }

    /// Walk the transmit ring while the DMA owns what it finds.
    pub(crate) fn run(&mut self) {
        if !self.transmitting() {
            return;
        }
        let entries = self.reg(regs::CH_TX_RING_LENGTH) + 1;
        let tail = self.reg(regs::CH_TX_TAIL);
        for _ in 0..entries {
            let at = self.descriptor(regs::CH_TX_LIST_HIGH, regs::CH_TX_LIST_LOW, self.tx_current);
            if at as u32 == tail {
                return;
            }
            let word3 = self.le32(at + 12);
            if word3 & desc::OWN == 0 {
                return;
            }
            let address = u64::from(self.le32(at)) | (u64::from(self.le32(at + 4)) << 32);
            let len = self.le32(at + 8) & desc::TX_LENGTH;
            assert_eq!(
                word3 & (desc::TX_FD | desc::TX_LD),
                desc::TX_FD | desc::TX_LD
            );
            assert_eq!(word3 & desc::TX_LENGTH, len, "frame length = buffer length");
            let frame = (0..u64::from(len))
                .map(|index| self.read_byte(address + index))
                .collect();
            self.sent.push(frame);
            let mut back = 0;
            if self.fail_sends > 0 {
                self.fail_sends -= 1;
                back |= desc::TX_ES;
            }
            self.set_le32(at + 12, back | desc::TX_FD | desc::TX_LD);
            self.raise(regs::CH_TI);
            self.tx_current = (self.tx_current + 1) % entries;
        }
    }

    /// A frame arrives from the wire; it is FCS-less, as the core strips it
    /// with `CST` set. Answers whether a descriptor took it.
    pub(crate) fn inject(&mut self, frame: &[u8]) -> bool {
        if !self.receiving() {
            return false;
        }
        assert_ne!(
            self.reg(regs::MAC_CONFIGURATION) & regs::MAC_CST,
            0,
            "the driver asks the core to strip the check sequence"
        );
        let entries = self.reg(regs::CH_RX_RING_LENGTH) + 1;
        let at = self.descriptor(regs::CH_RX_LIST_HIGH, regs::CH_RX_LIST_LOW, self.rx_current);
        let word3 = self.le32(at + 12);
        if word3 & desc::OWN == 0 || at as u32 == self.reg(regs::CH_RX_TAIL) {
            self.missed += 1;
            self.raise(regs::CH_RBU);
            return false;
        }
        assert_ne!(word3 & desc::RX_BUF1V, 0, "buffer 1 marked valid");
        let address = u64::from(self.le32(at)) | (u64::from(self.le32(at + 4)) << 32);
        let size = (self.reg(regs::CH_RX_CONTROL) >> regs::CH_RBSZ_SHIFT) & 0x3FFF;
        assert!(frame.len() as u32 <= size, "a test frame fits one buffer");
        for (index, byte) in frame.iter().enumerate() {
            self.write_byte(address + index as u64, *byte);
        }
        let mut back = desc::RX_FD | desc::RX_LD | (frame.len() as u32 & desc::RX_PL);
        if !self.lies.is_empty() {
            match self.lies.remove(0) {
                Lie::Length(len) => back = desc::RX_FD | desc::RX_LD | (len & desc::RX_PL),
                Lie::Error => back |= desc::RX_ES,
                Lie::Context => back |= desc::RX_CTXT,
                Lie::FirstOnly => back &= !desc::RX_LD,
            }
        }
        // The core overwrites the read form's words.
        self.set_le32(at, 0);
        self.set_le32(at + 4, 0);
        self.set_le32(at + 8, 0);
        self.set_le32(at + 12, back);
        self.raise(regs::CH_RI);
        self.rx_current = (self.rx_current + 1) % entries;
        true
    }

    /// A fatal bus error: the DMA stops.
    pub(crate) fn fail_bus(&mut self) {
        self.bus_error = true;
        self.raise(regs::CH_FBE);
    }
}

/// The model, shared between the driver's parts and the test.
pub(crate) type Shared = Rc<RefCell<Model>>;

/// The model's registers.
#[derive(Debug)]
pub(crate) struct Regs(pub(crate) Shared);

impl Registers for Regs {
    fn read(&self, offset: u32) -> u32 {
        assert_eq!(offset % 4, 0, "an aligned register");
        self.0.borrow_mut().register_read(offset)
    }

    fn write(&mut self, offset: u32, value: u32) {
        assert_eq!(offset % 4, 0, "an aligned register");
        self.0.borrow_mut().register_write(offset, value);
    }
}

/// Pages of the model's memory, at addresses that are neither ascending
/// nor adjacent, as an IOMMU domain may hand them out.
#[derive(Debug)]
pub(crate) struct Mem {
    model: Shared,
    pages: Vec<u64>,
    /// A write since the last barrier.
    unfenced: Cell<bool>,
    /// Descriptors handed over with their other words maybe not visible.
    pub(crate) unordered: Rc<Cell<u32>>,
}

impl Mem {
    /// `count` pages somewhere above `base`.
    pub(crate) fn new(model: &Shared, base: u64, count: usize) -> Self {
        let pages = (0..count)
            .map(|index| base + ((count - 1 - index) as u64) * 3 * PAGE as u64)
            .collect();
        Mem::at(model, pages)
    }

    /// Pages at exactly these addresses.
    pub(crate) fn at(model: &Shared, pages: Vec<u64>) -> Self {
        Mem {
            model: Rc::clone(model),
            pages,
            unfenced: Cell::new(false),
            unordered: Rc::new(Cell::new(0)),
        }
    }

    pub(crate) fn address(&self, offset: u64) -> u64 {
        self.pages[(offset / PAGE as u64) as usize] + offset % PAGE as u64
    }

    /// Write `bytes` from `offset`, as the network stack fills a frame.
    pub(crate) fn put(&self, offset: u64, bytes: &[u8]) {
        let mut model = self.model.borrow_mut();
        for (index, byte) in bytes.iter().enumerate() {
            model.write_byte(self.address(offset + index as u64), *byte);
        }
    }

    /// The `len` bytes from `offset`.
    pub(crate) fn get(&self, offset: u64, len: usize) -> Vec<u8> {
        let model = &mut self.model.borrow_mut();
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
        assert_eq!(offset % 4, 0, "an aligned word");
        let address = self.address(offset as u64);
        self.model.borrow_mut().le32(address)
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        assert_eq!(offset % 4, 0, "an aligned word");
        if offset % DESCRIPTOR_BYTES == 12 && value & desc::OWN != 0 && self.unfenced.get() {
            self.unordered.set(self.unordered.get() + 1);
        }
        self.unfenced.set(true);
        let address = self.address(offset as u64);
        self.model.borrow_mut().set_le32(address, value);
    }

    fn barrier(&self) {
        self.unfenced.set(false);
    }
}

/// A clock a test moves, and every pause moves.
#[derive(Debug, Clone)]
pub(crate) struct FakeClock(pub(crate) Rc<Cell<u64>>);

impl Clock for FakeClock {
    fn now_nanos(&self) -> u64 {
        self.0.get()
    }

    fn pause(&mut self, nanos: u64) {
        self.0.set(self.0.get() + nanos.max(1));
    }
}
