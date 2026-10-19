//! `/proc/loadavg`, and the arithmetic of the load average behind it.
//!
//! ```text
//! 0.52 0.58 0.59 2/1283 3474930
//! ```
//!
//! The averages over one, five and fifteen minutes, the runnable tasks and
//! all of them, and the last number handed out, as `loadavg_proc_show` in
//! `fs/proc/loadavg.c` writes them: `"%lu.%02lu %lu.%02lu %lu.%02lu %u/%d
//! %d\n"`.
//!
//! The averages are Linux's (`kernel/sched/loadavg.c`): every five seconds
//! the count of runnable tasks is folded into each, decaying as `e^(-5 s /
//! period)`, in fixed point with [`FSHIFT`] fractional bits. `getloadavg`
//! reads the same three from `sysinfo`, shifted to sixteen bits.

use alloc::vec::Vec;

use crate::text::put;

/// Fractional bits in a load: Linux's `FSHIFT`.
pub const FSHIFT: u32 = 11;
/// One, as a load: Linux's `FIXED_1`.
pub const FIXED_1: u64 = 1 << FSHIFT;
/// How often a count is folded in, in nanoseconds: Linux's `LOAD_FREQ`.
pub const LOAD_FREQ_NS: u64 = 5_000_000_000;
/// Each fold's decay for the one, five and fifteen minute averages:
/// `FIXED_1 / exp(5 s / 1 min)` and so on, Linux's `EXP_1`, `EXP_5`, `EXP_15`.
pub const EXP: [u64; 3] = [1884, 2014, 2037];
/// Folds past which every average has reached its count whatever it started
/// at: after 2,000 (2 h 46 min) the fifteen-minute average keeps
/// `(2037/2048)^2000` of its start, which rounds to nothing.
const MOST_FOLDS: u64 = 2000;

/// Linux's `calc_load`: one fold of `active`, a count in fixed point, into
/// `load`.
#[must_use]
pub const fn fold(load: u64, exp: u64, active: u64) -> u64 {
    let mut next = load * exp + active * (FIXED_1 - exp);
    if active >= load {
        next += FIXED_1 - 1;
    }
    next / FIXED_1
}

/// The three averages as they stand, and when the next fold is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Averages {
    /// When the next fold is due, on the clock [`Averages::advance`] is given.
    pub due: u64,
    /// The one, five and fifteen minute averages, in fixed point.
    pub loads: [u64; 3],
}

impl Averages {
    /// Nothing folded yet: every average zero, the first fold five seconds
    /// after the clock's zero, as Linux's first is after boot.
    pub const START: Self = Self {
        due: LOAD_FREQ_NS,
        loads: [0; 3],
    };

    /// Make every fold due by `now`, each with `running` tasks.
    ///
    /// A caller that cannot sample every five seconds folds what it sees now
    /// into every fold it missed: the load it reads is the load it carries
    /// back.
    pub fn advance(&mut self, now: u64, running: u64) {
        if now < self.due {
            return;
        }
        let folds = (now - self.due) / LOAD_FREQ_NS + 1;
        let active = running.saturating_mul(FIXED_1);
        for (load, exp) in self.loads.iter_mut().zip(EXP) {
            for _ in 0..folds.min(MOST_FOLDS) {
                *load = fold(*load, exp, active);
            }
        }
        self.due = self.due.saturating_add(folds.saturating_mul(LOAD_FREQ_NS));
    }
}

/// A load's whole part and hundredths, rounded as Linux's `LOAD_INT` and
/// `LOAD_FRAC` round them after adding `FIXED_1 / 200`.
#[must_use]
pub const fn hundredths(load: u64) -> (u64, u64) {
    let load = load + FIXED_1 / 200;
    (load >> FSHIFT, ((load & (FIXED_1 - 1)) * 100) >> FSHIFT)
}

/// What `/proc/loadavg` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loadavg {
    /// The three averages, in fixed point.
    pub loads: [u64; 3],
    /// Tasks runnable now.
    pub running: u64,
    /// Tasks there are.
    pub total: u64,
    /// The last task number handed out.
    pub last: u64,
}

/// Append `/proc/loadavg`'s one line.
pub fn render(out: &mut Vec<u8>, loadavg: &Loadavg) {
    for load in loadavg.loads {
        let (whole, part) = hundredths(load);
        put(out, format_args!("{whole}.{part:02} "));
    }
    put(
        out,
        format_args!("{}/{} {}\n", loadavg.running, loadavg.total, loadavg.last),
    );
}
