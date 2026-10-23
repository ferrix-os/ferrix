//! Which console this machine has: the PL011 UART, a 16550 behind `MMIO`, or
//! on a machine without either the kernel can reach, a console record in
//! `ramoops` memory.
//!
//! One of the two device drivers inside the kernel — see `crate::console` for
//! why it is here at all rather than in userspace. The PL011 is
//! `arch::arm_common::pl011`, shared with ARMv7-A; the other two are this
//! architecture's alone and live here.
//!
//! The `ramoops` backend is for the Pixel 7, whose UART is behind a debug
//! accessory on its USB-C port. Its loader passes
//! `console=ramoops,<address>,<size>`, naming the console zone of the region
//! Android's kernel keeps its log in, and has already started a record there;
//! this appends to it, in Linux's `persistent_ram_buffer` format, so the boot
//! log survives the watchdog reset that ends a run and Android shows it as
//! `/sys/fs/pstore/console-ramoops-0`. It has no input and never waits.
//!
//! The 16550 is for the guest crosvm makes on the Pixel 7, whose console is an
//! `ns16550a` at `MMIO` `0x3f8`: `console=uart8250,mmio,<address>`, as Linux
//! spells it. Its registers are bytes, one apart. It is polled both ways, and
//! crosvm's port sends each byte as it is written.
//!
//! Unlike x86-64's 16550, these are `MMIO`, so it has to be *mapped* before
//! it can be written, and mapped as device memory: through a normal cacheable
//! mapping the writes may be merged, reordered or held in a cache line, and the
//! symptom is a console that prints nothing at all.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_bootinfo::{BootView, KERNEL_VMAP_BASE, PAGE_SIZE};

use crate::arch::arm_common::pl011;
use crate::early::{EarlyError, EarlyMemory};

/// Physical address of the PL011 on `QEMU`'s `virt` machine.
///
/// **A stage 1 shortcut, and the only hardcoded device address in the tree.**
/// The right answer comes from the device tree's `stdout-path` or from ACPI's
/// SPCR table, both of which the loader already hands over
/// (`BootInfo::dtb`, `BootInfo::rsdp`); parsing them is stage 3 work, and until
/// then the boot test needs somewhere to talk.
const PL011_PHYS: u64 = 0x0900_0000;

/// Where a 16550's registers or a `ramoops` zone is mapped: the bottom of the
/// kernel's dynamic mapping area, where the PL011 driver maps its window when
/// the console is the PL011 instead.
const WINDOW: u64 = KERNEL_VMAP_BASE;

/// The mapped base address of a 16550 or a `ramoops` zone, once [`init`] has
/// mapped one. Zero when the console is the PL011, which keeps its own.
struct Base(UnsafeCell<u64>);

// SAFETY: (SHARED) early boot is single-threaded — no other CPU has been started and
// interrupts are masked — so there is never a second accessor.
unsafe impl Sync for Base {}

static BASE: Base = Base(UnsafeCell::new(0));

/// `PERSISTENT_RAM_SIG`, "DBGC": the first word of a `ramoops` record.
const RAMOOPS_SIGNATURE: u32 = 0x4347_4244;

/// Bytes of a `ramoops` record's header: the signature, the write offset and
/// the number of valid bytes.
const RAMOOPS_HEADER: u64 = 12;

/// Bytes of text the `ramoops` zone holds, or zero when the console is a
/// UART. Set once by [`init`], before any output, and only after [`BASE`].
static RAMOOPS_CAPACITY: AtomicU64 = AtomicU64::new(0);

/// Bytes of text in the `ramoops` record. Output is serialised by
/// `crate::console`, so a plain load and store are enough.
static RAMOOPS_LENGTH: AtomicU64 = AtomicU64::new(0);

/// Whether the console is a 16550 rather than the PL011. Set once by
/// [`init`], before any output, and only after [`BASE`].
static NS16550: AtomicBool = AtomicBool::new(false);

/// 16550 transmit holding and receive buffer register.
const UART_DATA: u64 = 0;
/// 16550 line status register.
const UART_LSR: u64 = 5;
/// `UART_LSR`: a received byte is waiting.
const LSR_DATA_READY: u8 = 1 << 0;
/// `UART_LSR`: the transmit holding register can take a byte.
const LSR_THR_EMPTY: u8 = 1 << 5;
/// `UART_LSR`: the transmitter is idle, holding register and shift register
/// both empty.
const LSR_TX_IDLE: u8 = 1 << 6;

/// Parse `uart8250,mmio,<address>`, the address in hexadecimal with a `0x`
/// prefix.
pub(super) fn ns16550_port(value: &str) -> Option<u64> {
    let address = value.strip_prefix("uart8250,mmio,")?;
    u64::from_str_radix(address.strip_prefix("0x")?, 16).ok()
}

/// True when the console is a 16550.
pub(crate) fn is_ns16550() -> bool {
    NS16550.load(Ordering::Relaxed)
}

/// Read one of the 16550's byte registers.
fn read_u8(offset: u64) -> u8 {
    // SAFETY: (SHARED) single-threaded, as documented on the `Sync` impl above.
    let base = unsafe { *BASE.0.get() };
    // SAFETY: (DEVICE) `base` is the port `init` mapped -- `NS16550` is set only after
    // it -- and `offset` one of its eight registers, all inside the mapped page.
    unsafe { core::ptr::read_volatile((base + offset) as *const u8) }
}

/// Write one of the 16550's byte registers.
fn write_u8(offset: u64, value: u8) {
    // SAFETY: (SHARED) single-threaded, as documented on the `Sync` impl above.
    let base = unsafe { *BASE.0.get() };
    // SAFETY: (DEVICE) as in `read_u8`.
    unsafe { core::ptr::write_volatile((base + offset) as *mut u8, value) };
}

/// Parse `ramoops,<address>,<size>`, the loader's `console=` value, both
/// numbers in hexadecimal with a `0x` prefix.
pub(super) fn ramoops_zone(value: &str) -> Option<(u64, u64)> {
    let mut fields = value.strip_prefix("ramoops,")?.split(',');
    let number = |text: &str| u64::from_str_radix(text.strip_prefix("0x")?, 16).ok();
    let base = number(fields.next()?)?;
    let size = number(fields.next()?)?;
    (fields.next().is_none() && base.is_multiple_of(PAGE_SIZE) && size > RAMOOPS_HEADER)
        .then_some((base, size))
}

/// Map the console the command line names -- the PL011, a 16550 or a
/// `ramoops` zone -- and record where it landed.
pub(crate) fn init(view: &BootView<'_>, memory: &mut EarlyMemory) -> Result<(), EarlyError> {
    if let Some(port) = view.option("console").and_then(ns16550_port) {
        let page = port & !(PAGE_SIZE - 1);
        memory.map_device(WINDOW, page, PAGE_SIZE)?;
        // SAFETY: (SHARED) single-threaded, as documented on the `Sync` impl above.
        unsafe { *BASE.0.get() = WINDOW + (port - page) };
        NS16550.store(true, Ordering::Relaxed);
        return Ok(());
    }
    if let Some((base, size)) = view.option("console").and_then(ramoops_zone) {
        // Device memory, so every byte reaches RAM in order and survives the
        // reset with no cache to be cleaned first.
        memory.map_device(WINDOW, base, size)?;
        // SAFETY: (SHARED) single-threaded, as documented on the `Sync` impl above.
        unsafe { *BASE.0.get() = WINDOW };
        let capacity = size - RAMOOPS_HEADER;
        // Continue the loader's record rather than start another, so the two
        // programs' logs read as one.
        let length = if ramoops_read(0) == RAMOOPS_SIGNATURE {
            u64::from(ramoops_read(8)).min(capacity)
        } else {
            0
        };
        RAMOOPS_LENGTH.store(length, Ordering::Relaxed);
        RAMOOPS_CAPACITY.store(capacity, Ordering::Relaxed);
        ramoops_write(0, RAMOOPS_SIGNATURE);
        return Ok(());
    }
    pl011::init(memory, PL011_PHYS)
}

/// True when the console is a `ramoops` record rather than a UART.
pub(crate) fn is_ramoops() -> bool {
    RAMOOPS_CAPACITY.load(Ordering::Relaxed) != 0
}

/// Append one byte to the `ramoops` record, dropping it once the zone is full.
fn ramoops_append(byte: u8) {
    let length = RAMOOPS_LENGTH.load(Ordering::Relaxed);
    if length >= RAMOOPS_CAPACITY.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY: (SHARED) single-threaded, as documented on the `Sync` impl above.
    let base = unsafe { *BASE.0.get() };
    // SAFETY: (DEVICE) the byte is inside the zone `init` mapped, past its header and
    // below its capacity.
    unsafe { core::ptr::write_volatile((base + RAMOOPS_HEADER + length) as *mut u8, byte) };
    let length = length + 1;
    RAMOOPS_LENGTH.store(length, Ordering::Relaxed);
    // `ramoops` zones are far smaller than 4 GiB.
    let word = u32::try_from(length).unwrap_or(u32::MAX);
    ramoops_write(4, word);
    ramoops_write(8, word);
}

/// Read one word of the `ramoops` record's header.
fn ramoops_read(offset: u64) -> u32 {
    // SAFETY: (SHARED) single-threaded, as documented on the `Sync` impl above.
    let base = unsafe { *BASE.0.get() };
    // SAFETY: (DEVICE) `base` is the zone `init` has just mapped or mapped earlier, and
    // `offset` is one of the three header words at its start.
    unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
}

/// Write one word of the `ramoops` record's header.
fn ramoops_write(offset: u64, value: u32) {
    // SAFETY: (SHARED) single-threaded, as documented on the `Sync` impl above.
    let base = unsafe { *BASE.0.get() };
    // SAFETY: (DEVICE) as in `ramoops_read`.
    unsafe { core::ptr::write_volatile((base + offset) as *mut u32, value) };
}

/// Send one byte, waiting for room in the transmit FIFO.
///
/// Firmware configured the baud rate and line format before handing over and
/// this driver does not disturb them: reprogramming a port a terminal is
/// already attached to is how a boot log turns into line noise halfway through.
///
/// The wait is bounded rather than a bare loop, because a panic that hangs
/// inside the console because nothing answered is worse than one nobody reads.
pub(crate) fn write_byte(byte: u8) {
    const SPIN_LIMIT: u32 = 100_000;

    if is_ramoops() {
        ramoops_append(byte);
        return;
    }
    if !is_ns16550() {
        pl011::write_byte(byte);
        return;
    }
    for _ in 0..SPIN_LIMIT {
        if read_u8(UART_LSR) & LSR_THR_EMPTY != 0 {
            break;
        }
        core::hint::spin_loop();
    }
    write_u8(UART_DATA, byte);
}

/// Wait until everything written has left the port, for a caller about to
/// power off or stop. Bounded, like the write; see `pl011::drain` for why.
pub(crate) fn drain() {
    /// About half a second of polling, as the PL011's.
    const DRAIN_LIMIT: u32 = 10_000_000;

    if is_ramoops() {
        return;
    }
    if !is_ns16550() {
        pl011::drain();
        return;
    }
    for _ in 0..DRAIN_LIMIT {
        if read_u8(UART_LSR) & LSR_TX_IDLE != 0 {
            return;
        }
        core::hint::spin_loop();
    }
}

/// One received byte, if the port holds one. A `ramoops` record has no input.
pub(crate) fn read_byte() -> Option<u8> {
    if is_ramoops() {
        return None;
    }
    if !is_ns16550() {
        return pl011::read_byte();
    }
    (read_u8(UART_LSR) & LSR_DATA_READY != 0).then(|| read_u8(UART_DATA))
}

/// Let the port interrupt when it has received something: the PL011 only,
/// since the 16550 is polled and a `ramoops` record has no input.
pub(crate) fn enable_receive_interrupt() {
    if !is_ramoops() && !is_ns16550() {
        pl011::enable_receive_interrupt();
    }
}

/// How many bytes the port can take now without anyone waiting: one while the
/// transmit FIFO or holding register has room, which the caller asks again
/// after each.
pub(crate) fn transmit_room() -> usize {
    // A record in memory always has room: a full zone drops the byte instead.
    if is_ramoops() {
        return 1;
    }
    if !is_ns16550() {
        return pl011::transmit_room();
    }
    usize::from(read_u8(UART_LSR) & LSR_THR_EMPTY != 0)
}

/// Hand the port one byte, for a caller [`transmit_room`] said it had room
/// for.
pub(crate) fn put(byte: u8) {
    if is_ramoops() {
        ramoops_append(byte);
    } else if is_ns16550() {
        write_u8(UART_DATA, byte);
    } else {
        pl011::put(byte);
    }
}

/// Let the PL011 interrupt when its transmit FIFO has drained, or stop it; the
/// other two never interrupt.
pub(crate) fn transmit_interrupt(on: bool) {
    if !is_ramoops() && !is_ns16550() {
        pl011::transmit_interrupt(on);
    }
}

/// Whether the port still has a reason to interrupt: never worth asking, since
/// its line is level-triggered and raises the interrupt again by itself. See
/// `console::input`'s handler for the port that has to be asked.
#[expect(
    clippy::missing_const_for_fn,
    reason = "another architecture's version of this reads the port"
)]
pub(crate) fn interrupt_pending() -> bool {
    false
}

/// What the port sends from, for the boot line that says how the console
/// sends.
pub fn transmit_buffer() -> &'static str {
    if is_ramoops() {
        "a ramoops record"
    } else if is_ns16550() {
        "a 16550's holding register"
    } else {
        "a PL011's FIFO"
    }
}
