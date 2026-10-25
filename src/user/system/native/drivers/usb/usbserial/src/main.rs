//! `usbserial`: the class driver of one USB serial interface, in a process
//! of its own (`docs/USB-SERIAL.md` §6.1).
//!
//! devmgr starts it for an interface `usbhid` exported whose identity its
//! table names -- an FTDI UART or a USB-Blaster -- with BIND on its bootstrap
//! channel: the device's number and the interface channel. The host's
//! HELLO waits in that channel with the shared VMO. Then:
//!
//! 1. both bulk endpoints opened as streams, and the chip set up as Linux's
//!    `ftdi_sio` sets it up at open (`ferrix_usb_serial::port`);
//! 2. a pseudoterminal ([`tty`]), its slave linked as `/tmp/ttyUSB<N>`, or
//!    for a Blaster `/tmp/jtag<N>`, which takes JTAG command lines;
//! 3. one wait, a `ppoll` on the port the interface channel is watched on
//!    and on the terminal's master, and on each wake the settings, the
//!    program's bytes out and the chip's bytes in.
//!
//! The interface channel closing is the device going, or `usbhid`: the
//! terminal hangs up and the driver ends with 0. Its own death is heard by
//! `usbhid`, which attaches the interface again; devmgr starts a new driver
//! under the same number, whose budget its restart policy keeps.
//!
//! What it does is said on standard error, which is the console. The exit
//! status names the step that failed ([`Step`]).

#![no_std]
#![no_main]

mod link;
mod tty;

use core::fmt::Write as _;

use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::signals::Signals;
use ferrix_rt::native::channel::Channel;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::vmo::Vmo;
use ferrix_rt::{Bootstrap, Kernel, linux};
use ferrix_usb_serial::blaster::{self, Command, Jtag, MAX_TAPS, SCAN_BYTES, Tap};
use ferrix_usb_serial::ftdi::Role;
use ferrix_usb_serial::mpsse::{self, Outcome};
use ferrix_usb_serial::port::{Cable, Identity, Serial};
use ferrix_usbif::{MAX_BYTES, Message};

use crate::link::Link;
use crate::tty::{Kind, Tty};

ferrix_rt::entry!(main);

/// The interface channel's port key.
const KEY_LINK: u64 = 1;
/// What one look moves each way at most.
const PUMP_BYTES: usize = 4096;
/// The longest command line kept.
const LINE_BYTES: usize = 256;
/// The longest answer line.
const ANSWER_BYTES: usize = 192;
/// `POLLIN`.
const POLLIN: i16 = 0x001;

/// Where a run stopped, as the exit status.
#[derive(Clone, Copy, Debug)]
#[repr(i32)]
enum Step {
    /// No bootstrap channel, or its first message was not BIND.
    Start = 1,
    /// The interface channel's first message was not HELLO, or HELLO named
    /// no bulk endpoint each way.
    Hello = 2,
    /// The streams would not open.
    Streams = 3,
    /// The chip would not be set up.
    Chip = 4,
    /// No terminal could be made.
    Terminal = 5,
    /// The port or the wait could not be arranged.
    Events = 6,
}

fn main(bootstrap: Bootstrap) -> i32 {
    let Some(boot) = bootstrap else {
        return Step::Start as i32;
    };
    match run(&boot) {
        Ok(()) => 0,
        Err(step) => {
            ferrix_rt::err!("usbserial: stopped at {step:?}");
            step as i32
        }
    }
}

/// Whether `path`, NUL-terminated, names a file root owns, not following a
/// last symbolic link: `statx`'s `stx_uid`, the same place on every
/// architecture.
fn owned_by_root(path: &[u8]) -> bool {
    const AT_FDCWD: isize = -100;
    const AT_SYMLINK_NOFOLLOW: usize = 0x100;
    const STATX_UID: usize = 0x8;
    let mut statx = [0_u8; 256];
    // SAFETY: `path` is NUL-terminated and `statx` is 256 bytes, what the
    // call writes, both alive for the call.
    let found = unsafe {
        linux::call(
            linux::numbers::STATX,
            [
                AT_FDCWD as usize,
                path.as_ptr().addr(),
                AT_SYMLINK_NOFOLLOW,
                STATX_UID,
                statx.as_mut_ptr().addr(),
                0,
            ],
        )
    };
    found.is_ok() && statx.get(20..24) == Some(&[0, 0, 0, 0])
}

/// The first message on `channel` and its one handle.
fn first(channel: &Channel<Kernel>) -> Option<(Message, OwnedHandle<Kernel>)> {
    let _ = channel.wait_one(Signals::READABLE, Deadline::Never).ok()?;
    let mut bytes = [0_u8; MAX_BYTES];
    let mut handles = [Handle::INVALID; 1];
    let received = channel.read(&mut bytes, &mut handles).ok()?;
    let handle = OwnedHandle::from_raw(Kernel, *handles.first()?);
    if received.handles != 1 {
        return None;
    }
    let message = Message::decode(bytes.get(..received.bytes).unwrap_or_default()).ok()?;
    Some((message, handle))
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

/// An answer line, built in place.
struct Answer {
    bytes: [u8; ANSWER_BYTES],
    len: usize,
}

impl core::fmt::Write for Answer {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.len + text.len();
        let slot = self.bytes.get_mut(self.len..end).ok_or(core::fmt::Error)?;
        slot.copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

struct Driver {
    number: u8,
    link: Link,
    serial: Serial,
    tty: Tty,
    port: Port<Kernel>,
    port_fd: i32,
    armed: bool,
    /// The modem lines last said on the console.
    modem_said: Option<u8>,
}

fn run(boot: &Channel<Kernel>) -> Result<(), Step> {
    let Some((Message::Bind { number }, interface)) = first(boot) else {
        return Err(Step::Start);
    };
    let channel = Channel::from_owned(interface);
    let Some((
        Message::Hello {
            identity,
            count,
            endpoints,
            ..
        },
        vmo,
    )) = first(&channel)
    else {
        return Err(Step::Hello);
    };
    let endpoints = endpoints.get(..count).unwrap_or_default();
    let bulk = |inward: bool| {
        endpoints
            .iter()
            .find(|endpoint| endpoint.is_bulk() && endpoint.is_in() == inward)
            .copied()
    };
    let (Some(inward), Some(outward)) = (bulk(true), bulk(false)) else {
        return Err(Step::Hello);
    };
    let mut link = Link::new(
        channel,
        Vmo::from_owned(vmo),
        inward.address,
        outward.address,
    );
    link.open_streams().map_err(|error| {
        ferrix_rt::err!("usbserial: {number}: the streams would not open: {error:?}");
        Step::Streams
    })?;
    let chip = Identity {
        vendor: identity.vendor,
        product: identity.product,
        release: identity.release,
        interface: identity.interface,
        max_packet: inward.max_packet,
    };
    let serial = Serial::open(&mut link, &chip).map_err(|error| {
        ferrix_rt::err!(
            "usbserial: {number}: {:04x}:{:04x} would not be set up: {error:?}",
            identity.vendor,
            identity.product
        );
        Step::Chip
    })?;
    let kind = match serial.role {
        Role::Uart => Kind::Uart,
        Role::Blaster => Kind::Jtag,
    };
    let tty = Tty::open(usize::from(number), kind).ok_or_else(|| {
        ferrix_rt::err!("usbserial: {number}: no terminal: /dev/ptmx would not open");
        Step::Terminal
    })?;
    ferrix_rt::info!(
        "usbserial: serial {number} {:04x}:{:04x} ({} {}, {}-byte packets, serial {}) is {} (/dev/pts/{})",
        identity.vendor,
        identity.product,
        serial.chip.word(),
        serial.role.word(),
        serial.max_packet,
        core::str::from_utf8(identity.serial.as_bytes()).unwrap_or("?"),
        tty.link(),
        tty.number
    );
    let port = port::create(Kernel).map_err(|_| Step::Events)?;
    let port_fd = port.descriptor(true).map_err(|_| Step::Events)?;
    let mut driver = Driver {
        number,
        link,
        serial,
        tty,
        port,
        port_fd,
        armed: false,
        modem_said: None,
    };
    if driver.serial.role == Role::Blaster {
        driver.say_chain();
    }
    driver.serve()
}

impl Driver {
    /// Wait, move, and again, until the interface goes.
    fn serve(&mut self) -> Result<(), Step> {
        loop {
            if !self.armed {
                self.link
                    .channel
                    .wait_async(
                        &self.port,
                        Signals::READABLE | Signals::PEER_CLOSED,
                        KEY_LINK,
                    )
                    .map_err(|_| Step::Events)?;
                self.armed = true;
            }
            self.wait(tty::POLL_SETTINGS);
            while let Ok(packet) = self.port.wait(Deadline::At(0)) {
                if packet.key == KEY_LINK {
                    self.armed = false;
                }
            }
            self.link.take_messages();
            if self.link.is_gone() {
                ferrix_rt::info!(
                    "usbserial: serial {} unplugged: {} hung up",
                    self.number,
                    self.tty.link()
                );
                return Ok(());
            }
            match self.serial.role {
                Role::Uart => {
                    self.pump_uart();
                    self.say_modem();
                    self.loopback_if_asked();
                }
                Role::Blaster => self.take_commands(),
            }
        }
    }

    /// `ppoll` on the port's descriptor and the master, for at most
    /// `nanos`.
    fn wait(&self, nanos: u64) {
        let mut fds = [
            PollFd {
                fd: self.port_fd,
                events: POLLIN,
                revents: 0,
            },
            PollFd {
                fd: i32::try_from(self.tty.fd()).unwrap_or(-1),
                events: POLLIN,
                revents: 0,
            },
        ];
        let timeout: [isize; 2] = [
            isize::try_from(nanos / 1_000_000_000).unwrap_or(isize::MAX),
            isize::try_from(nanos % 1_000_000_000).unwrap_or(0),
        ];
        // SAFETY: `fds` and `timeout` live for the whole call; no signal
        // mask.
        let _ = unsafe {
            linux::call(
                linux::numbers::PPOLL,
                [
                    fds.as_mut_ptr().addr(),
                    fds.len(),
                    timeout.as_ptr().addr(),
                    0,
                    8,
                    0,
                ],
            )
        };
    }

    /// Settings first, so bytes written after a `tcsetattr` go at the new
    /// rate; then the program's bytes out, and the chip's in.
    fn pump_uart(&mut self) {
        let number = self.number;
        if let Some((termios, hang_up)) = self.tty.changed() {
            let current = self.serial.line.baud;
            let line = termios.line(current);
            if let Err(error) = self.serial.modem(&mut self.link, !hang_up, !hang_up) {
                ferrix_rt::warn!(
                    "usbserial: serial {number}'s DTR and RTS were not set: {error:?}"
                );
            }
            match self.serial.configure(&mut self.link, &line) {
                Ok(()) => ferrix_rt::info!(
                    "usbserial: serial {number} set to {} baud ({} on the chip), {} data bits, parity {:?}, {} stop bits, flow {:?}",
                    line.baud,
                    self.serial.actual_baud(),
                    line.data_bits,
                    line.parity,
                    if line.two_stop_bits { 2 } else { 1 },
                    line.flow
                ),
                Err(error) => ferrix_rt::warn!(
                    "usbserial: serial {number} cannot be set to {} baud: {error:?}; it stays at {current}",
                    line.baud
                ),
            }
        }
        let mut buffer = [0_u8; PUMP_BYTES];
        // Only what the link has room for is read from the terminal: the
        // rest waits in the pty, whose writer the kernel holds back.
        let room = self.link.room().min(PUMP_BYTES);
        if room > 0 {
            let out = self.tty.read(buffer.get_mut(..room).unwrap_or_default());
            if out > 0
                && let Err(error) = self
                    .serial
                    .write(&mut self.link, buffer.get(..out).unwrap_or_default())
            {
                ferrix_rt::warn!("usbserial: serial {number} dropped {out} bytes: {error:?}");
            }
        }
        let mut flags = [0_u8; PUMP_BYTES];
        loop {
            let got = self.serial.read(&mut self.link, &mut buffer, &mut flags);
            if got == 0 {
                break;
            }
            self.tty.write(buffer.get(..got).unwrap_or_default());
        }
        let lost = self.link.take_lost();
        if lost > 0 {
            ferrix_rt::warn!(
                "usbserial: serial {number} lost {lost} bytes to a stalled OUT endpoint"
            );
        }
    }

    /// Say the modem lines when they change, as the chip's status bytes
    /// give them.
    fn say_modem(&mut self) {
        let modem = self.serial.modem;
        if !self.serial.seen() || self.modem_said == Some(modem) {
            return;
        }
        self.modem_said = Some(modem);
        let bit = |mask: u8| u8::from(modem & mask != 0);
        ferrix_rt::info!(
            "usbserial: serial {} modem lines: CTS {} DSR {} RI {} DCD {}",
            self.number,
            bit(ferrix_usb_serial::ftdi::MODEM_CTS),
            bit(ferrix_usb_serial::ftdi::MODEM_DSR),
            bit(ferrix_usb_serial::ftdi::MODEM_RI),
            bit(ferrix_usb_serial::ftdi::MODEM_DCD)
        );
    }

    /// The MPSSE loopback ([`mpsse::loopback`]), when root asks for it with
    /// an empty file `/tmp/usbserial-loopback-<N>` of its own: the
    /// diagnostic that shows a chip's pipes carry bytes both ways with
    /// nothing wired to its pins. The file is taken away first, so it runs
    /// once a file.
    fn loopback_if_asked(&mut self) {
        let mut path = [0_u8; 48];
        let mut at = 0;
        let mut put = |text: &[u8]| {
            for &byte in text {
                if let Some(slot) = path.get_mut(at) {
                    *slot = byte;
                    at += 1;
                }
            }
        };
        put(b"/tmp/usbserial-loopback-");
        let mut digits = [0_u8; 3];
        let mut number = self.number;
        let mut count = 0;
        loop {
            if let Some(slot) = digits.get_mut(count) {
                *slot = b'0' + number % 10;
            }
            count += 1;
            number /= 10;
            if number == 0 {
                break;
            }
        }
        for index in (0..count).rev() {
            put(&[digits.get(index).copied().unwrap_or(b'0')]);
        }
        if !owned_by_root(&path) {
            return;
        }
        let _ = linux::unlink(&path);
        let pattern: [u8; mpsse::MAX_PATTERN] =
            core::array::from_fn(|at| (at as u32).wrapping_mul(2_654_435_761).to_le_bytes()[3]);
        let number = self.number;
        match mpsse::loopback(&mut self.link, &mut self.serial, &pattern) {
            Ok(Outcome::Equal) => ferrix_rt::info!(
                "usbserial: serial {number} loopback through the chip's MPSSE: {} bytes back as sent",
                pattern.len()
            ),
            Ok(Outcome::Differs(at)) => ferrix_rt::warn!(
                "usbserial: serial {number} loopback through the chip's MPSSE: byte {at} came back otherwise"
            ),
            Ok(Outcome::NoEngine) => ferrix_rt::warn!(
                "usbserial: serial {number} loopback: the chip's MPSSE did not answer"
            ),
            Err(error) => ferrix_rt::warn!("usbserial: serial {number} loopback failed: {error:?}"),
        }
    }

    /// Read the chain behind the cable and say it.
    fn say_chain(&mut self) {
        let number = self.number;
        let mut taps = [Tap::Bypass; MAX_TAPS];
        let read = match Cable::new(&mut self.link, &self.serial) {
            Some(mut cable) => Jtag::new(&mut cable).idcodes(&mut taps),
            None => return,
        };
        match read {
            Ok(count) => {
                ferrix_rt::info!("usbserial: jtag {number}: {count} taps");
                for (tap, found) in taps.iter().take(count).enumerate() {
                    match found {
                        Tap::Idcode(idcode) => {
                            let (version, part, maker) = blaster::fields(*idcode);
                            ferrix_rt::info!(
                                "usbserial: jtag {number} tap {tap}: idcode {idcode:#010x} (version {version}, part {part:#06x}, maker {maker:#05x})"
                            );
                        }
                        Tap::Bypass => {
                            ferrix_rt::info!(
                                "usbserial: jtag {number} tap {tap}: bypass, no idcode"
                            );
                        }
                    }
                }
            }
            Err(error) => {
                ferrix_rt::warn!("usbserial: jtag {number}: the chain did not read: {error:?}");
            }
        }
    }

    /// Run every whole line a program wrote to the cable's terminal.
    fn take_commands(&mut self) {
        let mut buffer = [0_u8; LINE_BYTES];
        let got = self.tty.read(&mut buffer);
        if got == 0 {
            return;
        }
        // The terminal is canonical: a read is a line, or the start of one
        // too long for the buffer, which is refused as it is.
        for line in buffer
            .get(..got)
            .unwrap_or_default()
            .split(|&byte| byte == b'\n' || byte == b'\r')
        {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let answer = self.run_command(line);
            self.tty
                .write(answer.bytes.get(..answer.len).unwrap_or_default());
        }
    }

    fn run_command(&mut self, line: &[u8]) -> Answer {
        let mut answer = Answer {
            bytes: [0; ANSWER_BYTES],
            len: 0,
        };
        let command = match Command::parse(line) {
            Ok(command) => command,
            Err(refused) => {
                let _ = writeln!(answer, "! {}", refused.words());
                return answer;
            }
        };
        let Some(mut cable) = Cable::new(&mut self.link, &self.serial) else {
            let _ = writeln!(answer, "! not a cable");
            return answer;
        };
        let mut jtag = Jtag::new(&mut cable);
        let _ = match command {
            Command::Reset => match jtag.reset() {
                Ok(()) => writeln!(answer, "= reset"),
                Err(error) => writeln!(answer, "! {error:?}"),
            },
            Command::Idcode => {
                let mut taps = [Tap::Bypass; MAX_TAPS];
                match jtag.idcodes(&mut taps) {
                    Ok(count) => {
                        let _ = write!(answer, "= {count}");
                        for tap in taps.iter().take(count) {
                            let _ = match tap {
                                Tap::Idcode(idcode) => write!(answer, " {idcode:#010x}"),
                                Tap::Bypass => write!(answer, " bypass"),
                            };
                        }
                        writeln!(answer)
                    }
                    Err(error) => writeln!(answer, "! {error:?}"),
                }
            }
            Command::Scan { ir, bits, tdi } => {
                let mut tdo = [0_u8; SCAN_BYTES];
                match jtag.scan(ir, bits, &tdi, &mut tdo) {
                    Ok(()) => {
                        let _ = write!(answer, "= ");
                        let _ = blaster::write_hex(&mut answer, &tdo, bits);
                        writeln!(answer)
                    }
                    Err(error) => writeln!(answer, "! {error:?}"),
                }
            }
        };
        answer
    }
}
