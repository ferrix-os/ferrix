//! The far side of an FTDI chip, for the xHCI model: the bytes a UART's
//! line sent and received, and for a USB-Blaster the CPLD and a JTAG
//! chain behind it.
//!
//! The chip answers a bulk IN request with packets of its endpoint's size,
//! each starting with the two status bytes, until it runs out of bytes or
//! of room; with nothing to send it NAKs, as QEMU's `usb-serial` does,
//! unless a status-only packet is due, as a real chip's latency timer
//! sends one.

use std::collections::VecDeque;
use std::vec;
use std::vec::Vec;

use super::super::model::Device;
use crate::usb::Speed;

/// The modem-status byte the model's chip sends: CTS, DSR and carrier up,
/// and the low nibble the chips always set.
pub(in crate::tests) const MODEM: u8 = 0xB1;
/// The line-status byte: transmitter empty.
pub(in crate::tests) const LINE: u8 = 0x60;

/// An FTDI chip's line, as the model sees it.
#[derive(Clone, Debug)]
pub(in crate::tests) struct Serial {
    pub(in crate::tests) device: usize,
    pub(in crate::tests) max_packet: usize,
    /// What the line sent, waiting for the host's IN requests.
    pub(in crate::tests) to_host: VecDeque<u8>,
    /// Status-only packets to answer with before any data.
    pub(in crate::tests) status_only: u32,
    /// The line-status byte the next data packet carries, once.
    pub(in crate::tests) line_error: u8,
    /// Every byte the host sent.
    pub(in crate::tests) from_host: Vec<u8>,
    /// The modem-status byte its packets carry.
    pub(in crate::tests) modem: u8,
    /// NAK every OUT request: what a chip does while CTS holds it off.
    pub(in crate::tests) hold_out: bool,
    /// Stall every OUT request.
    pub(in crate::tests) stall_out: bool,
    /// A USB-Blaster's CPLD and chain.
    pub(in crate::tests) cable: Option<Cable>,
}

impl Serial {
    pub(in crate::tests) fn new(device: usize, max_packet: usize) -> Serial {
        Serial {
            device,
            max_packet,
            to_host: VecDeque::new(),
            status_only: 0,
            line_error: 0,
            from_host: Vec::new(),
            hold_out: false,
            modem: MODEM,
            stall_out: false,
            cable: None,
        }
    }

    /// The answer to an IN request of `asked` bytes: `None` for a NAK.
    pub(in crate::tests) fn bulk_in(&mut self, asked: usize) -> Option<Vec<u8>> {
        if self.status_only > 0 {
            self.status_only -= 1;
            return Some(vec![self.modem, LINE]);
        }
        if self.to_host.is_empty() {
            return None;
        }
        let mut out = Vec::new();
        while !self.to_host.is_empty() && asked - out.len() >= self.max_packet {
            let line = LINE | core::mem::take(&mut self.line_error);
            out.extend_from_slice(&[self.modem, line]);
            let take = self.to_host.len().min(self.max_packet - 2);
            out.extend(self.to_host.drain(..take));
            if take < self.max_packet - 2 {
                break;
            }
        }
        Some(out)
    }

    /// The host's OUT data.
    pub(in crate::tests) fn bulk_out(&mut self, bytes: &[u8]) {
        self.from_host.extend_from_slice(bytes);
        if let Some(cable) = &mut self.cable {
            for &byte in bytes {
                if let Some(answer) = cable.take(byte) {
                    self.to_host.push_back(answer);
                }
            }
        }
    }
}

/// An FTDI device: one vendor interface, bulk IN on endpoint 1 and bulk OUT
/// on endpoint 2 -- the FT232H's layout, and QEMU's.
pub(in crate::tests) fn chip(speed: Speed, vendor: u16, product: u16, release: u16) -> Device {
    let packet: u16 = if speed == Speed::High { 512 } else { 64 };
    let size = packet.to_le_bytes();
    let configuration = vec![
        9, 2, 32, 0, 1, 1, 0, 0x80, 45, //
        9, 4, 0, 0, 2, 0xFF, 0xFF, 0xFF, 2, //
        7, 5, 0x81, 2, size[0], size[1], 0, //
        7, 5, 0x02, 2, size[0], size[1], 0,
    ];
    super::super::model::device(
        speed,
        super::super::model::device_descriptor(0, 64, vendor, product, release),
        configuration,
        &[(1, "FTDI"), (2, "Model Serial")],
    )
}

// ---------------------------------------------------------------------------
// The USB-Blaster's CPLD, and the chain behind it
// ---------------------------------------------------------------------------

/// The sixteen states of IEEE 1149.1's controller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Reset,
    Idle,
    SelectDr,
    CaptureDr,
    ShiftDr,
    Exit1Dr,
    PauseDr,
    Exit2Dr,
    UpdateDr,
    SelectIr,
    CaptureIr,
    ShiftIr,
    Exit1Ir,
    PauseIr,
    Exit2Ir,
    UpdateIr,
}

impl State {
    const fn next(self, tms: bool) -> State {
        use State::*;
        match (self, tms) {
            (Reset | SelectIr, true) => Reset,
            (Reset | Idle | UpdateDr | UpdateIr, false) => Idle,
            (Idle | UpdateDr | UpdateIr, true) => SelectDr,
            (SelectDr, false) => CaptureDr,
            (SelectDr, true) => SelectIr,
            (CaptureDr | ShiftDr | Exit2Dr, false) => ShiftDr,
            (CaptureDr | ShiftDr, true) => Exit1Dr,
            (Exit1Dr | PauseDr, false) => PauseDr,
            (Exit1Dr | Exit2Dr, true) => UpdateDr,
            (PauseDr, true) => Exit2Dr,
            (SelectIr, false) => CaptureIr,
            (CaptureIr | ShiftIr | Exit2Ir, false) => ShiftIr,
            (CaptureIr | ShiftIr, true) => Exit1Ir,
            (Exit1Ir | PauseIr, false) => PauseIr,
            (Exit1Ir | Exit2Ir, true) => UpdateIr,
            (PauseIr, true) => Exit2Ir,
        }
    }
}

/// One tap: its IR's length, and its IDCODE, or none.
#[derive(Clone, Debug)]
pub(in crate::tests) struct Tap {
    state: State,
    ir_length: usize,
    /// The instruction that selects IDCODE.
    idcode_instruction: u64,
    idcode: Option<u32>,
    instruction: u64,
    shift: u64,
    length: usize,
    /// Every instruction an Update-IR made current.
    pub(in crate::tests) updates: Vec<u64>,
}

impl Tap {
    pub(in crate::tests) fn new(
        ir_length: usize,
        idcode_instruction: u64,
        idcode: Option<u32>,
    ) -> Tap {
        Tap {
            state: State::Reset,
            ir_length,
            idcode_instruction,
            idcode,
            instruction: 0,
            shift: 0,
            length: 1,
            updates: Vec::new(),
        }
    }

    fn bypass(&self) -> u64 {
        (1 << self.ir_length) - 1
    }

    /// TDO, as the tap drives it now: the shift register's first bit while
    /// it shifts; otherwise nothing drives it, and it is pulled up.
    fn tdo(&self) -> bool {
        match self.state {
            State::ShiftDr | State::ShiftIr => self.shift & 1 != 0,
            _ => true,
        }
    }

    /// A rising TCK edge.
    fn clock(&mut self, tms: bool, tdi: bool) {
        match self.state {
            State::Reset => {
                self.instruction = if self.idcode.is_some() {
                    self.idcode_instruction
                } else {
                    self.bypass()
                };
            }
            State::CaptureDr => match self.idcode {
                Some(idcode) if self.instruction == self.idcode_instruction => {
                    self.shift = u64::from(idcode);
                    self.length = 32;
                }
                _ => {
                    self.shift = 0;
                    self.length = 1;
                }
            },
            State::CaptureIr => {
                self.shift = 0b01;
                self.length = self.ir_length;
            }
            State::ShiftDr | State::ShiftIr => {
                self.shift = (self.shift >> 1) | (u64::from(tdi) << (self.length - 1));
            }
            State::UpdateIr => {
                self.instruction = self.shift & self.bypass();
                self.updates.push(self.instruction);
            }
            _ => {}
        }
        self.state = self.state.next(tms);
    }
}

/// The CPLD: bit-bang and byte-shift bytes in, TDO bytes out.
#[derive(Clone, Debug)]
pub(in crate::tests) struct Cable {
    /// The chain, the tap nearest TDI first.
    pub(in crate::tests) taps: Vec<Tap>,
    tck: bool,
    /// Byte-shift bytes still to come, and whether they are read.
    shifting: usize,
    reading: bool,
    /// What the host did that a cable would not take kindly to.
    pub(in crate::tests) faults: Vec<&'static str>,
}

impl Cable {
    pub(in crate::tests) fn new(taps: Vec<Tap>) -> Cable {
        Cable {
            taps,
            tck: false,
            shifting: 0,
            reading: false,
            faults: Vec::new(),
        }
    }

    /// TDO, and one TCK through the chain.
    fn pulse(&mut self, tms: bool, tdi: bool) -> bool {
        let outs: Vec<bool> = self.taps.iter().map(Tap::tdo).collect();
        for (index, tap) in self.taps.iter_mut().enumerate() {
            let input = if index == 0 { tdi } else { outs[index - 1] };
            tap.clock(tms, input);
        }
        outs.last().copied().unwrap_or(tdi)
    }

    fn tdo(&self) -> bool {
        self.taps.last().is_none_or(Tap::tdo)
    }

    /// One byte from the host: what it answers, if anything.
    fn take(&mut self, byte: u8) -> Option<u8> {
        if self.shifting > 0 {
            self.shifting -= 1;
            let mut answer = 0;
            for bit in 0..8 {
                let tdo = self.pulse(false, byte >> bit & 1 != 0);
                answer |= u8::from(tdo) << bit;
            }
            return self.reading.then_some(answer);
        }
        if byte & 0x80 != 0 {
            if self.tck {
                self.faults.push("a byte-shift header with TCK high");
            }
            self.shifting = usize::from(byte & 0x3F);
            self.reading = byte & 0x40 != 0;
            return None;
        }
        let answer = (byte & 0x40 != 0).then(|| u8::from(self.tdo()));
        let tck = byte & 1 != 0;
        if tck && !self.tck {
            let _ = self.pulse(byte & 2 != 0, byte & 0x10 != 0);
        }
        self.tck = tck;
        answer
    }
}
