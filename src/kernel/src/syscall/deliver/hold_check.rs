//! The crash hold, checked (`fs::crashes`; the consultant's conditions,
//! ledger lines 3501 (9) and 3524).
//!
//! The faults are made by kernel tasks of check processes calling the fault
//! path's own entry -- [`super::fault_signal`] once, unarmed, and
//! [`super::force_held`] after -- so the case runs the same on every
//! architecture and writes the identity line once. The reader is a task of a
//! process of its own, which opens `/dev/ferrix-crashes` through the
//! namespace as a program would. A thread that is to park comes back
//! through [`super::return_to_user`] as the delivery cap's check drives it.
//!
//! Each sub-case is required, in this order:
//!
//! * (a) unarmed, a fault ends its process at once, by its signal;
//! * the open: read-only `EINVAL`, a second open `EBUSY`; with nothing
//!   queued a read is `EAGAIN` under `O_NONBLOCK` and a wait a signal ends
//!   (`ERESTARTSYS`);
//! * (f) started first: with no `done`, the hold ends at the deadline,
//!   measured from the fault, within [`MARGIN_NANOS`];
//! * (b) armed, a process of two threads is held: a buffer too small for
//!   its line is `EINVAL`, the line kept; the line says two
//!   threads, none unparked; `/proc/<pid>/status` says `t (tracing stop)`,
//!   its `maps` and `fd` read; it is not stopped, and `SIGCONT` does not
//!   release it;
//! * (c) `done` releases it at once, by its signal; `done` for a pid not held,
//!   a malformed line and a partial one are `EINVAL`;
//! * (d) `SIGKILL` releases one at once, by the fault's signal;
//! * (i) another thread's `exit_group(0)` during the hold ends it by the
//!   fault's signal too;
//! * (5d) a process holding the reader's description is not held;
//! * (h) sixteen held at once, the seventeenth not, and counted;
//! * (e) the reader's close releases all sixteen at once;
//! * (g) last, a fault of the reader's own process is not held.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_linux_abi::types::{SIG_DFL, SIGCONT, SIGKILL, SIGSEGV, SIGUSR1};
use ferrix_sync::IrqControl;
use ferrix_vfs::{Errno, OpenFile, OpenFlags};

use crate::arch;
use crate::fs::crashes::{HOLD_NANOS, SLOTS};
use crate::sched;
use crate::sync::SpinLock;
use crate::syscall::process::{self, Process};
use crate::syscall::signal::{self, Origin};
use crate::syscall::thread::{self, Thread};
use crate::syscall::{kill, registry};

/// How late past its deadline a hold may end, and how soon a release must
/// take effect.
const MARGIN_NANOS: u64 = 1_000_000_000;

/// How long a release by `done`, `SIGKILL`, `exit_group` or a close may take.
const AT_ONCE_NANOS: u64 = 500_000_000;

/// How long the check waits for anything else.
const PATIENCE_NANOS: u64 = 10_000_000_000;

/// The poll of the check's waits.
const POLL_NANOS: u64 = 1_000_000;

/// The fault the cases take: `SEGV_MAPERR` at a small address.
const CODE: i32 = 1;
const ADDRESS: u64 = 0x10;
const PC: u64 = 0x40_1000;

/// What the reader found, for the boot line.
#[derive(Debug, Clone, Copy, Default)]
pub struct Report {
    /// Microseconds from `done` to the process's end.
    pub done_micros: u64,
    /// Milliseconds from the fault to its end at the deadline.
    pub deadline_millis: u64,
    /// How many the full ring held.
    pub held_at_once: u32,
    /// How many faults the next line said were not held.
    pub dropped: u32,
}

static OUTCOME: SpinLock<Option<Result<Report, &'static str>>> = SpinLock::new(None);

/// Set by (a)'s fault: the unarmed fault path said the process ended.
static UNARMED_ENDED: AtomicBool = AtomicBool::new(false);

/// Set once (i)'s second thread may end its process.
static EXIT_NOW: AtomicBool = AtomicBool::new(false);

/// When (f)'s fault was taken.
static DEADLINE_FAULT_AT: AtomicU64 = AtomicU64::new(0);

/// Sleep until `ready` or `deadline`; whether it became ready.
fn wait_for(mut ready: impl FnMut() -> bool, deadline: u64) -> bool {
    while !ready() {
        if crate::timer::now_nanos() > deadline {
            return false;
        }
        sched::sleep_for(POLL_NANOS);
    }
    true
}

fn after(nanos: u64) -> u64 {
    crate::timer::now_nanos().saturating_add(nanos)
}

/// A fault task: the fault path's hold, then the thread's end.
fn faulter(_: usize) {
    let _ = super::force_held(SIGSEGV, CODE, ADDRESS, PC);
    process::leave_current()
}

/// (f)'s fault task, which says when it faulted.
fn timed_faulter(_: usize) {
    DEADLINE_FAULT_AT.store(crate::timer::now_nanos(), Ordering::Release);
    faulter(0);
}

/// (a)'s fault task, through the whole fault entry.
fn unarmed_faulter(_: usize) {
    let outcome = super::fault_signal(SIGSEGV, CODE, ADDRESS, PC);
    UNARMED_ENDED.store(
        matches!(outcome, crate::trap::FaultOutcome::Ended { .. }),
        Ordering::Release,
    );
    process::leave_current()
}

/// A second thread that goes back towards user mode whenever it has to, and
/// so parks while its process is held.
fn parker(_: usize) {
    loop {
        let Some(me) = thread::current() else {
            process::leave_current()
        };
        let process = me.process();
        if process.must_leave(&me) {
            break;
        }
        let mut context = arch::UserContext::from_trap(&arch::TrapFrame::default());
        let saved = <arch::Irq as IrqControl>::disable();
        if crate::trap::attention_due(&super::RETURN_PATH) {
            super::return_to_user(&mut context);
        }
        <arch::Irq as IrqControl>::restore(saved);
        sched::sleep_for(POLL_NANOS);
    }
    process::leave_current()
}

/// (i)'s second thread: `exit_group(0)` once the process is held.
fn exiter(_: usize) {
    while !EXIT_NOW.load(Ordering::Acquire) {
        sched::sleep_for(POLL_NANOS);
    }
    process::exit_current(0)
}

/// A process of one fault task, and `with` as a second thread.
/// `process`'s first thread, made out of line.
#[inline(never)]
fn leader(process: &Arc<Process>) -> Option<Arc<Thread>> {
    Thread::leader(process).ok().map(Arc::new)
}

/// A second thread of `process`, beside `first`, made out of line.
#[inline(never)]
fn sibling(process: &Arc<Process>, first: &Thread) -> Option<Arc<Thread>> {
    let tid = registry::allocate_thread(process)?;
    Thread::sibling(process, tid, first).ok().map(Arc::new)
}

#[inline(never)]
fn faulting(entry: fn(usize), with: Option<fn(usize)>) -> Result<Arc<Process>, &'static str> {
    const NO_MEMORY: &str = "no memory for a crash hold check's process";
    let process = process::new_for_check().map_err(|_| NO_MEMORY)?;
    // A `Thread` is large: each is made in a frame of its own and kept in
    // its `Arc`. Two held by value here took six KiB of the reader's
    // stack, which an interrupt on top of it then overflowed (x86-64's
    // double fault, AArch64's fault in the guard page).
    let first = leader(&process).ok_or(NO_MEMORY)?;
    let second = match with {
        Some(other) => Some((sibling(&process, &first).ok_or(NO_MEMORY)?, other)),
        None => None,
    };
    // Both listed before either runs, as `clone` lists a thread before it
    // starts: a hold posts its park to the tasks listed.
    let mut ready = Vec::new();
    for (thread, entry) in [Some((first, entry)), second].into_iter().flatten() {
        process.add_thread(&thread);
        let prepared = sched::prepare_user("crash-hold", entry, thread, None, None)
            .map_err(|_| "no task for a crash hold check's thread")?;
        crate::object::process::Host::core(&*process)
            .list_task(prepared.task())
            .map_err(|_| NO_MEMORY)?;
        ready.push(prepared);
    }
    for prepared in ready {
        let _ = prepared.launch();
    }
    Ok(process)
}

/// Whether `process` ended by `SIGSEGV` before `deadline`.
#[inline(never)]
fn ended_by_fault(process: &Process, deadline: u64) -> bool {
    process.wait_for_exit(deadline).is_some() && process.ended_by_signal() == Some(SIGSEGV)
}

/// The next queue line, waited for until `deadline`.
#[inline(never)]
fn next_line(file: &OpenFile, deadline: u64) -> Result<Vec<u8>, &'static str> {
    if !wait_for(|| file.poll().readable, deadline) {
        return Err("no queue line came for a held fault");
    }
    let mut buf = alloc::vec![0_u8; 256];
    let read = file
        .read(&mut buf)
        .map_err(|_| "a queue line could not be read")?;
    buf.truncate(read);
    Ok(buf)
}

/// The words of a line.
fn words(line: &[u8]) -> Vec<&[u8]> {
    line.split(|&byte| byte == b' ' || byte == b'\n')
        .filter(|word| !word.is_empty())
        .collect()
}

/// The number after `name` in a line.
fn field(line: &[u8], name: &[u8]) -> Option<u64> {
    let words = words(line);
    let at = words.iter().position(|&word| word == name)?;
    core::str::from_utf8(words.get(at + 1)?).ok()?.parse().ok()
}

/// The pid of a line.
fn pid_of(line: &[u8]) -> Option<u32> {
    core::str::from_utf8(words(line).get(1)?).ok()?.parse().ok()
}

/// Read a `/proc` file of `pid` whole.
#[inline(never)]
fn proc_file(pid: u32, name: &str) -> Result<Vec<u8>, &'static str> {
    let ns = crate::fs::namespace();
    let ctx = ns.context();
    let path = alloc::format!("/proc/{pid}/{name}");
    let read = OpenFlags {
        read: true,
        ..OpenFlags::default()
    };
    let file = ns
        .open(&ctx, None, path.as_bytes(), &read, 0)
        .map_err(|_| "a held process's /proc file did not open")?;
    let mut out = Vec::new();
    let mut buf = alloc::vec![0_u8; 512];
    loop {
        let count = file
            .read(&mut buf)
            .map_err(|_| "a held process's /proc file did not read")?;
        if count == 0 {
            return Ok(out);
        }
        out.extend_from_slice(buf.get(..count).unwrap_or_default());
    }
}

/// Wait until `process` is held and its line read; the line.
#[inline(never)]
fn held_line(file: &OpenFile, process: &Process) -> Result<Vec<u8>, &'static str> {
    let line = next_line(file, after(PATIENCE_NANOS))?;
    if pid_of(&line) != Some(process.pid()) || !process.is_held() {
        return Err("a fault while armed was not held, or its line named another pid");
    }
    Ok(line)
}

/// The reader's cases, in its own process.
#[inline(never)]
fn cases() -> Result<Report, &'static str> {
    let mut report = Report::default();
    let ns = crate::fs::namespace();
    let ctx = ns.context();
    let rw = OpenFlags {
        read: true,
        write: true,
        ..OpenFlags::default()
    };
    let ro = OpenFlags {
        read: true,
        ..OpenFlags::default()
    };
    let open = |flags: &OpenFlags| {
        ns.open(&ctx, None, b"/dev/ferrix-crashes", flags, 0)
            .and_then(crate::fs::devfs::attach_device)
    };

    // (a) Unarmed.
    let a = faulting(unarmed_faulter, None)?;
    if !ended_by_fault(&a, after(AT_ONCE_NANOS)) || !UNARMED_ENDED.load(Ordering::Acquire) {
        return Err("(a) an unarmed fault did not end its process at once by its signal");
    }
    if open(&ro).err() != Some(Errno::EINVAL) {
        return Err("a read-only open of /dev/ferrix-crashes was not EINVAL");
    }
    let file = open(&rw).map_err(|_| "/dev/ferrix-crashes did not open read-write for root")?;
    if open(&rw).err() != Some(Errno::EBUSY) {
        return Err("a second open of /dev/ferrix-crashes was not EBUSY");
    }
    // Nothing queued: EAGAIN without waiting, and a wait a signal ends.
    let mut room = [0_u8; 256];
    if file.io().read_stream(&mut room, true) != Err(Errno::EAGAIN) {
        return Err("a read with nothing queued under O_NONBLOCK was not EAGAIN");
    }
    interrupted_read(&file)?;

    // (f) The deadline, its clock running under the rest.
    let f = faulting(timed_faulter, None)?;
    let _ = held_line(&file, &f)?;

    held_cases(&file, &mut report)?;
    ring_cases(file, &f, &mut report)?;
    Ok(report)
}

/// (b), (c), (d), (i) and (5d): one held process at a time.
#[inline(never)]
fn held_cases(file: &Arc<OpenFile>, report: &mut Report) -> Result<(), &'static str> {
    let ns = crate::fs::namespace();
    let ctx = ns.context();
    let ro = OpenFlags {
        read: true,
        ..OpenFlags::default()
    };
    let mut room = [0_u8; 16];
    // (b) Two threads, held, parked, not stopped; SIGCONT does not release.
    let b = faulting(faulter, Some(parker))?;
    if !wait_for(|| file.poll().readable, after(PATIENCE_NANOS)) {
        return Err("(b) no queue line came for a held process of two threads");
    }
    if file.io().read_stream(&mut room[..8], true) != Err(Errno::EINVAL) {
        return Err("(b) a read into a buffer too small for the line was not EINVAL");
    }
    let line = held_line(file, &b)?;
    if field(&line, b"threads") != Some(2) || field(&line, b"unparked") != Some(0) {
        return Err("(b) the line of a held process of two threads did not say both parked");
    }
    let status = proc_file(b.pid(), "status")?;
    if !status
        .windows(16)
        .any(|window| window == b"t (tracing stop)")
    {
        return Err("(b) /proc/<pid>/status of a held process does not say t (tracing stop)");
    }
    let _ = proc_file(b.pid(), "maps")?;
    let tid = b.pid();
    let _ = proc_file(b.pid(), &alloc::format!("task/{tid}/status"))?;
    let fds = ns.open(
        &ctx,
        None,
        alloc::format!("/proc/{}/fd", b.pid()).as_bytes(),
        &ro,
        0,
    );
    if fds.is_err() {
        return Err("(b) /proc/<pid>/fd of a held process did not open");
    }
    if b.is_stopped() {
        return Err("(b) a held process reads as stopped, which its parent would be told of");
    }
    kill::send(&b, SIGCONT, Origin::Kernel);
    sched::sleep_for(50_000_000);
    if !b.is_held() || b.is_terminated() {
        return Err("(b) SIGCONT released a held process");
    }

    // (c) done, and the writes refused.
    for bad in [&b"done 999999\n"[..], b"done x\n", b"done ", b"kill 1\n"] {
        if file.write(bad).err() != Some(Errno::EINVAL) {
            return Err("(c) a done for no held pid, a malformed or a partial line was not EINVAL");
        }
    }
    let at = crate::timer::now_nanos();
    let _ = file
        .write(alloc::format!("done {}\n", b.pid()).as_bytes())
        .map_err(|_| "(c) done for a held pid was refused")?;
    if !ended_by_fault(&b, after(AT_ONCE_NANOS)) {
        return Err("(c) done did not release a held process at once, by its fault's signal");
    }
    report.done_micros = crate::timer::now_nanos().saturating_sub(at) / 1000;

    // (d) SIGKILL: released at once, still by the fault's signal.
    let d = faulting(faulter, None)?;
    let _ = held_line(file, &d)?;
    kill::send(&d, SIGKILL, Origin::Kernel);
    if !ended_by_fault(&d, after(AT_ONCE_NANOS)) {
        return Err("(d) SIGKILL during a hold did not end it at once with the fault's signal");
    }

    // (i) exit_group(0) from another thread during the hold.
    EXIT_NOW.store(false, Ordering::Release);
    let i = faulting(faulter, Some(exiter))?;
    let _ = held_line(file, &i)?;
    EXIT_NOW.store(true, Ordering::Release);
    if !ended_by_fault(&i, after(AT_ONCE_NANOS)) {
        return Err("(i) an exit_group(0) during a hold did not end it with the fault's signal");
    }

    // (5d) A process holding the reader's description is not held.
    let sharing = process::new_for_check().map_err(|_| "no memory for the sharer")?;
    let _ = sharing
        .files()
        .lock()
        .insert(Arc::clone(file), false)
        .map_err(|_| "the sharer's descriptor table refused the reader")?;
    spawn_fault_in(&sharing)?;
    if !ended_by_fault(&sharing, after(AT_ONCE_NANOS)) || file.poll().readable {
        return Err("(5d) a process holding the reader's description was held");
    }

    Ok(())
}

/// (f)'s end, (h) and (e): the deadline, the full ring and the close.
#[inline(never)]
fn ring_cases(file: Arc<OpenFile>, f: &Process, report: &mut Report) -> Result<(), &'static str> {
    // (f) ends at its deadline.
    if !ended_by_fault(f, after(HOLD_NANOS.saturating_add(MARGIN_NANOS))) {
        return Err("(f) a hold with no done was not released by its deadline");
    }
    let took = crate::timer::now_nanos().saturating_sub(DEADLINE_FAULT_AT.load(Ordering::Acquire));
    if took < HOLD_NANOS.saturating_sub(50_000_000) {
        return Err("(f) a hold with no done ended well before its deadline");
    }
    report.deadline_millis = took / 1_000_000;

    // (h) Sixteen at once; the seventeenth not held, and counted.
    let mut held = Vec::new();
    for _ in 0..SLOTS {
        let process = faulting(faulter, None)?;
        let _ = held_line(&file, &process)?;
        held.push(process);
    }
    let extra = faulting(faulter, None)?;
    if !ended_by_fault(&extra, after(AT_ONCE_NANOS)) {
        return Err("(h) a seventeenth fault at once was held");
    }
    report.held_at_once = held.iter().filter(|process| process.is_held()).count() as u32;
    if report.held_at_once as usize != SLOTS {
        return Err("(h) sixteen faults were not all held at once");
    }
    // The next line counts it: one more held after a done.
    let first = held.remove(0);
    let _ = file
        .write(alloc::format!("done {}\n", first.pid()).as_bytes())
        .map_err(|_| "(h) done was refused")?;
    if !ended_by_fault(&first, after(AT_ONCE_NANOS)) {
        return Err("(h) done did not release a full ring's first");
    }
    let next = faulting(faulter, None)?;
    let line = held_line(&file, &next)?;
    report.dropped = field(&line, b"dropped").unwrap_or(0) as u32;
    if report.dropped != 1 {
        return Err("(h) the line after a fault the full ring did not hold did not count it");
    }
    held.push(next);

    // (e) The reader's close releases every one at once.
    drop(file);
    let deadline = after(AT_ONCE_NANOS);
    if !held.iter().all(|process| ended_by_fault(process, deadline)) {
        return Err("(e) the reader's close did not release every held process at once");
    }
    Ok(())
}

/// A blocking read with a signal pending for the reader: `ERESTARTSYS`, the
/// signal taken back after.
#[inline(never)]
fn interrupted_read(file: &OpenFile) -> Result<(), &'static str> {
    let me = thread::current().ok_or("the crash hold's reader has no thread")?;
    let process = me.process();
    // A handler, never run: a kernel task never goes to user mode, so the
    // signal stays pending until it is taken back below.
    process.with_signals(|signals| signals.install_action(SIGUSR1, 0x1000, 0));
    let _ = process.post_signal_to(&me, SIGUSR1, Origin::Kernel);
    let mut room = [0_u8; 256];
    let answer = file.io().read_stream(&mut room, false);
    let _ = me.with_signals(|shared, own| signal::take_from(shared, own, signal::bit(SIGUSR1)));
    process.with_signals(|signals| signals.install_action(SIGUSR1, SIG_DFL, 0));
    if answer == Err(Errno::ERESTARTSYS) {
        Ok(())
    } else {
        Err("a read waiting for a line was not ended by a signal with ERESTARTSYS")
    }
}

/// A fault task in `process`, which has no task yet.
#[inline(never)]
fn spawn_fault_in(process: &Arc<Process>) -> Result<(), &'static str> {
    crate::syscall::check::spawn_in(process, "crash-hold", faulter, None).map(drop)
}

/// The reader's task.
fn reader(_: usize) {
    let found = cases();
    let failed = found.is_err();
    *OUTCOME.lock() = Some(found);
    // (g) Last: the reader's own process faulting is not held. The fault
    // ends the process, which closes the description; it is taken even when
    // a case above failed, so that the check ends, but then it does not
    // replace that case's sentence.
    let ns = crate::fs::namespace();
    let ctx = ns.context();
    let rw = OpenFlags {
        read: true,
        write: true,
        ..OpenFlags::default()
    };
    let again = ns
        .open(&ctx, None, b"/dev/ferrix-crashes", &rw, 0)
        .map_err(|_| "(g) /dev/ferrix-crashes did not open again after its close");
    let at = crate::timer::now_nanos();
    let _ = super::force_held(SIGSEGV, CODE, ADDRESS, PC);
    let held = crate::timer::now_nanos().saturating_sub(at) > AT_ONCE_NANOS;
    if !failed {
        match &again {
            Ok(_) if held => *OUTCOME.lock() = Some(Err("(g) the reader's own fault was held")),
            Ok(_) => {}
            Err(problem) => *OUTCOME.lock() = Some(Err(problem)),
        }
    }
    drop(again);
    process::leave_current()
}

/// The crash hold's check: every sub-case above.
///
/// # Errors
///
/// The first sub-case that failed.
pub fn check_the_crash_hold() -> Result<Report, &'static str> {
    *OUTCOME.lock() = None;
    let reader_process =
        process::new_for_check().map_err(|_| "no memory for the crash hold's reader")?;
    let _task = crate::syscall::check::spawn_in(&reader_process, "crash-reader", reader, None)?;
    if !ended_by_fault(&reader_process, after(4 * PATIENCE_NANOS)) {
        // A case's own sentence, when one failed, says more than this one.
        if let Some(Err(problem)) = *OUTCOME.lock() {
            return Err(problem);
        }
        return Err("the crash hold's reader never ended, or not by its own fault");
    }
    drop(reader_process);
    sched::sleep_for(10_000_000);
    let _ = sched::reap();
    OUTCOME
        .lock()
        .take()
        .ok_or("the crash hold's reader ended without a result")?
}
