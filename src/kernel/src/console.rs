//! The kernel's early console.
//!
//! This is one of the two output devices the kernel drives itself, and
//! `docs/ARCHITECTURE.md` §1 names both as the exceptions they are: every other
//! device is driven from userspace. The other is the framebuffer a panic is
//! drawn on (`panic/screen.rs`), which draws what this module keeps of its
//! recent output, and which [`screen`] draws these lines on as they are
//! printed when the command line asks for it. It exists because a panic before `devmgr` starts has to say
//! something, and because a boot test with no serial output cannot tell a
//! kernel that hung from one that never ran.
//!
//! The port itself is architecture-specific — a 16550 behind x86-64 I/O ports,
//! a PL011 behind `MMIO` on `AArch64` — so the bytes go out through
//! `arch::console`.
//!
//! # One line at a time
//!
//! The port is behind a lock, taken once per [`println!`], so two CPUs
//! printing at once produce two whole lines rather than one line of both.
//! Behind the lock is a ring of bytes waiting to be sent, which the port's
//! transmit interrupt empties once there is one, so that a writer does not
//! spend the whole of its write polling the port with interrupts masked: see
//! [`output`] for who queues and who still polls.
//!
//! The lock is also the one way the console can make a failure *worse*: a CPU
//! that faults while holding it — in the middle of formatting, say — would
//! report the fault by printing, wait for its own lock, and turn a
//! `FERRIX-PANIC` line into a boot test that times out saying nothing. So once
//! [`begin_panic`] has been called, a writer waits a bounded time for the lock
//! and then writes without it. A panic report interleaved with another CPU's
//! line is legible; one that never appears is not.
//!
//! # The log
//!
//! Every byte sent, the kernel's lines and programs' output alike, is also
//! recorded in the kernel [`log`], which `syslog(2)` and a log control
//! channel read. Recording is the one thing this module asks of it, and it
//! takes no lock and never waits. The lines that say where the kernel is in
//! memory are sent with [`println_unlogged!`] instead and are not recorded:
//! [`write_unlogged`] lists them.

pub(crate) mod input;
pub(crate) mod log;
pub(crate) mod log_check;
pub(crate) mod output;
pub(crate) mod pstore;
pub(crate) mod screen;

use core::fmt::{self, Write};
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use ferrix_sync::IrqSpinLock;

use self::output::Transmit;

/// Whether [`crate::arch::init_console`] has run.
static READY: AtomicBool = AtomicBool::new(false);

/// Whether something is reporting a failure the kernel will not survive.
static PANICKING: AtomicBool = AtomicBool::new(false);

/// The port, and what waits to be sent to it, one writer at a time.
///
/// Interrupt-masking so that a handler that prints cannot interrupt a line
/// being printed on its own CPU and wait for it, and because the port's
/// transmit interrupt takes it.
static PORT: IrqSpinLock<Transmit, crate::arch::Irq> = IrqSpinLock::new(Transmit::new());

/// Attempts at the lock a panicking writer makes before writing without it.
///
/// Long enough for another CPU to finish any line it is part way through, and
/// short enough that a lock nobody will ever release costs a moment rather
/// than the boot test's timeout.
const PANIC_SPINS: u32 = 10_000_000;

/// How much recent console output is kept for a failure report.
///
/// A little more than the largest QR code carries, which is the most any
/// reader of it can use.
const RECENT_BYTES: usize = 4096;

/// The last [`RECENT_BYTES`] written to the port, as a ring.
///
/// Atomics rather than a lock, because the moment this is read is a panic,
/// possibly on a processor holding the port's lock or part way through a
/// line. A racing writer can leave a byte stale; nothing can leave the ring
/// unreadable.
static RECENT: [AtomicU8; RECENT_BYTES] = [const { AtomicU8::new(0) }; RECENT_BYTES];

/// How many bytes have ever gone into [`RECENT`]. The next one goes at this
/// count modulo the ring's length.
static RECENT_WRITTEN: AtomicUsize = AtomicUsize::new(0);

/// Record that the port is configured and may be written.
///
/// # Safety
///
/// (DEVICE) The caller must have configured the port `arch::console::write_byte` writes
/// to, including any mapping it needs.
pub(crate) unsafe fn mark_ready() {
    READY.store(true, Ordering::Release);
}

/// Say that what is printed from here on is a failure report.
///
/// Called by the panic handler and by the trap path's fatal reports, before
/// they print anything.
pub(crate) fn begin_panic() {
    PANICKING.store(true, Ordering::Relaxed);
}

/// A writer holding the port: somewhere for `format_args!` and a program's
/// bytes to go.
struct Writer<'port> {
    /// The port and its ring, under the lock.
    port: &'port mut Transmit,
    /// Whether bytes go into the ring for the transmit interrupt, or straight
    /// to the port by polling.
    queued: bool,
    /// Whether bytes are kept in [`RECENT`] for a failure report: the
    /// kernel's own lines and the records programs send the log are, a
    /// program's console output is not.
    remembered: bool,
    /// Whether bytes are recorded in the kernel [`log`]: everything is but
    /// the lines [`write_unlogged`] sends.
    logged: bool,
    /// The priority they are logged at.
    priority: u8,
    /// Whether a flush on the way in made room a waiting writer can use.
    wake: bool,
}

impl<'port> Writer<'port> {
    /// Start writing to `port`. A writer that polls empties the ring first, so
    /// that its bytes follow everything queued before them; one that queues
    /// first looks for a port that has stopped taking bytes.
    fn new(port: &'port mut Transmit, queued: bool, kind: Kind) -> Writer<'port> {
        let wake = if queued { port.unstall() } else { port.flush() };
        Writer {
            port,
            queued,
            remembered: kind.remembered(),
            logged: kind.logged(),
            priority: kind.priority(),
            wake,
        }
    }

    /// One byte, as this writer sends them.
    fn put(&mut self, byte: u8) {
        if self.queued {
            self.port.queue(byte);
        } else {
            crate::arch::console::write_byte(byte);
        }
    }

    /// `bytes`, a bare newline as CRLF if `crlf`.
    fn bytes(&mut self, bytes: &[u8], crlf: bool) {
        if self.logged {
            log::record(bytes, self.priority);
        }
        for &byte in bytes {
            // A serial terminal wants CRLF; a bare newline leaves the cursor
            // where it was and the boot log becomes a staircase.
            if crlf && byte == b'\n' {
                self.put(b'\r');
            }
            self.put(byte);
            if self.remembered {
                remember(byte);
            }
            // Under `ferrix.fbcon` a program's output is drawn too: on a
            // machine whose screen is its only output (the N150 box) a
            // driver's own lines and devmgr's word on a driver that failed
            // are the ones that say why.
            screen::put(byte);
        }
    }

    /// Give the port what it has room for of what was queued, and say whether
    /// writers waiting for room should be woken once the lock is dropped.
    fn finish(self) -> bool {
        let pumped = self.queued && self.port.pump(output::by_writer());
        self.wake || pumped
    }
}

impl Write for Writer<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.bytes(text.as_bytes(), true);
        Ok(())
    }
}

/// Somewhere for a failure report to go when the port's lock cannot be had:
/// straight to the port, past the ring and whoever holds it. Recorded in the
/// kernel [`log`] if `logged`.
struct Unlocked {
    /// Whether the bytes go into the kernel log too.
    logged: bool,
}

impl Write for Unlocked {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.logged {
            log::record(text.as_bytes(), log::KERNEL);
        }
        for byte in text.bytes() {
            if byte == b'\n' {
                crate::arch::console::write_byte(b'\r');
            }
            crate::arch::console::write_byte(byte);
            remember(byte);
        }
        Ok(())
    }
}

/// Keep `byte` in the recent-output ring.
fn remember(byte: u8) {
    let at = RECENT_WRITTEN.fetch_add(1, Ordering::Relaxed);
    if let Some(slot) = RECENT.get(at % RECENT_BYTES) {
        slot.store(byte, Ordering::Relaxed);
    }
}

/// Copy the most recent console output into `out`, oldest byte first, and
/// return how many bytes were copied: at most `out.len()`, and never more
/// than the ring has kept.
pub(crate) fn recent(out: &mut [u8]) -> usize {
    let written = RECENT_WRITTEN.load(Ordering::Relaxed);
    let count = written.min(RECENT_BYTES).min(out.len());
    let start = written.saturating_sub(count);
    for (offset, slot) in out.iter_mut().take(count).enumerate() {
        *slot = RECENT
            .get(start.wrapping_add(offset) % RECENT_BYTES)
            .map_or(b'?', |byte| byte.load(Ordering::Relaxed));
    }
    count
}

/// Whether a writer here may queue for the transmit interrupt rather than
/// poll: the interrupt is installed, this processor takes interrupts, and
/// nothing is reporting a failure. Asked before the lock, which masks them.
fn may_queue() -> bool {
    output::interrupt_driven()
        && !PANICKING.load(Ordering::Relaxed)
        && crate::arch::interrupts_enabled()
}

/// Whose bytes a writer sends, which decides where else they are kept.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The kernel's own line: kept for a failure report and in the log.
    Line,
    /// A kernel line that says where the kernel is: kept for a failure
    /// report, and not in the log. See [`write_unlogged`].
    Unlogged,
    /// A program's output: kept in the log only.
    Program,
    /// A record a program sent to the log itself, `/dev/kmsg`'s, at its
    /// priority: kept in the log, as a line of its own, and for a failure
    /// report, as a kernel line is.
    Record(u8),
}

impl Kind {
    /// Whether the bytes are kept in [`RECENT`]. Every kind is drawn on the
    /// screen under `ferrix.fbcon`.
    const fn remembered(self) -> bool {
        matches!(self, Kind::Line | Kind::Unlogged | Kind::Record(_))
    }

    /// Whether the bytes are recorded in the kernel [`log`].
    const fn logged(self) -> bool {
        matches!(self, Kind::Line | Kind::Program | Kind::Record(_))
    }

    /// The priority the bytes are logged at.
    const fn priority(self) -> u8 {
        match self {
            Kind::Line | Kind::Unlogged => log::KERNEL,
            Kind::Program => log::PROGRAM,
            Kind::Record(priority) => priority,
        }
    }
}

/// Write formatted output to the console, if there is one yet, and record it
/// in the kernel [`log`].
///
/// Queued with interrupts on and polled with them masked, as [`output`]
/// explains, and never waiting for room: the idle task prints too.
pub(crate) fn write(arguments: fmt::Arguments<'_>) {
    write_as(arguments, Kind::Line);
}

/// Write formatted output to the console, as [`write`] does, but keep it out
/// of the kernel [`log`]: it goes to the port, the panic screen and the
/// recent-output ring a failure report draws, and to nothing a program can
/// read. [`println_unlogged!`] is the way in.
///
/// For the lines that print where the kernel is, and no others. The log is
/// readable by privileged programs (`syslog(2)`) and by the driver of a device
/// allowed to stream it off the machine (`logctl`), and the layout randomised
/// at boot (KASLR, `docs/certification/SPECULATION.md` §6.1) is worth only as
/// much as nothing a program can read gives it away. So these stay out:
///
/// * a panic's `kaslr slide` line and its backtrace, whose frames are the
///   image's own addresses (`panic.rs`);
/// * a fatal trap's registers and fault address, which hold kernel pointers
///   whenever the kernel was what trapped (`arch::report_trap`, and the
///   report's headline in `trap.rs`);
/// * stage 2's report of where the loader put the image, the direct map and
///   the page array, and the W^X and sealed-image sweeps' failure lines,
///   which name the kernel mapping that failed (`main.rs`).
///
/// Nothing else the kernel prints names a kernel virtual address: device
/// apertures, IOMMU pages and fault addresses in a program are physical,
/// device-side or the program's own. A panic's message is logged as written,
/// whatever it holds: by the time it is, every other processor is stopping
/// and no reader will run again.
pub(crate) fn write_unlogged(arguments: fmt::Arguments<'_>) {
    write_as(arguments, Kind::Unlogged);
}

/// [`write`] or [`write_unlogged`], as `kind` says.
fn write_as(arguments: fmt::Arguments<'_>, kind: Kind) {
    if !READY.load(Ordering::Acquire) {
        return;
    }
    if PANICKING.load(Ordering::Relaxed) {
        write_panicking(arguments, kind == Kind::Line);
        return;
    }
    let queued = may_queue();
    let wake = {
        let mut port = PORT.lock();
        let mut writer = Writer::new(&mut port, queued, kind);
        let _ = writer.write_fmt(arguments);
        writer.finish()
    };
    if wake {
        output::wake_writers();
    }
}

/// How [`write`] writes once [`begin_panic`] has been called: the lock waited
/// for a bounded time, then passed by, and the bytes polled straight out.
/// Recorded in the kernel [`log`] if `logged`.
///
/// Also run by the log's boot check (`console::log_check`) without a panic, so
/// that the path a failure report takes into the log is exercised on every
/// boot rather than only on the one that fails.
pub(crate) fn write_panicking(arguments: fmt::Arguments<'_>, logged: bool) {
    let kind = if logged { Kind::Line } else { Kind::Unlogged };
    for _ in 0..PANIC_SPINS {
        if let Some(mut port) = PORT.try_lock() {
            let _ = Writer::new(&mut port, false, kind).write_fmt(arguments);
            return;
        }
        spin_loop();
    }
    // Whoever holds the lock is not going to release it — quite possibly
    // because it is this CPU, part way through the line that failed. What was
    // queued stays queued; the report goes out.
    let _ = Unlocked { logged }.write_fmt(arguments);
}

/// Send everything written so far, and wait until the port has, for a caller
/// about to power off, reset or stop.
///
/// The ring first, polled out, then the port's own FIFO and shift register:
/// `arch::drain_console` alone waits for a FIFO that is empty while the last
/// lines are still in the ring, and a power-off straight after cuts them off,
/// as `SYSTEM_OFF` cut a DK1's last line mid-word before the port was
/// drained at all. During a failure report the lock is waited for a bounded
/// time, as [`write`] waits for it, and the ring left as it is if the lock
/// never comes.
pub(crate) fn drain() {
    if READY.load(Ordering::Acquire) {
        if PANICKING.load(Ordering::Relaxed) {
            for _ in 0..PANIC_SPINS {
                if let Some(mut port) = PORT.try_lock() {
                    let _ = port.flush();
                    break;
                }
                spin_loop();
            }
        } else {
            let wake = PORT.lock().flush();
            if wake {
                output::wake_writers();
            }
        }
    }
    crate::arch::drain_console();
}

/// The line every power-off prints last, before the console drains and the
/// firmware or QEMU is asked to stop the machine. A run that ends by itself
/// without it did not power off: xtask's power-off gates require it, because
/// a triple fault or a reset under `-no-reboot` ends QEMU just as quietly
/// (F-46). Nothing but the drain and the power-off write comes after it.
///
/// Here rather than in `power.rs`, which orchestrates the power-off but is
/// outside the core: every `arch::shutdown` calls this itself, and the core
/// may not depend upward on the item (`docs/certification/ITEM.md`).
pub(crate) fn announce_power_off() {
    println!("FERRIX-POWER-OFF");
}

/// How long a power-off write is given to take hold before the kernel says it
/// did not. QEMU 10.2's debug-exit device, unlike 8.2's, only asks its main
/// loop to stop the machine and lets the write return, and the processor runs
/// on until the main loop gets to it.
const POWER_OFF_GRACE_NANOS: u64 = 1_000_000_000;

/// Said when the power-off write returned and the machine is still running a
/// second later: no firmware's power-off, or no QEMU debug-exit device. The
/// gates refuse a run that shows it after [`announce_power_off`]'s line.
pub(crate) fn announce_power_off_failed() {
    // An uncalibrated counter reads 0 forever, and a wait on it would never end.
    if crate::timer::counter_hz() != 0 {
        let asked = crate::timer::now_nanos();
        while crate::timer::now_nanos().wrapping_sub(asked) < POWER_OFF_GRACE_NANOS {
            spin_loop();
        }
    }
    println!("FERRIX-POWER-OFF-FAILED");
    drain();
}

/// Write raw bytes to the console.
///
/// What `write(2)` to file descriptor 1 or 2 ends up calling. Separate from
/// [`write`] because a program's output is bytes, not `format_args!`: it need
/// not be valid UTF-8, and a `\0` or a stray `\xFF` in the middle of it is
/// the program's business rather than something to refuse.
///
/// The newline translation is kept, and it is standing in for something: a
/// terminal line discipline's `ONLCR`, which is what turns a bare `\n` into
/// CRLF on a real Linux tty. Ferrix has no tty layer yet, and without the
/// translation every program's output would climb the screen in a staircase.
/// When stage 15 brings ttys, this moves there and this function stops
/// translating.
pub(crate) fn write_bytes(bytes: &[u8]) {
    emit(bytes, true);
}

/// Write raw bytes to the console exactly as they are: a terminal whose
/// program turned `ONLCR` or `OPOST` off. See `crate::fs::terminal`.
pub(crate) fn write_raw(bytes: &[u8]) {
    emit(bytes, false);
}

/// Write `line`, a record a program sent to the kernel log at `priority`
/// (`/dev/kmsg`), to the console as a line of its own: the log begins a line
/// for it whatever came before, the port is sent a newline first if another
/// writer's line was left open, and it ends in a newline.
///
/// Written as a kernel line is, never waiting for room, under the port's lock
/// from its first byte to its newline, so no other writer's bytes land inside
/// it.
pub(crate) fn write_record(priority: u8, line: &[u8]) {
    if !READY.load(Ordering::Acquire) {
        return;
    }
    let queued = may_queue();
    let wake = {
        let mut port = PORT.lock();
        let cut = log::break_line();
        let mut writer = Writer::new(&mut port, queued, Kind::Record(priority));
        // A line another writer left without its newline is ended on the
        // port, as Linux's console ends a continued line a record cuts, and
        // only there: the log has ended it already.
        if cut {
            writer.logged = false;
            writer.bytes(b"\n", true);
            writer.logged = true;
        }
        writer.bytes(line, true);
        if line.last() != Some(&b'\n') {
            writer.bytes(b"\n", true);
        }
        writer.finish()
    };
    if wake {
        output::wake_writers();
    }
}

/// Send `bytes` to the port, a bare newline as CRLF if asked.
///
/// A task that may sleep queues them and waits for room while the ring is
/// full, as a program writing to a Linux tty does; anything else writes them
/// as [`write`] writes a line. See [`output`].
fn emit(bytes: &[u8], crlf: bool) {
    if !READY.load(Ordering::Acquire) {
        return;
    }
    let queued = may_queue();
    if queued && crate::sched::may_block() {
        output::write_waiting(bytes, crlf);
        return;
    }
    let wake = {
        let mut port = PORT.lock();
        let mut writer = Writer::new(&mut port, queued, Kind::Program);
        writer.bytes(bytes, crlf);
        writer.finish()
    };
    if wake {
        output::wake_writers();
    }
}

/// Print to the kernel console, with a newline.
macro_rules! println {
    () => { $crate::console::write(format_args!("\n")) };
    ($($argument:tt)*) => {
        $crate::console::write(format_args!("{}\n", format_args!($($argument)*)))
    };
}

pub(crate) use println;

/// Print to the kernel console, with a newline, and keep the line out of the
/// kernel log: see [`write_unlogged`] for which lines, and why.
macro_rules! println_unlogged {
    ($($argument:tt)*) => {
        $crate::console::write_unlogged(format_args!("{}\n", format_args!($($argument)*)))
    };
}

pub(crate) use println_unlogged;
