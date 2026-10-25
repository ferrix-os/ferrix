//! The self-checks of a pseudoterminal as a serial port's terminal: what a
//! ring-3 serial driver holding the master needs of it, and what programs
//! on the slave see (`docs/USB-SERIAL.md` §5, the consultant's ledger
//! 4317). Each check is a load statement's (`serial-N`), named there.
//!
//! They run from `syscall::check::check_handlers`, with a process of their
//! own and a page of its memory for the structures an `ioctl` reads and
//! writes, and every request goes in through `syscall::tty`'s handlers as a
//! program's does.

use ferrix_linux_abi::types::{
    BOTHER, CBAUD, IBSHIFT, TCGETS2, TCSETS, TCSETS2, TERMIOS_BYTES, TERMIOS2_BYTES,
};
use ferrix_vfs::Errno;

use crate::fs::pty::{self, MasterFile, SlaveFile};
use crate::fs::terminal::Termios;
use crate::syscall::process::Process;
use crate::syscall::{tty, uaccess};

/// `B9600`.
const B9600: u32 = 0x000D;

/// A pair made for a check: its master, and its slave opened.
struct Pair {
    master: alloc::sync::Arc<MasterFile>,
    slave: alloc::sync::Arc<SlaveFile>,
}

fn pair() -> Result<Pair, &'static str> {
    let master = pty::open_master().map_err(|_| "no pseudoterminal pair to check")?;
    pty::set_locked(&master.pty, false);
    let slave = pty::open_slave(master.pty.number)
        .map_err(|_| "the check's pseudoterminal slave did not open")?;
    Ok(Pair { master, slave })
}

/// Stage `bytes` at `at` in the check's memory.
fn stage(process: &Process, at: u64, bytes: &[u8]) -> Result<(), &'static str> {
    uaccess::copy_to_user(process.space(), at, bytes).map_err(|_| "could not stage a structure")
}

/// Read `N` bytes back from `at`.
fn read_back<const N: usize>(process: &Process, at: u64) -> Result<[u8; N], &'static str> {
    let mut bytes = [0_u8; N];
    uaccess::copy_from_user(process.space(), at, &mut bytes)
        .map_err(|_| "could not read a structure back")?;
    Ok(bytes)
}

fn answered(got: Result<usize, Errno>, what: &'static str) -> Result<(), &'static str> {
    match got {
        Ok(_) => Ok(()),
        Err(_) => Err(what),
    }
}

/// A `struct termios2` of `termios` with these speeds.
fn termios2(termios: Termios, input: u32, output: u32) -> [u8; TERMIOS2_BYTES] {
    let mut bytes = [0_u8; TERMIOS2_BYTES];
    let all = termios
        .to_bytes()
        .into_iter()
        .chain(input.to_le_bytes())
        .chain(output.to_le_bytes());
    for (slot, byte) in bytes.iter_mut().zip(all) {
        *slot = byte;
    }
    bytes
}

/// What `TCGETS2` reported: `c_cflag`, the input and the output speeds.
fn reported(bytes: &[u8; TERMIOS2_BYTES]) -> (u32, u32, u32) {
    let word = |at: usize| {
        let mut four = [0_u8; 4];
        if let Some(slice) = bytes.get(at..at + 4) {
            four.copy_from_slice(slice);
        }
        u32::from_le_bytes(four)
    };
    (word(8), word(TERMIOS_BYTES), word(TERMIOS_BYTES + 4))
}

/// **serial-1** (K3): a pseudoterminal keeps its speeds as numbers.
/// `TCSETS2` at `BOTHER` 250000 reads back from `TCGETS2` as 250000 both
/// ways with `c_cflag` still `BOTHER`, from the master as from the slave; a
/// `TCSETS` naming `BOTHER` keeps the numbers; split speeds stay split; and
/// a `B` code is that code's speed, as before. The console's own speed is
/// `check_the_console_answers_as_a_terminal`'s, unchanged.
pub(crate) fn check_a_pty_keeps_its_speeds_as_numbers(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    let pair = pair()?;
    let base = Termios {
        cflag: (Termios::DEFAULT.cflag & !(CBAUD | (CBAUD << IBSHIFT))) | BOTHER,
        ..Termios::DEFAULT
    };
    let get = |slave: bool| -> Result<[u8; TERMIOS2_BYTES], &'static str> {
        let got = if slave {
            tty::slave_ioctl(process, &pair.slave, TCGETS2, page)
        } else {
            tty::master_ioctl(process, &pair.master, TCGETS2, page)
        };
        answered(got, "TCGETS2 on a pseudoterminal was refused")?;
        read_back(process, page)
    };

    stage(process, page, &termios2(base, 250_000, 250_000))?;
    answered(
        tty::slave_ioctl(process, &pair.slave, TCSETS2, page),
        "TCSETS2 on a pseudoterminal's slave was refused",
    )?;
    for slave in [true, false] {
        let (cflag, input, output) = reported(&get(slave)?);
        if cflag & CBAUD != BOTHER || (input, output) != (250_000, 250_000) {
            return Err("a pseudoterminal set to BOTHER 250000 did not read back 250000 both ways");
        }
    }

    // TCSETS has no speeds: BOTHER keeps the numbers there are.
    stage(process, page, &base.to_bytes())?;
    answered(
        tty::slave_ioctl(process, &pair.slave, TCSETS, page),
        "TCSETS on a pseudoterminal was refused",
    )?;
    if reported(&get(true)?) != (base.cflag, 250_000, 250_000) {
        return Err("a TCSETS naming BOTHER lost the pseudoterminal's numeric speeds");
    }

    // Split: the input speed BOTHER too, at 1200.
    let split = Termios {
        cflag: base.cflag | (BOTHER << IBSHIFT),
        ..base
    };
    stage(process, page, &termios2(split, 1200, 250_000))?;
    answered(
        tty::slave_ioctl(process, &pair.slave, TCSETS2, page),
        "TCSETS2 with split speeds was refused",
    )?;
    let (_, input, output) = reported(&get(true)?);
    if (input, output) != (1200, 250_000) {
        return Err("a pseudoterminal's split numeric speeds did not stay split");
    }

    // A B code is its own speed, whatever number came beside it.
    let coded = Termios {
        cflag: (base.cflag & !(CBAUD | (CBAUD << IBSHIFT))) | B9600,
        ..base
    };
    stage(process, page, &termios2(coded, 4321, 4321))?;
    answered(
        tty::slave_ioctl(process, &pair.slave, TCSETS2, page),
        "TCSETS2 with a B code was refused",
    )?;
    let (cflag, input, output) = reported(&get(true)?);
    if cflag & CBAUD != B9600 || (input, output) != (9600, 9600) {
        return Err("a pseudoterminal set to B9600 did not read back 9600");
    }
    Ok(())
}
