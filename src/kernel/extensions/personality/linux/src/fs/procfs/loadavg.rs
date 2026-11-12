//! The load average's state: `/proc/loadavg`, and the `loads` of `sysinfo`
//! that `getloadavg` reads. The arithmetic is `ferrix_procfs::loadavg`'s,
//! Linux's own.
//!
//! The count folded in is the run queues' runnable tasks, each running one
//! included and the idle tasks not -- `/proc/stat`'s `procs_running`. Linux
//! counts tasks in uninterruptible sleep as well; nothing here sleeps that
//! way.
//!
//! **Folded in when it is read, not on a timer.** Linux folds from the
//! scheduler's tick. The tick is the certified item's (`docs/certification`)
//! and a load average is nothing the item needs, so this lives in the load
//! and asks the scheduler only what it already tells `/proc/stat`. The cost
//! is what a stretch nobody read looks like: every fold since the last
//! reading is made with the count as it is now. A reader that reads every
//! few seconds -- `top`, `btop`, `uptime` in a loop -- sees Linux's numbers;
//! one that reads once after an hour's quiet sees that moment's load carried
//! back over the hour.

use ferrix_procfs::loadavg::Averages;

use crate::sched;
use crate::sync::SpinLock;
use crate::syscall::time;

static AVERAGES: SpinLock<Averages> = SpinLock::new(Averages::START);

/// The runnable tasks now, and the one, five and fifteen minute averages
/// with every fold due by now made, in `ferrix_procfs::loadavg`'s fixed
/// point.
pub(crate) fn now() -> (u64, [u64; 3]) {
    let running = sched::cpu_times()
        .map(|times| {
            times
                .iter()
                .fold(0_u64, |sum, time| sum.saturating_add(time.runnable as u64))
        })
        .unwrap_or(0);
    let clock = time::now_nanos();
    let mut averages = AVERAGES.lock();
    averages.advance(clock, running);
    (running, averages.loads)
}
