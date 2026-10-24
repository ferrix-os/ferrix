//! A view of a task that is not running, from outside it (`docs/DEBUG.md`
//! §1; `H.SCHED.15`): the system call or trap it waits in, and its kernel
//! call chain. What `/proc/<pid>/task/<tid>/syscall`, `wchan` and `stack`
//! render.
//!
//! It adds nothing to any path the task takes. Everything it reads is
//! already in memory: the entry frame the way in from user mode saved at the
//! top of the task's kernel stack ([`arch::entry_frame`]), whether it was in
//! a call ([`Task::in_call`], stored by `carry_in_call`), and the frame
//! `ferrix_switch` left at its saved stack pointer ([`arch::SWITCH_FRAME`]).
//!
//! # Sampled under the queue's lock, read without it
//!
//! The saved stack pointer, the state, `in_call` and the switch count are
//! read holding the lock of the run queue that owns the task, as the switch
//! writes them (`switch_chosen`, which holds that lock until the incoming
//! context's `finish_switch`): so a task found blocked and not its queue's
//! current under the lock has finished its switch away, and its stack
//! pointer is the one it will resume from. Nothing more is done under the
//! queue's lock. The stack's words are read after, each on its own through
//! [`mm::read_kernel_word`], which takes the page tables' lock alone: the two
//! locks are never held together.
//!
//! The words are advisory. The task may be woken and run while they are
//! read, and its own stores race the reads; the switch count read again
//! after them, behind an acquire fence, catches a task that was switched to
//! in between, but the switch's own count is a relaxed store, so on Arm a read
//! racing a wake can still mix old and new words (the consultant's design
//! review, ledger 2831, condition 4). Nothing in the kernel acts on a view;
//! it is printed. And no value is printed that names the direct map or the
//! vmap arena (`docs/certification/SPECULATION.md` §6.1): the call's fields
//! are the program's own registers, and the chain is return addresses that
//! passed [`backtrace::in_image`], with the image's slide taken off.

use alloc::sync::Arc;
use core::sync::atomic::{Ordering, fence};

use ferrix_sync::IrqControl;

use super::Task;
use crate::arch;
use crate::backtrace;
use crate::mm;
use crate::trap::Abi;

/// The most frames [`chain_of`] reports: the panic report's.
pub const MAX_FRAMES: usize = backtrace::MAX_FRAMES;

/// What a blocked task's entry frame says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    /// It waits inside a system call: the number and arguments as the
    /// program passed them, and where its stack and its next instruction are.
    Blocked {
        /// The number, in the table of the entry it came through.
        number: u64,
        /// The six argument registers.
        args: [u64; 6],
        /// The program's stack pointer; zero where the frame has none
        /// (ARMv7-A).
        sp: u64,
        /// Where the program resumes.
        pc: u64,
    },
    /// It waits outside any call, inside a trap from user mode: a page fault.
    Trapped {
        /// As above.
        sp: u64,
        /// As above.
        pc: u64,
    },
}

/// What [`sample`] read under the owning queue's lock.
#[derive(Debug, Clone, Copy)]
struct Sample {
    /// The switch count, to tell a task that ran since.
    switches: u64,
    /// Where it was switched away from.
    stack_pointer: u64,
    /// Whether it was in a call then.
    in_call: bool,
    /// Its kernel stack's lowest usable address.
    base: u64,
    /// One past its highest.
    top: u64,
}

/// The task's saved state, read under the lock of the run queue that owns
/// it, if it is blocked and switched away from; `None` if it runs, is
/// runnable, has ended or has no stack of its own.
fn sample(task: &Arc<Task>) -> Option<Sample> {
    let stack = task.stack()?;
    if task.is_dead() {
        return None;
    }
    let saved = <arch::Irq as IrqControl>::disable();
    let found = loop {
        let cpu = task.cpu();
        let Some(lock) = super::queue_of(cpu) else {
            break None;
        };
        let queue = lock.lock();
        // It may have moved between the read and the lock, as in `wake`.
        if task.cpu() != cpu {
            drop(queue);
            continue;
        }
        let current = queue
            .current
            .as_ref()
            .is_some_and(|running| Arc::ptr_eq(running, task));
        if current || !task.is_blocked() {
            break None;
        }
        // SAFETY: (SHARED) this holds the lock of the queue that owns the task,
        // which orders this read after the switch that wrote it; and the task,
        // blocked and not the queue's current under that lock, has finished
        // that switch, since `switch_chosen` holds the lock until the incoming
        // context lets it go.
        let stack_pointer = unsafe { task.saved_stack_pointer() };
        break Some(Sample {
            switches: task.switches(),
            stack_pointer,
            in_call: task.in_call(),
            base: stack.base,
            top: stack.top,
        });
    };
    <arch::Irq as IrqControl>::restore(saved);
    // What is read after this is read after the sample.
    fence(Ordering::Acquire);
    found
}

/// Whether the task is still as [`sample`] found it: blocked, and not
/// switched to since. Read behind an acquire fence, after the words.
fn still(task: &Task, sample: &Sample) -> bool {
    fence(Ordering::Acquire);
    task.is_blocked() && task.switches() == sample.switches
}

/// The word at `at`, through [`mm::read_kernel_word`].
fn word_at(at: u64) -> Option<u64> {
    mm::read_kernel_word(at)
}

/// Whether this architecture's entry frame is at a place the view can read
/// (`arch::entry_frame`): x86-64's is, at the top of the task's stack; the
/// Arm architectures' is not yet (`docs/DEBUG.md` §1.1).
pub const fn calls_are_visible() -> bool {
    arch::entry_frame(Abi::Native, true).is_some()
}

/// The system call or trap `task` waits in, read from its entry frame, the
/// frame of the entry `abi` names; `None` if it runs, ran during the read,
/// has no user half, or its frame could not be read.
///
/// Checks nothing of the program's: the values are the registers the
/// program passed, as Linux's `/proc/<pid>/syscall` prints them.
pub fn call_of(task: &Arc<Task>, abi: Abi) -> Option<Call> {
    let _ = task.thread()?;
    let sample = sample(task)?;
    let frame = arch::entry_frame(abi, sample.in_call)?;
    let start = sample.top.checked_sub(u64::try_from(frame.bytes).ok()?)?;
    let call = read_call(&frame, start, sample.in_call)?;
    still(task, &sample).then_some(call)
}

/// The word `offset` bytes into the frame at `start`.
fn field(start: u64, offset: usize) -> Option<u64> {
    word_at(start.checked_add(u64::try_from(offset).ok()?)?)
}

/// What the entry frame at `start` holds, as a call when `in_call` says the
/// task was in one, and as a trap otherwise.
fn read_call(frame: &arch::inspect::EntryFrame, start: u64, in_call: bool) -> Option<Call> {
    let pc = field(start, frame.pc)?;
    let sp = match frame.sp {
        Some(offset) => field(start, offset)?,
        None => 0,
    };
    if !in_call {
        return Some(Call::Trapped { sp, pc });
    }
    let mut args = [0_u64; 6];
    for (arg, &offset) in args.iter_mut().zip(frame.args.iter()) {
        *arg = field(start, offset)?;
    }
    Some(Call::Blocked {
        number: field(start, frame.number)?,
        args,
        sp,
        pc,
    })
}

/// `task`'s kernel call chain while it is switched away, innermost first, as
/// the addresses in the ELF the image was linked as: written to `out`, and
/// how many returned; `None` if it runs or ran during the read.
///
/// The chain starts at the switch frame ([`arch::SWITCH_FRAME`]) and follows
/// the frame pointers as `backtrace::walk` does, more tightly: every record
/// inside the task's own stack, each above the last and aligned, every
/// return address inside the image ([`backtrace::in_image`]), at most
/// [`MAX_FRAMES`]. A word that fails ends the chain, and is never reported.
pub fn chain_of(task: &Arc<Task>, out: &mut [u64; MAX_FRAMES]) -> Option<usize> {
    let sample = sample(task)?;
    let found = walk(&sample, out);
    still(task, &sample).then_some(found)
}

/// A frame record of the chain: the return address it holds and the record
/// above it.
#[derive(Debug, Clone, Copy)]
struct Link {
    /// The return address.
    address: u64,
    /// The next record's address.
    previous: u64,
}

/// The switch frame's link: its saved frame pointer and return address.
fn first_link(sample: &Sample) -> Option<Link> {
    let switch = arch::SWITCH_FRAME;
    let saved = sample.stack_pointer;
    let at = |offset: usize| word_at(saved.checked_add(u64::try_from(offset).ok()?)?);
    Some(Link {
        address: at(switch.return_address)?,
        previous: at(switch.frame_pointer)?,
    })
}

/// The record at `frame`, if it is one the chain may follow: aligned,
/// above `last`, inside the stack, and its words readable.
fn link_at(sample: &Sample, frame: u64, last: u64) -> Option<Link> {
    let word = u64::try_from(size_of::<usize>()).ok()?;
    let inside = frame >= sample.base && frame.saturating_add(2 * word) <= sample.top;
    if !frame.is_multiple_of(word) || frame <= last || !inside {
        return None;
    }
    Some(Link {
        previous: word_at(frame)?,
        address: word_at(frame.checked_add(word)?)?,
    })
}

/// Follow the chain from the switch frame into `out`, and say how many.
fn walk(sample: &Sample, out: &mut [u64; MAX_FRAMES]) -> usize {
    let slide = backtrace::slide();
    let inside = sample.stack_pointer >= sample.base && sample.stack_pointer < sample.top;
    let mut link = if inside { first_link(sample) } else { None };
    let mut last = sample.stack_pointer;
    let mut found = 0_usize;
    while let Some(current) = link {
        if !backtrace::in_image(current.address) {
            break;
        }
        let Some(slot) = out.get_mut(found) else {
            break;
        };
        *slot = current.address.wrapping_sub(slide);
        found += 1;
        let frame = current.previous;
        link = link_at(sample, frame, last);
        last = frame;
    }
    found
}
