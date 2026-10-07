//! A model of the SDMMC controller with an SD card behind it, at register
//! level: what the driver writes runs the card's state machine, what it
//! reads comes from it. It is strict where the hardware or the
//! specification would fail quietly: a command sent with the wrong response
//! length, at an identification clock above 400 kHz, or with a data path
//! not set up for it, is recorded as a violation, and a test fails on any.
//!
//! Time is the model's own: the card moves [`State::pace`] words each time
//! the status register is read, so a driver that reads the FIFO slower
//! than the card fills it sees what a late processor sees -- a full FIFO,
//! which with flow control stops the card and without it overruns.

extern crate std;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::format;
use std::rc::Rc;
use std::string::String;
use std::vec;
use std::vec::Vec;

use crate::regs::*;
use crate::{BLOCK_WORDS, Clock, Registers};

/// The DK1's SDMMC1 kernel clock: PLL4's P output.
pub(super) const KERNEL_HZ: u64 = 99_000_000;
/// `CLKCR.NEGEDGE`, which firmware sets on the DK1 and the driver keeps.
pub(super) const NEGEDGE: u32 = 1 << 16;
/// The card's address.
pub(super) const RCA: u16 = 0x4567;

/// The card's states, as R1's `CURRENT_STATE` numbers them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CardState {
    Idle = 0,
    Ready = 1,
    Ident = 2,
    Stby = 3,
    Tran = 4,
    Data = 5,
    Rcv = 6,
    Prg = 7,
}

/// A transfer in progress.
#[derive(Debug)]
pub(super) struct Transfer {
    pub(super) read: bool,
    /// The next block the card reads or writes.
    pub(super) block: u64,
    /// Words of the transfer still to move between card and FIFO.
    pub(super) left: usize,
    /// Words of the current block moved so far, for a write.
    pub(super) partial: Vec<u32>,
}

/// Everything the model holds.
#[derive(Debug)]
pub(super) struct State {
    pub(super) power: u32,
    pub(super) clkcr: u32,
    pub(super) argr: u32,
    pub(super) cmdr: u32,
    pub(super) dtimer: u32,
    pub(super) dlenr: u32,
    pub(super) dctrl: u32,
    pub(super) star: u32,
    pub(super) maskr: u32,
    pub(super) idmactrlr: u32,
    pub(super) resp: [u32; 4],
    pub(super) fifo: VecDeque<u32>,
    pub(super) transfer: Option<Transfer>,
    /// Every write the driver made: offset and value.
    pub(super) writes: Vec<(u32, u32)>,
    /// Every command the card got: index and argument.
    pub(super) commands: Vec<(u8, u32)>,
    pub(super) violations: Vec<String>,
    /// Words the card moves per status read.
    pub(super) pace: usize,
    /// Status reads the card stays busy on D0 after an R1b.
    pub(super) busy_reads: u32,

    // The card.
    pub(super) state: CardState,
    pub(super) app: bool,
    /// A card of version 1, which does not answer CMD8.
    pub version1: bool,
    pub(super) high_capacity: bool,
    /// ACMD41s before the card says it has powered up.
    pub(super) power_up_polls: u32,
    pub(super) wide: bool,
    pub(super) blocks: Vec<[u32; BLOCK_WORDS]>,
    /// CMD13s a write stays in programming for.
    pub(super) program_polls: u32,
    pub(super) programming: u32,
    /// A block whose read fails its CRC once.
    pub(super) bad_block: Option<u64>,
}

impl State {
    fn card_hz(&self) -> u64 {
        let divider = u64::from(self.clkcr & CLKDIV_MASK);
        if divider == 0 {
            KERNEL_HZ
        } else {
            KERNEL_HZ / (2 * divider)
        }
    }

    fn r1(&self) -> u32 {
        let mut r1 = (self.state as u32) << 9;
        if self.state == CardState::Tran {
            r1 |= 1 << 8;
        }
        if self.app {
            r1 |= 1 << 5;
        }
        r1
    }

    fn violation(&mut self, what: String) {
        self.violations.push(what);
    }

    /// The card's answer to command `index`: `None` is no response, and
    /// the expected `WAITRESP` beside it.
    fn command(&mut self, index: u8, argument: u32, cmdr: u32) -> Option<([u32; 4], u32)> {
        let app = core::mem::replace(&mut self.app, false);
        let short = |word: u32| ([word, 0, 0, 0], WAITRESP_SHORT);
        let addressed = argument >> 16 == u32::from(RCA);
        match (app, index, self.state) {
            (_, 0, _) => {
                self.state = CardState::Idle;
                Some(([0; 4], WAITRESP_NONE))
            }
            (false, 8, CardState::Idle) if !self.version1 => Some(short(argument & 0xFFF)),
            (_, 55, _) => {
                self.app = true;
                Some(short(self.r1()))
            }
            (true, 41, CardState::Idle | CardState::Ready) => {
                let ready = if self.power_up_polls == 0 {
                    self.state = CardState::Ready;
                    1 << 31
                } else {
                    self.power_up_polls -= 1;
                    0
                };
                let ccs = if self.high_capacity && argument & (1 << 30) != 0 {
                    1 << 30
                } else {
                    0
                };
                Some(([ready | ccs | 0x00FF_8000, 0, 0, 0], WAITRESP_SHORT_NO_CRC))
            }
            (false, 2, CardState::Ready) => {
                self.state = CardState::Ident;
                Some((
                    [0x1D41_4453, 0x4420_2020, 0x1012_3456, 0x7801_4100],
                    WAITRESP_LONG,
                ))
            }
            (false, 3, CardState::Ident | CardState::Stby) => {
                let r6 = (u32::from(RCA) << 16) | ((self.state as u32) << 9);
                self.state = CardState::Stby;
                Some(short(r6))
            }
            (false, 9, CardState::Stby) if addressed => Some((self.csd(), WAITRESP_LONG)),
            (false, 7, CardState::Stby) if addressed => {
                let r1 = self.r1();
                self.state = CardState::Tran;
                self.busy_reads = 3;
                Some(short(r1))
            }
            (true, 6, CardState::Tran) => {
                self.wide = argument == 2;
                Some(short(self.r1()))
            }
            (false, 16, CardState::Tran) => Some(short(self.r1())),
            (false, 13, _) if addressed => {
                if self.state == CardState::Prg {
                    if self.programming == 0 {
                        self.state = CardState::Tran;
                    } else {
                        self.programming -= 1;
                    }
                }
                Some(short(self.r1()))
            }
            (false, 17 | 18 | 24 | 25, CardState::Tran) => self.start_data(index, argument, cmdr),
            (false, 12, CardState::Data | CardState::Rcv) => {
                let r1 = self.r1();
                if cmdr & CMDSTOP == 0 {
                    self.violation("CMD12 without CMDSTOP".into());
                }
                self.state = if self.state == CardState::Rcv {
                    self.programming = self.program_polls;
                    CardState::Prg
                } else {
                    CardState::Tran
                };
                self.transfer = None;
                self.star &= !DPSMACT;
                self.busy_reads = 2;
                Some(short(r1))
            }
            _ => None,
        }
    }

    fn csd(&self) -> [u32; 4] {
        let blocks = self.blocks.len() as u128;
        let csd: u128 = if self.high_capacity {
            // Structure 1, C_SIZE = blocks / 1024 - 1 in bits 69:48.
            (1_u128 << 126) | ((blocks / 1024 - 1) << 48)
        } else {
            // Structure 0: READ_BL_LEN 9, C_SIZE_MULT 7 (x512), C_SIZE.
            let size = blocks / 512 - 1;
            (9_u128 << 80) | (size << 62) | (7_u128 << 47)
        };
        [
            (csd >> 96) as u32,
            (csd >> 64) as u32,
            (csd >> 32) as u32,
            csd as u32,
        ]
    }

    fn start_data(&mut self, index: u8, argument: u32, cmdr: u32) -> Option<([u32; 4], u32)> {
        let r1 = self.r1();
        let read = matches!(index, 17 | 18);
        let block = if self.high_capacity {
            u64::from(argument)
        } else {
            if !argument.is_multiple_of(512) {
                self.violation(format!("byte address {argument:#x} not on a block"));
            }
            u64::from(argument) / 512
        };
        let words = self.dlenr as usize / 4;
        let blocks = words / BLOCK_WORDS;
        if cmdr & CMDTRANS == 0 {
            self.violation(format!("CMD{index} without CMDTRANS"));
        }
        if (self.dctrl & DTDIR != 0) != read || (self.dctrl >> DBLOCKSIZE_SHIFT) & 0xF != 9 {
            self.violation(format!("CMD{index} with DCTRL {:#x}", self.dctrl));
        }
        if matches!(index, 17 | 24) != (blocks == 1) || !words.is_multiple_of(BLOCK_WORDS) {
            self.violation(format!("CMD{index} with DLENR {}", self.dlenr));
        }
        if !self.wide || self.clkcr & WIDBUS_MASK != WIDBUS_4 {
            self.violation("data on one line, or the card and host disagree".into());
        }
        if block + blocks as u64 > self.blocks.len() as u64 {
            return Some(([r1 | (1 << 31), 0, 0, 0], WAITRESP_SHORT));
        }
        self.state = if read {
            CardState::Data
        } else {
            CardState::Rcv
        };
        self.transfer = Some(Transfer {
            read,
            block,
            left: words,
            partial: Vec::new(),
        });
        self.star |= DPSMACT;
        Some(([r1, 0, 0, 0], WAITRESP_SHORT))
    }

    /// The card moves up to `pace` words, as the status is read.
    fn pump(&mut self) {
        if self.busy_reads > 0 {
            self.busy_reads -= 1;
            self.star |= BUSYD0;
        } else {
            self.star &= !BUSYD0;
        }
        for _ in 0..self.pace {
            let Some(read) = self.transfer.as_ref().map(|transfer| transfer.read) else {
                break;
            };
            let moved = if read {
                self.card_sends()
            } else {
                self.card_takes()
            };
            if !moved {
                break;
            }
            if self
                .transfer
                .as_ref()
                .is_some_and(|transfer| transfer.left == 0)
            {
                self.transfer_done(read);
            }
        }
    }

    /// Whether the transfer may move a word now.
    fn moving(&self) -> bool {
        self.transfer
            .as_ref()
            .is_some_and(|transfer| transfer.left > 0)
            && self.star & (DATA_ERRORS | DATAEND) == 0
    }

    /// The card puts a word in the FIFO, if it can.
    fn card_sends(&mut self) -> bool {
        if !self.moving() {
            return false;
        }
        if self.fifo.len() >= 16 {
            if self.clkcr & HWFC_EN == 0 {
                self.star |= RXOVERR;
            }
            return false;
        }
        let total = self.dlenr as usize / 4;
        let transfer = self.transfer.as_mut().unwrap();
        let done = total - transfer.left;
        let block = transfer.block + (done / BLOCK_WORDS) as u64;
        if done.is_multiple_of(BLOCK_WORDS) && self.bad_block == Some(block) {
            self.bad_block = None;
            self.star |= DCRCFAIL;
            return false;
        }
        transfer.left -= 1;
        let word = self.blocks[block as usize][done % BLOCK_WORDS];
        self.fifo.push_back(word);
        true
    }

    /// The card takes a word from the FIFO, if there is one.
    fn card_takes(&mut self) -> bool {
        if !self.moving() {
            return false;
        }
        let Some(word) = self.fifo.pop_front() else {
            return false;
        };
        let transfer = self.transfer.as_mut().unwrap();
        transfer.partial.push(word);
        transfer.left -= 1;
        if transfer.partial.len() == BLOCK_WORDS {
            let words: [u32; BLOCK_WORDS] = transfer
                .partial
                .drain(..)
                .collect::<Vec<_>>()
                .try_into()
                .unwrap();
            let at = transfer.block as usize;
            transfer.block += 1;
            self.blocks[at] = words;
        }
        true
    }

    /// Everything moved: the data path ends, and a single block ends the
    /// card's part by itself.
    fn transfer_done(&mut self, read: bool) {
        self.star |= DATAEND;
        self.star &= !DPSMACT;
        if self.dlenr as usize / 4 != BLOCK_WORDS {
            return;
        }
        if read {
            self.state = CardState::Tran;
        } else {
            self.programming = self.program_polls;
            self.state = CardState::Prg;
        }
    }

    fn status(&self) -> u32 {
        let mut star = self.star;
        if self.fifo.len() >= 8 {
            star |= RXFIFOHF;
        }
        if self.fifo.is_empty() {
            star |= RXFIFOE;
        }
        if self.fifo.len() <= 8 {
            star |= TXFIFOHE;
        }
        star
    }

    fn write_cmdr(&mut self, value: u32) {
        self.cmdr = value;
        if value & CPSMEN == 0 {
            return;
        }
        let index = (value & CMDINDEX_MASK) as u8;
        let argument = self.argr;
        self.commands.push((index, argument));
        let identifying = matches!(
            self.state,
            CardState::Idle | CardState::Ready | CardState::Ident
        );
        if identifying && self.card_hz() > 400_000 {
            self.violation(format!(
                "CMD{index} at {} Hz while identifying",
                self.card_hz()
            ));
        }
        match self.command(index, argument, value) {
            None => self.star |= CTIMEOUT,
            Some((response, wanted)) => {
                let wait = value & (0b11 << 8);
                if wait != wanted {
                    self.violation(format!(
                        "CMD{index} with WAITRESP {wait:#x}, not {wanted:#x}"
                    ));
                }
                if wanted == WAITRESP_NONE {
                    self.star |= CMDSENT;
                } else {
                    self.resp = response;
                    self.star |= CMDREND;
                }
            }
        }
    }
}

/// The model, shared between the controller under test and the test.
#[derive(Clone, Debug)]
pub(super) struct Fake(pub Rc<RefCell<State>>);

impl Fake {
    /// A powered controller as U-Boot leaves it, and an SDHC card of
    /// `blocks` blocks whose block `n` holds `n` in every word.
    pub(super) fn sdhc(blocks: usize) -> Fake {
        let content = (0..blocks).map(|n| [n as u32; BLOCK_WORDS]).collect();
        Fake(Rc::new(RefCell::new(State {
            power: PWRCTRL_ON,
            clkcr: NEGEDGE | WIDBUS_4 | 2,
            argr: 0,
            cmdr: 0,
            dtimer: 0,
            dlenr: 0,
            dctrl: 0,
            star: 0,
            maskr: 0x3FF,
            idmactrlr: 1,
            resp: [0; 4],
            fifo: VecDeque::new(),
            transfer: None,
            writes: Vec::new(),
            commands: Vec::new(),
            violations: Vec::new(),
            pace: 3,
            busy_reads: 0,
            state: CardState::Tran,
            app: false,
            version1: false,
            high_capacity: true,
            power_up_polls: 3,
            wide: true,
            blocks: content,
            program_polls: 2,
            programming: 0,
            bad_block: None,
        })))
    }

    /// The same, as an SDSC card (byte addressed, CSD version 1).
    pub(super) fn sdsc(blocks: usize) -> Fake {
        let fake = Fake::sdhc(blocks);
        fake.0.borrow_mut().high_capacity = false;
        fake
    }

    pub(super) fn state(&self) -> std::cell::RefMut<'_, State> {
        self.0.borrow_mut()
    }

    pub(super) fn violations(&self) -> Vec<String> {
        self.0.borrow().violations.clone()
    }
}

impl Registers for Fake {
    fn read(&self, offset: u32) -> u32 {
        let mut state = self.0.borrow_mut();
        match offset {
            POWER => state.power,
            CLKCR => state.clkcr,
            ARGR => state.argr,
            CMDR => state.cmdr,
            RESP1R => state.resp[0],
            RESP2R => state.resp[1],
            RESP3R => state.resp[2],
            RESP4R => state.resp[3],
            DTIMER => state.dtimer,
            DLENR => state.dlenr,
            DCTRL => state.dctrl,
            STAR => {
                state.pump();
                state.status()
            }
            MASKR => state.maskr,
            IDMACTRLR => state.idmactrlr,
            FIFOR => match state.fifo.pop_front() {
                Some(word) => word,
                None => {
                    state.violation("FIFO read while empty".into());
                    0
                }
            },
            _ => {
                state.violation(format!("read of {offset:#x}"));
                0
            }
        }
    }

    fn write(&mut self, offset: u32, value: u32) {
        let mut state = self.0.borrow_mut();
        state.writes.push((offset, value));
        match offset {
            CLKCR => state.clkcr = value,
            ARGR => state.argr = value,
            CMDR => state.write_cmdr(value),
            DTIMER => state.dtimer = value,
            DLENR => state.dlenr = value,
            DCTRL => {
                if value & FIFORST != 0 {
                    if state.star & DPSMACT != 0 {
                        state.violation("FIFORST while the data path runs".into());
                    }
                    state.fifo.clear();
                    state.transfer = None;
                }
                state.dctrl = value;
            }
            ICR => state.star &= !(value & ICR_ALL),
            MASKR => state.maskr = value,
            // The internal DMA's registers, 0x050 to 0x05F: only a zero to
            // its enable, which turns it off, is a write the driver may make.
            IDMACTRLR if value == 0 => state.idmactrlr = value,
            0x050..=0x05F => {
                state.violation(format!("DMA register {offset:#x} written {value:#x}"));
            }
            FIFOR => {
                if state.fifo.len() >= 16 {
                    state.violation("FIFO write while full".into());
                }
                state.fifo.push_back(value);
            }
            _ => state.violation(format!("write of {value:#x} to {offset:#x}")),
        }
    }
}

/// A clock that moves a microsecond each time it is read and a millisecond
/// each pause.
#[derive(Debug, Default)]
pub(super) struct FakeClock(Cell<u64>);

impl Clock for FakeClock {
    fn now_nanos(&self) -> u64 {
        self.0.set(self.0.get() + 1_000);
        self.0.get()
    }

    fn pause(&mut self) {
        self.0.set(self.0.get() + 1_000_000);
    }
}

/// A buffer of `blocks` blocks for transfers.
pub(super) fn buffer(blocks: usize) -> Vec<u32> {
    vec![0; blocks * BLOCK_WORDS]
}
