//! One FTDI chip on its claimed interface: a serial port, as Linux's
//! `ftdi_sio` makes one, and for a USB-Blaster the JTAG cable behind it.
//!
//! [`Serial::open`] sets the chip up as `ftdi_sio` does at open: the UART
//! reset, both FIFOs purged, the latency timer set, DTR and RTS raised, and
//! a UART at [`Line::DEFAULT`]. From then on:
//!
//! * [`Serial::read`] strips the two status bytes from every packet and
//!   gives each byte a flag the way `ftdi_process_packet` does: a packet
//!   whose line status has a parity or framing error marks every byte of it
//!   so, a break marks its last byte if that is a NUL, and an overrun puts
//!   a NUL marked as one before the packet's bytes. The modem lines are
//!   kept, with Linux's `TIOCGICOUNT` counters of their changes and of the
//!   errors;
//! * [`Serial::write`] queues, never waits ([`Usb::send`]);
//! * [`Serial::configure`], [`Serial::modem`], [`Serial::set_break`] and
//!   [`Serial::set_latency`] are the chip's requests.

use crate::blaster;
use crate::ftdi::{self, Chip, Line, Role};
use crate::{CHUNK_BYTES, Usb};

/// A byte's flag, as Linux's `TTY_*` numbers it.
pub const FLAG_NORMAL: u8 = 0;
/// A break: the byte is the NUL the break was received as.
pub const FLAG_BREAK: u8 = 1;
/// A framing error.
pub const FLAG_FRAME: u8 = 2;
/// A parity error.
pub const FLAG_PARITY: u8 = 3;
/// An overrun: the byte is a NUL standing for what was lost.
pub const FLAG_OVERRUN: u8 = 4;

/// How long a UART's chip holds a part-filled packet: FTDI's default, and
/// Linux's.
pub const UART_LATENCY_MS: u8 = 16;
/// The same for a USB-Blaster: `OpenOCD`'s two, since every scan waits for
/// its answer.
pub const BLASTER_LATENCY_MS: u8 = 2;

/// What the class driver was given, as far as a chip needs it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Identity {
    /// `idVendor`.
    pub vendor: u16,
    /// `idProduct`.
    pub product: u16,
    /// `bcdDevice`.
    pub release: u16,
    /// `bInterfaceNumber`.
    pub interface: u8,
    /// The bulk IN endpoint's packet size: where the status bytes recur.
    pub max_packet: u16,
}

/// Why a request failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error<E> {
    /// The interface's.
    Usb(E),
    /// Not a chip this drives.
    Unknown,
    /// A rate the chip cannot come within 3% of.
    Rate,
    /// A cable that did not answer in time.
    Timeout,
}

/// The counters Linux's `TIOCGICOUNT` gives.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counters {
    /// Changes of CTS, DSR, RI and carrier.
    pub cts: u32,
    /// See `cts`.
    pub dsr: u32,
    /// See `cts`.
    pub rng: u32,
    /// See `cts`.
    pub dcd: u32,
    /// Bytes received and sent.
    pub rx: u32,
    /// See `rx`.
    pub tx: u32,
    /// Framing errors, overruns, parity errors and breaks received.
    pub frame: u32,
    /// See `frame`.
    pub overrun: u32,
    /// See `frame`.
    pub parity: u32,
    /// See `frame`.
    pub brk: u32,
}

/// One chip, set up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Serial {
    /// Its family.
    pub chip: Chip,
    /// What it is used as.
    pub role: Role,
    /// The `wIndex` port byte its requests carry.
    pub port: u16,
    /// Its bulk IN packet size.
    pub max_packet: u16,
    /// The line it was last set to.
    pub line: Line,
    /// The modem lines, as the last packet gave them ([`ftdi::MODEM_CTS`]
    /// and the rest).
    pub modem: u8,
    /// Whether the line is held in break.
    pub held_break: bool,
    /// The counters.
    pub counters: Counters,
    /// The transmitter was empty at the last packet.
    pub transmit_empty: bool,
    /// Whether a packet has been seen, so the first one's lines are not
    /// counted as changes.
    seen: bool,
}

impl Serial {
    /// Set up the chip `identity` names, if it is one of [`ftdi`]'s.
    ///
    /// # Errors
    ///
    /// [`Error::Unknown`] for a device that is none; a request's failure.
    pub fn open<U: Usb>(usb: &mut U, identity: &Identity) -> Result<Serial, Error<U::Error>> {
        let (chip, role) = ftdi::identify(identity.vendor, identity.product, identity.release)
            .ok_or(Error::Unknown)?;
        let port = chip.port(identity.interface);
        let mut serial = Serial {
            chip,
            role,
            port,
            max_packet: identity.max_packet,
            line: Line::DEFAULT,
            modem: 0,
            held_break: false,
            counters: Counters::default(),
            transmit_empty: true,
            seen: false,
        };
        control(usb, &ftdi::reset(port))?;
        for setup in &ftdi::purge(port) {
            control(usb, setup)?;
        }
        let latency = match role {
            Role::Uart => UART_LATENCY_MS,
            Role::Blaster => BLASTER_LATENCY_MS,
        };
        serial.set_latency(usb, latency)?;
        serial.modem(usb, true, true)?;
        if role == Role::Uart {
            serial.configure(usb, &Line::DEFAULT)?;
        }
        Ok(serial)
    }

    /// Set the line's rate, framing and flow control.
    ///
    /// # Errors
    ///
    /// [`Error::Rate`] for a rate the chip cannot have, which leaves the
    /// line as it was; a request's failure.
    pub fn configure<U: Usb>(&mut self, usb: &mut U, line: &Line) -> Result<(), Error<U::Error>> {
        let setups = ftdi::configure(self.chip, self.port, line).ok_or(Error::Rate)?;
        for setup in &setups {
            control(usb, setup)?;
        }
        self.line = *line;
        self.held_break = false;
        Ok(())
    }

    /// The rate the chip runs at for the line it was set to: what a Linux
    /// driver reports back with `tty_encode_baud_rate`.
    #[must_use]
    pub fn actual_baud(&self) -> u32 {
        ftdi::divisor(self.chip, self.line.baud)
            .and_then(|value| ftdi::rate(self.chip, value))
            .unwrap_or(self.line.baud)
    }

    /// Set DTR and RTS.
    ///
    /// # Errors
    ///
    /// The request's failure.
    pub fn modem<U: Usb>(
        &mut self,
        usb: &mut U,
        dtr: bool,
        rts: bool,
    ) -> Result<(), Error<U::Error>> {
        control(usb, &ftdi::modem(self.port, dtr, rts))
    }

    /// Hold the line in break, or let it go.
    ///
    /// # Errors
    ///
    /// The request's failure.
    pub fn set_break<U: Usb>(&mut self, usb: &mut U, on: bool) -> Result<(), Error<U::Error>> {
        control(usb, &ftdi::framing(self.port, &self.line, on))?;
        self.held_break = on;
        Ok(())
    }

    /// Set the latency timer, 1 to 255 ms.
    ///
    /// # Errors
    ///
    /// The request's failure.
    pub fn set_latency<U: Usb>(&mut self, usb: &mut U, ms: u8) -> Result<(), Error<U::Error>> {
        control(usb, &ftdi::latency(self.port, ms))
    }

    /// Queue `bytes`, without waiting: how many.
    ///
    /// # Errors
    ///
    /// The interface is gone.
    pub fn write<U: Usb>(&mut self, usb: &mut U, bytes: &[u8]) -> Result<usize, Error<U::Error>> {
        let sent = usb.send(bytes).map_err(Error::Usb)?;
        self.counters.tx = self
            .counters
            .tx
            .wrapping_add(u32::try_from(sent).unwrap_or(u32::MAX));
        Ok(sent)
    }

    /// What the chip received, into `data` with each byte's flag in
    /// `flags`, without waiting: how many bytes. A chunk is taken only
    /// while both have room for all of it, so nothing is dropped here.
    pub fn read<U: Usb>(&mut self, usb: &mut U, data: &mut [u8], flags: &mut [u8]) -> usize {
        let room = data.len().min(flags.len());
        let mut got = 0;
        let mut chunk = [0_u8; CHUNK_BYTES];
        while room - got >= CHUNK_BYTES {
            let Some(len) = usb.take_chunk(&mut chunk) else {
                break;
            };
            let max_packet = usize::from(self.max_packet).max(2);
            for packet in chunk.get(..len).unwrap_or_default().chunks(max_packet) {
                let (Some(data), Some(flags)) = (data.get_mut(got..room), flags.get_mut(got..room))
                else {
                    break;
                };
                got += self.packet(packet, data, flags);
            }
        }
        got
    }

    /// One packet: its status kept and counted, its bytes and flags put
    /// at the start of `data` and `flags`: how many.
    fn packet(&mut self, packet: &[u8], data: &mut [u8], flags: &mut [u8]) -> usize {
        let [modem, line, bytes @ ..] = packet else {
            return 0;
        };
        let modem = modem & 0xF0;
        if self.seen {
            let changed = modem ^ self.modem;
            let count = |counter: &mut u32, bit: u8| {
                if changed & bit != 0 {
                    *counter = counter.wrapping_add(1);
                }
            };
            count(&mut self.counters.cts, ftdi::MODEM_CTS);
            count(&mut self.counters.dsr, ftdi::MODEM_DSR);
            count(&mut self.counters.rng, ftdi::MODEM_RI);
            count(&mut self.counters.dcd, ftdi::MODEM_DCD);
        }
        self.seen = true;
        self.modem = modem;
        self.transmit_empty = line & ftdi::LINE_EMPTY != 0;
        if bytes.is_empty() {
            // Status only: errors count only with data, as Linux's do.
            return 0;
        }
        let mut at = 0;
        let mut put = |byte: u8, flag: u8| {
            if let (Some(to), Some(mark)) = (data.get_mut(at), flags.get_mut(at)) {
                *to = byte;
                *mark = flag;
                at += 1;
            }
        };
        let mut flag = FLAG_NORMAL;
        let mut breaking = false;
        if line & (ftdi::LINE_OVERRUN | ftdi::LINE_PARITY | ftdi::LINE_FRAMING | ftdi::LINE_BREAK)
            != 0
        {
            if line & ftdi::LINE_BREAK != 0 && bytes.last() == Some(&0) {
                self.counters.brk = self.counters.brk.wrapping_add(1);
                breaking = true;
            }
            if line & ftdi::LINE_PARITY != 0 {
                flag = FLAG_PARITY;
                self.counters.parity = self.counters.parity.wrapping_add(1);
            } else if line & ftdi::LINE_FRAMING != 0 {
                flag = FLAG_FRAME;
                self.counters.frame = self.counters.frame.wrapping_add(1);
            }
            if line & ftdi::LINE_OVERRUN != 0 {
                self.counters.overrun = self.counters.overrun.wrapping_add(1);
                put(0, FLAG_OVERRUN);
            }
        }
        self.counters.rx = self
            .counters
            .rx
            .wrapping_add(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
        let last = bytes.len() - 1;
        for (index, &byte) in bytes.iter().enumerate() {
            let mark = if breaking && index == last {
                FLAG_BREAK
            } else {
                flag
            };
            put(byte, mark);
        }
        at
    }
}

fn control<U: Usb>(usb: &mut U, setup: &crate::Setup) -> Result<(), Error<U::Error>> {
    usb.control(setup, &mut []).map(|_| ()).map_err(Error::Usb)
}

/// How long a USB-Blaster has to take or answer bytes: a scan of a few
/// hundred bits answers within a few of its latency timer's 2 ms.
pub const CABLE_PATIENCE: u64 = 500_000_000;

/// A USB-Blaster, as [`blaster::Link`] reaches it.
#[derive(Debug)]
pub struct Cable<'a, U> {
    usb: &'a mut U,
    max_packet: usize,
    /// Answer bytes stripped of their status and not yet read.
    held: [u8; CHUNK_BYTES],
    at: usize,
    len: usize,
}

impl<'a, U: Usb> Cable<'a, U> {
    /// The cable behind `serial`, which must be a Blaster's.
    pub fn new(usb: &'a mut U, serial: &Serial) -> Option<Self> {
        (serial.role == Role::Blaster).then_some(Cable {
            usb,
            max_packet: usize::from(serial.max_packet).max(2),
            held: [0; CHUNK_BYTES],
            at: 0,
            len: 0,
        })
    }
}

impl<U: Usb> Cable<'_, U> {
    /// Take the next chunk, if there is one, its status bytes stripped,
    /// as what is held: whether there was one.
    fn refill(&mut self) -> bool {
        let mut chunk = [0_u8; CHUNK_BYTES];
        let Some(len) = self.usb.take_chunk(&mut chunk) else {
            return false;
        };
        let (mut held, mut len_held) = ([0_u8; CHUNK_BYTES], 0);
        let mut keep = |data: &[u8]| {
            if let Some(slot) = held.get_mut(len_held..len_held + data.len()) {
                slot.copy_from_slice(data);
                len_held += data.len();
            }
        };
        ftdi::payload(
            chunk.get(..len).unwrap_or_default(),
            self.max_packet,
            |_, data| {
                keep(data);
            },
        );
        self.held = held;
        self.at = 0;
        self.len = len_held;
        true
    }
}

impl<U: Usb> blaster::Link for Cable<'_, U> {
    type Error = Error<U::Error>;

    fn write(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        let until = self.usb.now().saturating_add(CABLE_PATIENCE);
        let mut sent = 0;
        while sent < bytes.len() {
            sent += self
                .usb
                .send(bytes.get(sent..).unwrap_or_default())
                .map_err(Error::Usb)?;
            if sent == bytes.len() {
                break;
            }
            if self.usb.now() >= until {
                return Err(Error::Timeout);
            }
            self.usb.pause().map_err(Error::Usb)?;
        }
        Ok(())
    }

    fn read(&mut self, into: &mut [u8]) -> Result<(), Self::Error> {
        let until = self.usb.now().saturating_add(CABLE_PATIENCE);
        let mut got = 0;
        loop {
            let take = (self.len - self.at).min(into.len() - got);
            if let (Some(to), Some(from)) = (
                into.get_mut(got..got + take),
                self.held.get(self.at..self.at + take),
            ) {
                to.copy_from_slice(from);
            }
            got += take;
            self.at += take;
            if got == into.len() {
                return Ok(());
            }
            if self.refill() {
                continue;
            }
            if self.usb.now() >= until {
                return Err(Error::Timeout);
            }
            self.usb.pause().map_err(Error::Usb)?;
        }
    }
}
