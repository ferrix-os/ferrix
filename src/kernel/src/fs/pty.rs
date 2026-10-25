//! Pseudoterminals: `/dev/ptmx`, `/dev/pts/<n>`, and the pair between them.
//!
//! A pseudoterminal is a terminal with a program at each end instead of a
//! screen and a keyboard. The program that opens `/dev/ptmx` -- a terminal
//! emulator, `script`, a session over the network -- holds the *master*; the
//! program it starts opens `/dev/pts/<n>`, the *slave*, and cannot tell it
//! from a serial console. That is what a terminal emulator on Ferrix needs,
//! and `docs/ROADMAP.md` stage 18's exit asks for one.
//!
//! # What goes which way
//!
//! * The master writes what the person typed. Those bytes go through the
//!   line discipline -- the same [`Discipline`] the console's terminal uses,
//!   so `ICANON`, `ECHO`, `ISIG` and the rest behave exactly as they do
//!   there -- and the slave reads what it makes ready, waiting as `VMIN`
//!   and `VTIME` say by the same rule as the console's read. The echo goes
//!   back to the master, because on a pseudoterminal the *terminal* is the
//!   program holding the master, and it is the one that has to draw it.
//! * The slave writes the program's output. `OPOST` and `ONLCR` are applied,
//!   as the console's writes are, and the master reads the result. At most
//!   `OUTPUT_LIMIT` bytes wait for the master; a write that finds no room
//!   waits for the master to read, and returns once all of it is written, as
//!   Linux's `n_tty_write` does. A program writing a whole screen at once --
//!   btop's first frame is tens of kilobytes -- must not be told that only
//!   part of it went: C's `stdio`, and C++'s streams on top of it, take a
//!   write of nothing as an error, and btop's `cout` then dropped every frame
//!   after its first eight kilobytes.
//!
//! # What a pair is, and when it goes
//!
//! Opening `/dev/ptmx` makes a pair and gives back the master. Its number is
//! `TIOCGPTN`'s answer, and `/dev/pts/<number>` is its slave for as long as
//! the master is open. Closing the master takes the pair away: the slave's
//! reads then give end of file and its writes `EIO`, which is what Linux
//! does and what a shell reads as "the terminal has gone". Closing every
//! slave leaves the master readable with nothing to read, as Linux's does
//! until it is closed too.
//!
//! `TIOCSPTLCK` is answered, and its lock honoured: a slave may not be opened
//! while the master has not unlocked it. `openpty` and `posix_openpt` both
//! unlock before they open the slave.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU32, Ordering};

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{
    BOTHER, CBAUD, IBSHIFT, ONLCR, OPOST, SIGWINCH, TERMIOS_BYTES, TERMIOS2_BYTES,
};
use ferrix_vfs::initramfs::makedev;
use ferrix_vfs::{FileType, Inode, Metadata, Readiness, Result as VfsResult, Timespec};

use crate::fs::terminal::{Discipline, ReadStep, ReadTimer, Termios, Winsize};
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::syscall::process;

/// The major number Linux gives the pseudoterminal masters' multiplexer,
/// which is `/dev/ptmx` at 5:2 (`Documentation/admin-guide/devices.txt`).
pub(crate) const PTMX_MAJOR: u32 = 5;
/// `/dev/ptmx`'s minor.
pub(crate) const PTMX_MINOR: u32 = 2;

/// The major number a Unix 98 slave has: `/dev/pts/<n>` is 136:n for the
/// first 256, which is the range this kernel hands out.
pub(crate) const SLAVE_MAJOR: u32 = 136;

/// The most pairs at once, which is the range of `SLAVE_MAJOR`'s minors.
const MAX_PAIRS: u32 = 256;

/// The most bytes a slave's output may hold before a write waits.
///
/// Linux's pseudoterminal buffer is 8 KiB by default; this is the same, and
/// the reason is the same: a program writing to a terminal nobody is reading
/// has to stop somewhere.
const OUTPUT_LIMIT: usize = 8192;

/// Linux's `TTY_THRESHOLD_THROTTLE`: with less room than this left for
/// input, the pair throttles -- `IXOFF`'s stop character goes to the master,
/// and a driver in packet mode hears it (K1, K2).
const THROTTLE_ROOM: usize = 128;
/// Linux's `TTY_THRESHOLD_UNTHROTTLE`: once the slave's reader has left
/// fewer bytes than this held, it unthrottles.
const UNTHROTTLE_HELD: usize = 128;

/// `c_iflag`: `IXON`'s stop and start act on output; any character
/// restarts it; and the input is flow-controlled with the two characters.
const IXON: u32 = 0o2000;
const IXANY: u32 = 0o4000;
const IXOFF: u32 = 0o10_000;
/// `c_cc` indices of the start and stop characters.
const VSTART: usize = 8;
const VSTOP: usize = 9;

/// A slave's inode number is this plus its number.
///
/// A range of its own, above the event nodes' `1 << 41`: two nodes of one
/// file system may not share a number.
pub(crate) const SLAVE_INO_BASE: u64 = 1 << 42;

/// `/dev/pts`'s inode number.
pub(crate) const PTS_INO: u64 = 1 << 38;

/// One pair.
pub(crate) struct Pty {
    /// Its number: `TIOCGPTN`'s answer and the slave's name.
    pub(crate) number: u32,
    /// Whose the slave is: the filesystem user and group of the process
    /// that opened `/dev/ptmx`, as Linux's devpts gives them, so a terminal
    /// a user's program makes is one that user may open. Root's for a pair
    /// the kernel makes for itself.
    owner: (u32, u32),
    state: SpinLock<State>,
    /// Woken when either side has something to read, or an end has gone.
    changed: Arc<WaitQueue>,
}

/// What the two ends share.
#[derive(Debug)]
struct State {
    /// What the master wrote, on its way to the slave.
    discipline: Discipline,
    /// What the slave wrote, on its way to the master.
    output: VecDeque<u8>,
    /// The size the master says the terminal is.
    winsize: Winsize,
    /// The foreground process group, as `TIOCSPGRP` set it.
    foreground: u32,
    /// The session the slave is the controlling terminal of.
    session: u32,
    /// Whether the master is still open.
    master: bool,
    /// How many slaves are open.
    slaves: usize,
    /// `TIOCSPTLCK`: a slave may not be opened while this is set, which is
    /// how it starts.
    locked: bool,
    /// The input and output speeds as numbers, as the last `TCSETS2` gave
    /// them: what `TCGETS2` reports for a speed `c_cflag` names `BOTHER`
    /// (K3). The console has no such numbers and keeps its own speed.
    speeds: (u32, u32),
    /// `IXON`: the slave's output is stopped, by the stop character the
    /// master sent, until the start character (or any, with `IXANY`).
    stopped: bool,
    /// The input is throttled: it had less than [`THROTTLE_ROOM`] left.
    throttled: bool,
    /// Whether a slave was ever opened: a master write that waits ends with
    /// `EIO` once every one has closed again.
    slave_seen: bool,
}

impl core::fmt::Debug for Pty {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Pty")
            .field("number", &self.number)
            .finish_non_exhaustive()
    }
}

/// Every pair that exists, by number.
static PAIRS: SpinLock<BTreeMap<u32, Arc<Pty>>> = SpinLock::new(BTreeMap::new());

/// The slave that is the controlling terminal of `session`, opened, for
/// `/dev/tty` (`devfs`): `ENXIO` when none is, and none for session 0.
///
/// The pairs are copied out from under `PAIRS`' lock -- at most
/// [`MAX_PAIRS`] of them -- so it and a pair's own lock are never held
/// together. The pair found is the pair opened: its session is checked
/// again under its own lock as its slave is counted open, so a pair freed
/// and its number given to another in between is never the one returned.
pub(crate) fn open_slave_of_session(session: u32) -> VfsResult<Arc<SlaveFile>> {
    if session == 0 {
        return Err(Errno::ENXIO);
    }
    let pairs: Vec<Arc<Pty>> = PAIRS.lock().values().cloned().collect();
    for pty in pairs {
        let mut state = pty.state.lock();
        if state.session != session || !state.master || state.locked {
            continue;
        }
        state.slaves = state.slaves.saturating_add(1);
        drop(state);
        return Ok(Arc::new(SlaveFile { pty }));
    }
    Err(Errno::ENXIO)
}

/// A session whose leader has ended is no pty's to hold: as Linux's
/// `disassociate_ctty`, so a later session given the same number by a
/// reused pid finds no terminal of the dead one's (`docs/AUTH.md` §1).
pub(crate) fn forget_session(session: u32) {
    if session == 0 {
        return;
    }
    let pairs: Vec<Arc<Pty>> = PAIRS.lock().values().cloned().collect();
    for pty in pairs {
        let mut state = pty.state.lock();
        if state.session == session {
            state.session = 0;
            state.foreground = 0;
        }
    }
}

/// How many pairs have ever been made, for the boot report.
static MADE: AtomicU32 = AtomicU32::new(0);

/// How many pairs have ever been made.
pub(crate) fn made() -> u32 {
    MADE.load(Ordering::Relaxed)
}

/// The numbers of the pairs that exist, in order.
pub(crate) fn numbers() -> Vec<u32> {
    PAIRS.lock().keys().copied().collect()
}

/// The pair `number` names, if it is there.
pub(crate) fn pair(number: u32) -> Option<Arc<Pty>> {
    PAIRS.lock().get(&number).map(Arc::clone)
}

/// Make a pair and give back its master, as opening `/dev/ptmx` does.
///
/// # Errors
///
/// `ENOSPC` when every number is taken, which is what Linux answers when its
/// pseudoterminal limit is reached.
pub(crate) fn open_master() -> VfsResult<Arc<MasterFile>> {
    // Taken before the pairs' lock, so it nests no lock of the process's.
    let owner = process::current().map_or((0, 0), |opener| {
        opener.with_credentials(|credentials| {
            (credentials.user.filesystem, credentials.group.filesystem)
        })
    });
    let mut pairs = PAIRS.lock();
    let number = (0..MAX_PAIRS)
        .find(|number| !pairs.contains_key(number))
        .ok_or(Errno::ENOSPC)?;
    let pty = Arc::new(Pty {
        number,
        owner,
        state: SpinLock::new(State {
            discipline: Discipline::new(),
            output: VecDeque::new(),
            winsize: Winsize::DEFAULT,
            foreground: 0,
            session: 0,
            master: true,
            slaves: 0,
            locked: true,
            speeds: (0, 0),
            stopped: false,
            throttled: false,
            slave_seen: false,
        }),
        changed: Arc::new(WaitQueue::new()),
    });
    let _ = pairs.insert(number, Arc::clone(&pty));
    drop(pairs);
    let _ = MADE.fetch_add(1, Ordering::Relaxed);
    Ok(Arc::new(MasterFile { pty }))
}

/// Open the slave of pair `number`, as opening `/dev/pts/<number>` does.
///
/// # Errors
///
/// `ENXIO` for a pair that is not there or whose master has gone, and `EIO`
/// for one `TIOCSPTLCK` has not unlocked, which is what Linux answers.
pub(crate) fn open_slave(number: u32) -> VfsResult<Arc<SlaveFile>> {
    let pty = pair(number).ok_or(Errno::ENXIO)?;
    {
        let mut state = pty.state.lock();
        if !state.master {
            return Err(Errno::ENXIO);
        }
        if state.locked {
            return Err(Errno::EIO);
        }
        state.slaves = state.slaves.saturating_add(1);
        state.slave_seen = true;
    }
    Ok(Arc::new(SlaveFile { pty }))
}

impl Pty {
    /// The settings.
    pub(crate) fn termios(&self) -> Termios {
        self.state.lock().discipline.termios()
    }

    /// Change the settings.
    pub(crate) fn set_termios(&self, termios: Termios, flush: bool) {
        let mut state = self.state.lock();
        if flush {
            state.discipline.flush_input();
        }
        state.discipline.set_termios(termios);
    }

    /// The settings as `TCGETS2` reports them: `struct termios`, then the
    /// input and output speeds as numbers. A speed `c_cflag` names by a
    /// `B` code is that code's; one it names `BOTHER` is the number the
    /// last `TCSETS2` gave, kept as given, as Linux keeps `c_ospeed` (K3);
    /// an input speed of zero bits is the output speed.
    pub(crate) fn termios2(&self) -> [u8; TERMIOS2_BYTES] {
        let (termios, (input, output)) = {
            let state = self.state.lock();
            (state.discipline.termios(), state.speeds)
        };
        let output = if termios.cflag & CBAUD == BOTHER {
            output
        } else {
            termios.output_speed()
        };
        let input = match (termios.cflag >> IBSHIFT) & CBAUD {
            0 => output,
            BOTHER => input,
            _ => termios.input_speed(),
        };
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

    /// Change the settings from a `struct termios2`, keeping its speeds as
    /// numbers: `BOTHER` stays `BOTHER`, with the number beside it, rather
    /// than becoming the nearest code or the old speed (K3).
    pub(crate) fn set_termios2(&self, bytes: &[u8; TERMIOS2_BYTES], flush: bool) {
        let Some((head, speeds)) = bytes.split_first_chunk::<TERMIOS_BYTES>() else {
            return;
        };
        let word = |four: Option<&[u8; 4]>| four.copied().map_or(0, u32::from_le_bytes);
        let input = word(speeds.first_chunk::<4>());
        let output = word(speeds.last_chunk::<4>());
        let termios = Termios::from_bytes(head);
        let mut state = self.state.lock();
        if flush {
            state.discipline.flush_input();
        }
        state.discipline.set_termios(termios);
        state.speeds = (input, output);
    }

    /// The size the terminal says it is.
    pub(crate) fn winsize(&self) -> Winsize {
        self.state.lock().winsize
    }

    /// Say the terminal is another size, and tell the foreground group.
    ///
    /// Linux raises `SIGWINCH` on the foreground process group when the size
    /// changes, and a program that draws in a window -- a shell's line
    /// editor, a pager -- redraws when it arrives.
    pub(crate) fn set_winsize(&self, winsize: Winsize) {
        let changed = {
            let mut state = self.state.lock();
            let was = state.winsize;
            state.winsize = winsize;
            was != winsize
        };
        if changed {
            self.signal_foreground(SIGWINCH);
        }
    }

    /// The foreground process group.
    pub(crate) fn foreground(&self) -> u32 {
        self.state.lock().foreground
    }

    /// Set it, as `tcsetpgrp` does.
    pub(crate) fn set_foreground(&self, group: u32) {
        self.state.lock().foreground = group;
    }

    /// The session the slave is the controlling terminal of.
    pub(crate) fn session(&self) -> u32 {
        self.state.lock().session
    }

    /// Make it the controlling terminal of `session`, as `TIOCSCTTY` does.
    pub(crate) fn set_session(&self, session: u32, foreground: u32) {
        let mut state = self.state.lock();
        state.session = session;
        state.foreground = foreground;
    }

    /// How many bytes the slave could read without waiting.
    pub(crate) fn slave_available(&self) -> usize {
        self.state.lock().discipline.available()
    }

    /// How many bytes the master could read without waiting.
    pub(crate) fn master_available(&self) -> usize {
        self.state.lock().output.len()
    }

    /// Whether the slave's output has room for another byte, and is not
    /// stopped by `IXON`.
    fn output_room(&self) -> bool {
        let state = self.state.lock();
        state.output.len() < OUTPUT_LIMIT && !state.stopped
    }

    /// How many bytes a master write may hand the line discipline now
    /// without any being dropped (K1).
    pub(crate) fn input_room(&self) -> usize {
        self.state.lock().discipline.room()
    }

    /// Whether a master write that waits for room has nothing left to wait
    /// for: a slave was opened and every one has closed.
    fn slaves_gone(&self) -> bool {
        let state = self.state.lock();
        state.slave_seen && state.slaves == 0
    }

    /// Throw away what has been typed and not read.
    pub(crate) fn flush_input(&self) {
        self.state.lock().discipline.flush_input();
    }

    /// Throw away what the slave wrote and the master has not read, which
    /// makes room for a writer that was waiting for it.
    pub(crate) fn flush_output(&self) {
        self.state.lock().output.clear();
        self.changed.wake_all();
    }

    /// Send `signal` to every process in the foreground group.
    fn signal_foreground(&self, signal: u32) {
        let group = self.foreground();
        if group == 0 {
            return;
        }
        // As `tty::signal_foreground_group`: lost, with no memory to list.
        for target in crate::syscall::registry::live().unwrap_or_default() {
            if target.pgid() == group {
                crate::syscall::kill::send(&target, signal, crate::syscall::signal::Origin::Kernel);
            }
        }
    }

    /// Put what the master wrote through the line discipline, as much as it
    /// has room for (K1): how many bytes it took, and the signal they ask
    /// for, if any.
    ///
    /// Under `IXON` the stop and start characters stop and start the
    /// slave's output and go no further; with `IXANY` any character starts
    /// it. The echo goes to the master, and what of it the output has no
    /// room for is dropped, as `n_tty` drops it: an echo never makes a
    /// master write wait, so a driver that writes before it reads cannot
    /// stop itself.
    fn typed(&self, bytes: &[u8]) -> (usize, Option<u32>) {
        let mut signal = None;
        let mut taken = 0;
        {
            let mut state = self.state.lock();
            let mut echo = Vec::new();
            let termios = state.discipline.termios();
            for &byte in bytes {
                if state.discipline.room() == 0 {
                    break;
                }
                taken += 1;
                if flow_control(&mut state, &termios, byte) {
                    continue;
                }
                if let Some(raised) = state.discipline.receive(byte, &mut echo) {
                    signal = Some(raised);
                }
            }
            // The echo goes to the master: on a pseudoterminal the terminal
            // is the program at that end, and drawing what was typed is its
            // work.
            for byte in echo {
                if state.output.len() < OUTPUT_LIMIT {
                    state.output.push_back(byte);
                }
            }
            if !state.throttled && state.discipline.room() < THROTTLE_ROOM {
                state.throttled = true;
                if termios.iflag & IXOFF != 0
                    && let Some(&stop) = termios.cc.get(VSTOP)
                {
                    state.output.push_back(stop);
                }
            }
        }
        self.changed.wake_all();
        (taken, signal)
    }

    /// After the slave read: unthrottle once what is held is below
    /// [`UNTHROTTLE_HELD`], sending `IXOFF`'s start character.
    fn unthrottle(&self) {
        let mut state = self.state.lock();
        if state.throttled && state.discipline.held() < UNTHROTTLE_HELD {
            state.throttled = false;
            let termios = state.discipline.termios();
            if termios.iflag & IXOFF != 0
                && let Some(&start) = termios.cc.get(VSTART)
            {
                state.output.push_back(start);
            }
        }
    }

    /// One look by the slave's read, as [`Discipline::read_step`] decides:
    /// taken under the lock, which is released before anybody waits.
    fn read_step(&self, buf: &mut [u8], timer: &mut ReadTimer, nonblock: bool) -> ReadStep {
        let now = crate::timer::now_nanos();
        let step = self
            .state
            .lock()
            .discipline
            .read_step(buf, timer, now, nonblock);
        if matches!(step, ReadStep::Took(_)) {
            self.unthrottle();
            self.changed.wake_all();
        }
        step
    }

    /// Put what the slave wrote where the master can read it, applying
    /// `OPOST` as the console's writes do.
    fn wrote(&self, bytes: &[u8]) -> usize {
        let mut written = 0;
        {
            let mut state = self.state.lock();
            // Stopped by `IXON`: nothing goes until the start character.
            if state.stopped {
                return 0;
            }
            let post = state.discipline.termios().oflag;
            let newlines = post & OPOST != 0 && post & ONLCR != 0;
            for byte in bytes {
                if state.output.len() >= OUTPUT_LIMIT {
                    break;
                }
                if newlines && *byte == b'\n' {
                    state.output.push_back(b'\r');
                }
                state.output.push_back(*byte);
                written += 1;
            }
        }
        self.changed.wake_all();
        written
    }

    /// Take what the master may read, which makes room for a writer that
    /// was waiting for it.
    fn read_written(&self, buf: &mut [u8]) -> usize {
        let mut taken = 0;
        {
            let mut state = self.state.lock();
            while taken < buf.len() {
                let Some(byte) = state.output.pop_front() else {
                    break;
                };
                if let Some(slot) = buf.get_mut(taken) {
                    *slot = byte;
                }
                taken += 1;
            }
        }
        if taken > 0 {
            self.changed.wake_all();
        }
        taken
    }

    /// Whether the master is still open.
    fn master_open(&self) -> bool {
        self.state.lock().master
    }

    /// Wait until `ready`, the caller is signalled, or the counter reaches
    /// `deadline` (`u64::MAX` for never). Only the signal is an error: at the
    /// deadline the caller looks again, as it does when `ready`.
    fn wait_for(&self, ready: impl Fn() -> bool, deadline: u64) -> Result<(), Errno> {
        let caller = process::current();
        let killed = || {
            caller
                .as_ref()
                .is_some_and(|process| process.signal_pending())
        };
        let due = || crate::timer::now_nanos() >= deadline;
        while !ready() && !killed() && !due() {
            let _ = self
                .changed
                .wait_until_deadline(|| ready() || killed(), deadline);
        }
        // As before the deadline: not ready and not due is a signal, or a
        // wake that lost its input to another reader, and restarts.
        if ready() || (due() && !killed()) {
            Ok(())
        } else {
            Err(Errno::ERESTARTSYS)
        }
    }

    /// A blocking master write's wait for room in the input (K1): until a
    /// read makes room, or every slave that was opened has closed, which
    /// ends it with `EIO` when there is still no room; a signal ends it as
    /// [`Self::wait_for`] does.
    fn wait_for_room(&self) -> Result<(), Errno> {
        self.wait_for(|| self.input_room() > 0 || self.slaves_gone(), u64::MAX)?;
        if self.input_room() == 0 && self.slaves_gone() {
            Err(Errno::EIO)
        } else {
            Ok(())
        }
    }
}

/// `IXON`'s stop and start characters, as `n_tty` takes them (K1): the
/// stop character stops the slave's output, the start character starts it,
/// and with `IXANY` any character does. Whether `byte` was one of the two,
/// which go no further.
fn flow_control(state: &mut State, termios: &Termios, byte: u8) -> bool {
    if termios.iflag & IXON == 0 {
        return false;
    }
    let start = termios.cc.get(VSTART).copied().unwrap_or(0);
    let stop = termios.cc.get(VSTOP).copied().unwrap_or(0);
    if stop != 0 && byte == stop {
        state.stopped = true;
        return true;
    }
    if start != 0 && byte == start {
        state.stopped = false;
        return true;
    }
    if termios.iflag & IXANY != 0 {
        state.stopped = false;
    }
    false
}

/// The master end: what a terminal emulator holds.
#[derive(Debug)]
pub(crate) struct MasterFile {
    pub(crate) pty: Arc<Pty>,
}

/// The slave end: what the program on the terminal holds.
#[derive(Debug)]
pub(crate) struct SlaveFile {
    pub(crate) pty: Arc<Pty>,
}

impl Drop for MasterFile {
    /// Closing the master takes the pair away, as Linux's `pty_close` does:
    /// the slave's reads end and its writes fail, and the number is free for
    /// the next `/dev/ptmx`.
    fn drop(&mut self) {
        {
            let mut state = self.pty.state.lock();
            state.master = false;
            state.output.clear();
        }
        let _ = PAIRS.lock().remove(&self.pty.number);
        self.pty.changed.wake_all();
    }
}

impl Drop for SlaveFile {
    fn drop(&mut self) {
        {
            let mut state = self.pty.state.lock();
            state.slaves = state.slaves.saturating_sub(1);
        }
        self.pty.changed.wake_all();
    }
}

impl Inode for MasterFile {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: PTS_INO,
            kind: FileType::CharDevice,
            permissions: 0o666,
            nlink: 1,
            rdev: makedev(PTMX_MAJOR, PTMX_MINOR),
            block_size: 4096,
            ..blank()
        }
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.read_stream(buf, false)
    }

    /// What the slave wrote. A pair whose every slave has closed reads
    /// nothing and is not at end of file: Linux keeps the master readable,
    /// because a slave may be opened again.
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let ready = || self.pty.master_available() > 0;
        if !ready() {
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            self.pty.wait_for(ready, u64::MAX)?;
        }
        Ok(self.pty.read_written(buf))
    }

    fn write_at(&self, offset: u64, buf: &[u8], _append: bool) -> VfsResult<(usize, u64)> {
        self.write_stream(buf, false)
            .map(|written| (written, offset))
    }

    /// What the person typed, or the serial line received, through the line
    /// discipline: only as much as it has room for (K1). Non-blocking, that
    /// is the short count, or `EAGAIN` with no room at all; blocking, the
    /// write waits for the slave's reader to make room, killably, and ends
    /// with `EIO` once every slave that was opened has closed -- with what
    /// was written so far, and the error only when that is nothing. Nothing
    /// is ever taken and then dropped.
    fn write_stream(&self, buf: &[u8], nonblock: bool) -> VfsResult<usize> {
        let mut written = 0;
        loop {
            let (taken, signal) = self.pty.typed(buf.get(written..).unwrap_or_default());
            written += taken;
            if let Some(signal) = signal {
                self.pty.signal_foreground(signal);
            }
            if written >= buf.len() {
                return Ok(written);
            }
            let ended = if nonblock {
                Err(Errno::EAGAIN)
            } else if self.pty.slaves_gone() {
                Err(Errno::EIO)
            } else {
                self.pty.wait_for_room()
            };
            if let Err(error) = ended {
                return if written > 0 { Ok(written) } else { Err(error) };
            }
        }
    }

    fn poll(&self) -> Readiness {
        Readiness {
            readable: self.pty.master_available() > 0,
            // Writable only with room for input (K1); a pair whose slaves
            // have all gone does not wait, it fails.
            writable: self.pty.input_room() > 0 || self.pty.slaves_gone(),
            hangup: false,
            error: false,
            priority: false,
        }
    }

    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        visit(crate::fs::wake::shared(&self.pty.changed));
        true
    }

    /// The same queues, and the same answer, for its whole life.
    fn poll_queues_fixed(&self) -> bool {
        true
    }

    fn poll_changes(&self) -> Option<u64> {
        Some(self.pty.changed.wakes())
    }
}

impl Inode for SlaveFile {
    fn metadata(&self) -> Metadata {
        // Its own pair's owner: once the master closes, the number may be
        // gone or another pair's.
        slave_metadata_owned(self.pty.number, self.pty.owner)
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.read_stream(buf, false)
    }

    /// What the master typed: a line in canonical mode, and otherwise what
    /// `VMIN` and `VTIME` say, as the console's read does -- both decide by
    /// [`Discipline::read_step`]. A master that has closed is end of file,
    /// which is what a shell reads as its terminal going away.
    ///
    /// The wait is for the input to change, the master to go, a signal, or
    /// `VTIME`'s deadline; after each, the read looks again under the lock.
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut timer = ReadTimer::new(crate::timer::now_nanos());
        loop {
            let deadline = match self.pty.read_step(buf, &mut timer, nonblock) {
                ReadStep::Took(taken) => return Ok(taken),
                ReadStep::Wait(deadline) => deadline,
            };
            if !self.pty.master_open() {
                return Ok(0);
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            let seen = timer.seen();
            self.pty.wait_for(
                || self.pty.slave_available() != seen || !self.pty.master_open(),
                deadline,
            )?;
        }
    }

    fn write_at(&self, offset: u64, buf: &[u8], _append: bool) -> VfsResult<(usize, u64)> {
        self.write_stream(buf, false)
            .map(|written| (written, offset))
    }

    /// The program's output, on its way to the master: all of it, waiting
    /// for the master to read whenever the output is full, unless `nonblock`,
    /// when it is as much as fits and `EAGAIN` if nothing does. A signal, or
    /// the master closing, ends the wait with what was written so far, and
    /// with the error only when that is nothing.
    fn write_stream(&self, buf: &[u8], nonblock: bool) -> VfsResult<usize> {
        let mut written = 0;
        loop {
            if !self.pty.master_open() {
                return if written > 0 {
                    Ok(written)
                } else {
                    Err(Errno::EIO)
                };
            }
            written += self.pty.wrote(buf.get(written..).unwrap_or_default());
            if written >= buf.len() {
                return Ok(written);
            }
            if nonblock {
                return if written > 0 {
                    Ok(written)
                } else {
                    Err(Errno::EAGAIN)
                };
            }
            if let Err(error) = self.pty.wait_for(
                || self.pty.output_room() || !self.pty.master_open(),
                u64::MAX,
            ) {
                return if written > 0 { Ok(written) } else { Err(error) };
            }
        }
    }

    fn poll(&self) -> Readiness {
        let gone = !self.pty.master_open();
        Readiness {
            readable: self.pty.slave_available() > 0 || gone,
            // A write to a pair whose master has gone does not wait either:
            // it fails at once.
            writable: gone || self.pty.output_room(),
            hangup: gone,
            error: false,
            priority: false,
        }
    }

    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        visit(crate::fs::wake::shared(&self.pty.changed));
        true
    }

    /// The same queues, and the same answer, for its whole life.
    fn poll_queues_fixed(&self) -> bool {
        true
    }

    fn poll_changes(&self) -> Option<u64> {
        Some(self.pty.changed.wakes())
    }
}

/// A slave node's metadata, its owner the process that opened its master's:
/// for `/dev/pts/<number>`, the pair that has the number now.
pub(crate) fn slave_metadata(number: u32) -> Metadata {
    slave_metadata_owned(number, pair(number).map_or((0, 0), |pty| pty.owner))
}

/// A slave's metadata, owned by `(uid, gid)`.
fn slave_metadata_owned(number: u32, (uid, gid): (u32, u32)) -> Metadata {
    Metadata {
        ino: SLAVE_INO_BASE + u64::from(number),
        kind: FileType::CharDevice,
        // What Linux's devpts gives a slave: the owner's to read and write,
        // and the `tty` group's to write. Ferrix has no `tty` group, so the
        // group is the opener's.
        permissions: 0o620,
        nlink: 1,
        uid,
        gid,
        rdev: makedev(SLAVE_MAJOR, number),
        block_size: 4096,
        ..blank()
    }
}

/// The fields every node here shares.
fn blank() -> Metadata {
    let zero = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    Metadata {
        ino: 0,
        kind: FileType::CharDevice,
        permissions: 0o666,
        nlink: 1,
        uid: 0,
        gid: 0,
        size: 0,
        rdev: 0,
        blocks: 0,
        block_size: 4096,
        atime: zero,
        mtime: zero,
        ctime: zero,
    }
}

/// The open master `io` is, if it is one.
pub(crate) fn master_of(io: &Arc<dyn Inode>) -> Option<Arc<MasterFile>> {
    Arc::clone(io).into_any().downcast::<MasterFile>().ok()
}

/// The open slave `io` is, if it is one.
pub(crate) fn slave_of(io: &Arc<dyn Inode>) -> Option<Arc<SlaveFile>> {
    Arc::clone(io).into_any().downcast::<SlaveFile>().ok()
}

/// Unlock a pair, as `TIOCSPTLCK` does with a zero.
pub(crate) fn set_locked(pty: &Pty, locked: bool) {
    pty.state.lock().locked = locked;
}
