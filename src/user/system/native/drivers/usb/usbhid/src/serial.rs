//! The serial adapters on the bus, each served through its terminal
//! ([`crate::tty`]): bytes both ways and the settings for a UART, command
//! lines for a USB-Blaster.

use core::fmt::Write as _;

use ferrix_rt::linux;
use ferrix_usb_host::blaster::{self, Command, Jtag, MAX_TAPS, SCAN_BYTES, Tap};
use ferrix_usb_host::bus::SerialChange;
use ferrix_usb_host::ftdi::Role;
use ferrix_usb_host::host::Host as Controller;

use crate::tty::{Kind, Tty};
use crate::{Driver, Time};

/// What one look moves each way at most: a bulk transfer's worth.
const PUMP_BYTES: usize = 4096;
/// The longest command line kept.
const LINE_BYTES: usize = 256;
/// The longest answer line.
const ANSWER_BYTES: usize = 192;

/// `POLLIN`.
const POLLIN: i16 = 0x001;

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

impl Answer {
    const fn new() -> Answer {
        Answer {
            bytes: [0; ANSWER_BYTES],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or_default()
    }
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

impl<H: Controller<Clock = Time>> Driver<H> {
    /// Open and close terminals as adapters come and go, and move what
    /// waits each way. Answers whether anything moved.
    pub(crate) fn serve_serials(&mut self) -> bool {
        let mut moved = false;
        while let Some(change) = self.bus.next_serial_change() {
            moved = true;
            match change {
                SerialChange::Attached(index) => self.attach_serial(index),
                SerialChange::Detached(index) => {
                    if let Some(slot) = self.ttys.get_mut(index)
                        && let Some(tty) = slot.take()
                    {
                        ferrix_rt::info!(
                            "usbhid: serial {index} unplugged: {} hung up",
                            tty.link()
                        );
                    }
                }
            }
        }
        for index in 0..self.ttys.len() {
            let role = self.bus.serial(index).map(|serial| serial.role);
            moved |= match role {
                Some(Role::Uart) => self.pump_uart(index),
                Some(Role::Blaster) => self.take_commands(index),
                None => false,
            };
        }
        moved
    }

    fn attach_serial(&mut self, index: usize) {
        let Some(serial) = self.bus.serial(index).copied() else {
            return;
        };
        let kind = match serial.role {
            Role::Uart => Kind::Uart,
            Role::Blaster => Kind::Jtag,
        };
        let Some(tty) = Tty::open(index, kind) else {
            ferrix_rt::err!(
                "usbhid: serial {index}, a {} {}, has no terminal: /dev/ptmx would not open",
                serial.chip.word(),
                serial.role.word()
            );
            return;
        };
        ferrix_rt::info!(
            "usbhid: serial {index} {:04x}:{:04x} ({} {}, {}-byte packets) is {} (/dev/pts/{})",
            serial.vendor,
            serial.product,
            serial.chip.word(),
            serial.role.word(),
            serial.max_packet,
            tty.link(),
            tty.number
        );
        if let Some(slot) = self.ttys.get_mut(index) {
            *slot = Some(tty);
        }
        if serial.role == Role::Blaster {
            self.say_chain(index);
        }
    }

    // -----------------------------------------------------------------------
    // A UART
    // -----------------------------------------------------------------------

    /// Settings first, so bytes written after a `tcsetattr` go at the new
    /// rate; then the program's bytes out, and the chip's in.
    fn pump_uart(&mut self, index: usize) -> bool {
        let Some(Some(tty)) = self.ttys.get_mut(index) else {
            return false;
        };
        if let Some((termios, hang_up)) = tty.changed() {
            let current = self
                .bus
                .serial(index)
                .map_or(9600, |serial| serial.line.baud);
            let line = termios.line(current);
            if let Err(error) = self.bus.serial_modem(index, !hang_up, !hang_up) {
                ferrix_rt::warn!("usbhid: serial {index}'s DTR and RTS were not set: {error:?}");
            }
            match self.bus.serial_configure(index, &line) {
                Ok(()) => ferrix_rt::info!(
                    "usbhid: serial {index} set to {} baud, {} data bits, parity {:?}, {} stop bits, flow {:?}",
                    line.baud,
                    line.data_bits,
                    line.parity,
                    if line.two_stop_bits { 2 } else { 1 },
                    line.flow
                ),
                Err(error) => ferrix_rt::warn!(
                    "usbhid: serial {index} cannot be set to {} baud: {error:?}; it stays at {current}",
                    line.baud
                ),
            }
        }
        let Some(Some(tty)) = self.ttys.get(index) else {
            return false;
        };
        let mut moved = false;
        let mut buffer = [0_u8; PUMP_BYTES];
        // Only what the OUT stream has room for is read from the terminal:
        // the rest waits in the pty, whose writer the kernel holds back.
        let room = self.bus.serial_room(index).min(PUMP_BYTES);
        let out = if room == 0 {
            0
        } else {
            tty.read(buffer.get_mut(..room).unwrap_or_default())
        };
        if out > 0 {
            moved = true;
            if let Err(error) = self
                .bus
                .serial_write(index, buffer.get(..out).unwrap_or_default())
            {
                ferrix_rt::warn!("usbhid: serial {index} dropped {out} bytes: {error:?}");
            }
        }
        let got = self.bus.serial_read(index, &mut buffer);
        if got > 0 {
            moved = true;
            if let Some(Some(tty)) = self.ttys.get(index) {
                tty.write(buffer.get(..got).unwrap_or_default());
            }
        }
        let errors = self.bus.take_serial_errors(index);
        if errors != 0 {
            ferrix_rt::warn!("usbhid: serial {index} reported line errors {errors:#04x}");
        }
        moved
    }

    // -----------------------------------------------------------------------
    // A USB-Blaster
    // -----------------------------------------------------------------------

    /// Read the chain behind cable `index` and say it.
    fn say_chain(&mut self, index: usize) {
        let mut taps = [Tap::Bypass; MAX_TAPS];
        let read = match self.bus.blaster(index) {
            Some(mut cable) => Jtag::new(&mut cable).idcodes(&mut taps),
            None => return,
        };
        match read {
            Ok(count) => {
                ferrix_rt::info!("usbhid: jtag {index}: {count} taps");
                for (tap, found) in taps.iter().take(count).enumerate() {
                    match found {
                        Tap::Idcode(idcode) => {
                            let (version, part, maker) = blaster::fields(*idcode);
                            ferrix_rt::info!(
                                "usbhid: jtag {index} tap {tap}: idcode {idcode:#010x} (version {version}, part {part:#06x}, maker {maker:#05x})"
                            );
                        }
                        Tap::Bypass => {
                            ferrix_rt::info!("usbhid: jtag {index} tap {tap}: bypass, no idcode");
                        }
                    }
                }
            }
            Err(error) => {
                ferrix_rt::warn!("usbhid: jtag {index}: the chain did not read: {error:?}");
            }
        }
    }

    /// Run every whole line a program wrote to cable `index`'s terminal.
    fn take_commands(&mut self, index: usize) -> bool {
        let Some(Some(tty)) = self.ttys.get(index) else {
            return false;
        };
        let mut buffer = [0_u8; LINE_BYTES];
        let got = tty.read(&mut buffer);
        if got == 0 {
            return false;
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
            let answer = self.run_command(index, line);
            if let Some(Some(tty)) = self.ttys.get(index) {
                tty.write(answer.as_bytes());
            }
        }
        true
    }

    fn run_command(&mut self, index: usize, line: &[u8]) -> Answer {
        let mut answer = Answer::new();
        let command = match Command::parse(line) {
            Ok(command) => command,
            Err(refused) => {
                let _ = writeln!(answer, "! {}", refused.words());
                return answer;
            }
        };
        let Some(mut cable) = self.bus.blaster(index) else {
            let _ = writeln!(answer, "! the cable is gone");
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

    // -----------------------------------------------------------------------
    // The wait
    // -----------------------------------------------------------------------

    /// Wait until `deadline` for the port or a terminal: whether a terminal
    /// woke it and the port did not, so the port need not be waited on.
    pub(crate) fn wait_for_terminals(&mut self, deadline: u64) -> bool {
        let Some(port_fd) = self.port_fd else {
            return false;
        };
        let mut fds = [PollFd {
            fd: -1,
            events: POLLIN,
            revents: 0,
        }; 1 + crate::MAX_SERIALS];
        if let Some(first) = fds.first_mut() {
            first.fd = port_fd;
        }
        for (slot, tty) in fds.iter_mut().skip(1).zip(self.ttys.iter()) {
            if let Some(tty) = tty {
                slot.fd = i32::try_from(tty.fd()).unwrap_or(-1);
            }
        }
        let now = linux::monotonic_nanos().unwrap_or(0);
        let left = deadline.saturating_sub(now);
        let timeout: [isize; 2] = [
            isize::try_from(left / 1_000_000_000).unwrap_or(isize::MAX),
            isize::try_from(left % 1_000_000_000).unwrap_or(0),
        ];
        // SAFETY: `fds` and `timeout` live for the whole call; no signal
        // mask. A descriptor of -1 is ignored, as poll does.
        let polled = unsafe {
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
        let port_ready = fds.first().is_some_and(|fd| fd.revents != 0);
        let tty_ready = fds.iter().skip(1).any(|fd| fd.revents != 0);
        polled.is_ok() && tty_ready && !port_ready
    }
}
