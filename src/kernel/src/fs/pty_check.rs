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

    // (C5) A TCSETS naming BOTHER with no TCSETS2 before it: the speed the
    // pair has, as Linux keeps c_ospeed, never 0.
    let fresh = self::pair()?;
    let coded = Termios {
        cflag: (Termios::DEFAULT.cflag & !(CBAUD | (CBAUD << IBSHIFT))) | B9600,
        ..Termios::DEFAULT
    };
    for set in [coded, base] {
        stage(process, page, &set.to_bytes())?;
        answered(
            tty::slave_ioctl(process, &fresh.slave, TCSETS, page),
            "TCSETS on a pseudoterminal was refused",
        )?;
    }
    stage(process, page, &[0_u8; TERMIOS2_BYTES])?;
    answered(
        tty::slave_ioctl(process, &fresh.slave, TCGETS2, page),
        "TCGETS2 on a pseudoterminal was refused",
    )?;
    let got: [u8; TERMIOS2_BYTES] = read_back(process, page)?;
    if reported(&got) != (base.cflag, 9600, 9600) {
        return Err("a TCSETS naming BOTHER after B9600 did not keep 9600");
    }
    Ok(())
}

/// Raw settings: no canonical lines, no echo, no signal characters, no
/// input mapping, no flow control, `VMIN` 0.
fn raw() -> Termios {
    use ferrix_linux_abi::types::{ECHO, ICANON, ICRNL, IEXTEN, ISIG, VMIN};
    let mut termios = Termios {
        iflag: Termios::DEFAULT.iflag & !(ICRNL | IXON),
        lflag: Termios::DEFAULT.lflag & !(ICANON | ECHO | ISIG | IEXTEN),
        ..Termios::DEFAULT
    };
    if let Some(slot) = termios.cc.get_mut(VMIN) {
        *slot = 0;
    }
    termios
}

/// `c_iflag` bits the flow-control cases set.
const IXON: u32 = 0o2000;
const IXANY: u32 = 0o4000;
const IXOFF: u32 = 0o10_000;
/// The stop and start characters, as `Termios::DEFAULT` has them.
const STOP: u8 = 0x13;
const START: u8 = 0x11;
/// The discipline's queue and longest line (`fs::terminal`).
const QUEUE: usize = 4096;
const LINE: usize = 4095;

/// **serial-2** (K1): a master write takes only what the line discipline
/// has room for. 64 KiB written non-blocking through a master to a slave
/// read a few hundred bytes at a time arrive whole and in order, 0 bytes
/// lost; the first write's count is exactly the queue's room, the next is
/// `EAGAIN`, the master polls not writable until a read makes room and
/// writable after it; a blocking write with no room and its slave gone ends
/// with `EIO`.
pub(crate) fn check_a_master_write_takes_only_what_fits() -> Result<(), &'static str> {
    use ferrix_vfs::Inode as _;

    let pair = pair()?;
    pair.master.pty.set_termios(raw(), true);
    let sent: alloc::vec::Vec<u8> = (0..64 * 1024_u32)
        .map(|at| (at.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let mut got = alloc::vec::Vec::with_capacity(sent.len());
    let mut written = 0;
    let mut first = true;
    let mut buf = [0_u8; 300];
    while got.len() < sent.len() {
        match pair
            .master
            .write_stream(sent.get(written..).unwrap_or_default(), true)
        {
            Ok(taken) => {
                if first && taken != QUEUE {
                    return Err(
                        "a master write took more than the line discipline had room for: bytes lost",
                    );
                }
                first = false;
                written += taken;
            }
            Err(Errno::EAGAIN) => {
                if pair.master.poll().writable {
                    return Err("a master with no room for input polled writable");
                }
            }
            Err(_) => return Err("a non-blocking master write failed"),
        }
        if written < sent.len()
            && pair
                .master
                .write_stream(sent.get(written..).unwrap_or_default(), true)
                != Err(Errno::EAGAIN)
        {
            return Err("a full master write was not EAGAIN: bytes lost");
        }
        match pair.slave.read_stream(&mut buf, true) {
            Ok(n) => got.extend_from_slice(buf.get(..n).unwrap_or_default()),
            Err(_) => return Err("a slave read of a full queue failed"),
        }
        if written < sent.len() && !pair.master.poll().writable {
            return Err("a master did not poll writable once a read made room");
        }
    }
    if got != sent {
        return Err(
            "64 KiB through a pseudoterminal did not arrive whole and in order: bytes lost",
        );
    }

    // Full again, and the slave gone: a blocking write does not wait.
    let filler = [b'x'; QUEUE];
    let _ = pair.master.write_stream(&filler, true);
    let Pair { master, slave } = pair;
    drop(slave);
    if master.write_stream(b"y", false) != Err(Errno::EIO) {
        return Err("a blocking master write with no room and no slave did not end EIO");
    }
    Ok(())
}

/// **serial-3** (K1): `IXON`, `IXANY` and `IXOFF` act on a pseudoterminal.
/// The stop character from the master stops the slave's output -- a
/// non-blocking slave write is `EAGAIN` -- and goes no further; the start
/// character starts it; with `IXANY` any character does. With `IXOFF` the
/// stop character goes to the master once the input has less than 128
/// bytes of room left, and the start character once a read has left fewer
/// than 128 held.
pub(crate) fn check_flow_control_acts_on_a_pty() -> Result<(), &'static str> {
    use ferrix_vfs::Inode as _;

    let pair = pair()?;
    let mut buf = [0_u8; 64];
    let mut flow = raw();
    flow.iflag |= IXON;
    pair.master.pty.set_termios(flow, true);
    let _ = pair.master.write_stream(&[STOP], false);
    if pair.slave.write_stream(b"held", true) != Err(Errno::EAGAIN) {
        return Err("IXON's stop character from the master did not stop the slave's output");
    }
    if pair.slave.read_stream(&mut buf, true) != Ok(0) {
        return Err("IXON's stop character reached the slave's input");
    }
    let _ = pair.master.write_stream(&[START], false);
    if pair.slave.write_stream(b"go", true) != Ok(2) {
        return Err("IXON's start character did not start the slave's output");
    }
    let _ = pair.master.read_stream(&mut buf, true);
    flow.iflag |= IXANY;
    pair.master.pty.set_termios(flow, true);
    let _ = pair.master.write_stream(&[STOP, b'k'], false);
    if pair.slave.write_stream(b"any", true) != Ok(3) {
        return Err("IXANY: a character after the stop did not start the output");
    }
    let _ = pair.master.read_stream(&mut buf, true);

    // IXOFF: the stop character to the master as the input fills.
    let mut throttled = raw();
    throttled.iflag |= IXOFF;
    pair.master.pty.set_termios(throttled, true);
    let _ = pair.slave.read_stream(&mut buf, true);
    let full = [b'f'; QUEUE - 100];
    let _ = pair.master.write_stream(&full, true);
    let mut said = [0_u8; 8];
    if pair.master.read_stream(&mut said, true) != Ok(1) || said.first() != Some(&STOP) {
        return Err("IXOFF did not send the stop character as the input filled");
    }
    // Drain to under 128 held: the start character.
    let mut drain = [0_u8; 512];
    while pair.slave.read_stream(&mut drain, true).unwrap_or(0) > 0 {}
    if pair.master.read_stream(&mut said, true) != Ok(1) || said.first() != Some(&START) {
        return Err("IXOFF did not send the start character as the input emptied");
    }
    Ok(())
}

/// The slave's output, `OUTPUT_LIMIT` bytes as the pair holds them.
const OUTPUT: usize = 8192;

/// Fill a pair's input with `IXOFF` set until it throttles, and take the
/// stop character it sends the master.
fn throttle(pair: &Pair) -> Result<(), &'static str> {
    use ferrix_vfs::Inode as _;
    let _ = pair.master.write_stream(&[b'f'; QUEUE - 100], true);
    let mut said = [0_u8; 8];
    if pair.master.read_stream(&mut said, true) != Ok(1) || said.first() != Some(&STOP) {
        return Err("IXOFF did not send the stop character as the input filled");
    }
    Ok(())
}

/// Whether the master's next read is `IXOFF`'s start character alone.
fn started(pair: &Pair) -> bool {
    use ferrix_vfs::Inode as _;
    let mut said = [0_u8; 8];
    pair.master.read_stream(&mut said, true) == Ok(1) && said.first() == Some(&START)
}

/// **serial-3**, continued (F1, C1, C2): flow control through a flush, a
/// full input and a full output.
///
/// - (F1) A throttled pair's input flushed -- `TCFLSH` `TCIFLUSH`, a
///   `TCSETSF` -- sends the master the start character; so do new settings
///   that drop `IXOFF`. No read is left to do it.
/// - (C1) With no room in the input the master's stop and start characters
///   still act: taken alone, and looked ahead at behind a byte not taken.
/// - (C2) `IXOFF`'s character reaches the master ahead of output the slave
///   wrote before it, and a full output does not keep it back or grow.
pub(crate) fn check_flow_control_through_a_flush_and_full_queues(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    use ferrix_linux_abi::types::{TCFLSH, TCIFLUSH, TCSETSF};
    use ferrix_vfs::Inode as _;

    // (F1) TCFLSH, TCSETSF, and IXOFF dropped.
    let pair = pair()?;
    let mut throttled = raw();
    throttled.iflag |= IXOFF;
    pair.master.pty.set_termios(throttled, true);
    throttle(&pair)?;
    answered(
        tty::slave_ioctl(process, &pair.slave, TCFLSH, TCIFLUSH),
        "TCFLSH TCIFLUSH on a pseudoterminal was refused",
    )?;
    if !started(&pair) {
        return Err("a throttled pseudoterminal's input flushed sent no start character");
    }
    throttle(&pair)?;
    stage(process, page, &throttled.to_bytes())?;
    answered(
        tty::slave_ioctl(process, &pair.slave, TCSETSF, page),
        "TCSETSF on a pseudoterminal was refused",
    )?;
    if !started(&pair) {
        return Err("a throttled pseudoterminal's TCSETSF sent no start character");
    }
    throttle(&pair)?;
    pair.master.pty.set_termios(raw(), false);
    if !started(&pair) {
        return Err("a throttled pseudoterminal's IXOFF dropped sent no start character");
    }

    // (C2) Ahead of the output, and outside its limit.
    pair.master.pty.set_termios(throttled, true);
    if pair.slave.write_stream(b"abc", true) != Ok(3) {
        return Err("a slave write to an empty output was not taken");
    }
    let _ = pair.master.write_stream(&[b'f'; QUEUE - 100], true);
    let mut said = [0_u8; 8];
    if pair.master.read_stream(&mut said, true) != Ok(4)
        || said.get(..4) != Some(&[STOP, b'a', b'b', b'c'][..])
    {
        return Err("IXOFF's stop character did not go ahead of the slave's output");
    }
    pair.master.pty.set_termios(throttled, true);
    let mut filled = 0;
    while let Ok(n @ 1..) = pair.slave.write_stream(&[b'o'; 512], true) {
        filled += n;
    }
    if filled != OUTPUT {
        return Err("a slave's output did not take OUTPUT_LIMIT bytes");
    }
    let _ = pair.master.write_stream(&[b'f'; QUEUE - 100], true);
    if pair.master.pty.master_available() != OUTPUT + 1 {
        return Err("IXOFF's stop character was held back or counted in a full output");
    }
    let mut one = [0_u8; 1];
    if pair.master.read_stream(&mut one, true) != Ok(1) || one != [STOP] {
        return Err("IXOFF's stop character did not go ahead of a full output");
    }
    let mut drain = [0_u8; 512];
    let mut rest = 0;
    while let Ok(n @ 1..) = pair.master.read_stream(&mut drain, true) {
        if drain
            .get(..n)
            .is_none_or(|got| got.iter().any(|&byte| byte != b'o'))
        {
            return Err("a full output did not read back as written");
        }
        rest += n;
    }
    if rest != OUTPUT {
        return Err("a full output did not read back as written");
    }

    // (C1) No room: the two characters act all the same.
    let pair = self::pair()?;
    let mut flow = raw();
    flow.iflag |= IXON;
    pair.master.pty.set_termios(flow, true);
    if pair.master.write_stream(&[b'q'; QUEUE], true) != Ok(QUEUE) {
        return Err("an empty input did not take its queue's worth");
    }
    if pair.master.write_stream(&[STOP], true) != Ok(1)
        || pair.slave.write_stream(b"x", true) != Err(Errno::EAGAIN)
    {
        return Err("IXON's stop character did not act on a full input");
    }
    if pair.master.write_stream(&[START], true) != Ok(1)
        || pair.slave.write_stream(b"x", true) != Ok(1)
    {
        return Err("IXON's start character did not act on a full input");
    }
    if pair.master.write_stream(&[b'q', STOP], true) != Err(Errno::EAGAIN)
        || pair.slave.write_stream(b"x", true) != Err(Errno::EAGAIN)
    {
        return Err("IXON's stop character behind a byte not taken was not looked ahead at");
    }
    let _ = pair.master.write_stream(&[START], true);
    if pair.slave.write_stream(b"x", true) != Ok(1) {
        return Err("IXON's start character did not act on a full input");
    }
    Ok(())
}

/// **serial-4** (K1): a full canonical line with no terminator does not
/// stop the master. More bytes are taken -- the write is not `EAGAIN` --
/// and dropped, and a newline still ends the line, which reads whole.
pub(crate) fn check_a_full_canonical_line_does_not_block_the_master() -> Result<(), &'static str> {
    use ferrix_linux_abi::types::{ECHO, ICRNL};
    use ferrix_vfs::Inode as _;

    let pair = pair()?;
    let canonical = Termios {
        iflag: Termios::DEFAULT.iflag & !(ICRNL | IXON),
        lflag: Termios::DEFAULT.lflag & !ECHO,
        ..Termios::DEFAULT
    };
    pair.master.pty.set_termios(canonical, true);
    let line = [b'a'; LINE];
    if pair.master.write_stream(&line, true) != Ok(LINE) {
        return Err("a canonical line of MAX_CANON bytes was not taken whole");
    }
    for _ in 0..10 {
        if pair.master.write_stream(b"b", true) != Ok(1) {
            return Err("a full canonical line made the master wait");
        }
    }
    if pair.master.write_stream(b"\n", true) != Ok(1) {
        return Err("a full canonical line refused its terminator");
    }
    let mut read = alloc::vec![0_u8; QUEUE];
    let got = pair
        .slave
        .read_stream(&mut read, true)
        .map_err(|_| "the full canonical line did not read")?;
    if got != LINE + 1
        || read
            .get(..LINE)
            .is_none_or(|bytes| bytes.iter().any(|&byte| byte != b'a'))
        || read.get(LINE) != Some(&b'\n')
    {
        return Err("a full canonical line did not read as its MAX_CANON bytes and its newline");
    }
    Ok(())
}

/// **serial-5** (K6): the master's last close hangs the slave up. The
/// leader of the session whose controlling terminal it was gets `SIGHUP`
/// (and `SIGCONT`) -- the check's leader, with no handler, ends by it --
/// the session loses the terminal, the slave reads end of
/// file, writes `EIO`, and polls hung up.
pub(crate) fn check_a_masters_close_hangs_its_slave_up() -> Result<(), &'static str> {
    use crate::syscall::process;
    use crate::syscall::signal::bit;
    use ferrix_linux_abi::types::SIGHUP;
    use ferrix_vfs::Inode as _;

    let leader = process::new_for_check().map_err(|_| "could not make a session leader")?;
    let _ = crate::syscall::family::sys_setsid(&leader);
    let sid = leader.sid();
    if sid == 0 || sid != leader.pid() {
        process::kill(&leader, 0);
        return Err("a check's process could not lead a session");
    }
    let Pair { master, slave } = pair()?;
    master.pty.set_session(sid, sid);
    drop(master);
    // A leader that takes SIGHUP's default action ends by it at once, as a
    // shell with no handler does; one that catches or blocks it has it
    // pending.
    let hung_up = leader.is_terminated()
        || leader.with_signals(|signals| signals.pending()) & bit(SIGHUP) != 0;
    let mut buf = [0_u8; 8];
    let outcome = if !hung_up {
        Err("the hang-up sent the session's leader no SIGHUP")
    } else if slave.pty.session() != 0 {
        Err("a hung-up pseudoterminal was still the session's terminal")
    } else if slave.write_stream(b"x", false) != Err(Errno::EIO) {
        Err("a hung-up slave's write was not EIO")
    } else if slave.read_stream(&mut buf, false) != Ok(0) {
        Err("a hung-up slave's read was not end of file")
    } else if !slave.poll().hangup {
        Err("a hung-up slave did not poll hung up")
    } else {
        Ok(())
    };
    process::kill(&leader, 0);
    outcome
}
