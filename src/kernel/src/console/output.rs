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
    super::log::record(chunk);
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
/// A description of the first property that did not hold.
pub fn check() -> Result<Option<Checked>, &'static str> {
    if !interrupt_driven() {
        return Ok(None);
    }
    if !crate::sched::may_block() {
        return Err("the transmit check ran where a writer could not wait");
    }
    let interrupt_before = BY_INTERRUPT.load(Ordering::Relaxed);
    let writer_before = BY_WRITER.load(Ordering::Relaxed);
    let stalls_before = stalls();

    super::write_bytes(CHECK_LINE);
    let line = CHECK_LINE.len().saturating_add(1);

    // What the writer left, the interrupt sends; two seconds is a hundred
    // times what the line takes on the slowest port.
    let deadline = crate::timer::now_nanos().saturating_add(2_000_000_000);
    if !WRITERS.wait_until_deadline(|| HELD.load(Ordering::Relaxed) == 0, deadline) {
        return Err("the transmit ring still held a task's write two seconds later");
    }
    if stalls() != stalls_before {
        return Err("the transmit interrupt stopped coming, and the ring was polled out");
    }
    let by_interrupt = BY_INTERRUPT
        .load(Ordering::Relaxed)
        .wrapping_sub(interrupt_before);
    let by_writer = BY_WRITER
        .load(Ordering::Relaxed)
        .wrapping_sub(writer_before);
    if by_interrupt.saturating_add(BURST as u64) < line as u64 {
        return Err("a task's write was not left to the transmit interrupt");
    }
    Ok(Some(Checked {
        line,
        by_interrupt,
        by_writer,
    }))
}
