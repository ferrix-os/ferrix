//! What happens when the kernel cannot go on.
//!
//! A fatal condition in the kernel is [`fatal!`], which names the catalog entry
//! that explains it and then panics with a sentence saying what went wrong. A
//! bare `panic!` is denied here as everywhere; the one inside the macro is out
//! of the lint's sight, so a fatal site cannot be written without choosing its
//! entry. This module is the other half, the report, so that no site prints a
//! marker and halts by hand and every failure arrives in the same shape:
//!
//! ```text
//! FERRIX-PANIC stage 3 self-check failed: the timer never fired
//!   at        src/kernel/src/main.rs:152:23
//!   on        processor 0 (hardware id 0x0)
//!   stopped   this processor halts here, and 3 more were asked to
//!   kaslr     slide 0x23912000
//!   trace     #0  0xffffffffa39138e7
//!   trace     #1  0xffffffffa391a12c
//!   code      FX-0302  the timer interrupt did not arrive as programmed
//!   means     ...
//!   causes    1. ...
//!   see       src/kernel/src/main.rs timer_check; docs/ROADMAP.md stage 3
//! ```
//!
//! The marker and the message share the first line because that is the line
//! the boot test judges on. `xtask` keeps reading for a moment after it, and
//! appends to each trace address the function it is in, from the kernel ELF it
//! booted, less the slide the line before the trace gives when the loader
//! moved the kernel (KASLR). When firmware left a framebuffer, the same text is drawn on it last
//! ([`screen`]).
//!
//! The trap path's fatal reports share everything after their opening lines:
//! they print their own marker and the saved registers, then [`conclude`].
//!
//! # Not making it worse
//!
//! Everything beyond printing the message is a chance to fault again, on a
//! machine already in a state nobody planned for. So the processor is named
//! from a register compared against the processor table, never followed; the
//! backtrace checks every frame before it reads one; and a second report, from
//! inside this one or on another processor while it is being written, prints
//! its opening lines and nothing else.

pub(crate) mod catalog;
pub(crate) mod reset;
pub(crate) mod screen;

use core::panic::PanicInfo;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use catalog::Explanation;

use crate::console::{self, println, println_unlogged};
use crate::{arch, backtrace, smp};

/// Whether a failure report has begun, on any processor.
static REPORTING: AtomicBool = AtomicBool::new(false);

/// The catalog entry the next panic is explained by, as [`fatal!`] recorded it.
///
/// The first one recorded wins. A second fatal condition, on another processor
/// while the first is on its way to the handler, would otherwise explain a
/// report that is not the one printed.
static EXPLANATION: AtomicPtr<Explanation> = AtomicPtr::new(ptr::null_mut());

/// How wide an explanation's text runs after its label, so that label and text
/// together stay inside eighty columns: a serial terminal's width, and the
/// width the panic screen keeps for text before it draws the QR code.
const TEXT_COLUMNS: usize = 66;

/// Stop the kernel on a fatal condition the catalog explains.
///
/// `fatal!(catalog::STAGE3_TIMER, "stage 3 self-check failed: {problem}")`
/// records the entry and panics with the message. The report prints the site's
/// own sentence first, where the boot test reads it, and the catalog's
/// explanation below the trace.
macro_rules! fatal {
    ($entry:expr, $($message:tt)+) => {{
        $crate::panic::explain(&$entry);
        panic!($($message)+)
    }};
}
pub(crate) use fatal;

/// Record `entry` as the explanation for the panic that is about to happen.
pub(crate) fn explain(entry: &'static Explanation) {
    let _ = EXPLANATION.compare_exchange(
        ptr::null_mut(),
        ptr::from_ref(entry).cast_mut(),
        Ordering::AcqRel,
        Ordering::Acquire,
    );
}

/// Where every kernel panic ends up.
///
/// There is no supervisor above us and no unwinder, so this says what happened
/// and stops the processor it is running on.
#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    let first = begin_report();
    println!();
    println!("FERRIX-PANIC {}", info.message());
    if let Some(location) = info.location() {
        println!("  at        {location}");
    }
    if !first {
        abridged()
    }
    let entry = EXPLANATION.load(Ordering::Acquire);
    // SAFETY: (SHARED) only `explain` stores here, and only the address of a `'static`
    // catalog entry, which is never written through.
    let entry = unsafe { entry.as_ref() };
    conclude(entry.or_else(|| allocation_failure(info)))
}

/// The explanation for a panic that is the standard library's allocation
/// error handler, if this is one: an allocation that cannot report failure
/// found the heap empty (finding F-23).
///
/// Recognised by its message, which is `alloc`'s own and fixed for the
/// pinned toolchain, and only when the heap has refused something, so a
/// panic that merely says the same words is not taken for one.
fn allocation_failure(info: &PanicInfo<'_>) -> Option<&'static Explanation> {
    /// What `alloc::alloc::__rdl_alloc_error_handler` says, up to the size.
    const MESSAGE: &str = "memory allocation of ";
    if crate::mm::heap_refusals() == 0 {
        return None;
    }
    let mut prefix = Prefix {
        wanted: MESSAGE.as_bytes(),
        matched: 0,
    };
    let _ = core::fmt::write(&mut prefix, format_args!("{}", info.message()));
    if prefix.matched != MESSAGE.len() {
        return None;
    }
    // Before the boot marker, a stop by design (`FATAL-ALLOC`); after it, a
    // site that should have reported the failure and could not.
    Some(if BOOTED.load(Ordering::Acquire) {
        &catalog::ALLOCATION_ABORTED
    } else {
        &catalog::BOOT_OUT_OF_MEMORY
    })
}

/// Whether the kernel has finished coming up: set as the boot marker is
/// printed, before init starts.
static BOOTED: AtomicBool = AtomicBool::new(false);

/// Say that the kernel has come up. An allocation failure that stops it from
/// here on is not one of the boot's (see [`allocation_failure`]).
pub(crate) fn mark_booted() {
    BOOTED.store(true, Ordering::Release);
}

/// A `fmt::Write` that checks what is written starts with `wanted`, without
/// keeping it: there is no heap a panic can trust.
struct Prefix {
    /// The bytes the text must start with.
    wanted: &'static [u8],
    /// How many of them it has matched so far.
    matched: usize,
}

impl core::fmt::Write for Prefix {
    fn write_str(&mut self, piece: &str) -> core::fmt::Result {
        for &byte in piece.as_bytes() {
            match self.wanted.get(self.matched) {
                Some(&expected) if expected == byte => self.matched += 1,
                Some(_) => return Err(core::fmt::Error),
                None => return Ok(()),
            }
        }
        Ok(())
    }
}

/// Begin a report of a failure the kernel will not survive, and say whether
/// it is the first.
///
/// Interrupts go first, before the other processors are asked to stop: an
/// inter-processor interrupt taken part way through the report would halt the
/// one processor writing it.
pub(crate) fn begin_report() -> bool {
    arch::disable_interrupts();
    console::begin_panic();
    !REPORTING.swap(true, Ordering::AcqRel)
}

/// End a report that is not the first. Its opening lines are printed; the
/// report already under way has everything else.
///
/// Under `ferrix.panic=reset`, a report inside the first one on the
/// processor that announced the reset -- a fault while it drew the screen --
/// still resets the machine ([`reset`]): the screen is the step likeliest to
/// fault, on exactly the machine that has nobody at it.
pub(crate) fn abridged() -> ! {
    println!("  stopped   during another report, so this one is abridged");
    console::drain();
    if reset::resetting_here() {
        reset::wait_then_reset()
    }
    arch::halt()
}

/// Everything after a report's opening lines: which processor, stopping the
/// others, the backtrace, the explanation if there is one, and the screen.
/// Then stop.
///
/// For the first report only, which [`begin_report`] says.
pub(crate) fn conclude(entry: Option<&Explanation>) -> ! {
    match smp::this_cpu_for_report() {
        Ok(cpu) => println!(
            "  on        processor {} (hardware id {:#x})",
            cpu.logical, cpu.hardware_id
        ),
        Err(which) => println!("  on        {which}"),
    }
    let others = smp::stop_others();
    let resetting = reset::armed();
    match (resetting, others) {
        (false, 0) => println!("  stopped   this processor halts here; nothing will recover it"),
        (false, _) => {
            println!("  stopped   this processor halts here, and {others} more were asked to");
        }
        (true, 0) => println!("  stopped   this processor resets the machine after the report"),
        (true, _) => println!(
            "  stopped   this processor resets the machine after the report, and {others} more were asked to stop"
        ),
    }
    report_backtrace();
    if let Some(entry) = entry {
        report_explanation(entry);
    }
    // Every line written is not every line sent: let the port finish, so the
    // report is whole on the wire before anything else can go wrong.
    console::drain();
    // Before the write-back and the screen, so the line is in the record and
    // drawn with the rest.
    if resetting {
        reset::announce();
    }
    // The log as the next boot's loader will find it, written back from the
    // caches a reset may discard: after the last line, before the step most
    // likely to fault.
    if console::pstore::write_back() {
        // Unlogged: after the write-back, the region is not to change. The
        // point from which a reset keeps the whole report (`test-postmortem`).
        println_unlogged!("  pstore    written back for the next boot");
        console::drain();
    }
    // Last, and after every line has gone to the serial port: the screen is
    // drawn from what the console printed, and drawing is the step most
    // likely to fault on a machine in this state.
    screen::draw();
    if resetting {
        reset::wait_then_reset()
    }
    arch::halt()
}

/// Print the chain of calls that reached the report.
///
/// Addresses only: the kernel carries no symbol table, and a panic is the
/// worst moment to go looking for one. `xtask` names them from the image it
/// booted as the report arrives.
///
/// The addresses are where the code ran, which in a kernel the loader moved
/// (KASLR) is not where the image was linked. So the report first says how far
/// it moved, and `xtask` takes that off before it looks an address up. Said on
/// the port and the screen and kept out of the kernel log, which privileged
/// programs read back (`console::write_unlogged`); `SPECULATION.md` §6 argues
/// why that is not a leak.
fn report_backtrace() {
    let slide = backtrace::image_start().wrapping_sub(ferrix_bootinfo::KERNEL_VIRT_BASE);
    if slide != 0 {
        println_unlogged!("  kaslr     slide {slide:#x}");
    }
    let mut index = 0;
    let found = backtrace::walk(arch::frame_pointer(), |address| {
        println_unlogged!("  trace     #{index:<2} {address:#018x}");
        index += 1;
    });
    if found == 0 {
        println!("  trace     no frame pointer chain to follow from here");
    } else if found == backtrace::MAX_FRAMES {
        println!("  trace     ... stopped at {found} frames");
    }
}

/// Print a catalog entry below the trace.
fn report_explanation(entry: &Explanation) {
    println!("  code      {}  {}", entry.code, entry.title);
    wrapped("means", None, entry.meaning);
    for (index, cause) in entry.causes.iter().enumerate() {
        let label = if index == 0 { "causes" } else { "" };
        wrapped(label, Some(index.saturating_add(1)), cause);
    }
    wrapped("see", None, entry.see);
}

/// Print `text` after `label`, numbered if `number` is given, wrapped at
/// [`TEXT_COLUMNS`] with each further line indented under the first.
///
/// Word by word, straight to the console: there is no allocator to assemble a
/// line in that a panic can trust.
fn wrapped(label: &str, number: Option<usize>, text: &str) {
    let indent: usize = match number {
        Some(number) => {
            console::write(format_args!("  {label:<10}{number}. "));
            3
        }
        None => {
            console::write(format_args!("  {label:<10}"));
            0
        }
    };
    let mut column = indent;
    for word in text.split_whitespace() {
        if column > indent && column.saturating_add(word.len()).saturating_add(1) > TEXT_COLUMNS {
            console::write(format_args!("\n  {:<10}{:indent$}", "", ""));
            column = indent;
        }
        if column > indent {
            console::write(format_args!(" "));
            column = column.saturating_add(1);
        }
        console::write(format_args!("{word}"));
        column = column.saturating_add(word.len());
    }
    console::write(format_args!("\n"));
}
