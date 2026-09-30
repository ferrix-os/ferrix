//! seccomp: what a filter sees of a system call, and the function the core's
//! entry asks about every call (`docs/SECCOMP.md` §3.2, §3.3).
//!
//! The core owns the way in and calls [`check`] first at each of its four
//! entries -- x86-64 `SYSCALL`, x86-64 `int $0x80`, AArch64 `svc`, ARMv7-A
//! `svc` -- before its own early answers and before the native range is split
//! from the Linux tables (`crate::trap::filter_system_call`). This file is the
//! personality's half: the `seccomp_data` a call becomes, and the decision.
//! What a filter decides can only make a call do less, which is all a
//! [`Verdict`] can carry.
//!
//! Until a program can install a filter, [`check`] answers
//! [`Verdict::Continue`] for every call of every program. The one thing that
//! can make it answer otherwise is [`arm_probe`], which the boot check uses to
//! prove the four entries obey what they are asked.

use core::sync::atomic::{AtomicUsize, Ordering};

use ferrix_seccomp::SeccompData;

use crate::arch;
use crate::sync::SpinLock;
use crate::trap::{Abi, SyscallArgs, Verdict};

/// The `arch` a call in the native range carries.
///
/// A native call (`0x1000..=0x1FFF` from the native entry) is not a Linux
/// call, and a filter that reads `arch` first, as Chromium's and systemd's do,
/// must not take it for one. So it has a token of its own, which Linux never
/// uses: an `e_machine` (0x0F1F) in the range IANA has not assigned, with the
/// 64-bit and little-endian flags every call made through a 64-bit register
/// file carries. A filter that wants these calls allows this `arch`
/// explicitly; one that allowlists numbers refuses them as it refuses any
/// number it does not know (`docs/SECCOMP.md` §3.3, §12 Q2).
pub(crate) const NATIVE_ARCH: u32 = 0xC000_0F1F;

/// What a filter judges a call by, from the registers the entry read.
///
/// The architecture token is the entry's own (SR1): the same [`Abi`] that
/// picks the table, never the image the process runs. A number is judged as
/// its low 32 bits, as Linux reads it (`int nr`); the dispatcher uses the whole
/// register, so a number with upper bits set is in no table and is `ENOSYS`
/// whatever the filter said, and a number is either dispatched as the value
/// the filter judged or not at all (SR2). The native range is decided by the
/// dispatcher's own predicate, so the token and the route agree.
pub(crate) fn data(args: &SyscallArgs) -> SeccompData {
    let native = args.abi == Abi::Native && ferrix_native_abi::nr::is_native(args.number);
    SeccompData {
        nr: args.number as i32,
        arch: if native {
            NATIVE_ARCH
        } else {
            arch::audit_arch(args.abi)
        },
        instruction_pointer: args.ip,
        args: args.args,
    }
}

/// The function the core asks about every system call.
///
/// Allocates nothing and takes no sleeping lock: it runs on every call of every
/// program, with interrupts masked, before anything else looks at the call.
pub(crate) fn check(args: &SyscallArgs) -> Verdict {
    if PROBE_TASK.load(Ordering::Relaxed) == 0 {
        return Verdict::Continue;
    }
    probed(args)
}

/// The boot check's rule, asked about the calls its own task makes.
type Rule = fn(&SeccompData) -> Verdict;

/// The task the armed probe judges, as an address; zero when none is armed.
static PROBE_TASK: AtomicUsize = AtomicUsize::new(0);

/// The armed probe's rule.
static PROBE_RULE: SpinLock<Option<Rule>> = SpinLock::new(None);

/// Judge the calls the running task makes with `rule`, until [`disarm_probe`].
///
/// Test-only, for the boot check that drives the four entries with frames of
/// its own: a call made by any other task is unaffected, so a program running
/// on another processor is judged as it always was. The running task is the
/// one the rule applies to, and only one probe can be armed at a time.
pub(crate) fn arm_probe(rule: Rule) {
    *PROBE_RULE.lock() = Some(rule);
    PROBE_TASK.store(task_key(), Ordering::Release);
}

/// Stop judging. Every call is `Continue` again.
pub(crate) fn disarm_probe() {
    PROBE_TASK.store(0, Ordering::Release);
    *PROBE_RULE.lock() = None;
}

/// Who is running, as a number that is never zero: the running task's address,
/// or 1 where no task is (the boot context before the scheduler has one).
fn task_key() -> usize {
    crate::sched::current().map_or(1, |task| alloc::sync::Arc::as_ptr(&task) as usize)
}

/// The slow path of [`check`]: a probe is armed, and this call may be its
/// task's.
fn probed(args: &SyscallArgs) -> Verdict {
    let armed = PROBE_TASK.load(Ordering::Acquire);
    if armed == 0 || armed != task_key() {
        return Verdict::Continue;
    }
    let rule = *PROBE_RULE.lock();
    rule.map_or(Verdict::Continue, |rule| rule(&data(args)))
}
