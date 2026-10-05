//! What the console port has received, between the interrupt that took it and
//! the thread that reads it.
//!
//! # Why a ring sits between the two
//!
//! The line discipline in `fs::terminal` echoes, raises signals and walks the
//! process registry, and none of that is work for an interrupt handler. What
//! cannot wait is emptying the port: a PL011 holds sixteen bytes, an STM32
//! USART without its FIFO holds one, and at 115200 baud the next byte arrives
//! in under a tenth of a millisecond. So the receive interrupt moves the bytes
//! here and wakes whoever waits, and the `console` thread takes them out
//! through `arch::read_console_byte` at its own pace.
//!
//! A port whose receive interrupt the kernel has not installed never touches
//! the ring: [`read_byte`] polls it, as every port was polled before.
//!
//! The port raises one interrupt for both directions, so the handler installed
//! here also serves the transmit side, `console::output`, whose ring is this
//! module's [`Ring`] run the other way.

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};

use ferrix_bootinfo::BootView;
use ferrix_sync::IrqSpinLock;

use crate::arch;
use crate::sched::WaitQueue;

/// Bytes held for the reader, as many as Linux's line discipline holds. One
/// more is dropped and counted in [`overruns`]. The transmit ring is the same
/// size, which is Linux's too: a serial port's transmit buffer is a page.
pub(super) const CAPACITY: usize = 4096;

/// The most bytes one call to [`receive`] takes from the port.
///
/// Twice what the ring holds: more than any port delivers while its interrupt
/// is being served, so a port that is merely busy is always emptied, and a
/// port that never reports itself empty still lets the handler return.
const RECEIVE_LIMIT: usize = CAPACITY * 2;

/// A fixed ring of bytes: received ones here, and the ones waiting to be sent
/// in `console::output`.
pub(super) struct Ring {
    /// The storage, used from `head` round to `head + len`.
    bytes: [u8; CAPACITY],
    /// Where the oldest byte is.
    head: usize,
    /// How many bytes are held.
    len: usize,
}

impl Ring {
    /// An empty ring.
    pub(super) const fn new() -> Ring {
        Ring {
            bytes: [0; CAPACITY],
            head: 0,
            len: 0,
        }
    }

    /// How many bytes are held.
    pub(super) const fn len(&self) -> usize {
        self.len
    }

    /// Append `byte`, or refuse it when the ring is full.
    pub(super) fn push(&mut self, byte: u8) -> bool {
        if self.len >= CAPACITY {
            return false;
        }
        let mut at = self.head.wrapping_add(self.len);
        if at >= CAPACITY {
            at = at.wrapping_sub(CAPACITY);
        }
        let Some(slot) = self.bytes.get_mut(at) else {
            return false;
        };
        *slot = byte;
        self.len = self.len.wrapping_add(1);
        true
    }

    /// Take the oldest byte.
    pub(super) fn pop(&mut self) -> Option<u8> {
        if self.len == 0 {
            return None;
        }
        let byte = self.bytes.get(self.head).copied();
        self.head = self.head.wrapping_add(1);
        if self.head >= CAPACITY {
            self.head = 0;
        }
        self.len = self.len.wrapping_sub(1);
        byte
    }
}

/// The received bytes. Interrupt-masking, because the receive handler fills it.
static RING: IrqSpinLock<Ring, arch::Irq> = IrqSpinLock::new(Ring::new());

/// How many bytes [`RING`] holds, kept beside it so that [`has_input`] takes no
/// lock. Written only under the ring's lock, and before any wake that follows.
static HELD: AtomicUsize = AtomicUsize::new(0);

/// Bytes dropped because the ring was full.
static OVERRUNS: AtomicU64 = AtomicU64::new(0);

/// Whether the port's receive interrupt is installed, so that input arrives
/// through the ring rather than by polling.
static INTERRUPT_DRIVEN: AtomicBool = AtomicBool::new(false);

/// Whoever waits for input: the `console` thread.
static WAITERS: WaitQueue = WaitQueue::new();

/// The byte an x86-64 interrupt remapping check loops back through the port
/// and waits for the receive path to read: `0` while no check waits,
/// [`CHECK_ARMED`] with the byte while one does, [`CHECK_TAKEN`] with the
/// byte once [`receive`] has read it from the port.
///
/// The receive path keeps such a byte out of the ring. In the ring the
/// `console` thread could take it first -- it drains the ring every 20 ms
/// once anything has read the console -- and echo it, and the check would
/// find nothing (FX-1012).
static CHECK_BYTE: AtomicU16 = AtomicU16::new(0);

/// [`CHECK_BYTE`]'s mark for a byte a check waits for.
const CHECK_ARMED: u16 = 0x100;

/// [`CHECK_BYTE`]'s mark for a byte the receive path has read.
const CHECK_TAKEN: u16 = 0x200;

/// What [`check`] established.
pub(crate) struct Checked {
    /// Bytes the ring held and gave back in order.
    pub(crate) held: usize,
    /// Bytes offered past its capacity and counted rather than kept.
    pub(crate) dropped: u64,
}

/// Install the console port's receive interrupt, if the machine has one this
/// kernel can take, and return its number.
///
/// `Ok(None)` when input stays polled: on an x86-64 machine whose MADT gives
/// COM1's line no I/O APIC input, and on an Arm machine whose device tree names
/// the port's interrupt in a shape `ferrix_fdt` does not follow.
///
/// Call with interrupts masked, before they are first enabled.
///
/// # Errors
///
/// When something else already holds the port's interrupt number.
pub(crate) fn init(view: &BootView<'_>) -> Result<Option<u32>, &'static str> {
    let Some(irq) = arch::console_receive_irq(view) else {
        return Ok(None);
    };
    crate::irq::register(irq, on_interrupt)
        .map_err(|_| "the console port's receive interrupt is already taken")?;
    // Before the port may raise it: from here the reader looks in the ring,
    // and a byte the port already holds arrives there the moment the line is
    // enabled. The transmit side starts with it: the handler serves both, and
    // from here a writer with interrupts on queues rather than polls.
    INTERRUPT_DRIVEN.store(true, Ordering::Relaxed);
    super::output::start();
    arch::enable_console_receive(irq);
    Ok(Some(irq))
}

/// The port's interrupt: empty the port into the ring and wake the reader,
/// then give the port what it has room for of what is waiting to be sent.
///
/// Again while the port still says it wants attention, which only a 16550
/// ever does. Its line reaches the I/O APIC edge-triggered, and it stays high
/// while *any* reason to interrupt is pending: a byte received after the
/// receive half looked, while the transmit half was still filling the FIFO,
/// would find the line already high, raise no edge, and never be taken. Linux's
/// 8250 driver loops on its identification register for the same reason. The
/// Arm ports' lines are level-triggered, and they say no at once.
fn on_interrupt(_irq: u32) {
    /// More passes than emptying a full transmit ring takes, a burst at a
    /// time, so that a port that never stops asking still lets the handler
    /// return.
    const PASSES: usize = CAPACITY / super::output::BURST + 4;

    for _ in 0..PASSES {
        receive(arch::take_console_byte);
        super::output::on_interrupt();
        if !arch::console::interrupt_pending() {
            break;
        }
    }
}

/// Take what `take` yields into the ring, then wake whoever waits for input.
///
/// `take` is called until it has nothing more, so the port is emptied even
/// when the ring is full: a byte that does not fit is counted in [`overruns`]
/// rather than left in the port to hold its interrupt asserted. The byte a
/// check waits for ([`arm_check_byte`]) is taken once and not kept.
fn receive(mut take: impl FnMut() -> Option<u8>) {
    let mut added = false;
    {
        let mut ring = RING.lock();
        for _ in 0..RECEIVE_LIMIT {
            let Some(byte) = take() else {
                break;
            };
            if CHECK_BYTE
                .compare_exchange(
                    CHECK_ARMED | u16::from(byte),
                    CHECK_TAKEN | u16::from(byte),
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                continue;
            }
            // NOALLOC: a fixed ring, which refuses when full.
            if ring.push(byte) {
                added = true;
            } else {
                let _ = OVERRUNS.fetch_add(1, Ordering::Relaxed);
            }
        }
        HELD.store(ring.len, Ordering::Relaxed);
    }
    // After the lock is dropped: the wake takes run-queue locks, and the
    // section with interrupts masked should stay as short as the copy.
    if added {
        waiters().wake_all();
    }
}

/// Wait for the receive path to read `byte` from the port, for x86-64's
/// interrupt remapping checks G3 and R5: from here until
/// [`disarm_check_byte`], the first `byte` [`receive`] reads is taken
/// rather than put in the ring, so that it never reaches a reader of the
/// console. Call before looping `byte` back.
pub(crate) fn arm_check_byte(byte: u8) {
    CHECK_BYTE.store(CHECK_ARMED | u16::from(byte), Ordering::Release);
}

/// Whether the receive path has read `byte` since [`arm_check_byte`].
pub(crate) fn check_byte_taken(byte: u8) -> bool {
    CHECK_BYTE.load(Ordering::Acquire) == CHECK_TAKEN | u16::from(byte)
}

/// Stop waiting for `byte`, and say whether the receive path read it since
/// [`arm_check_byte`]. A `byte` read after this goes into the ring.
pub(crate) fn disarm_check_byte(byte: u8) -> bool {
    CHECK_BYTE.swap(0, Ordering::AcqRel) == CHECK_TAKEN | u16::from(byte)
}

/// Take `byte` from the front of the ring, if it is there: for x86-64's
/// check R5, whose looped-back byte, delivered only after the check
/// stopped waiting for it, must not reach a reader of the console.
pub(crate) fn take_check_byte(byte: u8) -> bool {
    let mut ring = RING.lock();
    if ring.len() == 0 || ring.bytes.get(ring.head).copied() != Some(byte) {
        return false;
    }
    let _ = ring.pop();
    HELD.store(ring.len, Ordering::Relaxed);
    true
}

fn pop() -> Option<u8> {
    let mut ring = RING.lock();
    let byte = ring.pop();
    HELD.store(ring.len, Ordering::Relaxed);
    byte
}

/// The next byte typed at the console, if one is waiting.
///
/// From the ring once the receive interrupt is installed, and straight from the
/// port through `poll` until then — or for good, on a port with no interrupt.
pub(crate) fn read_byte(poll: fn() -> Option<u8>) -> Option<u8> {
    if interrupt_driven() { pop() } else { poll() }
}

/// Whether the ring holds a byte. Takes no lock, so it can be a wait's
/// condition.
pub(crate) fn has_input() -> bool {
    HELD.load(Ordering::Relaxed) != 0
}

/// The queue the receive interrupt wakes when it adds to the ring.
pub(crate) fn waiters() -> &'static WaitQueue {
    &WAITERS
}

/// How many received bytes were dropped because the ring was full.
pub(crate) fn overruns() -> u64 {
    OVERRUNS.load(Ordering::Relaxed)
}

/// Whether input arrives by interrupt. When it does not, a reader has to poll.
pub(crate) fn interrupt_driven() -> bool {
    INTERRUPT_DRIVEN.load(Ordering::Relaxed)
}

/// The ring, checked before the port can add to it: bytes offered come back in
/// order, the ones past its capacity are counted rather than kept, a wait for
/// input returns at once while input is there, and an emptied ring says so.
///
/// # Errors
///
/// A description of the first property that did not hold. Also when called
/// after [`init`] installed the interrupt, which would race the port.
pub(crate) fn check() -> Result<Checked, &'static str> {
    const EXTRA: usize = 3;

    if interrupt_driven() {
        return Err("the input ring was checked after the port could fill it");
    }
    let dropped_before = overruns();

    let offered = CAPACITY.wrapping_add(EXTRA);
    let mut given = 0usize;
    receive(|| {
        if given >= offered {
            return None;
        }
        let byte = given.to_le_bytes()[0];
        given = given.wrapping_add(1);
        Some(byte)
    });

    if !has_input() {
        return Err("the input ring said it was empty with bytes in it");
    }
    if !waiters().wait_until_deadline(has_input, crate::timer::now_nanos()) {
        return Err("a wait for input did not return while input was waiting");
    }
    for expected in 0..CAPACITY {
        match pop() {
            Some(byte) if byte == expected.to_le_bytes()[0] => {}
            Some(_) => return Err("the input ring gave bytes back out of order"),
            None => return Err("the input ring gave back fewer bytes than it held"),
        }
    }
    if pop().is_some() {
        return Err("the input ring gave back more bytes than it has room for");
    }
    if has_input() {
        return Err("the input ring said it held input after it was emptied");
    }
    let dropped = overruns().wrapping_sub(dropped_before);
    if dropped != EXTRA as u64 {
        return Err("the input ring did not count the bytes it had no room for");
    }
    Ok(Checked {
        held: CAPACITY,
        dropped,
    })
}
