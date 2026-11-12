//! The delivery cap, checked: more signals than one round of the way back
//! delivers are all delivered before it returns (`docs/OPAQUE-KERNEL.md`
//! §9.8, 2c, the consultant's condition 2 on landing).
//!
//! [`super::return_to_user`] acts on at most [`super::DELIVERY_ROUNDS`]
//! signals a pass, then makes its masked look again. With the pending-work
//! word that look reads the word, whose `SIGNAL` the look that brought the
//! thread there cleared, so a pass that ends on the cap posts `SIGNAL` again.
//! A thread can have more than 65 deliveries due at once: one signal never
//! queues twice in one set (`signal::Queue`), but the process's set and the
//! thread's each hold one, so 35 numbers pending in both are 70.
//!
//! The case runs on a kernel task of a check's process, which is that
//! process's only thread, and drives the way back itself with registers of
//! its own making. The 35 are ignored, so delivering one is taking it: no
//! handler runs and nothing is written to a stack, and the case stays a few
//! microseconds of the boot. Each was posted while blocked, which is what
//! keeps an ignored signal pending (Linux's rule, `signal::post_into`), and
//! the block is lifted before the way back, as `rt_sigprocmask` would.

use core::sync::atomic::{AtomicU32, Ordering};

use ferrix_linux_abi::types::SIG_IGN;
use ferrix_sync::IrqControl;

use crate::arch;
use crate::syscall::process;
use crate::syscall::signal::{self, Origin, Posted};
use crate::syscall::thread;

/// The signals the case posts, each to the process and to its thread.
const FIRST: u32 = 30;
const LAST: u32 = 64;

/// What the case's thread found: zero for nothing yet, then one of the
/// answers below.
static ANSWER: AtomicU32 = AtomicU32::new(0);
const PASSED: u32 = 1;
const NOT_PENDING: u32 = 2;
const LEFT_PENDING: u32 = 3;
const NO_THREAD: u32 = 4;

/// How long the check waits for its thread.
const PATIENCE_NANOS: u64 = 10_000_000_000;

/// The delivery cap's check: 70 deliveries due at once are all made before
/// the way back returns.
///
/// # Errors
///
/// A delivery left pending, a post that was not kept, or a thread that could
/// not be made or did not end.
/// Verifies: `L.syscall.23`
pub(crate) fn check_the_delivery_cap() -> Result<u32, &'static str> {
    ANSWER.store(0, Ordering::Release);
    let process =
        process::new_for_check().map_err(|_| "could not make the delivery cap's process")?;
    let task = crate::syscall::check::spawn_in(&process, "delivery cap", deliver_seventy, None)?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    if process.wait_for_exit(deadline).is_none() {
        return Err("the delivery cap's thread never finished");
    }
    crate::sched::wait_until_gone(&task, crate::sched::REAPER_PATIENCE_NANOS)?;
    match ANSWER.load(Ordering::Acquire) {
        PASSED => Ok(2 * (LAST - FIRST + 1)),
        NOT_PENDING => Err("a blocked, ignored signal posted for the delivery cap was not kept"),
        LEFT_PENDING => Err(
            "the way back to user mode returned with signals still deliverable: a pass that ended \
             on its delivery cap did not post SIGNAL again",
        ),
        _ => Err("the delivery cap's thread found no thread of its own"),
    }
}

/// The case's thread: 35 signals pending for the process and for itself,
/// unblocked, then one way back.
fn deliver_seventy(_: usize) {
    let answer = seventy();
    ANSWER.store(answer, Ordering::Release);
    process::exit_current(0);
}

fn seventy() -> u32 {
    let Some(me) = thread::current() else {
        return NO_THREAD;
    };
    let process = me.process();
    let set = (FIRST..=LAST).fold(0, |set, number| set | signal::bit(number));
    process.with_signals(|signals| {
        for number in FIRST..=LAST {
            signals.install_action(number, SIG_IGN, 0);
        }
    });
    signal::change_blocked(&me, |_, own| {
        let blocked = own.signals().blocked();
        let _ = own.replace_blocked(blocked | set);
    });
    for number in FIRST..=LAST {
        let shared = process.post_signal(number, Origin::Kernel);
        let own = process.post_signal_to(&me, number, Origin::Kernel);
        if shared != Posted::Pending || own != Posted::Pending {
            return NOT_PENDING;
        }
    }
    signal::change_blocked(&me, |_, own| {
        let blocked = own.signals().blocked();
        let _ = own.replace_blocked(blocked & !set);
    });
    let mut context = arch::UserContext::from_trap(&arch::TrapFrame::default());
    let saved = <arch::Irq as IrqControl>::disable();
    if crate::trap::attention_due(&super::RETURN_PATH) {
        super::return_to_user(&mut context);
    }
    <arch::Irq as IrqControl>::restore(saved);
    let left = me.with_signals(|shared, own| signal::deliverable(shared, own));
    if left == 0 { PASSED } else { LEFT_PENDING }
}
