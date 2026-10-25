//! The H family's MPSSE engine, as far as a loopback through it: what
//! proves a real FT232H's two bulk pipes carry bytes both ways when its
//! TX and RX pins are wired to nothing (`docs/USB-SERIAL.md` §7).
//!
//! `SET_BITMODE` mode 2 puts the chip in MPSSE mode; then the bytes sent
//! are commands (FTDI's AN108). [`loopback`] synchronises -- an unknown
//! command, 0xAA, is answered "bad command" and the byte, 0xFA 0xAA --
//! turns the internal loopback on (0x84), clocks the pattern out and in
//! (0x31, bytes, MSB first, out on the falling edge, in on the rising),
//! and reads what came back; then loopback off (0x85), the chip's own mode
//! again, and the line the UART had. Nothing reaches a pin in loopback,
//! but the pins are inputs while it runs: it is a diagnostic, run only
//! when asked.

use crate::Usb;
use crate::blaster::Link;
use crate::ftdi::{self, Chip};
use crate::port::{Cable, Error, Serial};

/// `SET_BITMODE`'s modes: the chip's own, and MPSSE.
const MODE_RESET: u8 = 0x00;
const MODE_MPSSE: u8 = 0x02;
/// MPSSE commands (AN108).
const LOOPBACK_ON: u8 = 0x84;
const LOOPBACK_OFF: u8 = 0x85;
const CLOCK_DIVISOR: u8 = 0x86;
const SEND_IMMEDIATE: u8 = 0x87;
const DIVIDE_BY_5_OFF: u8 = 0x8A;
const BYTES_OUT_IN: u8 = 0x31;
const BAD_COMMAND: u8 = 0xFA;
const BOGUS: u8 = 0xAA;
/// The most bytes one clocking command takes: 65536 by its length field;
/// one bulk transfer's worth here.
pub const MAX_PATTERN: usize = 4096;

/// What the loopback found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Every byte came back as sent.
    Equal,
    /// The engine did not answer the synchronisation as MPSSE does.
    NoEngine,
    /// The bytes came back, and the first that differs is at this place.
    Differs(usize),
}

/// Run the loopback with `pattern` on `serial`'s chip, then put the chip
/// back as a UART on the line it had.
///
/// # Errors
///
/// [`Error::Unknown`] for a chip with no MPSSE (not the H family), or a
/// pattern past [`MAX_PATTERN`]; a request's or the read's failure.
pub fn loopback<U: Usb>(
    usb: &mut U,
    serial: &mut Serial,
    pattern: &[u8],
) -> Result<Outcome, Error<U::Error>> {
    if serial.chip != Chip::H || pattern.is_empty() || pattern.len() > MAX_PATTERN {
        return Err(Error::Unknown);
    }
    let outcome = run(usb, serial, pattern);
    // Back to a UART, whatever came of it.
    let restored = restore(usb, serial);
    let outcome = outcome?;
    restored?;
    Ok(outcome)
}

fn control<U: Usb>(usb: &mut U, setup: &crate::Setup) -> Result<(), Error<U::Error>> {
    usb.control(setup, &mut []).map(|_| ()).map_err(Error::Usb)
}

fn run<U: Usb>(
    usb: &mut U,
    serial: &mut Serial,
    pattern: &[u8],
) -> Result<Outcome, Error<U::Error>> {
    let port = serial.port;
    serial.set_latency(usb, 1)?;
    control(usb, &ftdi::bitmode(port, 0, MODE_RESET))?;
    control(usb, &ftdi::bitmode(port, 0, MODE_MPSSE))?;
    for setup in &ftdi::purge(port) {
        control(usb, setup)?;
    }
    // Whatever the chip sent before the purge.
    let mut stale = [0_u8; crate::CHUNK_BYTES];
    while usb.take_chunk(&mut stale).is_some() {}
    let mut cable = Cable::raw(usb, serial);
    cable.write(&[BOGUS])?;
    let mut answer = [0_u8; 2];
    cable.read(&mut answer)?;
    if answer != [BAD_COMMAND, BOGUS] {
        return Ok(Outcome::NoEngine);
    }
    let length = u16::try_from(pattern.len() - 1)
        .unwrap_or(u16::MAX)
        .to_le_bytes();
    cable.write(&[
        LOOPBACK_ON,
        DIVIDE_BY_5_OFF,
        CLOCK_DIVISOR,
        0x05,
        0x00,
        BYTES_OUT_IN,
        length[0],
        length[1],
    ])?;
    cable.write(pattern)?;
    cable.write(&[SEND_IMMEDIATE, LOOPBACK_OFF])?;
    let mut back = [0_u8; MAX_PATTERN];
    let back = back.get_mut(..pattern.len()).ok_or(Error::Unknown)?;
    cable.read(back)?;
    Ok(back
        .iter()
        .zip(pattern)
        .position(|(got, sent)| got != sent)
        .map_or(Outcome::Equal, Outcome::Differs))
}

fn restore<U: Usb>(usb: &mut U, serial: &mut Serial) -> Result<(), Error<U::Error>> {
    let port = serial.port;
    control(usb, &ftdi::bitmode(port, 0, MODE_RESET))?;
    for setup in &ftdi::purge(port) {
        control(usb, setup)?;
    }
    serial.set_latency(usb, crate::port::UART_LATENCY_MS)?;
    let line = serial.line;
    serial.configure(usb, &line)
}
