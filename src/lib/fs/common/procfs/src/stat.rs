//! `/proc/<pid>/stat`.
//!
//! Fifty-two fields on one line, which `ps` and `top` read by position with a
//! `scanf` format fixed since Linux 3.5, so the only field that may be omitted
//! is none of them. The layout is `do_task_stat` in `fs/proc/array.c`; the
//! command name in parentheses is printed raw, which is why a careful reader
//! finds the *last* `)` on the line rather than the first.
//!
//! Every field [`Stat`] does not carry is one Linux itself prints as zero for
//! an ordinary process — `itrealvalue`, `kstkesp`, `kstkeip`, `wchan`,
//! `nswap`, `cnswap` — or one a kernel with no accounting of it would have to
//! invent; each is named at the place it is written.

use alloc::vec::Vec;

use crate::status::State;
use crate::text::put;

/// What the kernel can say about a process, by `proc(5)`'s field names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat<'a> {
    /// (1) `pid`.
    pub pid: u32,
    /// (2) `comm`, without the parentheses.
    pub comm: &'a [u8],
    /// (3) `state`.
    pub state: State,
    /// (4) `ppid`.
    pub ppid: u32,
    /// (5) `pgrp`.
    pub pgrp: u32,
    /// (6) `session`.
    pub session: u32,
    /// (7) `tty_nr`: the controlling terminal's device, zero for none.
    pub tty_nr: u32,
    /// (8) `tpgid`: its foreground process group, `-1` for none.
    pub tpgid: i32,
    /// (9) `flags`: the `PF_*` bits.
    pub flags: u32,
    /// (14) `utime`, in clock ticks.
    pub utime: u64,
    /// (15) `stime`, in clock ticks.
    pub stime: u64,
    /// (18) `priority`: 20 for an ordinary process at nice zero.
    pub priority: i64,
    /// (19) `nice`.
    pub nice: i64,
    /// (20) `num_threads`.
    pub threads: u32,
    /// (22) `starttime`: clock ticks after boot at which it started.
    pub start_time: u64,
    /// (23) `vsize`, in bytes.
    pub vsize: u64,
    /// (24) `rss`, in pages.
    pub rss: u64,
    /// (25) `rsslim`, in bytes.
    pub rss_limit: u64,
    /// (26) `startcode`.
    pub start_code: u64,
    /// (27) `endcode`.
    pub end_code: u64,
    /// (28) `startstack`.
    pub start_stack: u64,
    /// (31) `signal`: pending signals.
    pub pending: u64,
    /// (32) `blocked`.
    pub blocked: u64,
    /// (33) `sigignore`.
    pub ignored: u64,
    /// (34) `sigcatch`.
    pub caught: u64,
    /// (38) `exit_signal`: what the parent is sent at exit, 17 for `SIGCHLD`.
    pub exit_signal: i32,
    /// (39) `processor`: where it last ran.
    pub processor: u32,
    /// (47) `start_brk`.
    pub start_brk: u64,
    /// (48) `arg_start`.
    pub arg_start: u64,
    /// (49) `arg_end`.
    pub arg_end: u64,
    /// (50) `env_start`.
    pub env_start: u64,
    /// (51) `env_end`.
    pub env_end: u64,
}

/// Append the line, newline included.
pub fn render(out: &mut Vec<u8>, stat: &Stat<'_>) {
    put(out, format_args!("{} (", stat.pid));
    out.extend_from_slice(stat.comm);
    put(
        out,
        format_args!(
            ") {} {} {} {} {} {} {}",
            stat.state.letter(),
            stat.ppid,
            stat.pgrp,
            stat.session,
            stat.tty_nr,
            stat.tpgid,
            stat.flags,
        ),
    );
    // (10-13) minflt, cminflt, majflt, cmajflt: fault counts, which nothing
    // counts per process yet.
    out.extend_from_slice(b" 0 0 0 0");
    // (16-17) cutime, cstime: waited-for children's times.
    put(out, format_args!(" {} {} 0 0", stat.utime, stat.stime));
    // (21) itrealvalue: always zero since Linux 2.6.17.
    put(
        out,
        format_args!(
            " {} {} {} 0 {} {} {} {}",
            stat.priority,
            stat.nice,
            stat.threads,
            stat.start_time,
            stat.vsize,
            stat.rss,
            stat.rss_limit,
        ),
    );
    // (29-30) kstkesp, kstkeip: zero unless the reader may ptrace, and zero
    // for a running task even then.
    put(
        out,
        format_args!(
            " {} {} {} 0 0 {} {} {} {}",
            stat.start_code,
            stat.end_code,
            stat.start_stack,
            stat.pending,
            stat.blocked,
            stat.ignored,
            stat.caught,
        ),
    );
    // (35-37) wchan, nswap, cnswap: the last two have been zero since 2.6,
    // and wchan is zero for a task that is not blocked.
    put(
        out,
        format_args!(" 0 0 0 {} {}", stat.exit_signal, stat.processor),
    );
    // (40-46) rt_priority and policy, zero for SCHED_OTHER; the block I/O
    // delay and the two guest times, which need virtualisation to be
    // non-zero; start_data and end_data, which a copying loader has no file
    // segment boundaries for.
    out.extend_from_slice(b" 0 0 0 0 0 0 0");
    // (52) exit_code: zero while it runs.
    put(
        out,
        format_args!(
            " {} {} {} {} {} 0\n",
            stat.start_brk, stat.arg_start, stat.arg_end, stat.env_start, stat.env_end,
        ),
    );
}
