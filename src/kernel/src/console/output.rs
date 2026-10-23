//! What the console sends, between the writer that queued it and the transmit
//! interrupt that puts it on the wire.
//!
//! # Why a ring sits between the two
//!
//! A serial port takes a byte about every 87 microseconds at 115200 baud, and
//! a writer that waits for each one waits for all of them. Before this ring
//! every write did: under the port's lock, which masks interrupts, one byte at
//! a time, polling the port for room. A 27-byte line held a processor for
//! 2.3 ms with interrupts off, and a 4 KiB write for 350 ms. On a DK1, whose
//! desktop sends every program's output to this port, a test client printing
//! a line per mouse motion kept one of the board's two cores doing nothing
//! else for as long as the mouse moved, and the compositor's frames went from
//! 11 ms to between 300 and 650.
//!
//! So a writer now copies into this ring and gives the port only what it has
//! room for at that moment; the port's transmit interrupt, which the receive
//! side already installed (`console::input`), feeds it the rest as it empties.
//! A writer that finds the ring full sleeps on [`WRITERS`] until the interrupt
//! has made room — a Linux tty blocks its writer the same way.
//!
//! # Who queues, and who still polls
//!
//! - **A task that may sleep** (`sched::may_block`), writing a program's
//!   output through `fs::terminal`: queues, and waits for room when there is
//!   none. [`write_waiting`].
//! - **Anyone else with interrupts on**, which is every `println!` from a task
//!   or a thread: queues, and polls only when the ring is full, and then only
//!   enough to make room for its own bytes. Never sleeps, because a
//!   `println!` can come from the idle task, which must not.
//! - **Interrupts masked, the transmit interrupt not installed yet, or a
//!   failure being reported**: polls everything the ring holds out first, then
//!   writes straight to the port, as every writer did before. With interrupts
//!   masked the transmit interrupt may never come to this processor, and a
//!   line printed just before the kernel stops has to be on the wire when it
//!   does.
//!
//! # Order, and whole lines
//!
//! Everything happens under `console::PORT`, and the ring is first in, first
//! out, so a program's output and the kernel's lines leave in the order they
//! were written, and a writer that writes straight to the port empties the
//! ring before it does. Boot tests read the serial log line by line and rely
//! on that. A `println!` line goes into the ring under one hold of the lock,
//! so lines from two processors do not mix, as they did not before. A task's
//! write longer than [`CHUNK`] is queued in pieces cut after a newline where
//! one falls, so the only lines another writer can come between are ones
//! longer than a chunk.
//!
//! # A transmit interrupt that stops coming
//!
//! The one way this can go wrong that polling could not: bytes queued for an
//! interrupt that never arrives are never sent, and a writer waiting for room
//! waits for ever. So a writer that finds the ring holding bytes the port has
//! not moved for a tenth of a second polls them out itself, and counts it in
//! [`stalls`]; the boot check requires the count to stay where it was.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use super::PORT;
use super::input::{CAPACITY, Ring};
use crate::arch;
use crate::sched::WaitQueue;

/// The most bytes one call puts into the port.
///
/// More than any port this kernel drives holds — a 16550's FIFO and an STM32
/// USART's are sixteen bytes, a PL011's thirty-two — so on hardware the port
/// filling up is what stops a call, and on an emulated port, which sends
/// instantly and is never full, this is. It bounds how long the lock is held
/// with interrupts masked in either case.
pub(super) const BURST: usize = 64;

/// The most a task's write queues under one hold of the lock, and the room a
/// writer waits for: half the ring, so a writer woken when the ring drains to
/// half always fits.
const CHUNK: usize = CAPACITY / 2;

/// A writer waiting for room is woken when the ring drains to this many bytes.
const LOW_WATER: usize = CAPACITY - CHUNK;

/// The port has stalled when bytes have waited this fraction of a second with
/// nothing sent: a tenth. A sixteen-byte FIFO empties in 1.4 ms at 115200
/// baud, so a port that is merely slow never comes near it.
const STALL_SECONDS_DIVISOR: u64 = 10;

/// The transmit check gives up on the port's transmit interrupt when it has
/// sent nothing for this long while a task's write waited in the ring: 50 ms.
///
/// The interrupt is owed once the FIFO the writer filled has drained, and
/// again each time it drains after: 1.4 ms for a 16550's or an STM32 USART's
/// sixteen bytes at 115200 baud, 2.8 ms for a PL011's thirty-two, and 16.7 ms
/// for sixteen bytes even at 9600, a rate no port here runs at. So 50 ms is
/// eighteen times the longest wait a working port at 115200 can owe, and three
/// times one at 9600; it is above the 20 to 30 ms the boot's `late` check
/// keeps a processor from answering on purpose, and below [`STALL_SECONDS_DIVISOR`]'s
/// tenth of a second, so the check gives up before a writer can find the port
/// stalled and poll it out under it. It is measured from the last byte the
/// interrupt sent, never from the start, so a long ring that the interrupt is
/// draining is waited for (up to [`CHECK_LIMIT_NANOS`]). The N150 box's COM1,
/// whose interrupt never comes, waited two seconds here before.
const SILENT_NANOS: u64 = 50_000_000;

/// The longest the transmit check waits for its line to go out at all, the
/// interrupt still sending: two seconds, a hundred times what the line takes
/// on the slowest port, and five times a full ring at 115200 baud.
const CHECK_LIMIT_NANOS: u64 = 2_000_000_000;

/// What waits to be sent, and the state of the port's transmit interrupt.
pub(super) struct Transmit {
    /// The bytes, oldest first.
    ring: Ring,
    /// Whether the port's transmit interrupt is enabled: exactly while the
    /// ring holds something.
    armed: bool,
    /// The counter's reading when the port last took a byte, or the ring was
    /// last found empty.
    moved_at: u64,
}

/// Whether the port's interrupt is installed, so that a writer may queue.
static INTERRUPT_DRIVEN: AtomicBool = AtomicBool::new(false);

/// How many bytes the ring holds, beside it so that a waiting writer's
/// condition takes no lock. Written only under the lock, before any wake.
static HELD: AtomicUsize = AtomicUsize::new(0);

/// Bytes a writer put into the port itself, a burst at a time.
static BY_WRITER: AtomicU64 = AtomicU64::new(0);

/// Bytes the transmit interrupt put into the port.
static BY_INTERRUPT: AtomicU64 = AtomicU64::new(0);

/// Bytes put into the port by polling for room: taken out of the ring by a
/// writer that could not wait for the interrupt, or that found it full.
static POLLED: AtomicU64 = AtomicU64::new(0);

/// Times the ring was polled out because the port had moved nothing for a
/// tenth of a second.
static STALLS: AtomicU64 = AtomicU64::new(0);

/// Tasks waiting for room in the ring.
static WRITERS: WaitQueue = WaitQueue::new();

impl Transmit {
    /// Nothing to send, and the interrupt off.
    pub(super) const fn new() -> Transmit {
        Transmit {
            ring: Ring::new(),
            armed: false,
            moved_at: 0,
        }
    }

    /// How many more bytes the ring can take.
    const fn room(&self) -> usize {
        CAPACITY.saturating_sub(self.ring.len())
    }

    /// Queue `byte`. When the ring is full the oldest byte is polled out to
    /// make room, which is how a writer that must not sleep finishes its line.
    pub(super) fn queue(&mut self, byte: u8) {
        if self.ring.len() == 0 {
            self.moved_at = arch::counter_now();
        }
        // NOALLOC: a fixed ring, which refuses when full.
        if self.ring.push(byte) {
            return;
        }
        if let Some(oldest) = self.ring.pop() {
            arch::console::write_byte(oldest);
            let _ = POLLED.fetch_add(1, Ordering::Relaxed);
        }
        // NOALLOC: a fixed ring, with room made just above.
        let _ = self.ring.push(byte);
    }

    /// Poll everything the ring holds out to the port, for a writer about to
    /// write to the port directly. Answers whether writers waiting for room
    /// should be woken.
    pub(super) fn flush(&mut self) -> bool {
        // Every write before the port's interrupt is installed comes through
        // here, some before the counter is, and an empty ring has nothing to
        // settle: the interrupt is off whenever the lock is free and the ring
        // is empty.
        if self.ring.len() == 0 {
            return false;
        }
        let before = self.ring.len();
        while let Some(byte) = self.ring.pop() {
            arch::console::write_byte(byte);
            let _ = POLLED.fetch_add(1, Ordering::Relaxed);
        }
        self.settle(before, 0)
    }

    /// Poll the ring out if the port has moved nothing of it for a tenth of a
    /// second, and count that. Answers whether writers should be woken.
    pub(super) fn unstall(&mut self) -> bool {
        if self.ring.len() == 0 {
            return false;
        }
        let patience = arch::counter_hz() / STALL_SECONDS_DIVISOR;
        if arch::counter_now().wrapping_sub(self.moved_at) <= patience {
            return false;
        }
        let _ = STALLS.fetch_add(1, Ordering::Relaxed);
        self.flush()
    }

    /// Give the port what it has room for now, never waiting for more, and
    /// leave its transmit interrupt enabled exactly if something is left.
    /// `count` is whose bytes they are. Answers whether writers waiting for
    /// room should be woken.
    pub(super) fn pump(&mut self, count: &AtomicU64) -> bool {
        let before = self.ring.len();
        let mut moved = 0usize;
        'fill: while moved < BURST {
            let room = arch::console::transmit_room();
            if room == 0 {
                break;
            }
            for _ in 0..room {
                let Some(byte) = self.ring.pop() else {
                    break 'fill;
                };
                arch::console::put(byte);
                moved = moved.wrapping_add(1);
            }
        }
        let _ = count.fetch_add(moved as u64, Ordering::Relaxed);
        self.settle(before, moved)
    }

    /// After the ring went from `before` bytes to what it holds now, `moved`
    /// of them to the port: publish the count, note the progress, set the
    /// interrupt to match, and say whether a waiting writer now fits.
    fn settle(&mut self, before: usize, moved: usize) -> bool {
        let after = self.ring.len();
        if moved != 0 || after == 0 {
            self.moved_at = arch::counter_now();
        }
        HELD.store(after, Ordering::Relaxed);
        let wanted = after != 0;
        if wanted != self.armed {
            arch::console::transmit_interrupt(wanted);
            self.armed = wanted;
        }
        before > LOW_WATER && after <= LOW_WATER
    }
}

/// Run `f` with the port's transmit side quiet: under the port's lock, with
/// the ring polled out and the port drained, so nothing a writer sends is on
/// the wire while `f` has the port. For x86-64's check R5, which loops the
/// port back on itself for one byte (`docs/NVIDIA.md` §12.3).
#[cfg_attr(
    not(target_arch = "x86_64"),
    expect(dead_code, reason = "only x86-64 loops its port back for check R5")
)]
pub(crate) fn with_port_quiet<R>(f: impl FnOnce() -> R) -> R {
    let mut port = PORT.lock();
    let wake = port.flush();
    arch::drain_console();
    let result = f();
    drop(port);
    if wake {
        wake_writers();
    }
    result
}

/// Let writers queue: the port's interrupt is installed. Called by
/// `console::input::init`, with interrupts masked, before the line is enabled.
pub(super) fn start() {
    INTERRUPT_DRIVEN.store(true, Ordering::Relaxed);
}

/// Stop queuing and poll out what the ring holds: the transmit interrupt
/// does not come on this machine.
///
/// The N150 box has a port at COM1 that loops back and receives, but its
/// transmit interrupt never arrived, and [`check`] found every task's write
/// left in the ring. A machine whose serial line nobody reads is not stopped
/// for it: its writers poll, as before the interrupt was installed.
pub fn fall_back_to_polling() {
    INTERRUPT_DRIVEN.store(false, Ordering::Relaxed);
    let mut port = PORT.lock();
    let wake = port.flush();
    arch::drain_console();
    drop(port);
    if wake {
        wake_writers();
    }
}

/// Whether the port's interrupt is installed, so that a writer with interrupts
/// on may queue rather than poll.
pub(super) fn interrupt_driven() -> bool {
    INTERRUPT_DRIVEN.load(Ordering::Relaxed)
}

/// The count to charge a writer's own [`Transmit::pump`] to.
pub(super) fn by_writer() -> &'static AtomicU64 {
    &BY_WRITER
}

/// Wake the tasks waiting for room, for a writer whose pump or flush said to.
/// After the port's lock is dropped: the wake takes run-queue locks.
pub(super) fn wake_writers() {
    WRITERS.wake_all();
}

/// The transmit half of the port's interrupt: feed the port, and wake writers
/// when there is room for them.
pub(super) fn on_interrupt() {
    if !interrupt_driven() {
        return;
    }
    let wake = PORT.lock().pump(&BY_INTERRUPT);
    if wake {
        wake_writers();
    }
}

/// Queue `bytes` for the port, a bare newline as CRLF if `crlf`, waiting for
/// room while there is none. For a task that may sleep.
pub(super) fn write_waiting(bytes: &[u8], crlf: bool) {
    let mut rest = bytes;
    while !rest.is_empty() {
        let (take, need) = next_chunk(rest, crlf);
        let (chunk, after) = rest.split_at(take.min(rest.len()));
        loop {
            if let Some(wake) = try_queue(chunk, crlf, need) {
                if wake {
                    wake_writers();
                }
                break;
            }
            // Woken when the ring drains to half, which a chunk always fits;
            // and at the latest after the stall's patience, so that the next
            // look can find a port that stopped and poll it out.
            let patience =
                crate::timer::now_nanos().saturating_add(1_000_000_000 / STALL_SECONDS_DIVISOR);
            let _ = WRITERS.wait_until_deadline(
                || CAPACITY.saturating_sub(HELD.load(Ordering::Relaxed)) >= need,
                patience,
            );
        }
        rest = after;
    }
}

/// Queue `chunk` if the ring has room for all `need` bytes of it now, and give
/// the port what it takes. `None` when there is no room; otherwise whether
/// writers should be woken.
fn try_queue(chunk: &[u8], crlf: bool, need: usize) -> Option<bool> {
    let mut port = PORT.lock();
    // A stall polled out leaves the ring empty, so a chunk always fits after
    // one and the wake it asks for is never lost to the early return.
    let unstalled = port.unstall();
    if port.room() < need {
        return None;
    }
    // The program's bytes as it wrote them, before the newline translation:
    // recorded once, when they are queued, never for a try that found no room.
    super::log::record(chunk, super::Kind::Program.priority());
    for &byte in chunk {
        if crlf && byte == b'\n' {
            port.queue(b'\r');
        }
        port.queue(byte);
    }
    Some(port.pump(&BY_WRITER) || unstalled)
}

/// How much of `rest` to queue at once, and how many bytes that is on the wire:
/// all of it if that fits in a [`CHUNK`], otherwise as much as fits that ends
/// in a newline, and failing that as much as fits.
fn next_chunk(rest: &[u8], crlf: bool) -> (usize, usize) {
    let mut need = 0usize;
    let mut take = 0usize;
    let mut line_end = None;
    for &byte in rest {
        let cost = if crlf && byte == b'\n' { 2 } else { 1 };
        if need.saturating_add(cost) > CHUNK {
            break;
        }
        need = need.saturating_add(cost);
        take = take.saturating_add(1);
        if byte == b'\n' {
            line_end = Some((take, need));
        }
    }
    if take == rest.len() {
        return (take, need);
    }
    line_end.unwrap_or((take, need))
}

/// Times the ring was polled out because the port stopped taking bytes.
pub(crate) fn stalls() -> u64 {
    STALLS.load(Ordering::Relaxed)
}

/// Why [`check`] failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Failure {
    /// A task's write waited in the transmit ring while the port's transmit
    /// interrupt sent nothing for [`SILENT_NANOS`]: the interrupt never came,
    /// or stopped coming. The N150 box's COM1 does this; its writers may fall
    /// back to polling ([`fall_back_to_polling`]). `waited_us` is how long
    /// the check waited in all.
    Silent {
        /// Microseconds from the write to giving up.
        waited_us: u64,
    },
    /// A task's write was still in the transmit ring two seconds later, the
    /// interrupt still sending: as [`Failure::Silent`], the machine's, and
    /// fallen back from the same way.
    NeverCame,
    /// Anything else: this kernel's own defect, which stops the boot
    /// (FX-0504).
    Defect(&'static str),
}

impl Failure {
    /// Whether this is the machine's failure rather than this kernel's, one
    /// the boot goes on from by polling ([`fall_back_to_polling`]).
    pub const fn falls_back(&self) -> bool {
        matches!(self, Failure::Silent { .. } | Failure::NeverCame)
    }
}

impl core::fmt::Display for Failure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Failure::Silent { waited_us } => write!(
                f,
                "the transmit interrupt sent nothing for {} ms while a task's write waited \
                 in the ring (given up after {}.{:03} ms)",
                SILENT_NANOS / 1_000_000,
                waited_us / 1000,
                waited_us % 1000,
            ),
            Failure::NeverCame => {
                f.write_str("the transmit ring still held a task's write two seconds later")
            }
            Failure::Defect(what) => f.write_str(what),
        }
    }
}

/// What [`check`] established.
#[derive(Debug)]
pub struct Checked {
    /// The bytes the check's line is on the wire.
    pub line: usize,
    /// How many of them the transmit interrupt sent.
    pub by_interrupt: u64,
    /// How many the writer sent itself.
    pub by_writer: u64,
}

/// The line the check writes: longer than a [`BURST`], so that the port's
/// transmit interrupt has to send some of it.
const CHECK_LINE: &[u8] = concat!(
    "  output   this line is a task's write, longer than the port takes at once: ",
    "the writer queues it and goes on, and the port's transmit interrupt sends what the writer left\n",
)
.as_bytes();

/// A task's write, checked to have gone out the way a program's output should:
/// the writer putting at most a burst into the port, the transmit interrupt
/// sending the rest, and nothing polled out because the interrupt stopped.
///
/// `Ok(None)` on a port with no interrupt, where everything is polled as it
/// always was.
///
/// # Errors
///
/// The first property that did not hold: [`Failure::Silent`] for a task's
/// write left in the ring while the transmit interrupt sent nothing for
/// [`SILENT_NANOS`], and [`Failure::NeverCame`] for one still there two
/// seconds later, the failures that are the machine's rather than this
/// kernel's ([`Failure::falls_back`]), and [`Failure::Defect`] for every
/// other.
pub fn check() -> Result<Option<Checked>, Failure> {
    if !interrupt_driven() {
        return Ok(None);
    }
    if !crate::sched::may_block() {
        return Err(Failure::Defect(
            "the transmit check ran where a writer could not wait",
        ));
    }
    let interrupt_before = BY_INTERRUPT.load(Ordering::Relaxed);
    let writer_before = BY_WRITER.load(Ordering::Relaxed);
    let stalls_before = stalls();

    super::write_bytes(CHECK_LINE);
    let line = CHECK_LINE.len().saturating_add(1);

    // What the writer left, the interrupt sends. The wait looks again every
    // few milliseconds of its own accord (`wait_until_deadline`), so it sees
    // the interrupt's count move, and gives up once it has stood still for
    // SILENT_NANOS with bytes still waiting.
    let start = crate::timer::now_nanos();
    let deadline = start.saturating_add(CHECK_LIMIT_NANOS);
    let mut seen = interrupt_before;
    let mut moved_at = start;
    let mut silent = false;
    let drained = WRITERS.wait_until_deadline(
        || {
            if HELD.load(Ordering::Relaxed) == 0 {
                return true;
            }
            let now = crate::timer::now_nanos();
            let sent = BY_INTERRUPT.load(Ordering::Relaxed);
            if sent != seen {
                seen = sent;
                moved_at = now;
                return false;
            }
            silent = now.saturating_sub(moved_at) >= SILENT_NANOS;
            silent
        },
        deadline,
    );
    if silent {
        return Err(Failure::Silent {
            waited_us: crate::timer::now_nanos().saturating_sub(start) / 1000,
        });
    }
    if !drained {
        return Err(Failure::NeverCame);
    }
    if stalls() != stalls_before {
        return Err(Failure::Defect(
            "the transmit interrupt stopped coming, and the ring was polled out",
        ));
    }
    let by_interrupt = BY_INTERRUPT
        .load(Ordering::Relaxed)
        .wrapping_sub(interrupt_before);
    let by_writer = BY_WRITER
        .load(Ordering::Relaxed)
        .wrapping_sub(writer_before);
    if by_interrupt.saturating_add(BURST as u64) < line as u64 {
        return Err(Failure::Defect(
            "a task's write was not left to the transmit interrupt",
        ));
    }
    Ok(Some(Checked {
        line,
        by_interrupt,
        by_writer,
    }))
}
