//! FTDI's USB UARTs: which chips these are, the vendor requests that set
//! one up, and the two status bytes every bulk IN packet starts with.
//!
//! The chips have no class: their one interface is vendor-specific, with a
//! bulk IN and a bulk OUT endpoint, and everything else is a vendor
//! request on the control endpoint. What each request carries is what
//! Linux's `drivers/usb/serial/ftdi_sio.c` sends, which is also what FTDI's
//! application note AN232B-05 and the chips' data sheets describe:
//!
//! * `RESET` (0): the UART, or one of its two FIFOs purged;
//! * `SET_MODEM_CTRL` (1): DTR and RTS, each with a mask bit;
//! * `SET_FLOW_CTRL` (2): none, RTS/CTS, DTR/DSR or XON/XOFF, in the high
//!   byte of `wIndex`, the XON and XOFF characters in `wValue`;
//! * `SET_BAUD_RATE` (3): the divisor ([`divisor`]), spread over `wValue`
//!   and `wIndex`;
//! * `SET_DATA` (4): data bits, parity, stop bits and break;
//! * `SET_LATENCY_TIMER` (9): how long the chip holds a part-filled packet;
//! * `SET_BITMODE` (0x0B): what MPSSE and the bit-bang modes are entered
//!   with, which nothing here enters (`docs/USB-SERIAL.md` §6).
//!
//! The low byte of `wIndex` names the port: 0 on the single-port chips of
//! the B and R families, one more than the interface's number on the
//! H family and on FT-X.
//!
//! # Chip families
//!
//! `bcdDevice` tells them apart ([`Chip::of`]). What matters here is the
//! divisor: the B, R and X chips divide a 3 MHz clock (48 MHz over 16
//! samples a bit) by an integer and an eighth; the H chips divide 12 MHz
//! (120 MHz over 10 samples) the same way once bit 17 of the divisor turns
//! off their divide-by-2.5, which gives them up to 12 Mbaud but nothing
//! below 1200, where they fall back to the 3 MHz clock.
//!
//! # Bulk IN
//!
//! Every packet the chip sends begins with two status bytes -- the modem
//! lines, then the line status -- whether or not data follows, and it sends
//! a status-only packet each time its latency timer runs out. A transfer
//! of several packets has the pair at the start of each, so they are
//! stripped a packet at a time ([`payload`]), the endpoint's packet size
//! apart: 64 bytes at full speed, 512 at high speed.

use crate::Setup;

/// FTDI's vendor id.
pub const VENDOR_FTDI: u16 = 0x0403;
/// The FT232H's product id: one high-speed port ("Single RS232-HS").
pub const PRODUCT_FT232H: u16 = 0x6014;
/// The id of the FT232B and FT232R UARTs, and of the FT245B FIFO: what
/// QEMU's `usb-serial` presents too, as an FT232BM.
pub const PRODUCT_FT232: u16 = 0x6001;
/// Altera's vendor id.
pub const VENDOR_ALTERA: u16 = 0x09FB;
/// The original USB-Blaster: an FT245B in front of a CPLD that turns the
/// bytes it is sent into JTAG ([`crate::blaster`]).
pub const PRODUCT_USB_BLASTER: u16 = 0x6001;

/// `bmRequestType` of every request here: vendor, host to device.
const VENDOR_OUT: u8 = 0x40;

const REQUEST_RESET: u8 = 0;
const REQUEST_MODEM: u8 = 1;
const REQUEST_FLOW: u8 = 2;
const REQUEST_BAUD: u8 = 3;
const REQUEST_DATA: u8 = 4;
const REQUEST_LATENCY: u8 = 9;
const REQUEST_BITMODE: u8 = 0x0B;

/// `RESET`'s values: the whole UART, the receive FIFO, the transmit FIFO.
const RESET_SIO: u16 = 0;
const PURGE_RX: u16 = 1;
const PURGE_TX: u16 = 2;

/// The B family's clock for the divisor: 48 MHz.
const CLOCK_B: u64 = 48_000_000;
/// The H family's: 120 MHz, sampled ten times a bit.
const CLOCK_H: u64 = 120_000_000;
/// The divisor bit that turns an H chip's divide-by-2.5 off.
const H_CLOCK_BIT: u32 = 1 << 17;
/// Which eighth each three-bit code adds, by eighths: the chips' encoding.
const EIGHTHS: [u32; 8] = [0, 3, 2, 4, 1, 5, 6, 7];

/// The status bytes' modem lines (byte 0): CTS, DSR, RI, carrier.
pub const MODEM_CTS: u8 = 1 << 4;
/// DSR.
pub const MODEM_DSR: u8 = 1 << 5;
/// Ring indicator.
pub const MODEM_RI: u8 = 1 << 6;
/// Carrier detect (RLSD).
pub const MODEM_DCD: u8 = 1 << 7;
/// The line status (byte 1): overrun, parity, framing, break.
pub const LINE_OVERRUN: u8 = 1 << 1;
/// A parity error.
pub const LINE_PARITY: u8 = 1 << 2;
/// A framing error.
pub const LINE_FRAMING: u8 = 1 << 3;
/// A break.
pub const LINE_BREAK: u8 = 1 << 4;
/// The transmitter is empty.
pub const LINE_EMPTY: u8 = 1 << 6;

/// A chip family, by what its divisor is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Chip {
    /// FT232B and FT245B (`bcdDevice` 0x400): what QEMU emulates.
    B,
    /// FT232R and FT245R (0x600).
    R,
    /// FT-X (0x1000).
    X,
    /// FT2232H, FT4232H and FT232H (0x700, 0x800, 0x900).
    H,
}

impl Chip {
    /// The family `bcdDevice` names; `None` for one whose divisor this
    /// does not compute (the AM and SIO chips, and anything newer).
    #[must_use]
    pub const fn of(release: u16) -> Option<Chip> {
        match release {
            0x400 => Some(Chip::B),
            0x600 => Some(Chip::R),
            0x1000 => Some(Chip::X),
            0x700 | 0x800 | 0x900 => Some(Chip::H),
            _ => None,
        }
    }

    /// A word for the console.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Chip::B => "FT232B",
            Chip::R => "FT232R",
            Chip::X => "FT-X",
            Chip::H => "FT232H",
        }
    }

    /// The port byte of `wIndex` for the chip's interface `interface`.
    #[must_use]
    pub const fn port(self, interface: u8) -> u16 {
        match self {
            Chip::B | Chip::R => 0,
            Chip::X | Chip::H => interface as u16 + 1,
        }
    }
}

/// What a matched device is used as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// A serial port.
    Uart,
    /// A USB-Blaster: JTAG through the CPLD behind the FIFO.
    Blaster,
}

impl Role {
    /// A word for the console.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Role::Uart => "UART",
            Role::Blaster => "USB-Blaster",
        }
    }
}

/// What a device with these ids is, if it is one of FTDI's here.
#[must_use]
pub const fn identify(vendor: u16, product: u16, release: u16) -> Option<(Chip, Role)> {
    let Some(chip) = Chip::of(release) else {
        return None;
    };
    match (vendor, product) {
        (VENDOR_FTDI, PRODUCT_FT232H | PRODUCT_FT232) => Some((chip, Role::Uart)),
        (VENDOR_ALTERA, PRODUCT_USB_BLASTER) => Some((chip, Role::Blaster)),
        _ => None,
    }
}

/// Divide `numerator` by `denominator`, rounding to the nearest.
const fn rounded(numerator: u64, denominator: u64) -> u64 {
    (numerator + denominator / 2) / denominator
}

/// A divisor in eighths, encoded: the integer part in bits 13..0, the
/// eighths' code in bits 16..14, and the two values the chips read
/// specially -- 1 for exactly 1, 0x4001 for 1.5 -- moved where they read
/// them.
fn encode(eighths: u64) -> u32 {
    let whole = (eighths >> 3) as u32;
    let fraction = EIGHTHS.get((eighths & 7) as usize).copied().unwrap_or(0);
    match whole | (fraction << 14) {
        1 => 0,
        0x4001 => 1,
        other => other,
    }
}

/// The divisor `chip` is set to for `baud`, as `SET_BAUD_RATE` carries it,
/// or `None` for a rate the chip cannot come within 3% of.
#[must_use]
pub fn divisor(chip: Chip, baud: u32) -> Option<u32> {
    let wanted = u64::from(baud);
    let value = match chip {
        Chip::H if (1200..=12_000_000).contains(&baud) => {
            encode(rounded(8 * CLOCK_H, 10 * wanted)) | H_CLOCK_BIT
        }
        Chip::B | Chip::R | Chip::X | Chip::H if (184..=3_000_000).contains(&baud) => {
            encode(rounded(CLOCK_B, 2 * wanted))
        }
        _ => return None,
    };
    let got = u64::from(rate(chip, value)?);
    // Within 3%, as the chips' data sheets promise.
    (got.abs_diff(wanted) * 100 <= wanted * 3).then_some(value)
}

/// The rate `value`, a divisor as [`divisor`] encodes it, gives on `chip`.
#[must_use]
pub fn rate(chip: Chip, value: u32) -> Option<u32> {
    let code = (value >> 14) & 7;
    let fraction = EIGHTHS.iter().position(|&eighths| eighths == code)? as u64;
    let eighths = match value & !H_CLOCK_BIT {
        0 => 8,
        1 => 12,
        _ => u64::from(value & 0x3FFF) * 8 + fraction,
    };
    let fast = chip == Chip::H && value & H_CLOCK_BIT != 0;
    let clock = if fast { 8 * CLOCK_H / 10 } else { CLOCK_B / 2 };
    u32::try_from(rounded(clock, eighths)).ok()
}

/// A character's parity bit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Parity {
    /// None.
    None,
    /// Odd.
    Odd,
    /// Even.
    Even,
    /// Always one.
    Mark,
    /// Always zero.
    Space,
}

/// Flow control.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flow {
    /// None.
    None,
    /// RTS and CTS.
    RtsCts,
    /// DTR and DSR.
    DtrDsr,
    /// XON and XOFF, with these two characters.
    XonXoff {
        /// The character that restarts output.
        start: u8,
        /// The one that stops it.
        stop: u8,
    },
}

/// A line's settings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Line {
    /// Bits a second.
    pub baud: u32,
    /// Seven or eight.
    pub data_bits: u8,
    /// The parity bit.
    pub parity: Parity,
    /// Two stop bits, rather than one.
    pub two_stop_bits: bool,
    /// Flow control.
    pub flow: Flow,
}

impl Line {
    /// 9600 baud, eight bits, no parity, one stop bit: what Linux opens a
    /// `ttyUSB` with.
    pub const DEFAULT: Line = Line {
        baud: 9600,
        data_bits: 8,
        parity: Parity::None,
        two_stop_bits: false,
        flow: Flow::None,
    };
}

/// The bits of a `struct termios` a line is read from, and from no others:
/// `c_iflag`, `c_cflag`, the output speed and the start and stop
/// characters.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Termios {
    /// `c_iflag`.
    pub iflag: u32,
    /// `c_cflag`.
    pub cflag: u32,
    /// `c_ospeed`, from `struct termios2`.
    pub ospeed: u32,
    /// `c_cc[VSTART]`.
    pub start: u8,
    /// `c_cc[VSTOP]`.
    pub stop: u8,
}

/// `c_cflag`'s character size bits, and `CS7` among them
/// (`asm-generic/termbits.h`, the same on all three architectures).
const CSIZE: u32 = 0o60;
const CS7: u32 = 0o40;
/// `c_cflag`: two stop bits.
const CSTOPB: u32 = 0o100;
/// `c_cflag`: a parity bit, odd rather than even, and stuck rather than
/// computed.
const PARENB: u32 = 0o400;
const PARODD: u32 = 0o1000;
const CMSPAR: u32 = 0o10_000_000_000;
/// `c_cflag`: RTS/CTS flow control.
const CRTSCTS: u32 = 0o20_000_000_000;
/// `c_iflag`: XON/XOFF on input, as `IXON` is on output.
const IXOFF: u32 = 0o10_000;
/// `c_iflag`: XON/XOFF on output.
const IXON: u32 = 0o2000;

impl Termios {
    /// The line these settings ask for. A speed of 0 is the hang-up
    /// request, which leaves the speed as it was and is the caller's to
    /// act on.
    #[must_use]
    pub const fn line(&self, current: u32) -> Line {
        let data_bits = if self.cflag & CSIZE == CS7 { 7 } else { 8 };
        let parity = match (
            self.cflag & PARENB != 0,
            self.cflag & PARODD != 0,
            self.cflag & CMSPAR != 0,
        ) {
            (false, _, _) => Parity::None,
            (true, false, false) => Parity::Even,
            (true, true, false) => Parity::Odd,
            (true, false, true) => Parity::Space,
            (true, true, true) => Parity::Mark,
        };
        let flow = if self.cflag & CRTSCTS != 0 {
            Flow::RtsCts
        } else if self.iflag & (IXON | IXOFF) != 0 {
            Flow::XonXoff {
                start: self.start,
                stop: self.stop,
            }
        } else {
            Flow::None
        };
        Line {
            baud: if self.ospeed == 0 {
                current
            } else {
                self.ospeed
            },
            data_bits,
            parity,
            two_stop_bits: self.cflag & CSTOPB != 0,
            flow,
        }
    }
}

const fn vendor(request: u8, value: u16, index: u16) -> Setup {
    Setup {
        request_type: VENDOR_OUT,
        request,
        value,
        index,
        length: 0,
    }
}

/// `RESET` of the whole UART on port `port`.
#[must_use]
pub const fn reset(port: u16) -> Setup {
    vendor(REQUEST_RESET, RESET_SIO, port)
}

/// `RESET`, purging both FIFOs of port `port`: the receive one, then the
/// transmit one, as two requests.
#[must_use]
pub const fn purge(port: u16) -> [Setup; 2] {
    [
        vendor(REQUEST_RESET, PURGE_RX, port),
        vendor(REQUEST_RESET, PURGE_TX, port),
    ]
}

/// `SET_MODEM_CTRL`: DTR and RTS both set as given.
#[must_use]
pub const fn modem(port: u16, dtr: bool, rts: bool) -> Setup {
    // Both mask bits in the high byte, so both lines are written.
    let value = 0x0300 | (dtr as u16) | ((rts as u16) << 1);
    vendor(REQUEST_MODEM, value, port)
}

/// `SET_LATENCY_TIMER`: hold a part-filled packet at most `ms`
/// milliseconds, 1 to 255.
#[must_use]
pub const fn latency(port: u16, ms: u8) -> Setup {
    let ms = if ms == 0 { 1 } else { ms };
    vendor(REQUEST_LATENCY, ms as u16, port)
}

/// `SET_BITMODE`: `mode` with the pin direction `mask`. 0 is the reset to
/// the chip's own mode (a UART, or a FIFO); 2 is MPSSE.
#[must_use]
pub const fn bitmode(port: u16, mask: u8, mode: u8) -> Setup {
    vendor(REQUEST_BITMODE, ((mode as u16) << 8) | mask as u16, port)
}

/// The requests that set `line` on `chip`'s port `port`: the rate, the
/// framing and the flow control, or `None` for a rate the chip cannot have.
#[must_use]
pub fn configure(chip: Chip, port: u16, line: &Line) -> Option<[Setup; 3]> {
    let divisor = divisor(chip, line.baud)?;
    let high = (divisor >> 16) as u16;
    let index = match chip {
        Chip::B | Chip::R => high,
        Chip::X | Chip::H => (high << 8) | port,
    };
    let baud = vendor(REQUEST_BAUD, divisor as u16, index);
    let data = framing(port, line, false);
    let (value, handshake) = match line.flow {
        Flow::None => (0, 0),
        Flow::RtsCts => (0, 0x0100),
        Flow::DtrDsr => (0, 0x0200),
        Flow::XonXoff { start, stop } => ((u16::from(stop) << 8) | u16::from(start), 0x0400),
    };
    let flow = vendor(REQUEST_FLOW, value, handshake | port);
    Some([baud, data, flow])
}

/// `SET_DATA` for `line`'s framing on port `port`, with the line held in
/// break (TX low) or not: a break is the framing request again with bit
/// 14 set, and its end the same request without it, as `ftdi_sio`'s
/// `break_ctl` sends.
#[must_use]
pub fn framing(port: u16, line: &Line, held_break: bool) -> Setup {
    let parity: u16 = match line.parity {
        Parity::None => 0,
        Parity::Odd => 1,
        Parity::Even => 2,
        Parity::Mark => 3,
        Parity::Space => 4,
    };
    let stop: u16 = if line.two_stop_bits { 2 } else { 0 };
    vendor(
        REQUEST_DATA,
        u16::from(line.data_bits) | (parity << 8) | (stop << 11) | (u16::from(held_break) << 14),
        port,
    )
}

/// The two status bytes at the start of a packet.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Status {
    /// The modem lines: [`MODEM_CTS`] and the rest.
    pub modem: u8,
    /// The line status: [`LINE_OVERRUN`] and the rest.
    pub line: u8,
}

/// Hand each packet of a bulk IN transfer to `take`: its status, and the
/// data after it, which may be none. `max_packet` is the endpoint's packet
/// size; a packet shorter than two bytes is not one the chips send, and is
/// dropped.
pub fn payload(transfer: &[u8], max_packet: usize, mut take: impl FnMut(Status, &[u8])) {
    if max_packet < 2 {
        return;
    }
    for packet in transfer.chunks(max_packet) {
        let [modem, line, data @ ..] = packet else {
            continue;
        };
        take(
            Status {
                // The low nibble is the chip's own, always 1.
                modem: modem & 0xF0,
                line: *line,
            },
            data,
        );
    }
}
