//! Stage 8's self-checks for `/dev` and `/proc`.
//!
//! The host tests in `src/lib/fs/procfs` hold the byte formats against lines a real
//! Linux printed. What they cannot hold is the kernel's side: that a device
//! node opened through the namespace reaches the device, that every name a
//! `/proc` listing reports can be walked to — the property `busybox ls -R
//! /proc` fails on, by looping or by printing an error per name — and that
//! `/proc/self/maps` read by a process describes that process's address
//! space, one line per region, with the heap and stack named.
//!
//! # Why half of it runs in a task
//!
//! `/proc/self` means the process of the task that is running. The boot task
//! has none, so the `/proc` half runs in a task spawned into a process of its
//! own: a kernel-mode entry that never goes to user mode, with that process
//! current exactly as it is for a program's system call. It hands its result
//! back through a static and ends its process, which is what the boot task's
//! wait is waiting for.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::sync::SpinLock;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::types::{AT_FDCWD, SIGSTOP};
use ferrix_procfs::kstat::{self, Parsed};
use ferrix_procfs::maps;
use ferrix_procfs::status::State;
use ferrix_vfs::initramfs::makedev;
use ferrix_vfs::{Context, Errno, FileType, Namespace, OpenFile, OpenFlags, Whence};
use ferrix_vma::VmaFlags;

use crate::fs;
use crate::fs::socket::SocketType;
use crate::sched;
use crate::smp;
use crate::syscall::process::{self, Process, Startup};
use crate::syscall::system::{self, RELEASE, SYSNAME, VERSION};
use crate::syscall::{fd, path, registry, uaccess};

/// What the checks measured, for the boot log.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Report {
    /// Device nodes whose numbers were checked against Linux's.
    pub(crate) devices: u32,
    /// What the block registry's check measured.
    pub(crate) blocks: fs::devfs::check::Report,
    /// Names under `/proc` listed and walked back to, recursively.
    pub(crate) listed: u32,
    /// Lines of `/proc/self/maps` parsed.
    pub(crate) maps_lines: u32,
    /// Of those, the ones naming `[heap]` or `[stack]`.
    pub(crate) named: u32,
    /// `cpuN` lines in `/proc/stat`, one per online processor.
    pub(crate) stat_cpus: u32,
    /// Clock ticks `/proc/stat`'s `cpu` line advanced between its two reads.
    pub(crate) stat_ticks: u64,
    /// How far apart those reads were, in milliseconds.
    pub(crate) stat_apart_ms: u64,
    /// Values under `/proc/sys` walked to and read.
    pub(crate) sysctl_values: u32,
}

/// How far apart the two reads of `/proc/stat` are: five ticks at
/// `USER_HZ`, so that even a processor idle throughout counts some of it.
const STAT_APART_NANOS: u64 = 50_000_000;

/// Nanoseconds in a clock tick at `USER_HZ`.
const TICK_NANOS: u64 = 10_000_000;

/// Every node in `/dev`, with the number Linux gives it.
const NUMBERS: [(&[u8], u32, u32); 7] = [
    (b"/dev/null", 1, 3),
    (b"/dev/zero", 1, 5),
    (b"/dev/full", 1, 7),
    (b"/dev/random", 1, 8),
    (b"/dev/urandom", 1, 9),
    (b"/dev/tty", 5, 0),
    (b"/dev/console", 5, 1),
];

/// More names than any listing here can honestly have. A walk that reaches it
/// is a walk that does not terminate.
const LISTING_LIMIT: u32 = 4096;

/// Where the check process's image region goes: low, and a user address on
/// all three architectures.
const IMAGE: u64 = 0x40_0000;

/// How long the boot task waits for the check task before calling it lost.
/// Generous, for the reason `syscall::check`'s patience is: TCG under load.
const PATIENCE_NANOS: u64 = 120_000_000_000;

/// How long the boot task gives the check task to finish switching away once
/// its process has ended, before reaping its stack.
const SETTLE_NANOS: u64 = 20_000_000;

/// The status the check task ends its process with. It only says the task got
/// as far as reporting; what it found is in [`OUTCOME`].
const REPORTED: i32 = 0;

/// What the check task found: names listed, maps lines, named lines, and
/// values read under `/proc/sys`.
type Found = Result<(u32, u32, u32, u32), &'static str>;

/// A directory's entries: name, kind and inode number.
type Listing = Vec<(Vec<u8>, FileType, u64)>;

/// The check task's result, handed back to the boot task. A static because a
/// task's entry takes a `usize` and nothing else; one check runs, at boot.
static OUTCOME: SpinLock<Option<Found>> = SpinLock::new(None);

/// The layout the boot task gave the check process, for the task to check
/// the maps against.
static LAYOUT: SpinLock<Option<Layout>> = SpinLock::new(None);

/// Run them. `Err` names the first thing that was not true.
pub(crate) fn run() -> Result<Report, &'static str> {
    let devices = check_devices()?;
    let blocks = fs::devfs::check::run()?;
    // In the boot task: `/proc/stat` describes the machine, not the reader.
    let (stat_cpus, stat_ticks) = check_stat(fs::namespace())?;
    check_thread_times()?;

    let process = process::new_for_check().map_err(|_| "no address space for the /proc check")?;
    if process.pid() == 0 || registry::find(process.pid()).is_none() {
        return Err("a new process was not findable by its pid");
    }
    *LAYOUT.lock() = Some(lay_out(&process)?);
    *OUTCOME.lock() = None;
    let pid = alloc::format!("{}", process.pid()).into_bytes();
    let ns = fs::namespace();
    let ctx = ns.context();

    // Stopped before it has run, as a `SIGSTOP` leaves it; nothing is told,
    // because a process made for a check has no parent.
    process.enter_stop(SIGSTOP);
    let stopped = check_state(
        ns,
        &ctx,
        &pid,
        State::Stopped,
        "/proc/<pid> does not say T (stopped) for a stopped process",
    );
    process.leave_stop();
    stopped?;

    let task = sched::spawn_user(
        "procfs-check",
        in_the_process,
        Arc::new(
            crate::syscall::thread::Thread::leader(&process)
                .map_err(|_| "no memory for a check's thread")?,
        ),
        None,
        None,
    )
    .map_err(|_| "no task for the /proc check")?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    if process.wait_for_exit(deadline) != Some(REPORTED) {
        return Err("the /proc check's task never reported");
    }
    // Ended, and nobody has reaped it: a zombie.
    check_state(
        ns,
        &ctx,
        &pid,
        State::Zombie,
        "/proc/<pid> does not say Z (zombie) for a process that has ended",
    )?;
    let found = OUTCOME
        .lock()
        .take()
        .ok_or("the /proc check's task ended without a result");

    // Leave no stack behind for a later check's frame count to find: the
    // stage 7 and stage 9 checks measure frames around their own work, and a
    // stack reaped inside that window moves the count.
    drop(task);
    drop(process);
    sched::sleep_for(SETTLE_NANOS);
    let _ = sched::reap();

    let (listed, maps_lines, named, sysctl_values) = found??;
    Ok(Report {
        devices,
        blocks,
        listed,
        maps_lines,
        named,
        stat_cpus,
        stat_ticks,
        stat_apart_ms: STAT_APART_NANOS / 1_000_000,
        sysctl_values,
    })
}

/// The check task's entry: run the `/proc` checks as the process, hand the
/// result back, and end the process so that the boot task's wait returns.
fn in_the_process(_argument: usize) {
    let layout = LAYOUT.lock().take();
    let found = match (process::current(), layout) {
        (Some(me), Some(layout)) => check_proc(&me, &layout),
        (None, _) => Err("the /proc check's task has no process"),
        (_, None) => Err("the /proc check's task was given no layout"),
    };
    *OUTCOME.lock() = Some(found);
    // Every process reference taken above has been dropped by here, which
    // matters: `exit_current` does not return, so nothing after it would.
    process::exit_current(REPORTED)
}

/// Open-for-reading-and-writing flags.
fn read_write() -> OpenFlags {
    OpenFlags {
        read: true,
        write: true,
        ..OpenFlags::default()
    }
}

/// Open-for-reading flags.
fn read_only() -> OpenFlags {
    OpenFlags {
        read: true,
        ..OpenFlags::default()
    }
}

/// `/dev`: the numbers, and the memory devices doing what they are for.
fn check_devices() -> Result<u32, &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    let open = |path: &[u8]| {
        ns.open(&ctx, None, path, &read_write(), 0)
            .map_err(|_| "a /dev node did not open")
    };

    let zero = open(b"/dev/zero")?;
    let mut buf = [0xA5_u8; 64];
    if zero.read(&mut buf) != Ok(buf.len()) || buf.iter().any(|&byte| byte != 0) {
        return Err("/dev/zero did not read as zeros");
    }

    let null = open(b"/dev/null")?;
    if null.write(b"swallowed") != Ok(9) {
        return Err("/dev/null refused a write");
    }
    if null.read(&mut buf) != Ok(0) {
        return Err("/dev/null did not read as end of file");
    }
    for path in MEMORY_DEVICES {
        check_seeks_to_zero(&*open(path)?)?;
    }

    if open(b"/dev/full")?.write(b"x") != Err(Errno::ENOSPC) {
        return Err("/dev/full accepted a write");
    }

    // Thirty-two zero bytes from the generator would be a one in 2^256 event,
    // so seeing them means the node is not reaching it.
    let mut noise = [0_u8; 32];
    if open(b"/dev/urandom")?.read(&mut noise) != Ok(noise.len())
        || noise.iter().all(|&byte| byte == 0)
    {
        return Err("/dev/urandom did not read from the generator");
    }

    for (path, major, minor) in NUMBERS {
        let at = ns
            .resolve(&ctx, None, path, true)
            .map_err(|_| "a /dev node Linux has is missing")?;
        let stat = ns.stat(&at).map_err(|_| "a /dev node cannot be stat'ed")?;
        if stat.metadata.kind != FileType::CharDevice || stat.metadata.rdev != makedev(major, minor)
        {
            return Err("a /dev node does not have the number Linux gives it");
        }
    }
    Ok(NUMBERS.len() as u32)
}

/// The memory devices, which `lseek`, `pread64` and `pwrite64` reach as on
/// Linux rather than being refused as a pipe's are.
const MEMORY_DEVICES: [&[u8]; 5] = [
    b"/dev/null",
    b"/dev/zero",
    b"/dev/full",
    b"/dev/random",
    b"/dev/urandom",
];

/// A memory device is at 0 whatever `lseek` asks, a read between two seeks
/// included, and a positioned read or write does what a plain one does:
/// what a Linux 7.0 host answered, measured (`src/kernel/src/fs/devfs.rs`).
/// busybox `dd of=/dev/null seek=1` dies if the first of these is refused.
fn check_seeks_to_zero(device: &OpenFile) -> Result<(), &'static str> {
    let seeks = [
        (100, Whence::Set),
        (-10, Whence::Set),
        (5, Whence::Current),
        (-5, Whence::Current),
        (10, Whence::End),
        (3, Whence::Data),
        (3, Whence::Hole),
    ];
    for (offset, whence) in seeks {
        if device.seek(offset, whence) != Ok(0) {
            return Err("a /dev memory device did not seek to 0, as Linux's does");
        }
    }
    let mut buf = [0xA5_u8; 16];
    let read = device
        .read(&mut buf)
        .map_err(|_| "a /dev memory device refused a read")?;
    if device.seek(0, Whence::Current) != Ok(0) {
        return Err("a read moved a /dev memory device's position from 0");
    }
    let plain_write = device.write(b"x");
    if device.read_at(1000, &mut buf) != Ok(read) || device.write_at(1000, b"x") != plain_write {
        return Err("pread or pwrite on a /dev memory device did not do what read and write do");
    }
    Ok(())
}

/// `/proc/stat`, read twice across a sleep: each read parses back, has a
/// `cpuN` line for every online processor and no other, a `cpu` line that is
/// their sum, and no processor that has counted more time than has passed
/// since boot; and between the reads no counter went backwards and the total
/// advanced — `top` divides by that difference. Returns the processor lines
/// and how many ticks the total advanced.
fn check_stat(ns: &Namespace) -> Result<(u32, u64), &'static str> {
    let ctx = ns.context();
    let read = || -> Result<Parsed, &'static str> {
        let text = read_all(ns, &ctx, b"/proc/stat", 64)?;
        kstat::parse(&text).ok_or("/proc/stat does not parse back")
    };
    let before = read()?;
    sched::sleep_for(STAT_APART_NANOS);
    let after = read()?;
    let uptime_ticks = crate::timer::now_nanos() / TICK_NANOS;

    let online: Vec<u32> = smp::topology().map_or_else(
        || vec![0],
        |topology| {
            topology
                .cpus()
                .iter()
                .filter(|cpu| cpu.is_online())
                .map(|cpu| u32::try_from(cpu.logical).unwrap_or(u32::MAX))
                .collect()
        },
    );
    for parsed in [&before, &after] {
        if !parsed
            .cpus
            .iter()
            .map(|(cpu, _)| *cpu)
            .eq(online.iter().copied())
        {
            return Err("/proc/stat does not have one cpuN line per online processor");
        }
        let mut sums = [0_u64; 10];
        for (_, times) in &parsed.cpus {
            for (sum, field) in sums.iter_mut().zip(times.fields()) {
                *sum = sum.saturating_add(field);
            }
        }
        // Summed in nanoseconds and then converted, so each field may be up
        // to a tick a processor more than its lines added up.
        let slack = parsed.cpus.len() as u64;
        if parsed
            .total
            .fields()
            .iter()
            .zip(sums)
            .any(|(&total, sum)| total < sum || total > sum.saturating_add(slack))
        {
            return Err("/proc/stat's cpu line is not the sum of its cpuN lines");
        }
        if parsed
            .cpus
            .iter()
            .any(|(_, times)| times.ticks() > uptime_ticks.saturating_add(1))
        {
            return Err("a processor in /proc/stat has counted more time than has passed");
        }
        if parsed.running == 0 {
            return Err("/proc/stat counts no task running, though its reader is one");
        }
    }

    let went_back = before
        .cpus
        .iter()
        .zip(&after.cpus)
        .map(|((_, first), (_, second))| (*first, *second))
        .chain([(before.total, after.total)])
        .any(|(first, second)| {
            first
                .fields()
                .iter()
                .zip(second.fields())
                .any(|(&first, second)| first > second)
        });
    if went_back {
        return Err("a processor's time in /proc/stat went backwards between two reads");
    }
    if after.interrupts < before.interrupts
        || after.context_switches < before.context_switches
        || after.processes < before.processes
    {
        return Err("a counter in /proc/stat went backwards between two reads");
    }
    let advanced = after.total.ticks().saturating_sub(before.total.ticks());
    if advanced == 0 {
        return Err("/proc/stat's time did not advance across a sleep");
    }
    Ok((
        u32::try_from(after.cpus.len()).unwrap_or(u32::MAX),
        advanced,
    ))
}

/// What the check process's address space was given.
#[derive(Debug, Clone, Copy)]
struct Layout {
    /// The heap `brk` made.
    heap: (u64, u64),
    /// The region the stack pointer is in.
    stack: (u64, u64),
}

/// Give the check process an image, a heap, a stack and a guard, and say how
/// it was started. Done by the boot task, before the check task exists:
/// nothing here needs the space installed.
fn lay_out(process: &Process) -> Result<Layout, &'static str> {
    let space = process.space();
    let refused = |_| "the check process's address space refused a region";
    let _ = space
        .map_anonymous(IMAGE, 2 * PAGE_SIZE, VmaFlags::READ_EXECUTE)
        .map_err(refused)?;
    process.set_heap_base(IMAGE + 2 * PAGE_SIZE);
    let (start, _) = process
        .heap_range()
        .ok_or("the check process has no heap after placing one")?;
    let want = start + 3 * PAGE_SIZE;
    if process.set_break(want) != want {
        return Err("brk did not grow the check process's heap");
    }
    let heap = process.heap_range().ok_or("the heap went away")?;

    let stack = space
        .map_anywhere(None, 8 * PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(refused)?;
    let _ = space
        .map_anywhere(None, PAGE_SIZE, VmaFlags::NONE)
        .map_err(refused)?;
    let top = stack + 8 * PAGE_SIZE;
    // The stack pointer a program would have been handed is what names
    // `[stack]`. Nothing ever enters at `IMAGE`: the check task's entry is a
    // kernel function.
    process.set_startup(Startup {
        entry: IMAGE,
        stack: top - 64,
        argument: 0,
        vdso: 0,
        abi: crate::trap::Abi::Native,
    });
    process.record_exec(
        b"/sbin/procfs-check",
        None,
        &[b"procfs-check", b"--self-test"],
    );
    Ok(Layout {
        heap,
        stack: (stack, top),
    })
}

/// `/proc`, read by the process the running task belongs to.
fn check_proc(process: &Arc<Process>, layout: &Layout) -> Found {
    let ns = fs::namespace();
    let ctx = ns.context();

    let pid = alloc::format!("{}", process.pid()).into_bytes();
    if ns.read_link(&ctx, None, b"/proc/self") != Ok(pid.clone()) {
        return Err("/proc/self is not a link to the current process's pid");
    }

    check_state(
        ns,
        &ctx,
        &pid,
        State::Running,
        "/proc/<pid> does not say R (running) for the process reading it",
    )?;
    let descriptors = check_descriptors(ns, &ctx, process)?;
    check_threads(ns, &ctx, &pid)?;
    let listed = check_listing(ns, &ctx, &pid)?;
    let (lines, named) = check_maps(ns, &ctx, process, layout)?;
    drop(descriptors);

    if read_all(ns, &ctx, b"/proc/self/cmdline", 5)? != b"procfs-check\0--self-test\0" {
        return Err("/proc/self/cmdline is not the arguments, each ended by a NUL");
    }
    if ns.read_link(&ctx, None, b"/proc/self/exe") != Ok(b"/sbin/procfs-check".to_vec()) {
        return Err("/proc/self/exe does not name the path the process was started from");
    }
    if read_all(ns, &ctx, b"/proc/self/comm", 64)? != b"procfs-check\n" {
        return Err("/proc/self/comm is not the last component of that path");
    }
    check_oom_score_adj(ns, &ctx)?;

    // The heap is the check's user memory: `getcwd` and `uname` write into
    // it as they would into a program's buffer.
    let buffer = layout.heap.0;
    check_cwd_and_root(ns, &ctx, process, &pid, buffer)?;
    check_object_links(ns, &ctx, process, buffer)?;
    let values = check_sysctl(ns, &ctx, process, buffer)?;
    if !read_all(ns, &ctx, b"/proc/partitions", 5)?.is_empty() {
        return Err("/proc/partitions is not empty, with no block device to list");
    }
    Ok((listed, lines, named, values))
}

/// How long the busy thread of [`check_thread_times`] runs for, by its own
/// clock: six ticks at `USER_HZ`, so that its `stat` counts five at least
/// whatever the first thread has used by then.
const BUSY_NANOS: u64 = 6 * TICK_NANOS;

/// How often the threads of [`check_thread_times`] look at what they wait
/// for. Long enough that the first thread, which does nothing else, uses
/// far less than a tick of processor time while the busy one runs.
const TIMES_POLL_NANOS: u64 = 10_000_000;

/// Set by the busy thread of [`check_thread_times`] once it has run for
/// [`BUSY_NANOS`], or given up.
static TIMES_BUSY_DONE: AtomicBool = AtomicBool::new(false);

/// Set by the boot task once it has read the three files: both threads of
/// [`check_thread_times`] leave.
static TIMES_READ: AtomicBool = AtomicBool::new(false);

/// The first thread of [`check_thread_times`]'s process: asleep until the
/// files have been read, then the end of the process.
fn times_first_thread(_argument: usize) {
    while !TIMES_READ.load(Ordering::Acquire) {
        sched::sleep_for(TIMES_POLL_NANOS);
    }
    process::exit_current(REPORTED)
}

/// The busy thread of [`check_thread_times`]'s process: on a processor
/// until its own clock, `CLOCK_THREAD_CPUTIME_ID`'s, has counted
/// [`BUSY_NANOS`], or the boot task's patience is spent; then asleep until
/// the files have been read, alive so that its time is still the process's.
fn times_busy_thread(_argument: usize) {
    let began = crate::timer::now_nanos();
    let start = sched::current_runtime();
    while sched::current_runtime().saturating_sub(start) < BUSY_NANOS
        && crate::timer::now_nanos().saturating_sub(began) < PATIENCE_NANOS
    {
        core::hint::spin_loop();
    }
    TIMES_BUSY_DONE.store(true, Ordering::Release);
    while !TIMES_READ.load(Ordering::Acquire) {
        sched::sleep_for(TIMES_POLL_NANOS);
    }
    process::exit_thread_current(REPORTED)
}

/// Start `thread` of `process`, listed with its task as `clone` lists them,
/// running `entry` in kernel mode.
fn spawn_times_thread(
    process: &Arc<Process>,
    thread: crate::syscall::thread::Thread,
    name: &'static str,
    entry: fn(usize),
) -> Result<Arc<sched::Task>, &'static str> {
    let thread = Arc::new(thread);
    process.add_thread(&thread);
    let prepared = sched::prepare_user(name, entry, thread, None, None)
        .map_err(|_| "no task for a thread of the /proc times check")?;
    crate::object::process::Host::core(&**process)
        .list_task(prepared.task())
        .map_err(|_| "no memory to list a thread of the /proc times check")?;
    Ok(prepared.launch())
}

/// The processor time a `stat` file counts, in clock ticks: fields 14 and
/// 15, `utime` and `stime`, the twelfth and thirteenth words after the
/// name, which is in parentheses and may hold any byte. Their sum, because
/// the threads read here never leave the kernel: all of it is `utime` while
/// nothing splits a task's time, and would be `stime` once something does.
fn ticks_of(stat: &[u8]) -> Option<u64> {
    let close = stat.iter().rposition(|&byte| byte == b')')?;
    let mut words = stat
        .get(close.checked_add(1)?..)?
        .split(|&byte| byte == b' ' || byte == b'\n')
        .filter(|word| !word.is_empty())
        .skip(11);
    let mut ticks = || -> Option<u64> { core::str::from_utf8(words.next()?).ok()?.parse().ok() };
    ticks()?.checked_add(ticks()?)
}

/// `/proc/<pid>/task/<tid>/stat` counts the one thread's processor time,
/// the first thread's included, and `/proc/<pid>/stat` the whole process's,
/// as Linux's do.
///
/// A process of two threads: the first sleeps, the second runs for six
/// ticks by its own clock and then sleeps too. The busy thread's file must
/// count that time; the first thread's must count less than it; and the two
/// together must not come to more than the process's file, read last, says
/// the process has used. The first thread's id is the process's, and its
/// file was given the process's sum until 2026-10-10: then the two came to
/// the busy thread's time more than the whole. Read by the boot task, by
/// pid, so that neither thread is charged for the reading.
fn check_thread_times() -> Result<(), &'static str> {
    use crate::syscall::thread::Thread;
    const NO_MEMORY: &str = "no memory for a thread of the /proc times check";
    let ns = fs::namespace();
    let ctx = ns.context();
    let process =
        process::new_for_check().map_err(|_| "no address space for the /proc times check")?;
    TIMES_BUSY_DONE.store(false, Ordering::Release);
    TIMES_READ.store(false, Ordering::Release);

    let first = Thread::leader(&process).map_err(|_| NO_MEMORY)?;
    let tid = registry::allocate_thread(&process)
        .ok_or("no thread id for the /proc times check's busy thread")?;
    let busy = Thread::sibling(&process, tid, &first).map_err(|_| NO_MEMORY)?;
    let outcome = (|| {
        let first = spawn_times_thread(&process, first, "procfs-times", times_first_thread)?;
        let busy = spawn_times_thread(&process, busy, "procfs-busy", times_busy_thread)?;
        let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
        while !TIMES_BUSY_DONE.load(Ordering::Acquire) {
            if crate::timer::now_nanos() > deadline {
                return Err("the /proc times check's busy thread never finished running");
            }
            sched::sleep_for(TIMES_POLL_NANOS);
        }
        let read = |tid: Option<u32>| {
            let path = match tid {
                Some(tid) => alloc::format!("/proc/{}/task/{tid}/stat", process.pid()),
                None => alloc::format!("/proc/{}/stat", process.pid()),
            };
            ticks_of(&read_all(ns, &ctx, path.as_bytes(), 64)?)
                .ok_or("a stat file of the /proc times check has no times to read")
        };
        let first_ticks = read(Some(process.pid()))?;
        let busy_ticks = read(Some(tid))?;
        // Last: both threads are alive, so it is no less than what the two
        // had been charged when their own files were read.
        let process_ticks = read(None)?;
        drop((first, busy));
        if busy_ticks < BUSY_NANOS / TICK_NANOS {
            return Err(
                "/proc/<pid>/task/<tid>/stat counts less time for a thread than its own \
                 clock said it ran",
            );
        }
        if first_ticks.saturating_add(busy_ticks) > process_ticks {
            return Err(
                "/proc/<pid>/task/<pid>/stat counts another thread's time as the first \
                 thread's: two threads' times come to more than /proc/<pid>/stat's",
            );
        }
        if first_ticks >= busy_ticks {
            return Err(
                "/proc/<pid>/task/<tid>/stat counts as much time for a thread that slept as \
                 for one that ran",
            );
        }
        Ok(())
    })();

    // Whatever was found: the threads leave, and no stack stays behind for
    // a later check's frame count to find (see `run`).
    TIMES_READ.store(true, Ordering::Release);
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let ended = process.wait_for_exit(deadline);
    drop(process);
    sched::sleep_for(SETTLE_NANOS);
    let _ = sched::reap();
    outcome?;
    if ended != Some(REPORTED) {
        return Err("the /proc times check's process never ended");
    }
    Ok(())
}

/// `/proc/<pid>/stat`'s state letter and `/proc/<pid>/status`'s `State:`
/// line both say `want`, or `Err(why)`.
fn check_state(
    ns: &Namespace,
    ctx: &Context,
    pid: &[u8],
    want: State,
    why: &'static str,
) -> Result<(), &'static str> {
    let directory = [b"/proc/", pid].concat();
    let stat = read_all(ns, ctx, &[directory.as_slice(), b"/stat"].concat(), 64)?;
    // The letter follows the name, which is in parentheses and may hold any.
    let letter = stat
        .iter()
        .rposition(|&byte| byte == b')')
        .and_then(|close| stat.get(close.saturating_add(2)))
        .map(|&byte| char::from(byte));
    let status = read_all(ns, ctx, &[directory.as_slice(), b"/status"].concat(), 64)?;
    let line = [b"State:\t", want.description().as_bytes(), b"\n"].concat();
    if letter != Some(want.letter()) || !status.windows(line.len()).any(|seen| seen == line) {
        return Err(why);
    }
    Ok(())
}

/// `/proc/<pid>/task`: the check process's one thread is listed under its
/// pid, as a directory, and its `status` and `stat` name it; `status` counts
/// one thread. A process of several threads is counted by the threads exit
/// test, which reads these files from inside one.
fn check_threads(ns: &Namespace, ctx: &Context, pid: &[u8]) -> Result<(), &'static str> {
    let listing = list(ns, ctx, b"/proc/self/task")?;
    let [(only, FileType::Directory, _)] = listing.as_slice() else {
        return Err("/proc/self/task does not list the process's one thread as a directory");
    };
    if only != pid {
        return Err("/proc/self/task names the process's one thread by another id");
    }

    let mut id_lines = b"\nTgid:\t".to_vec();
    id_lines.extend_from_slice(pid);
    id_lines.extend_from_slice(b"\nNgid:\t0\nPid:\t");
    id_lines.extend_from_slice(pid);
    id_lines.push(b'\n');
    let contains = |haystack: &[u8], needle: &[u8]| {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    };

    let mut path = b"/proc/self/task/".to_vec();
    path.extend_from_slice(pid);
    let mut status = path.clone();
    status.extend_from_slice(b"/status");
    let text = read_all(ns, ctx, &status, 64)?;
    if !contains(&text, &id_lines) {
        return Err("/proc/self/task/<pid>/status does not give the thread's Tgid and Pid");
    }
    if !contains(&text, b"\nThreads:\t1\n") {
        return Err("/proc/self/task/<pid>/status does not count one thread");
    }
    if !contains(
        &read_all(ns, ctx, b"/proc/self/status", 64)?,
        b"\nThreads:\t1\n",
    ) {
        return Err("/proc/self/status does not count one thread");
    }

    let mut stat = path;
    stat.extend_from_slice(b"/stat");
    let mut first = pid.to_vec();
    first.extend_from_slice(b" (");
    if !read_all(ns, ctx, &stat, 64)?.starts_with(&first) {
        return Err("/proc/self/task/<pid>/stat does not start with the thread's id");
    }
    Ok(())
}

/// `/proc/<pid>/cwd` and `root`: the working directory reads as `getcwd`
/// answers it, through `self` and through the pid, and the root as `/`.
fn check_cwd_and_root(
    ns: &Namespace,
    ctx: &Context,
    process: &Process,
    pid: &[u8],
    buffer: u64,
) -> Result<(), &'static str> {
    let dev = ns
        .resolve(ctx, None, b"/dev", true)
        .map_err(|_| "/dev cannot be walked to for the cwd check")?;
    let old = core::mem::replace(&mut process.fs_context().lock().cwd, dev);
    // Dropped with the lock released, as `chdir` drops it.
    drop(old);

    let len = path::sys_getcwd(process, buffer, 256).map_err(|_| "getcwd was refused")?;
    let mut answer = vec![0_u8; len];
    uaccess::copy_from_user(process.space(), buffer, &mut answer)
        .map_err(|_| "getcwd's answer could not be read back")?;
    if answer.pop() != Some(0) || answer != b"/dev" {
        return Err("getcwd does not answer the directory the check moved to");
    }
    let by_pid = [b"/proc/".as_slice(), pid, b"/cwd".as_slice()].concat();
    if ns.read_link(ctx, None, b"/proc/self/cwd") != Ok(answer.clone())
        || ns.read_link(ctx, None, &by_pid) != Ok(answer)
    {
        return Err("/proc/<pid>/cwd is not the path getcwd answers");
    }
    if ns.read_link(ctx, None, b"/proc/self/root") != Ok(b"/".to_vec()) {
        return Err("/proc/self/root is not /");
    }
    Ok(())
}

/// The host name's value under `/proc/sys`.
const HOSTNAME: &[u8] = b"/proc/sys/kernel/hostname";

/// `/proc/sys`: every value walked to is one line; those with a source
/// elsewhere read as the source says; a read-only one refuses a write as
/// Linux does; and a host name written to `kernel/hostname` is the one
/// `uname` reports. The kernel is left as the check found it, whatever the
/// check found: a name that was set is written back, and a name that was
/// never set is forgotten again. Returns the values read.
fn check_sysctl(
    ns: &Namespace,
    ctx: &Context,
    process: &Process,
    buffer: u64,
) -> Result<u32, &'static str> {
    let values = check_sysctl_values(ns, ctx)?;

    let write_only = OpenFlags {
        write: true,
        ..OpenFlags::default()
    };
    let ostype = ns
        .open(ctx, None, b"/proc/sys/kernel/ostype", &write_only, 0)
        .map_err(|_| "/proc/sys/kernel/ostype did not open for writing")?;
    if ostype.write(b"Ferrix\n") != Err(Errno::EACCES) {
        return Err("a read-only /proc/sys value did not refuse a write with EACCES");
    }
    let truncating = OpenFlags {
        truncate: true,
        ..write_only
    };
    if ns
        .open(ctx, None, b"/proc/sys/kernel/ostype", &truncating, 0)
        .err()
        != Some(Errno::EACCES)
    {
        return Err("a read-only /proc/sys value opened to be truncated");
    }

    check_sysctl_open_access(process, buffer)?;

    let before = read_all(ns, ctx, HOSTNAME, 64)?;
    let was_set = system::hostname_is_set();
    let outcome = check_host_name_reaches_uname(ns, ctx, process, buffer);
    let put_back = if was_set {
        write_value(ns, ctx, HOSTNAME, &before)
    } else {
        system::forget_hostname();
        Ok(())
    };
    let restored = put_back.and_then(|()| {
        if read_all(ns, ctx, HOSTNAME, 64)? == before {
            Ok(())
        } else {
            Err("the host name could not be put back through /proc/sys")
        }
    });
    outcome?;
    restored?;
    Ok(values)
}

/// Walk `/proc/sys`, read every value, and hold the ones with a source
/// elsewhere to it.
fn check_sysctl_values(ns: &Namespace, ctx: &Context) -> Result<u32, &'static str> {
    let mut pending: Vec<Vec<u8>> = vec![b"/proc/sys".to_vec()];
    let mut values = 0_u32;
    while let Some(dir) = pending.pop() {
        for (name, kind, _) in list(ns, ctx, &dir)? {
            let path = [dir.as_slice(), b"/", &name].concat();
            match kind {
                FileType::Directory => pending.push(path),
                FileType::Regular => {
                    let value = read_all(ns, ctx, &path, 3)?;
                    let lines = value.iter().filter(|&&byte| byte == b'\n').count();
                    if value.last() != Some(&b'\n') || lines != 1 {
                        return Err("a /proc/sys value is not one line");
                    }
                    values += 1;
                }
                _ => return Err("/proc/sys holds something neither a directory nor a value"),
            }
            if values > LISTING_LIMIT {
                return Err("walking /proc/sys does not end");
            }
        }
    }

    let line = |text: &dyn core::fmt::Display| alloc::format!("{text}\n").into_bytes();
    let sourced: [(&[u8], Vec<u8>); 5] = [
        (b"/proc/sys/kernel/ostype", line(&SYSNAME)),
        (b"/proc/sys/kernel/osrelease", line(&RELEASE)),
        (b"/proc/sys/kernel/version", line(&VERSION)),
        (b"/proc/sys/kernel/pid_max", line(&registry::PID_MAX)),
        (
            b"/proc/sys/vm/overcommit_memory",
            line(&super::render::OVERCOMMIT_ALWAYS),
        ),
    ];
    for (path, want) in sourced {
        if read_all(ns, ctx, path, 64)? != want {
            return Err(
                "a /proc/sys value is not what uname, the pid registry or the memory policy says",
            );
        }
    }
    Ok(values)
}

/// `openat` refuses a read-only `/proc/sys` value for writing at the open,
/// with `EACCES`, for `O_WRONLY`, `O_RDWR` and `O_TRUNC` alike, as Linux's
/// `proc_sys_permission` does; and it still opens that value for reading and
/// a writable one for writing. The namespace open above bypasses `openat`,
/// which is how the write refusal behind this one is reached.
fn check_sysctl_open_access(process: &Process, buffer: u64) -> Result<(), &'static str> {
    const O_WRONLY: u32 = 0o1;
    const O_RDWR: u32 = 0o2;
    const O_TRUNC: u32 = 0o1000;
    const OSTYPE: &[u8] = b"/proc/sys/kernel/ostype\0";
    let open = |path: &[u8], flags: u32| {
        uaccess::copy_to_user(process.space(), buffer, path)
            .map_err(|_| "could not stage a /proc/sys path")
            .map(|()| fd::sys_openat(process, AT_FDCWD, buffer, flags, 0))
    };
    for flags in [O_WRONLY, O_RDWR, O_WRONLY | O_TRUNC] {
        if open(OSTYPE, flags)? != Err(Errno::EACCES) {
            return Err("openat did not refuse a read-only /proc/sys value for writing");
        }
    }
    let allowed: [(&[u8], u32); 2] = [(OSTYPE, 0), (b"/proc/sys/kernel/hostname\0", O_WRONLY)];
    for (path, flags) in allowed {
        let opened = open(path, flags)?.map_err(|_| "openat refused a /proc/sys open it allows")?;
        let number = i32::try_from(opened).map_err(|_| "openat's descriptor is not an int")?;
        let _ =
            fd::sys_close(process, number).map_err(|_| "a /proc/sys descriptor did not close")?;
    }
    Ok(())
}

/// A name written to `kernel/hostname`, newline and all, is `uname`'s
/// `nodename` without the newline, and reads back with it.
fn check_host_name_reaches_uname(
    ns: &Namespace,
    ctx: &Context,
    process: &Process,
    buffer: u64,
) -> Result<(), &'static str> {
    write_value(ns, ctx, HOSTNAME, b"procfs-check\n")?;
    let _ = system::sys_uname(process, buffer).map_err(|_| "uname was refused")?;
    let mut uts = [0_u8; 130];
    uaccess::copy_from_user(process.space(), buffer, &mut uts)
        .map_err(|_| "uname's answer could not be read back")?;
    if uts.get(65..78) != Some(b"procfs-check\0".as_slice()) {
        return Err("uname's nodename is not the name written to /proc/sys/kernel/hostname");
    }
    if read_all(ns, ctx, HOSTNAME, 64)? != b"procfs-check\n" {
        return Err("/proc/sys/kernel/hostname does not read back the name written to it");
    }
    Ok(())
}

/// `/proc/self/oom_score_adj` starts at 0, keeps what is written to it, and
/// refuses a value out of range with `EINVAL`, keeping the one it had.
fn check_oom_score_adj(ns: &Namespace, ctx: &Context) -> Result<(), &'static str> {
    const PATH: &[u8] = b"/proc/self/oom_score_adj";
    if read_all(ns, ctx, PATH, 16)? != b"0\n" {
        return Err("/proc/self/oom_score_adj does not start at 0");
    }
    write_value(ns, ctx, PATH, b"300\n")?;
    if read_all(ns, ctx, PATH, 16)? != b"300\n" {
        return Err("/proc/self/oom_score_adj does not read back what was written to it");
    }
    let flags = OpenFlags {
        write: true,
        ..OpenFlags::default()
    };
    let file = ns
        .open(ctx, None, PATH, &flags, 0)
        .map_err(|_| "/proc/self/oom_score_adj did not open for writing")?;
    if file.write(b"2000\n") != Err(Errno::EINVAL) {
        return Err("/proc/self/oom_score_adj took a value past 1000");
    }
    if read_all(ns, ctx, PATH, 16)? != b"300\n" {
        return Err("a refused write changed /proc/self/oom_score_adj");
    }
    Ok(())
}

/// Write `data` to a value under `/proc/sys`, as `echo … >` does: opened to
/// truncate, and written whole.
fn write_value(
    ns: &Namespace,
    ctx: &Context,
    path: &[u8],
    data: &[u8],
) -> Result<(), &'static str> {
    let flags = OpenFlags {
        write: true,
        truncate: true,
        ..OpenFlags::default()
    };
    let file = ns
        .open(ctx, None, path, &flags, 0)
        .map_err(|_| "a writable /proc/sys value did not open for writing")?;
    if file.write(data) != Ok(data.len()) {
        return Err("a writable /proc/sys value did not take the whole write");
    }
    Ok(())
}

/// `/proc/<pid>/fd`: a descriptor's link names where it was opened, and says
/// ` (deleted)` once that name is gone. Returns the descriptors, which the
/// caller keeps open until the listing has walked them.
fn check_descriptors(
    ns: &Namespace,
    ctx: &Context,
    process: &Process,
) -> Result<Vec<i32>, &'static str> {
    let null = ns
        .open(ctx, None, b"/dev/null", &read_only(), 0)
        .map_err(|_| "/dev/null did not open for the descriptor check")?;
    let create = OpenFlags {
        create: true,
        exclusive: true,
        ..read_write()
    };
    let doomed = ns
        .open(ctx, None, b"/tmp/procfs-check", &create, 0o600)
        .map_err(|_| "a file under /tmp could not be made")?;
    let (null_fd, doomed_fd) = {
        let mut files = process.files().lock();
        let null_fd = files.insert(null, false);
        let doomed_fd = files.insert(doomed, false);
        (null_fd, doomed_fd)
    };
    let (Ok(null_fd), Ok(doomed_fd)) = (null_fd, doomed_fd) else {
        return Err("the check process's descriptor table refused a file");
    };
    ns.unlink(ctx, None, b"/tmp/procfs-check")
        .map_err(|_| "the file under /tmp could not be removed")?;

    let link = |fd: i32| {
        let path = alloc::format!("/proc/self/fd/{fd}").into_bytes();
        ns.read_link(ctx, None, &path)
    };
    if link(null_fd) != Ok(b"/dev/null".to_vec()) {
        return Err("/proc/self/fd names a descriptor's path wrongly");
    }
    if link(doomed_fd) != Ok(b"/tmp/procfs-check (deleted)".to_vec()) {
        return Err("/proc/self/fd does not say a removed file was deleted");
    }
    if link(doomed_fd.saturating_add(100)).is_ok() {
        return Err("/proc/self/fd has a link for a descriptor that is not open");
    }
    Ok(vec![null_fd, doomed_fd])
}

/// `/proc/self/fd/<n>` of a socket and of an anonymous file, as `lsof`
/// uses it: `stat` through the link is `fstat` of the descriptor -- the
/// same device, inode number and type, a socket's `S_IFSOCK` -- while
/// `lstat` is the link itself and `readlink` names the object; and `openat`
/// through it is `ENXIO`, as Linux's `sock_no_open` and `no_open` answer.
fn check_object_links(
    ns: &Namespace,
    ctx: &Context,
    process: &Process,
    buffer: u64,
) -> Result<(), &'static str> {
    let (socket, _peer) = fs::socket::new_pair(SocketType::Stream, false, process)
        .map_err(|_| "no socket pair for the /proc/self/fd check")?;
    let eventfd = fs::eventfd::create(0, false, false)
        .map_err(|_| "no eventfd for the /proc/self/fd check")?;
    let socket_ino = socket.inode().metadata().ino;
    let objects: [(Arc<OpenFile>, FileType, Vec<u8>); 2] = [
        (
            socket,
            FileType::Socket,
            alloc::format!("socket:[{socket_ino}]").into_bytes(),
        ),
        (eventfd, FileType::Regular, b"anon_inode:[eventfd]".to_vec()),
    ];
    for (file, kind, name) in objects {
        let described = ns
            .stat(file.location())
            .map_err(|_| "fstat of a socket or eventfd failed")?;
        let number = process
            .files()
            .lock()
            .insert(Arc::clone(&file), false)
            .map_err(|_| "the check process's descriptor table refused a file")?;
        let link = alloc::format!("/proc/self/fd/{number}").into_bytes();
        let found = check_object_link(ns, ctx, process, buffer, &link, kind, &name, &described);
        let closed = fd::sys_close(process, number);
        found?;
        let _ = closed.map_err(|_| "a /proc/self/fd check descriptor did not close")?;
    }
    Ok(())
}

/// [`check_object_links`] for one descriptor, open at `link`.
#[expect(
    clippy::too_many_arguments,
    reason = "one descriptor's facts, each compared once"
)]
fn check_object_link(
    ns: &Namespace,
    ctx: &Context,
    process: &Process,
    buffer: u64,
    link: &[u8],
    kind: FileType,
    name: &[u8],
    described: &ferrix_vfs::Stat,
) -> Result<(), &'static str> {
    let followed = ns
        .resolve(ctx, None, link, true)
        .and_then(|at| ns.stat(&at))
        .map_err(|_| "stat of /proc/self/fd/<n> of a socket or eventfd failed")?;
    if followed.metadata.kind != kind
        || followed.metadata.ino != described.metadata.ino
        || followed.dev != described.dev
    {
        return Err("stat of /proc/self/fd/<n> is not fstat of a socket or eventfd");
    }
    let itself = ns
        .resolve(ctx, None, link, false)
        .and_then(|at| ns.stat(&at))
        .map_err(|_| "lstat of /proc/self/fd/<n> of a socket or eventfd failed")?;
    if itself.metadata.kind != FileType::Symlink {
        return Err("lstat of /proc/self/fd/<n> is not the link itself");
    }
    if ns.read_link(ctx, None, link).as_deref() != Ok(name) {
        return Err("/proc/self/fd/<n> does not name a socket or eventfd as Linux does");
    }
    let mut path = link.to_vec();
    path.push(0);
    uaccess::copy_to_user(process.space(), buffer, &path)
        .map_err(|_| "could not stage a /proc/self/fd path")?;
    if fd::sys_openat(process, AT_FDCWD, buffer, 0, 0) != Err(Errno::ENXIO) {
        return Err("openat of /proc/self/fd/<n> of a socket or eventfd is not ENXIO");
    }
    Ok(())
}

/// One directory's entries: name, kind and inode number, `.` and `..` left
/// out.
fn list(ns: &Namespace, ctx: &Context, path: &[u8]) -> Result<Listing, &'static str> {
    let flags = OpenFlags {
        directory: true,
        ..read_only()
    };
    let dir = ns
        .open(ctx, None, path, &flags, 0)
        .map_err(|_| "a directory /proc listed did not open")?;
    let mut entries = Vec::new();
    dir.read_dir(&mut |entry| {
        if entry.name != b"." && entry.name != b".." {
            entries.push((entry.name.to_vec(), entry.kind, entry.ino));
        }
        true
    })
    .map_err(|_| "a directory /proc listed could not be read")?;
    Ok(entries)
}

/// `ls -R /proc`: list every directory without following links, walk to
/// every name listed, and require the walk to end; and every inode number
/// listed to fit 32 bits and be one name's alone, as a 32-bit program's
/// `readdir` needs and `find` assumes.
fn check_listing(ns: &Namespace, ctx: &Context, pid: &[u8]) -> Result<u32, &'static str> {
    let mut pending: Vec<Vec<u8>> = vec![b"/proc".to_vec()];
    let mut numbers = alloc::collections::BTreeSet::new();
    let mut listed = 0_u32;
    let mut seen_self = false;
    let mut seen_pid = false;
    while let Some(dir) = pending.pop() {
        let top = dir == b"/proc";
        for (name, kind, ino) in list(ns, ctx, &dir)? {
            let mut path = dir.clone();
            path.push(b'/');
            path.extend_from_slice(&name);
            walk_back(ns, ctx, &path, kind, ino)?;
            if u32::try_from(ino).is_err() {
                return Err("a /proc inode number does not fit 32 bits");
            }
            if !numbers.insert(ino) {
                return Err("two /proc names share an inode number");
            }
            listed += 1;
            if listed > LISTING_LIMIT {
                return Err("listing /proc recursively does not end");
            }
            seen_self |= top && name == b"self" && kind == FileType::Symlink;
            seen_pid |= top && name == pid && kind == FileType::Directory;
            if kind == FileType::Directory {
                pending.push(path);
            }
        }
    }
    if !seen_self {
        return Err("/proc does not list self as a symbolic link");
    }
    if !seen_pid {
        return Err("/proc does not list the current process's directory");
    }
    Ok(listed)
}

/// A listed name must lead, without following a link, to what the listing
/// said it was.
fn walk_back(
    ns: &Namespace,
    ctx: &Context,
    path: &[u8],
    kind: FileType,
    ino: u64,
) -> Result<(), &'static str> {
    let at = ns
        .resolve(ctx, None, path, false)
        .map_err(|_| "a name /proc listed cannot be walked to")?;
    let stat = ns
        .stat(&at)
        .map_err(|_| "a name /proc listed cannot be stat'ed")?;
    if stat.metadata.kind != kind || stat.metadata.ino != ino {
        return Err("a /proc listing and stat disagree about a name");
    }
    Ok(())
}

/// Read a whole file `piece` bytes at a time.
fn read_all(
    ns: &Namespace,
    ctx: &Context,
    path: &[u8],
    piece: usize,
) -> Result<Vec<u8>, &'static str> {
    let file = ns
        .open(ctx, None, path, &read_only(), 0)
        .map_err(|_| "a /proc file did not open")?;
    read_rest(&file, piece, Vec::new())
}

/// Read the rest of an open file onto `out`, `piece` bytes at a time.
fn read_rest(file: &OpenFile, piece: usize, mut out: Vec<u8>) -> Result<Vec<u8>, &'static str> {
    let mut buf = vec![0_u8; piece];
    loop {
        let count = file
            .read(&mut buf)
            .map_err(|_| "a /proc file could not be read")?;
        let Some(got) = buf.get(..count).filter(|got| !got.is_empty()) else {
            return Ok(out);
        };
        out.extend_from_slice(got);
    }
}

/// `/proc/self/maps`: read seven bytes at a time with the map changing
/// underneath, it is still the map as it was at open, a line per region, with
/// the heap and the stack named where the process put them.
fn check_maps(
    ns: &Namespace,
    ctx: &Context,
    process: &Process,
    layout: &Layout,
) -> Result<(u32, u32), &'static str> {
    let regions = process
        .space()
        .regions()
        .map_err(|_| "no memory to list the regions")?
        .len();
    let file = ns
        .open(ctx, None, b"/proc/self/maps", &read_only(), 0)
        .map_err(|_| "/proc/self/maps did not open")?;
    let mut first = [0_u8; 7];
    let count = file
        .read(&mut first)
        .map_err(|_| "/proc/self/maps could not be read")?;
    let _ = process
        .space()
        .map_anywhere(None, PAGE_SIZE, VmaFlags::READ)
        .map_err(|_| "the check process's address space refused a region")?;
    let text = read_rest(&file, 7, first.get(..count).unwrap_or_default().to_vec())?;

    let mut lines = 0_u32;
    let mut named = 0_u32;
    let (mut heap, mut stack) = (false, false);
    for line in text
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let mapping = maps::parse(line).ok_or("a /proc/self/maps line does not parse")?;
        lines += 1;
        let inside = |(start, end): (u64, u64)| mapping.start >= start && mapping.end <= end;
        match mapping.name {
            None => {}
            Some(b"[heap]") if inside(layout.heap) && mapping.write => heap = true,
            Some(b"[stack]") if inside(layout.stack) && mapping.write => stack = true,
            Some(_) => return Err("/proc/self/maps names a region it should not"),
        }
        named += u32::from(mapping.name.is_some());
    }
    if lines as usize != regions {
        return Err("/proc/self/maps is not one line per region as they were at open");
    }
    if !heap || !stack {
        return Err("/proc/self/maps does not name the heap and the stack");
    }
    Ok((lines, named))
}
