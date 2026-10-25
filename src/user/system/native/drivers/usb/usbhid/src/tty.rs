//! A serial adapter's terminal: a pseudoterminal this process holds the
//! master of, its slave named by a link in `/tmp` (`docs/USB-SERIAL.md` §3).
//!
//! `/dev` is a fixed table nothing can be added to (`src/kernel/src/fs/
//! devfs.rs`), and the kernel forwards no character device of a ring-3
//! driver but NVIDIA's (`ferrix_chardevctl::node`). A pseudoterminal is the
//! terminal the kernel already gives a program, line discipline, `termios`
//! and hang-up included, so the slave is a serial port to every program
//! that opens it, and the master is where the driver stands:
//!
//! * what the chip receives is written to the master, which is what a
//!   terminal's input is;
//! * what a program writes to the slave is read from the master and sent;
//! * the settings a program makes on the slave are read back from the
//!   master (`TCGETS2`), before every write and every [`POLL_SETTINGS`],
//!   and sent to the chip when they change, `B0` as a hang-up;
//! * unplugged, the master is closed, and the slave reads end of file and
//!   writes `EIO`, as a Linux `ttyUSB` hung up does.
//!
//! The link is `/tmp/ttyUSB<N>` for a UART and `/tmp/jtag<N>` for a
//! USB-Blaster, `N` the bus's index.

use core::fmt::Write as _;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{
    AT_FDCWD, CBAUD, ECHO, ECHOCTL, ECHOE, ECHOK, ECHOKE, O_CLOEXEC, O_NOCTTY, O_NONBLOCK,
    O_RDWR, OPOST, TCGETS2, TCSETS2, TERMIOS2_BYTES, TIOCGPTN, TIOCSPTLCK,
};
use ferrix_rt::linux;
use ferrix_usb_host::ftdi::Termios;

/// How often a terminal's settings are looked at when nothing else wakes
/// the driver: a program's `stty` reaches the chip within this.
pub(crate) const POLL_SETTINGS: u64 = 50 * 1_000_000;

/// `B9600`, the speed a terminal starts at, as Linux's `ttyUSB` does.
const B9600: u32 = 0x000D;
/// `c_cc` indices of the start and stop characters.
const VSTART: usize = 8;
const VSTOP: usize = 9;
/// Where the words lie in `struct termios2`.
const IFLAG: usize = 0;
const OFLAG: usize = 4;
const CFLAG: usize = 8;
const LFLAG: usize = 12;
const CC: usize = 17;
const ISPEED: usize = 36;
const OSPEED: usize = 40;

/// The longest link path, with its NUL.
const LINK_BYTES: usize = 24;

/// A `struct termios2`'s bytes.
pub(crate) type Termios2 = [u8; TERMIOS2_BYTES];

fn word(termios: &Termios2, at: usize) -> u32 {
    let mut four = [0_u8; 4];
    if let Some(bytes) = termios.get(at..at + 4) {
        four.copy_from_slice(bytes);
    }
    u32::from_le_bytes(four)
}

fn set_word(termios: &mut Termios2, at: usize, value: u32) {
    if let Some(bytes) = termios.get_mut(at..at + 4) {
        bytes.copy_from_slice(&value.to_le_bytes());
    }
}

/// What a terminal is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    /// A serial port.
    Uart,
    /// A USB-Blaster's command lines.
    Jtag,
}

/// The master of a pseudoterminal, and the link to its slave.
pub(crate) struct Tty {
    master: usize,
    link: [u8; LINK_BYTES],
    /// The slave's number, `/dev/pts/<number>`.
    pub(crate) number: u32,
    /// The settings last sent to the chip.
    sent: Termios2,
}

/// A path's bytes, NUL-terminated, in a buffer of its own.
struct Path {
    bytes: [u8; LINK_BYTES],
    len: usize,
}

impl core::fmt::Write for Path {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.len + text.len();
        // One byte stays for the NUL.
        let slot = self
            .bytes
            .get_mut(self.len..end)
            .filter(|_| end < LINK_BYTES)
            .ok_or(core::fmt::Error)?;
        slot.copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

fn path(args: core::fmt::Arguments<'_>) -> Option<Path> {
    let mut path = Path {
        bytes: [0; LINK_BYTES],
        len: 0,
    };
    path.write_fmt(args).ok()?;
    Some(path)
}

fn ioctl(fd: usize, request: u32, argument: usize) -> Result<usize, Errno> {
    // SAFETY: every request here reads or writes the object `argument`
    // points at, which the caller keeps alive and large enough for it.
    unsafe {
        linux::call(
            linux::numbers::IOCTL,
            [fd, request as usize, argument, 0, 0, 0],
        )
    }
}

impl Tty {
    /// A new pseudoterminal for adapter `index`, set up for `kind`, its
    /// slave linked from `/tmp`.
    pub(crate) fn open(index: usize, kind: Kind) -> Option<Tty> {
        let flags = O_RDWR | O_NOCTTY | O_NONBLOCK | O_CLOEXEC;
        // SAFETY: the path is a NUL-terminated literal, read for the call.
        let master = unsafe {
            linux::call(
                linux::numbers::OPENAT,
                [
                    AT_FDCWD as usize,
                    c"/dev/ptmx".as_ptr().addr(),
                    flags as usize,
                    0,
                    0,
                    0,
                ],
            )
        }
        .ok()?;
        let mut tty = Tty {
            master,
            link: [0; LINK_BYTES],
            number: 0,
            sent: [0; TERMIOS2_BYTES],
        };
        let unlock = 0_i32;
        let _ = ioctl(master, TIOCSPTLCK, core::ptr::from_ref(&unlock).addr()).ok()?;
        let mut number = 0_u32;
        let _ = ioctl(master, TIOCGPTN, core::ptr::from_mut(&mut number).addr()).ok()?;
        tty.number = number;
        let mut termios = tty.settings()?;
        // 9600 baud, as Linux opens a ttyUSB.
        let cflag = (word(&termios, CFLAG) & !(CBAUD | (CBAUD << 16))) | B9600;
        set_word(&mut termios, CFLAG, cflag);
        set_word(&mut termios, ISPEED, 9600);
        set_word(&mut termios, OSPEED, 9600);
        if kind == Kind::Jtag {
            // The answers written to the master would be echoed back as
            // commands, and the commands come back with a carriage return
            // that is no part of them.
            let lflag = word(&termios, LFLAG) & !(ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE);
            set_word(&mut termios, LFLAG, lflag);
            let oflag = word(&termios, OFLAG) & !OPOST;
            set_word(&mut termios, OFLAG, oflag);
        }
        let _ = ioctl(master, TCSETS2, termios.as_ptr().addr()).ok()?;
        tty.sent = tty.settings()?;
        let name = match kind {
            Kind::Uart => "ttyUSB",
            Kind::Jtag => "jtag",
        };
        let link = path(format_args!("/tmp/{name}{index}"))?;
        let target = path(format_args!("/dev/pts/{number}"))?;
        let _ = linux::unlink(&link.bytes);
        // SAFETY: both paths are NUL-terminated buffers that live for the
        // call.
        let linked = unsafe {
            linux::call(
                linux::numbers::SYMLINKAT,
                [
                    target.bytes.as_ptr().addr(),
                    AT_FDCWD as usize,
                    link.bytes.as_ptr().addr(),
                    0,
                    0,
                    0,
                ],
            )
        };
        if linked.is_err() {
            ferrix_rt::warn!(
                "usbhid: /tmp/{name}{index} could not be linked to /dev/pts/{number}"
            );
        }
        tty.link = link.bytes;
        Some(tty)
    }

    /// The link's path, without its NUL.
    pub(crate) fn link(&self) -> &str {
        let end = self.link.iter().position(|&byte| byte == 0).unwrap_or(0);
        core::str::from_utf8(self.link.get(..end).unwrap_or_default()).unwrap_or("?")
    }

    /// The master's descriptor, for `ppoll`.
    pub(crate) fn fd(&self) -> usize {
        self.master
    }

    /// The pair's settings as they are.
    fn settings(&self) -> Option<Termios2> {
        let mut termios = [0_u8; TERMIOS2_BYTES];
        let _ = ioctl(self.master, TCGETS2, termios.as_mut_ptr().addr()).ok()?;
        Some(termios)
    }

    /// The settings, if they changed since the last that were taken: the
    /// line they ask for, as [`Termios::line`] reads it, and whether the
    /// speed is `B0`, the hang-up.
    pub(crate) fn changed(&mut self) -> Option<(Termios, bool)> {
        let now = self.settings()?;
        let relevant = |termios: &Termios2| {
            (
                word(termios, IFLAG),
                word(termios, CFLAG),
                word(termios, OSPEED),
                termios.get(CC + VSTART).copied(),
                termios.get(CC + VSTOP).copied(),
            )
        };
        if relevant(&now) == relevant(&self.sent) {
            return None;
        }
        self.sent = now;
        let cflag = word(&now, CFLAG);
        // `TCGETS2` gives the speed as a number whatever `c_cflag` names it
        // by; zero is `B0`.
        let hang_up = cflag & CBAUD == 0;
        let ospeed = word(&now, OSPEED);
        Some((
            Termios {
                iflag: word(&now, IFLAG),
                cflag,
                ospeed,
                start: now.get(CC + VSTART).copied().unwrap_or(0x11),
                stop: now.get(CC + VSTOP).copied().unwrap_or(0x13),
            },
            hang_up,
        ))
    }

    /// What programs wrote to the slave, without waiting: how many bytes.
    pub(crate) fn read(&self, into: &mut [u8]) -> usize {
        linux::read(self.master, into).unwrap_or(0)
    }

    /// Hand `bytes` to the slave's readers, as a terminal's input. The
    /// master takes all of it: what the slave's line discipline has no room
    /// for is dropped there (`docs/USB-SERIAL.md` §4).
    pub(crate) fn write(&self, bytes: &[u8]) {
        let mut at = 0;
        while at < bytes.len() {
            match linux::write(self.master, bytes.get(at..).unwrap_or_default()) {
                Ok(0) | Err(_) => break,
                Ok(written) => at += written,
            }
        }
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        let _ = linux::unlink(&self.link);
        let _ = linux::close(self.master);
    }
}
