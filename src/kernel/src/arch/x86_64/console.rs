//! The 16550 UART behind COM1.
//!
//! One of the two device drivers inside the kernel — see `crate::console` for
//! why it is here at all rather than in userspace.

use core::sync::atomic::{AtomicBool, Ordering};

use super::cpu;

/// COM1. Fixed by convention since the IBM PC and still where firmware puts the
/// debug console, which is what `-serial stdio` connects to.
const COM1: u16 = 0x3F8;

/// Transmit and receive buffer, and the low half of the divisor when the
/// divisor latch is open.
const DATA: u16 = COM1;
/// Interrupt enable, and the high half of the divisor.
const INTERRUPT_ENABLE: u16 = COM1 + 1;
/// FIFO control.
const FIFO_CONTROL: u16 = COM1 + 2;
/// Line control, whose top bit is the divisor latch.
const LINE_CONTROL: u16 = COM1 + 3;
/// Modem control.
const MODEM_CONTROL: u16 = COM1 + 4;
/// Line status.
const LINE_STATUS: u16 = COM1 + 5;
/// Scratch: a byte of storage every 16450 and 16550 has and nothing reads.
const SCRATCH: u16 = COM1 + 7;

/// `LINE_CONTROL`: open the divisor latch.
const DIVISOR_LATCH: u8 = 0x80;
/// `LINE_CONTROL`: eight data bits, no parity, one stop bit.
const EIGHT_N_ONE: u8 = 0x03;
/// `FIFO_CONTROL`: enable, clear both FIFOs, interrupt at 14 bytes.
const FIFO_ENABLE: u8 = 0xC7;
/// `MODEM_CONTROL`: data terminal ready, request to send, auxiliary output 2.
const MODEM_READY: u8 = 0x0B;
/// `LINE_STATUS`: a received byte is waiting in the data register.
const DATA_READY: u8 = 1;
/// `LINE_STATUS`: the transmit holding register is empty.
const TRANSMIT_EMPTY: u8 = 1 << 5;

/// Divisor for 115200 baud from the 1.8432 MHz clock.
const DIVISOR: u16 = 1;

/// Configure the port.
///
/// Firmware has usually done this already, but "usually" is not a property to
/// build a panic handler on.
pub(crate) fn init() {
    if !probe() {
        return;
    }
    PRESENT.store(true, Ordering::Relaxed);
    // Interrupts off: there is no interrupt controller yet, and a panic must be
    // able to print without one. `enable_receive_interrupt` turns receive on
    // once the I/O APIC routes the line.
    write_register(INTERRUPT_ENABLE, 0x00);
    write_register(LINE_CONTROL, DIVISOR_LATCH);
    write_register(DATA, DIVISOR as u8);
    write_register(INTERRUPT_ENABLE, (DIVISOR >> 8) as u8);
    write_register(LINE_CONTROL, EIGHT_N_ONE);
    write_register(FIFO_CONTROL, FIFO_ENABLE);
    write_register(MODEM_CONTROL, MODEM_READY);
    HAS_FIFOS.store(
        read_register(INTERRUPT_ID) & FIFOS_ENABLED == FIFOS_ENABLED,
        Ordering::Relaxed,
    );
}

/// The ISA interrupt COM1 raises.
pub(crate) const ISA_IRQ: u8 = 4;

/// Interrupt enable register: raise the port's line while received data waits.
const RECEIVE_DATA_AVAILABLE: u8 = 1;
/// Interrupt enable register: raise the port's line while the transmit holding
/// register — the whole transmit FIFO, with the FIFO on — is empty.
const TRANSMIT_HOLDING_EMPTY: u8 = 1 << 1;

/// Interrupt identification, read at [`FIFO_CONTROL`]'s address.
const INTERRUPT_ID: u16 = COM1 + 2;
/// `INTERRUPT_ID`: set when the port has no reason to interrupt.
const NO_INTERRUPT_PENDING: u8 = 1;
/// `INTERRUPT_ID`: both bits set when the FIFOs are on — a 16550A. An 8250 or
/// a 16450 has none, and ignores the write that asks for them.
const FIFOS_ENABLED: u8 = 0xC0;

/// How many bytes the transmit FIFO holds.
const FIFO_DEPTH: usize = 16;

/// Whether [`init`] found a UART at `COM1`. A PC without one -- most desktop
/// boards today have no COM1 on their back panel and many none at all --
/// reads every register of the absent port as `0xFF`; nothing is sent to it,
/// and its interrupt is never routed, rather than the console trusting a
/// line status that always says "empty" and a receiver that always says
/// "ready" (`docs/BAREMETAL.md`).
static PRESENT: AtomicBool = AtomicBool::new(false);

/// Whether a UART answers at `COM1`: its line status is not the all-ones an
/// absent port reads as, and its scratch register keeps what is written to it.
fn probe() -> bool {
    if read_register(LINE_STATUS) == 0xFF {
        return false;
    }
    [0x5A, 0xA5].into_iter().all(|pattern| {
        write_register(SCRATCH, pattern);
        read_register(SCRATCH) == pattern
    })
}

/// Whether [`init`] found a port: what the console's interrupt routing and
/// the boot line ask before using it.
pub(crate) fn present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

/// Whether [`init`] found the FIFOs on. Asked once, there: reading
/// [`INTERRUPT_ID`] also acknowledges a transmit interrupt, which a question
/// asked on every write must not do.
static HAS_FIFOS: AtomicBool = AtomicBool::new(false);

/// Let the port raise its interrupt when a byte arrives.
///
/// Reading the byte clears it, which [`read_byte`] does. `MODEM_CONTROL`
/// already sets `OUT2`, the bit that on a PC connects the 16550's interrupt
/// output to the interrupt controller at all. The transmit half is left as it
/// is: `console::output` turns it on and off as it has something to send.
pub(crate) fn enable_receive_interrupt() {
    if !present() {
        return;
    }
    write_register(
        INTERRUPT_ENABLE,
        read_register(INTERRUPT_ENABLE) | RECEIVE_DATA_AVAILABLE,
    );
}

/// How many bytes the port can take now without anyone waiting: a FIFO's
/// worth once the holding register says it is empty, nothing until then. A
/// port without FIFOs takes one.
pub(crate) fn transmit_room() -> usize {
    // No port: every byte is taken at once and goes nowhere, so the ring
    // never waits on an interrupt that cannot come.
    if !present() {
        return FIFO_DEPTH;
    }
    if read_register(LINE_STATUS) & TRANSMIT_EMPTY == 0 {
        return 0;
    }
    if HAS_FIFOS.load(Ordering::Relaxed) {
        FIFO_DEPTH
    } else {
        1
    }
}

/// Hand the port one byte, for a caller [`transmit_room`] said it had room
/// for.
pub(crate) fn put(byte: u8) {
    if !present() {
        return;
    }
    write_register(DATA, byte);
}

/// Let the port interrupt when its transmit FIFO has emptied, or stop it.
///
/// Turned on with the holding register already empty, a 16550 interrupts at
/// once, which is how a queue that starts on an idle port gets going.
pub(crate) fn transmit_interrupt(on: bool) {
    if !present() {
        return;
    }
    let enabled = read_register(INTERRUPT_ENABLE);
    let wanted = if on {
        enabled | TRANSMIT_HOLDING_EMPTY
    } else {
        enabled & !TRANSMIT_HOLDING_EMPTY
    };
    write_register(INTERRUPT_ENABLE, wanted);
}

/// Whether the port still has a reason to interrupt, which its handler asks
/// before it returns: see `console::input`'s handler for why a 16550 on an
/// edge-triggered line has to be asked.
///
/// Reading the identification register is what acknowledges a transmit
/// interrupt, and the handler wants exactly that: an empty FIFO it has
/// nothing more for should stop asking.
pub(crate) fn interrupt_pending() -> bool {
    if !present() {
        return false;
    }
    read_register(INTERRUPT_ID) & NO_INTERRUPT_PENDING == 0
}

/// What the port sends from, for the boot line that says how the console
/// sends.
pub(crate) fn transmit_buffer() -> &'static str {
    if !present() {
        return "no port: COM1 does not answer";
    }
    if HAS_FIFOS.load(Ordering::Relaxed) {
        "a 16550's FIFO"
    } else {
        "a 16450's one register"
    }
}

/// Write one of COM1's registers.
///
/// Safe, and the reason it is safe is worth stating once here rather than at
/// seven call sites: every `port` passed in is one of the constants above, all
/// of which name a register of the 16550 the platform fixes at `COM1`.
/// `MODEM_CONTROL`: loopback, the transmitter wired to the receiver.
const LOOPBACK: u8 = 1 << 4;

/// Send `byte` to the port's own receiver, through its loopback, and wait
/// until it has left the transmitter: for interrupt remapping's check R5,
/// which then requires it to arrive by the line's interrupt. The caller has
/// the port quiet (`console::output::with_port_quiet`), so no byte a writer
/// sends is looped back with it.
pub(crate) fn loop_back(byte: u8) {
    /// `LINE_STATUS`: the holding register and the shift register are both
    /// empty.
    const TRANSMITTER_IDLE: u8 = 1 << 6;
    if !present() {
        return;
    }
    write_register(MODEM_CONTROL, MODEM_READY | LOOPBACK);
    write_register(DATA, byte);
    for _ in 0..1_000_000 {
        if read_register(LINE_STATUS) & TRANSMITTER_IDLE != 0 {
            break;
        }
        core::hint::spin_loop();
    }
    write_register(MODEM_CONTROL, MODEM_READY);
}

fn write_register(port: u16, value: u8) {
    // SAFETY: (DEVICE) `port` is one of COM1's own registers and `value` is the setting
    // the device's documented initialisation sequence calls for.
    unsafe { cpu::outb(port, value) };
}

/// Read one of COM1's registers.
fn read_register(port: u16) -> u8 {
    // SAFETY: (DEVICE) as `write_register`. Reading `LINE_STATUS` and
    // `INTERRUPT_ENABLE` has no side effects; reading `INTERRUPT_ID`
    // acknowledges a transmit interrupt, and `DATA` consumes a received
    // character, which is what their callers read them for.
    unsafe { cpu::inb(port) }
}

/// Send one byte, waiting for room.
///
/// The wait is bounded rather than a bare loop: a panic that hangs forever
/// inside the console because no serial port answered is strictly worse than a
/// panic nobody can read.
pub(crate) fn write_byte(byte: u8) {
    const SPIN_LIMIT: u32 = 100_000;

    if !present() {
        return;
    }
    for _ in 0..SPIN_LIMIT {
        if read_register(LINE_STATUS) & TRANSMIT_EMPTY != 0 {
            break;
        }
        core::hint::spin_loop();
    }

    // If the wait above timed out the byte is dropped by the hardware, which is
    // the right outcome for a port nothing is listening to.
    write_register(DATA, byte);
}

/// Wait until everything written has left the port, for a caller about to
/// power off or stop.
///
/// [`write_byte`] waits for the holding register, which empties as soon as the
/// byte moves to the shift register, so the last byte can still be sending when
/// it returns. Harmless under QEMU, whose port sends instantly; on hardware a
/// power-off straight after would cut the line, as it did on a DK1's USART.
/// Bounded, like the write.
pub(crate) fn drain() {
    /// `LINE_STATUS`: the holding register and the shift register are both
    /// empty.
    const TRANSMITTER_IDLE: u8 = 1 << 6;
    /// Far longer than a sixteen-byte FIFO takes to empty at 115200 baud.
    const DRAIN_LIMIT: u32 = 10_000_000;

    if !present() {
        return;
    }
    for _ in 0..DRAIN_LIMIT {
        if read_register(LINE_STATUS) & TRANSMITTER_IDLE != 0 {
            return;
        }
        core::hint::spin_loop();
    }
}

/// One received byte, if the port has one.
///
/// A plain read, for both paths: polled before the port's receive interrupt is
/// installed and during a panic, which is why [`init`] leaves the port's
/// interrupts off; and called from that interrupt once `console::input` has
/// routed it, to empty the port into the receive ring.
pub(crate) fn read_byte() -> Option<u8> {
    if !present() {
        return None;
    }
    if read_register(LINE_STATUS) & DATA_READY == 0 {
        return None;
    }
    Some(read_register(DATA))
}
