//! `/proc/stat`: what every processor has spent its time on, and the kernel's
//! counters since boot.
//!
//! ```text
//! cpu  9076855 270647 1436233 94650326 308715 0 31227 0 169401 437
//! cpu0 425646 12091 77968 3850347 14446 0 12100 0 11352 6
//! intr 1540466352 127 0 0 0
//! ctxt 3405985925
//! btime 1789246016
//! processes 2384717
//! procs_running 6
//! procs_blocked 0
//! softirq 256823522 47856483 13436684 9923 12532756 236740 0 353061 83875241 911 98521723
//! ```
//!
//! The layout is `show_stat` in `fs/proc/stat.c`. The total line is `cpu` and
//! then *two* spaces, because Linux writes the label `"cpu "` and then each
//! value with a space before it; `top`, `mpstat`, `iostat` and `nmeter` all
//! find it by that prefix, and a reader that counts `cpuN` lines to learn how
//! many processors there are stops at the first line that is neither. Times
//! are in clock ticks, `USER_HZ`, which is what `AT_CLKTCK` tells a program.
//!
//! The file is named `kstat` here because `stat` is `/proc/<pid>/stat`, and
//! because `kernel_stat` is what Linux calls the counters it prints.

use alloc::vec::Vec;

use crate::text::put;

/// One processor's time, or the sum over all of them, in clock ticks, by
/// `proc(5)`'s field names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    /// Running in user mode.
    pub user: u64,
    /// Running in user mode at a positive nice value.
    pub nice: u64,
    /// Running in the kernel.
    pub system: u64,
    /// Idle.
    pub idle: u64,
    /// Idle with a task waiting on I/O.
    pub iowait: u64,
    /// Serving hardware interrupts.
    pub irq: u64,
    /// Serving softirqs.
    pub softirq: u64,
    /// Taken by the hypervisor for another guest.
    pub steal: u64,
    /// Running a guest; already counted in `user`.
    pub guest: u64,
    /// Running a niced guest; already counted in `nice`.
    pub guest_nice: u64,
}

impl CpuTimes {
    /// The fields in the order they are printed.
    pub const fn fields(&self) -> [u64; 10] {
        [
            self.user,
            self.nice,
            self.system,
            self.idle,
            self.iowait,
            self.irq,
            self.softirq,
            self.steal,
            self.guest,
            self.guest_nice,
        ]
    }

    /// The same fields back from their printed order.
    pub const fn from_fields(fields: [u64; 10]) -> CpuTimes {
        let [
            user,
            nice,
            system,
            idle,
            iowait,
            irq,
            softirq,
            steal,
            guest,
            guest_nice,
        ] = fields;
        CpuTimes {
            user,
            nice,
            system,
            idle,
            iowait,
            irq,
            softirq,
            steal,
            guest,
            guest_nice,
        }
    }

    /// Every tick accounted, as `top` adds them up: the first eight fields,
    /// the two guest times being part of `user` and `nice` already.
    pub fn ticks(&self) -> u64 {
        self.fields()
            .iter()
            .take(8)
            .fold(0_u64, |sum, &field| sum.saturating_add(field))
    }
}

/// How many softirq kinds Linux counts: `NR_SOFTIRQS`.
pub const SOFTIRQS: usize = 10;

/// What the kernel can say, by the labels Linux prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kstat<'a> {
    /// The `cpu` line.
    pub total: CpuTimes,
    /// A `cpuN` line per online processor, by logical number.
    pub cpus: &'a [(u32, CpuTimes)],
    /// `intr`: interrupts serviced since boot.
    pub interrupts: u64,
    /// The counts after it, one per interrupt number, which may be none.
    pub per_interrupt: &'a [u64],
    /// `ctxt`: context switches since boot.
    pub context_switches: u64,
    /// `btime`: when the machine booted, in seconds since the epoch.
    pub boot_time: u64,
    /// `processes`: tasks made since boot.
    pub processes: u64,
    /// `procs_running`: tasks runnable now.
    pub running: u64,
    /// `procs_blocked`: tasks waiting on I/O now.
    pub blocked: u64,
    /// `softirq`: softirqs serviced since boot.
    pub softirqs: u64,
    /// The counts after it, one per kind.
    pub per_softirq: [u64; SOFTIRQS],
}

/// Append the whole file.
pub fn render(out: &mut Vec<u8>, stat: &Kstat<'_>) {
    out.extend_from_slice(b"cpu ");
    times(out, &stat.total);
    for (cpu, cpu_times) in stat.cpus {
        put(out, format_args!("cpu{cpu}"));
        times(out, cpu_times);
    }
    put(out, format_args!("intr {}", stat.interrupts));
    for count in stat.per_interrupt {
        put(out, format_args!(" {count}"));
    }
    put(
        out,
        format_args!(
            "\nctxt {}\nbtime {}\nprocesses {}\nprocs_running {}\nprocs_blocked {}\n",
            stat.context_switches, stat.boot_time, stat.processes, stat.running, stat.blocked,
        ),
    );
    put(out, format_args!("softirq {}", stat.softirqs));
    for count in stat.per_softirq {
        put(out, format_args!(" {count}"));
    }
    out.push(b'\n');
}

/// A processor line's values, each after a space, and the newline.
fn times(out: &mut Vec<u8>, cpu_times: &CpuTimes) {
    for field in cpu_times.fields() {
        put(out, format_args!(" {field}"));
    }
    out.push(b'\n');
}

/// The one-number lines, in the order they are printed and [`parse`] keeps
/// them.
const LABELS: [&[u8]; 6] = [
    b"intr",
    b"ctxt",
    b"btime",
    b"processes",
    b"procs_running",
    b"procs_blocked",
];

/// What [`parse`] reads back: everything but the per-interrupt and softirq
/// counts, which no check here has a use for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// The `cpu` line.
    pub total: CpuTimes,
    /// The `cpuN` lines, in the order they came.
    pub cpus: Vec<(u32, CpuTimes)>,
    /// `intr`'s total.
    pub interrupts: u64,
    /// `ctxt`.
    pub context_switches: u64,
    /// `btime`.
    pub boot_time: u64,
    /// `processes`.
    pub processes: u64,
    /// `procs_running`.
    pub running: u64,
    /// `procs_blocked`.
    pub blocked: u64,
}

/// Read a file [`render`] could have written, or `None` for one it could not:
/// a line with no label this knows, a processor line without its ten values,
/// a number that is not one, or a label that is missing or repeated.
pub fn parse(text: &[u8]) -> Option<Parsed> {
    let mut total = None;
    let mut cpus = Vec::new();
    let mut counters: [Option<u64>; 6] = [None; 6];
    let mut softirq = false;
    for line in text.split(|&byte| byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"cpu  ") {
            if total.replace(cpu_fields(rest)?).is_some() {
                return None;
            }
            continue;
        }
        let mut words = line.split(|&byte| byte == b' ');
        let label = words.next()?;
        if let Some(number) = label.strip_prefix(b"cpu") {
            let rest = line.get(label.len().checked_add(1)?..)?;
            let number = u32::try_from(decimal(number)?).ok()?;
            cpus.push((number, cpu_fields(rest)?));
            continue;
        }
        if label == b"softirq" {
            if softirq {
                return None;
            }
            softirq = true;
            if words.any(|word| decimal(word).is_none()) {
                return None;
            }
            continue;
        }
        let index = LABELS.iter().position(|known| *known == label)?;
        let value = decimal(words.next()?)?;
        // `intr` alone may carry more numbers after its total.
        if index == 0 {
            if words.any(|word| decimal(word).is_none()) {
                return None;
            }
        } else if words.next().is_some() {
            return None;
        }
        if counters.get_mut(index)?.replace(value).is_some() {
            return None;
        }
    }
    let [
        interrupts,
        context_switches,
        boot_time,
        processes,
        running,
        blocked,
    ] = counters;
    if !softirq {
        return None;
    }
    Some(Parsed {
        total: total?,
        cpus,
        interrupts: interrupts?,
        context_switches: context_switches?,
        boot_time: boot_time?,
        processes: processes?,
        running: running?,
        blocked: blocked?,
    })
}

/// Exactly ten values, each after one space.
fn cpu_fields(rest: &[u8]) -> Option<CpuTimes> {
    let mut fields = [0_u64; 10];
    let mut words = rest.split(|&byte| byte == b' ');
    for field in &mut fields {
        *field = decimal(words.next()?)?;
    }
    if words.next().is_some() {
        return None;
    }
    Some(CpuTimes::from_fields(fields))
}

/// A decimal number with at least one digit and nothing else.
fn decimal(word: &[u8]) -> Option<u64> {
    if word.is_empty() {
        return None;
    }
    word.iter().try_fold(0_u64, |value, &byte| {
        let digit = byte.checked_sub(b'0').filter(|digit| *digit < 10)?;
        value.checked_mul(10)?.checked_add(u64::from(digit))
    })
}
